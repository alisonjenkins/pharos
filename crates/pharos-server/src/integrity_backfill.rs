//! Library integrity sweep — read every file end to end, once, and say which
//! ones are rotten before a viewer finds out.
//!
//! # The gap this closes
//!
//! Nothing in pharos ever read a media file past its header. The scan probes —
//! `avformat_open_input`, which reads enough to answer codec, resolution and
//! duration — and a file whose header is intact but whose body has a hole of
//! null bytes in it answers every one of those questions correctly. It enters
//! the library indistinguishable from a healthy file. The damage surfaces later,
//! mid-episode, as a player that stalls or a client that dies, and the first
//! diagnostic anyone has is a crash report.
//!
//! That is how it went: the Google TV app died 2.8 s into one episode, and the
//! cause turned out to be a zeroed region at byte 8,461,332 breaking the H.264
//! stream. Scanning that season found three of its 26 episodes damaged. The
//! bytes had been unreadable for as long as they had been on disk; nothing had
//! ever looked at them.
//!
//! # Shape
//!
//! A slow, self-terminating sweep in the mould of [`crate::segment_backfill`]:
//! walk the library, skip anything already scanned in its current state, and
//! read the rest one at a time through the libav worker pool behind a `bg_io`
//! permit, so it stands down whenever someone is watching something (V34).
//!
//! The work is *whole-file*, which is the expensive part and not negotiable: a
//! head-only sample reports "clean" for a file that breaks two-thirds of the
//! way through, and a health check that confidently passes a broken file is
//! worse than none. The cost is bounded by memoisation instead — see
//! [`crate::integrity_memo`]. A library is read once, not once per pass.
//!
//! # Signals
//!
//! ```text
//! pharos_media_integrity_total{verdict}   clean | damaged | short | unreadable | error
//! pharos_media_integrity_seconds          per-file scan duration
//! pharos_media_damaged_files              gauge: files currently known damaged
//! ```
//!
//! `verdict="damaged"` moving off zero is the alert. The matching log line
//! carries the path, the position of the first fault and the demuxer's own
//! words for it, because "damaged" alone is another round of guessing.

use crate::bg_io::BgPermit;
use crate::integrity_memo::IntegrityMemo;
use crate::state::Stores;
use pharos_core::{MediaItem, MediaStore};
use pharos_transcode::worker::LibavWorkerPool;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

/// Delay before the first pass so boot I/O settles. Longer than the other
/// sweeps': this one is the heaviest reader in the process and has no reason
/// to be competing with a server that has just come up.
const WARMUP: Duration = Duration::from_secs(300);

/// Interval between passes. A pass over a fully-scanned library is a handful
/// of hash lookups and costs nothing, so this only governs how soon newly
/// added or replaced files get read.
const PASS_INTERVAL: Duration = Duration::from_secs(3600);

/// Files read per pass. The first pass over an unscanned library is a full
/// read of every file in it — hours of NFS traffic — and it must be resumable:
/// the memo is written at the end of each pass, so a cap turns "one enormous
/// uninterruptible sweep whose progress is lost on restart" into a sequence of
/// passes that each bank their results.
const MAX_PER_PASS: usize = 200;

struct Ctx {
    /// Background-work leadership (T85). Gates the sweep so replicas do not
    /// duplicate it.
    bg_leader: Arc<std::sync::atomic::AtomicBool>,
    stores: Stores,
    bg_io: Arc<Semaphore>,
    pool: LibavWorkerPool,
    memo: Arc<IntegrityMemo>,
}

/// Spawn the integrity sweep. Fire-and-forget: a failure aborts only this
/// sweep (logged), never the server.
pub fn spawn(
    stores: Stores,
    bg_leader: Arc<std::sync::atomic::AtomicBool>,
    bg_io: Arc<Semaphore>,
    pool: LibavWorkerPool,
    memo: Arc<IntegrityMemo>,
) {
    tracing::info!(
        memo = %memo.path().display(),
        already_scanned = memo.len(),
        "integrity sweep: spawning whole-file container check"
    );
    tokio::spawn(run_sweep(Ctx {
        stores,
        bg_leader,
        bg_io,
        pool,
        memo,
    }));
}

