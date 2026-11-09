//! Memory held by the server's child processes (B911).
//!
//! The libav `transcode-worker`s are separate processes, so neither
//! `process_resident_memory_bytes` nor `pharos_allocator_bytes` sees them, and
//! the container's cgroup figure mixes them with the server's own heap. On
//! every scrape this sums each live worker's anonymous memory from
//! `/proc/<pid>/smaps_rollup`, so the workers' share of RAM can be read by
//! query.

use std::path::Path;

/// The worker binary as `/proc/<pid>/stat` names it: the kernel keeps only the
/// first 15 bytes of a process name.
const WORKER_COMM: &str = "transcode-worke";

/// Child processes counted, as the `process` label.
pub const WORKER_LABEL: &str = "transcode-worker";

/// Live workers and the sum of their anonymous memory.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ChildMemory {
    pub processes: u64,
    pub anon_bytes: u64,
}

/// `(ppid, comm)` from the text of `/proc/<pid>/stat`. The name sits in
/// parentheses and may itself hold spaces or `)`, so it ends at the last `)`.
fn ppid_and_comm(stat: &str) -> Option<(u32, &str)> {
    let open = stat.find('(')?;
    let close = stat.rfind(')')?;
    let comm = stat.get(open + 1..close)?;
    let mut rest = stat.get(close + 1..)?.split_whitespace();
    let _state = rest.next()?;
    let ppid = rest.next()?.parse().ok()?;
    Some((ppid, comm))
}

/// The `Anonymous:` line of `smaps_rollup`, in bytes.
fn anon_bytes(smaps_rollup: &str) -> Option<u64> {
    let line = smaps_rollup.lines().find(|l| l.starts_with("Anonymous:"))?;
    let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kib * 1024)
}

/// Workers whose parent is `parent`, read from the procfs mounted at
/// `proc_root`. A process that exits mid-read is skipped.
pub fn collect_from(proc_root: &Path, parent: u32) -> ChildMemory {
    let mut out = ChildMemory::default();
    let entries = match std::fs::read_dir(proc_root) {
        Ok(e) => e,
        Err(e) => {
            tracing::debug!(root = %proc_root.display(), error = %e, "child memory: procfs unreadable");
            return out;
        }
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name
            .to_str()
            .is_some_and(|n| n.bytes().all(|b| b.is_ascii_digit()))
        {
            continue;
        }
        let dir = entry.path();
        let Ok(stat) = std::fs::read_to_string(dir.join("stat")) else {
            continue;
        };
        if ppid_and_comm(&stat) != Some((parent, WORKER_COMM)) {
            continue;
        }
        let Some(anon) = std::fs::read_to_string(dir.join("smaps_rollup"))
            .ok()
            .as_deref()
            .and_then(anon_bytes)
        else {
            continue;
        };
        out.processes += 1;
        out.anon_bytes += anon;
    }
    out
}

/// Publish this process's workers as `pharos_child_processes` and
/// `pharos_child_anon_bytes`.
pub fn collect() {
    record(collect_from(Path::new("/proc"), std::process::id()));
}

fn record(mem: ChildMemory) {
    metrics::gauge!("pharos_child_processes", "process" => WORKER_LABEL).set(mem.processes as f64);
    metrics::gauge!("pharos_child_anon_bytes", "process" => WORKER_LABEL)
        .set(mem.anon_bytes as f64);
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn fake_proc(root: &Path, pid: u32, comm: &str, ppid: u32, anon_kib: Option<u64>) {
        let dir = root.join(pid.to_string());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("stat"),
            format!("{pid} ({comm}) S {ppid} {pid} {pid} 0 -1 4194560 0 0"),
        )
        .unwrap();
        if let Some(kib) = anon_kib {
            std::fs::write(
                dir.join("smaps_rollup"),
                format!(
                    "55d0-7ffc ---p 00000000 00:00 0  [rollup]\nRss:  40000 kB\nAnonymous:  {kib} kB\nSwap: 0 kB\n"
                ),
            )
            .unwrap();
        }
    }

    #[test]
    fn sums_only_this_parents_workers() {
        let root = tempfile::tempdir().unwrap();
        fake_proc(root.path(), 10, "transcode-worke", 1, Some(7_000));
        fake_proc(root.path(), 11, "transcode-worke", 1, Some(26_000));
        fake_proc(root.path(), 12, "transcode-worke", 99, Some(5_000));
        fake_proc(root.path(), 13, "pharos", 1, Some(500_000));
        fake_proc(root.path(), 14, "transcode-worke", 1, None);
        std::fs::create_dir_all(root.path().join("self")).unwrap();
        assert_eq!(
            collect_from(root.path(), 1),
            ChildMemory {
                processes: 2,
                anon_bytes: 33_000 * 1024,
            }
        );
    }

    #[test]
    fn a_name_with_spaces_and_parens_still_parses() {
        assert_eq!(
            ppid_and_comm("7 (a b) c)) S 42 7 7 0"),
            Some((42, "a b) c)"))
        );
        assert_eq!(ppid_and_comm("garbage"), None);
    }

    #[test]
    fn missing_procfs_is_zero_not_a_panic() {
        assert_eq!(
            collect_from(Path::new("/nonexistent-procfs"), 1),
            ChildMemory::default()
        );
    }

    #[test]
    fn record_publishes_both_gauges() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};
        let recorder = DebuggingRecorder::new();
        let snap = recorder.snapshotter();
        metrics::with_local_recorder(&recorder, || {
            record(ChildMemory {
                processes: 3,
                anon_bytes: 4096,
            })
        });
        let mut got: Vec<(String, f64)> = snap
            .snapshot()
            .into_vec()
            .into_iter()
            .filter(|(ck, ..)| {
                ck.key()
                    .labels()
                    .any(|l| l.key() == "process" && l.value() == WORKER_LABEL)
            })
            .filter_map(|(ck, .., v)| match v {
                DebugValue::Gauge(g) => Some((ck.key().name().to_owned(), g.into_inner())),
                _ => None,
            })
            .collect();
        got.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            got,
            vec![
                ("pharos_child_anon_bytes".to_owned(), 4096.0),
                ("pharos_child_processes".to_owned(), 3.0),
            ]
        );
    }
}
