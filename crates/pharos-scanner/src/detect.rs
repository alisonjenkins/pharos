//! LIB-A9 — per-root change-detection tiering: decide the *best* mode each
//! media root can sustain.
//!
//! Native filesystem watching (inotify / kqueue / ReadDirectoryChangesW via
//! the `notify` crate, behind the `watch` feature) does not work on network
//! mounts (NFS / SMB / CIFS) or FUSE filesystems — a common pharos deployment
//! shape (a media share mounted over the LAN). Watching such a root either
//! silently delivers no events or fails to register, so we must detect those
//! up front and fall back to periodic incremental rescans.
//!
//! This module is **feature-independent**: the filesystem-type probe + the
//! magic→watchable decision compile in the default build (no `notify`), so the
//! server can make the tiering decision even when the `watch` feature is off
//! (in which case the answer is always "poll or manual", never "watch").
//!
//! The actual `statfs` probe is Linux-only; other platforms return
//! [`RootWatchability::Unknown`] (treated as watch-eligible — let the watcher
//! itself report `Unsupported` and trigger the runtime fallback). The
//! magic-number → category mapping is split into a pure function
//! ([`watchability_from_magic`]) so it is unit-testable without a real network
//! mount (V6-friendly: we never depend on the test environment's mount table).

use std::path::Path;

/// LIB-A9 — what kind of filesystem backs a media root, from the change-
/// detection point of view. Only the *category* matters here, not the exact
/// filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootWatchability {
    /// A local filesystem that delivers reliable inotify/kqueue events
    /// (ext4, xfs, btrfs, apfs, tmpfs, …). Native watch is preferred.
    Watchable,
    /// A network mount (NFS / SMB / CIFS). The kernel can't deliver change
    /// events for writes made by *other* hosts, so a native watch would miss
    /// most changes — periodic rescan is the correct tier.
    Network,
    /// A FUSE-backed filesystem (sshfs, rclone, gvfs, …). Event delivery is
    /// unreliable / absent; treat like a network mount → periodic rescan.
    Fuse,
    /// Could not determine the filesystem type (non-Linux platform, the
    /// `statfs` call failed, or an unrecognised magic). Treated as
    /// watch-eligible: we *try* the native watch and let it report
    /// `Unsupported` at init if the backend really can't cope, which the
    /// caller turns into the same poll fallback.
    Unknown,
}

impl RootWatchability {
    /// Whether a native filesystem watch is worth *attempting* for this root.
    /// `Network` / `Fuse` short-circuit to the poll tier without even trying
    /// (avoids registering a watch that silently no-ops); `Watchable` and
    /// `Unknown` are attempted (a failing init then falls back at runtime).
    pub fn watch_eligible(self) -> bool {
        matches!(
            self,
            RootWatchability::Watchable | RootWatchability::Unknown
        )
    }
}

// Linux `statfs.f_type` magic constants for the network / fuse filesystems we
// must NOT try to natively watch. Values per the Linux `statfs(2)` man page /
// `linux/magic.h`. Kept as a private table so [`watchability_from_magic`] is a
// pure, testable mapping (no real mount required).
//
// `i64` because `statfs.f_type` is a signed word on Linux (`__fsword_t`); the
// CIFS magic (0xFF534D42) is larger than i32::MAX so we compare as i64 to
// avoid a sign-extension mismatch across the 32/64-bit `f_type` widths.
const NFS_MAGIC: i64 = 0x6969;
const SMB_MAGIC: i64 = 0x517B; // older smbfs
const CIFS_MAGIC: i64 = 0xFF53_4D42; // SMB/CIFS
const SMB2_MAGIC: i64 = 0xFE53_4D42; // smb2 (cifs.ko variants report this)
const FUSE_MAGIC: i64 = 0x6573_5546; // "FUSE"
const FUSE_CTL_MAGIC: i64 = 0x6573_5543;

/// LIB-A9 — pure mapping from a Linux `statfs.f_type` magic to a
/// [`RootWatchability`] category. Split out from the syscall so the tiering
/// decision is unit-testable by injecting a magic directly (no NFS mount
/// needed in CI). Unrecognised magics map to [`RootWatchability::Watchable`]:
/// the overwhelming majority of local filesystems are watchable and we'd
/// rather attempt a watch (and let it fail loudly) than silently downgrade a
/// fast local root to polling.
pub fn watchability_from_magic(magic: i64) -> RootWatchability {
    match magic {
        NFS_MAGIC => RootWatchability::Network,
        SMB_MAGIC | CIFS_MAGIC | SMB2_MAGIC => RootWatchability::Network,
        FUSE_MAGIC | FUSE_CTL_MAGIC => RootWatchability::Fuse,
        _ => RootWatchability::Watchable,
    }
}