/// Await background-work leadership. Local copy rather than a dependency on
/// another background module; mirrors `AppState::wait_until_bg_leader`.
async fn wait_until_bg_leader(flag: &std::sync::atomic::AtomicBool) {
    while !flag.load(std::sync::atomic::Ordering::Relaxed) {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

async fn run_sweep(ctx: Ctx) {
    tokio::time::sleep(WARMUP).await;
    // T85 — one replica sweeps, not all of them. This loop lists the WHOLE
    // library and does heavy per-item work (the integrity sweep reads every file end to end), so a second replica
    // running it duplicates the most expensive background I/O in the system
    // against the same shared storage. Unlike the trickplay priority worker
    // there is no per-viewer component here to keep un-gated: every item this
    // touches is bulk work nobody is waiting on.
    wait_until_bg_leader(&ctx.bg_leader).await;
    loop {
        match ctx.stores.list().await {
            Ok(items) => run_pass(&ctx, &items).await,
            Err(e) => tracing::warn!(error = %e, "integrity sweep: item list failed"),
        }
        tokio::time::sleep(PASS_INTERVAL).await;
    }
}

/// Whether this item can be read end to end at all.
///
/// Remote origins (008) have a synthetic path that is not a file, so opening it
/// is ENOENT. That failure would not be self-limiting: the memo keys on a
/// `stat` that never succeeds, so nothing is ever recorded and the item is
/// retried on every pass, each attempt taking a background-I/O permit away
/// from live playback (V134).
fn scannable(it: &MediaItem) -> bool {
    it.origin().local().is_some()
}

async fn run_pass(ctx: &Ctx, items: &[MediaItem]) {
    let mut scanned = 0usize;
    let mut damaged_this_pass = 0usize;

    for it in items {
        if scanned >= MAX_PER_PASS {
            tracing::info!(
                cap = MAX_PER_PASS,
                "integrity sweep: pass cap reached; remaining files continue next pass"
            );
            break;
        }
        if !scannable(it) {
            continue;
        }
        let sig = tokio::fs::metadata(&it.path)
            .await
            .ok()
            .map(|m| (mtime_secs(&m), m.len()));
        if ctx.memo.is_scanned(it.id, sig) {
            continue;
        }

        let started = Instant::now();
        // Hold the gate only for the read itself. This is a whole-file demux
        // over NFS: the regulator parks all but `BG_IO_BUSY` permits while a
        // client streams, so the sweep ducks out of the way of playback rather
        // than competing with it.
        let outcome = {
            let _permit = BgPermit::acquire(&ctx.bg_io).await;
            ctx.pool.integrity(it.path.clone()).await
        };
        let elapsed = started.elapsed();
        metrics::histogram!("pharos_media_integrity_seconds").record(elapsed.as_secs_f64());
        scanned += 1;

        match outcome {
            Ok(report) => {
                metrics::counter!(
                    "pharos_media_integrity_total",
                    "verdict" => report.label(),
                )
                .increment(1);
                if report.is_damaged() {
                    damaged_this_pass += 1;
                    // The failure path states as much as the success path
                    // would, and then some: a bare "damaged" is what sent the
                    // last investigation to a crash dump.
                    tracing::warn!(
                        item = %it.id,
                        path = %it.path.display(),
                        verdict = report.label(),
                        first_fault_ms = report.first_fault_ms,
                        first_fault = report.first_fault.as_deref().unwrap_or("unspecified"),
                        demux_errors = report.demux_errors,
                        corrupt_packets = report.corrupt_packets,
                        read_errors = report.read_errors,
                        scanned_ms = report.scanned_ms,
                        declared_ms = report.declared_ms,
                        elapsed_s = elapsed.as_secs_f64(),
                        "integrity sweep: damaged media file",
                    );
                } else {
                    tracing::debug!(
                        item = %it.id,
                        path = %it.path.display(),
                        packets = report.packets,
                        elapsed_s = elapsed.as_secs_f64(),
                        "integrity sweep: clean",
                    );
                }
                ctx.memo.record(it.id, sig, report);
            }
            Err(e) => {
                // The scan itself failed — a worker died, timed out, or the
                // file could not be opened at all. Deliberately NOT recorded:
                // an infrastructure failure is not a verdict about the file,
                // and writing one would mark a healthy file unexamined-forever
                // on a single transient fault.
                metrics::counter!(
                    "pharos_media_integrity_total",
                    "verdict" => "error",
                )
                .increment(1);
                tracing::warn!(
                    item = %it.id,
                    path = %it.path.display(),
                    error = %e,
                    "integrity sweep: scan failed; will retry next pass",
                );
            }
        }
    }

    if scanned == 0 {
        return;
    }
    let known_damaged = ctx.memo.damaged().len();
    metrics::gauge!("pharos_media_damaged_files").set(known_damaged as f64);
    if let Err(e) = ctx.memo.save() {
        tracing::warn!(error = %e, "integrity sweep: memo save failed; this pass will repeat");
    }
    tracing::info!(
        scanned,
        damaged = damaged_this_pass,
        known_damaged,
        total_recorded = ctx.memo.len(),
        "integrity sweep: pass complete",
    );
}

/// Seconds since the epoch, matching the scanner's own signature convention.
fn mtime_secs(m: &std::fs::Metadata) -> i64 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use pharos_core::MediaKind;

    fn local(path: &str) -> MediaItem {
        MediaItem {
            id: 1,
            kind: MediaKind::Episode,
            path: path.into(),
            title: "ep".into(),
            ..Default::default()
        }
    }

    /// V134 — a remote origin (008) has a synthetic path that is not a file, so
    /// the scan would fail on it forever: nothing is ever recorded, so nothing
    /// is ever skipped, and every pass spends a background-I/O permit on it.
    /// The sweep must not offer one to the pool at all.
    #[test]
    fn a_remote_origin_is_never_read_from_disk() {
        assert!(scannable(&local("/tv/Show/s01e01.mkv")));
        assert!(
            !scannable(&local("ytdlp://youtube/dQw4w9WgXcQ")),
            "a synthetic path is not a file; scanning it can only ever fail"
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod leader_gate_tests {
    use super::*;

    /// T85 — this sweep lists the WHOLE library and does heavy per-item work,
    /// so a replica that is not the background leader must not run it: two
    /// replicas would duplicate the most expensive background I/O in the
    /// system against the same shared storage.
    #[tokio::test]
    async fn the_sweep_waits_for_background_leadership() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let flag = Arc::new(AtomicBool::new(false));
        let f = flag.clone();
        let waiting = tokio::spawn(async move { wait_until_bg_leader(&f).await });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!waiting.is_finished(), "a follower must not sweep");
        flag.store(true, Ordering::Relaxed);
        tokio::time::timeout(std::time::Duration::from_secs(5), waiting)
            .await
            .expect("election must release the gate")
            .expect("the waiter must not panic");
    }
}
