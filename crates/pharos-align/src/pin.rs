//! Pinning the forward pass to one hardware thread per physical core,
//! filling one L3 domain first (spec 039, R18). Spreading the GEMM threads
//! over SMT siblings and both CCDs cost the cpufwd spike about 40%.

use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinMode {
    /// Pin only when the allowed CPUs span more than one L3 domain or hold
    /// SMT siblings; elsewhere pinning has nothing to choose between.
    Auto,
    On,
    Off,
}

impl PinMode {
    /// The `PHAROS_ALIGNER_PIN` spelling, for the start log.
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::On => "on",
            Self::Off => "off",
        }
    }
}

impl std::str::FromStr for PinMode {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, ()> {
        match s {
            "auto" => Ok(Self::Auto),
            "on" => Ok(Self::On),
            "off" => Ok(Self::Off),
            _ => Err(()),
        }
    }
}

/// One allowed CPU: the first CPU of its core and of its L3 domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cpu {
    pub id: usize,
    pub core: usize,
    pub l3: usize,
}

/// What to pin to and why, for the start log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// `None` leaves the affinity alone.
    pub cpus: Option<Vec<usize>>,
    pub reason: &'static str,
}

/// The first number of a sysfs CPU list such as `0-7,16-23`.
fn first(list: &str) -> Option<usize> {
    list.trim().split([',', '-']).next()?.parse().ok()
}

/// Each allowed CPU's core and L3 domain from `sysfs` (`/sys/devices/system/cpu`).
/// A CPU without the files counts as its own core in one shared domain.
pub fn topology(sysfs: &Path, allowed: &[usize]) -> Vec<Cpu> {
    let read = |cpu: usize, file: &str| {
        std::fs::read_to_string(sysfs.join(format!("cpu{cpu}")).join(file))
            .ok()
            .and_then(|s| first(&s))
    };
    allowed
        .iter()
        .map(|&id| Cpu {
            id,
            core: read(id, "topology/thread_siblings_list").unwrap_or(id),
            l3: read(id, "cache/index3/shared_cpu_list").unwrap_or(0),
        })
        .collect()
}

/// Up to `threads` CPUs, one per core, taken from the largest L3 domain first.
pub fn plan(mode: PinMode, cpus: &[Cpu], threads: usize) -> Plan {
    // The first allowed thread of each core, which need not be the core's
    // lowest-numbered thread when the cgroup allows only a sibling.
    let mut seen = std::collections::BTreeSet::new();
    let primaries: Vec<&Cpu> = cpus.iter().filter(|c| seen.insert(c.core)).collect();
    let smt = primaries.len() < cpus.len();
    let mut by_l3: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for c in &primaries {
        by_l3.entry(c.l3).or_default().push(c.id);
    }
    let domains = by_l3.len();
    match mode {
        PinMode::Off => {
            return Plan {
                cpus: None,
                reason: "off",
            }
        }
        PinMode::Auto if domains <= 1 && !smt => {
            return Plan {
                cpus: None,
                reason: "one_l3_no_smt",
            }
        }
        PinMode::Auto | PinMode::On => {}
    }
    let mut groups: Vec<Vec<usize>> = by_l3.into_values().collect();
    // Largest domain first; equal sizes keep their CPU order.
    groups.sort_by_key(|g| std::cmp::Reverse(g.len()));
    let picked: Vec<usize> = groups.into_iter().flatten().take(threads).collect();
    if picked.is_empty() {
        return Plan {
            cpus: None,
            reason: "no_topology",
        };
    }
    let reason = match (mode, smt, domains > 1) {
        (PinMode::On, _, _) => "forced_on",
        (_, true, true) => "smt_and_l3_domains",
        (_, true, false) => "smt",
        _ => "l3_domains",
    };
    Plan {
        cpus: Some(picked),
        reason,
    }
}

/// The CPUs this process may run on.
pub fn allowed() -> std::io::Result<Vec<usize>> {
    // SAFETY: cpu_set_t is plain data, zeroed is its empty set, and the
    // kernel writes at most size_of::<cpu_set_t>() bytes into it.
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    if unsafe { libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut set) } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    let limit = usize::try_from(libc::CPU_SETSIZE).unwrap_or(0);
    // SAFETY: CPU_ISSET reads the set filled above, at indices below its size.
    Ok((0..limit)
        .filter(|&c| unsafe { libc::CPU_ISSET(c, &set) })
        .collect())
}