/// LIB-A9 — probe the filesystem backing `path` and classify it for the
/// change-detection tiering decision.
///
/// Linux: `statfs(2)` the path and run its `f_type` through
/// [`watchability_from_magic`]. The syscall is a single cheap `statfs` (not a
/// directory walk), but it *is* blocking IO — callers on the async reactor
/// must wrap this in `spawn_blocking` (V5). A failing `statfs` (path vanished,
/// permission) yields [`RootWatchability::Unknown`] so the caller still
/// attempts a watch rather than aborting.
///
/// Non-Linux: always [`RootWatchability::Unknown`] — we don't special-case
/// macOS/Windows network mounts here; the watcher's own init-error path is the
/// fallback trigger on those platforms.
#[cfg(target_os = "linux")]
pub fn detect_root_watchability(path: &Path) -> RootWatchability {
    match statfs_f_type(path) {
        Some(magic) => watchability_from_magic(magic),
        None => RootWatchability::Unknown,
    }
}

/// Non-Linux stub: classification is unavailable, so every root is
/// `Unknown` (watch-eligible, with runtime fallback on init failure).
#[cfg(not(target_os = "linux"))]
pub fn detect_root_watchability(_path: &Path) -> RootWatchability {
    RootWatchability::Unknown
}

/// Raw `statfs` → `f_type` (as `i64`). Returns `None` on syscall failure or a
/// path that can't be encoded as a C string. Linux-only.
#[cfg(target_os = "linux")]
fn statfs_f_type(path: &Path) -> Option<i64> {
    use std::os::unix::ffi::OsStrExt;

    let cpath = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `buf` is a fully-owned `libc::statfs` we hand to the kernel to
    // fill; `cpath` is a valid NUL-terminated C string that outlives the call.
    // We read `f_type` only on success (`rc == 0`).
    let mut buf = std::mem::MaybeUninit::<libc::statfs>::uninit();
    let rc = unsafe { libc::statfs(cpath.as_ptr(), buf.as_mut_ptr()) };
    if rc != 0 {
        return None;
    }
    // SAFETY: a `0` return from `statfs` means `buf` is initialised.
    let st = unsafe { buf.assume_init() };
    // `f_type` is `__fsword_t`, whose width varies by arch (i64 on
    // x86_64-linux, narrower elsewhere). The cast is a no-op on 64-bit but
    // load-bearing for portability, so silence the same-type lint here.
    #[allow(clippy::unnecessary_cast)]
    Some(st.f_type as i64)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn network_magics_map_to_network_tier() {
        // LIB-A9 — NFS + the SMB/CIFS family must classify as Network so the
        // tiering picks periodic rescan, never a native watch (events from
        // other hosts never reach the local kernel).
        assert_eq!(watchability_from_magic(0x6969), RootWatchability::Network); // NFS
        assert_eq!(
            watchability_from_magic(0xFF53_4D42),
            RootWatchability::Network
        ); // CIFS
        assert_eq!(watchability_from_magic(0x517B), RootWatchability::Network); // old smbfs
        assert_eq!(
            watchability_from_magic(0xFE53_4D42),
            RootWatchability::Network
        ); // smb2
    }

    #[test]
    fn fuse_magic_maps_to_fuse_tier() {
        assert_eq!(watchability_from_magic(0x6573_5546), RootWatchability::Fuse);
    }

    #[test]
    fn local_fs_magics_are_watchable() {
        // ext4 (0xEF53), btrfs (0x9123683E), xfs (0x58465342), tmpfs
        // (0x01021994) — none are special-cased, so they fall through to
        // Watchable (the default-watch-attempt branch).
        for magic in [0xEF53_i64, 0x9123_683E, 0x5846_5342, 0x0102_1994] {
            assert_eq!(
                watchability_from_magic(magic),
                RootWatchability::Watchable,
                "magic {magic:#x} should be watchable"
            );
        }
    }

    #[test]
    fn network_and_fuse_are_not_watch_eligible() {
        // The decision the server keys off: only Watchable + Unknown attempt a
        // native watch; Network + Fuse go straight to the poll tier.
        assert!(RootWatchability::Watchable.watch_eligible());
        assert!(RootWatchability::Unknown.watch_eligible());
        assert!(!RootWatchability::Network.watch_eligible());
        assert!(!RootWatchability::Fuse.watch_eligible());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn detect_on_tmpdir_is_watchable_or_unknown() {
        // A real probe of a tmpdir must not classify it as Network/Fuse: a
        // build tmpdir is a local fs (or tmpfs), both Watchable. (If statfs
        // somehow fails we accept Unknown — still watch-eligible.)
        let td = tempfile::TempDir::new().unwrap();
        let w = detect_root_watchability(td.path());
        assert!(
            w.watch_eligible(),
            "a local tmpdir should be watch-eligible, got {w:?}"
        );
    }
}