/// Restricts this thread, and every thread it starts afterwards, to `cpus`.
/// Call before the forward pass's pools exist, so their threads inherit it.
pub fn apply(cpus: &[usize]) -> std::io::Result<()> {
    // SAFETY: as in `allowed`; CPU_SET writes inside the set for ids below CPU_SETSIZE.
    let mut want: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    let limit = usize::try_from(libc::CPU_SETSIZE).unwrap_or(0);
    for &c in cpus.iter().filter(|&&c| c < limit) {
        unsafe { libc::CPU_SET(c, &mut want) };
    }
    if unsafe { libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &want) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// A fake `/sys/devices/system/cpu` with `l3s` domains of `cores` cores,
    /// each with `smt` threads numbered as Linux does (siblings `n` apart).
    fn sysfs(name: &str, l3s: usize, cores: usize, smt: usize) -> (std::path::PathBuf, Vec<usize>) {
        let root =
            std::env::temp_dir().join(format!("pharos-align-sysfs-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let per_thread = l3s * cores;
        let mut all = Vec::new();
        for t in 0..smt {
            for l in 0..l3s {
                for c in 0..cores {
                    let core = l * cores + c;
                    let id = t * per_thread + core;
                    let dir = root.join(format!("cpu{id}"));
                    std::fs::create_dir_all(dir.join("topology")).unwrap();
                    std::fs::create_dir_all(dir.join("cache/index3")).unwrap();
                    let siblings: Vec<String> = (0..smt)
                        .map(|s| (s * per_thread + core).to_string())
                        .collect();
                    std::fs::write(
                        dir.join("topology/thread_siblings_list"),
                        siblings.join(",") + "\n",
                    )
                    .unwrap();
                    let first = l * cores;
                    let mut l3 = format!("{first}-{}", first + cores - 1);
                    if smt > 1 {
                        l3 +=
                            &format!(",{}-{}", per_thread + first, per_thread + first + cores - 1);
                    }
                    std::fs::write(dir.join("cache/index3/shared_cpu_list"), l3 + "\n").unwrap();
                    all.push(id);
                }
            }
        }
        all.sort_unstable();
        (root, all)
    }

    #[test]
    fn one_l3_without_smt_is_not_pinned_by_auto() {
        // The production node: a VM of 44 vCPUs in one L3 domain, no SMT.
        let (root, cpus) = sysfs("vm", 1, 44, 1);
        let topo = topology(&root, &cpus);
        let auto = plan(PinMode::Auto, &topo, 8);
        assert_eq!(
            auto,
            Plan {
                cpus: None,
                reason: "one_l3_no_smt"
            }
        );
        let on = plan(PinMode::On, &topo, 8);
        assert_eq!(on.cpus, Some((0..8).collect()));
        assert_eq!(on.reason, "forced_on");
        assert_eq!(plan(PinMode::Off, &topo, 8).cpus, None);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn two_l3s_with_smt_pin_one_thread_a_core_in_one_domain() {
        // A desktop: two 8-core CCDs, two threads a core, cpu n and n+16 siblings.
        let (root, cpus) = sysfs("ccd", 2, 8, 2);
        let topo = topology(&root, &cpus);
        let p = plan(PinMode::Auto, &topo, 8);
        assert_eq!(p.cpus, Some((0..8).collect()));
        assert_eq!(p.reason, "smt_and_l3_domains");
        // More threads than one domain holds spill into the next, still no siblings.
        assert_eq!(plan(PinMode::Auto, &topo, 12).cpus, Some((0..12).collect()));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_largest_allowed_domain_is_filled_first() {
        // A pod allowed 2 cores of the first CCD and 5 of the second.
        let (root, _) = sysfs("partial", 2, 8, 2);
        let allowed: Vec<usize> = [6, 7, 9, 10, 11, 12, 13, 22, 23].to_vec();
        let p = plan(PinMode::Auto, &topology(&root, &allowed), 6);
        assert_eq!(p.cpus, Some(vec![9, 10, 11, 12, 13, 6]));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn missing_sysfs_is_one_domain_of_single_thread_cores() {
        let root = std::env::temp_dir().join("pharos-align-sysfs-none");
        let topo = topology(&root, &[0, 1, 2, 3]);
        assert_eq!(plan(PinMode::Auto, &topo, 2).cpus, None);
        assert_eq!(plan(PinMode::On, &topo, 2).cpus, Some(vec![0, 1]));
    }

    #[test]
    fn modes_parse() {
        assert_eq!("auto".parse(), Ok(PinMode::Auto));
        assert_eq!("on".parse(), Ok(PinMode::On));
        assert_eq!("off".parse(), Ok(PinMode::Off));
        assert_eq!("yes".parse::<PinMode>(), Err(()));
        for m in [PinMode::Auto, PinMode::On, PinMode::Off] {
            assert_eq!(m.label().parse(), Ok(m));
        }
    }
}
