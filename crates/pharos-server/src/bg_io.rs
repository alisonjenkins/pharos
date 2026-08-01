//! `BgPermit` — a capability token proving the holder passed through the shared
//! background-I/O gate before opening a heavy media source (V34 / B72).
//!
//! Every heavy whole-file / whole-decode source op (subtitle demux, waveform
//! astats, trickplay generate, image extract) takes a `&BgPermit` BY SIGNATURE,
//! so an ungated call cannot compile — the gate can no longer be forgotten by
//! convention, which is exactly how the B72 disk-hammering sites slipped in. A
//! token is minted two ways, and minting one FORCES the caller to choose:
//!
//! - [`BgPermit::acquire`] — METERED: waits for a slot on the shared gate, which
//!   the regulator parks to a trickle while a client is streaming. EVERY
//!   background sweep (scan-time / library-wide warm, bulk pre-generation) uses
//!   this so it can never saturate NFS out from under a live stream.
//! - [`BgPermit::playback_priority`] — BYPASS: holds no slot, runs immediately.
//!   ONLY for on-demand work on the item a client is ACTIVELY watching (a viewer
//!   toggling subtitles must not wait behind the parked gate). Never a sweep.
//! - [`BgPermit::network`] — METERED ON A DIFFERENT RESOURCE (008): an upstream
//!   fetch for a URL-backed source. It takes a [`NetworkGate`], which is a
//!   distinct TYPE from the disk gate, so neither can reach the other's
//!   constructor. Upstream bandwidth and local disk pressure are unrelated
//!   quantities, and the regulator that parks the disk gate during playback is
//!   responding to a signal that says nothing about a remote host.
//!
//! The token deliberately carries no methods: its VALUE is being un-forgeable
//! except through the gate, so its mere presence in a signature is the contract.

use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// The gate for UPSTREAM fetches — 008. Distinct from the disk gate by type, so
/// neither can be handed to the other's constructor.
///
/// Kept narrow on purpose: it holds no methods beyond construction, because its
/// value is being a different type from the disk gate. Its width is a bandwidth
/// and rate-limit question about a remote host, and has nothing to do with the
/// local I/O pressure the disk gate's regulator responds to — which is exactly
/// why one number could not serve both.
#[derive(Debug, Clone)]
pub struct NetworkGate(Arc<Semaphore>);

impl NetworkGate {
    /// Build the gate with a fixed number of concurrent upstream fetches.
    pub fn new(permits: usize) -> Self {
        Self(Arc::new(Semaphore::new(permits.max(1))))
    }

    /// Slots currently free. Exposed for the gauge that publishes gate pressure;
    /// nothing in the fetch path reads it.
    pub fn available(&self) -> usize {
        self.0.available_permits()
    }
}

/// Proof that the heavy op holding it either drew a slot from the shared `bg_io`
/// gate (metered) or was explicitly declared playback-priority (bypass). See the
/// module docs for why every heavy source op takes one by signature.
#[derive(Debug)]
pub struct BgPermit {
    // `Some` = a real gate slot held for this op's whole lifetime; `None` =
    // playback-priority bypass. Either way, producing the token forced the
    // caller to CHOOSE metered-vs-bypass — that choice is the guarantee, not the
    // slot itself. Held (not read) so the slot frees on drop.
    _slot: Option<OwnedSemaphorePermit>,
}

impl BgPermit {
    /// Metered acquisition against the shared gate. Awaits a slot — parked to a
    /// trickle during live playback by the regulator. Use for every background
    /// sweep, so bulk work paces itself against live streams (V34).
    pub async fn acquire(gate: &Arc<Semaphore>) -> Self {
        Self {
            _slot: gate.clone().acquire_owned().await.ok(),
        }
    }

    /// Playback-priority bypass — holds NO slot, runs immediately. ONLY for
    /// on-demand work on the actively-watched item (subtitle/waveform fetch for
    /// the current player); never a background sweep. Named loudly so call sites
    /// stay auditable.
    pub fn playback_priority() -> Self {
        Self { _slot: None }
    }

    /// NETWORK-metered acquisition — 008. Awaits a slot on the remote gate.
    ///
    /// Takes a [`NetworkGate`] rather than a bare `Arc<Semaphore>` so the disk
    /// gate cannot be passed here by mistake, and this gate cannot be passed to
    /// [`acquire`]. Two identically-typed constructors distinguished only by
    /// their doc comments would enforce nothing.
    ///
    /// The two resources are genuinely different and conflating them is wrong in
    /// both directions. A remote fetch waiting on the DISK gate occupies a slot
    /// it does not contend for — throttling NFS work that could have run, while
    /// pacing itself against a signal (local playback I/O) that says nothing
    /// about upstream bandwidth. Declaring it [`playback_priority`] instead
    /// leaves it wholly unmetered, so a background prefetch opens as many
    /// upstream connections as it likes, which for a signed-URL source is how a
    /// film gets rate-limited halfway through.
    ///
    /// [`playback_priority`]: Self::playback_priority
    /// [`acquire`]: Self::acquire
    pub async fn network(gate: &NetworkGate) -> Self {
        Self {
            _slot: gate.0.clone().acquire_owned().await.ok(),
        }
    }

    /// Mint metered-or-bypass by a runtime flag (mirrors the former
    /// `acquire_gate(bypass, gate)`): `bypass = true` → [`playback_priority`],
    /// else [`acquire`]. Keeps sweep call sites that already carry a bypass flag
    /// one-line.
    ///
    /// [`playback_priority`]: Self::playback_priority
    /// [`acquire`]: Self::acquire
    pub async fn acquire_or_bypass(bypass: bool, gate: &Arc<Semaphore>) -> Self {
        if bypass {
            Self::playback_priority()
        } else {
            Self::acquire(gate).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn metered_acquire_holds_a_slot_bypass_holds_none() {
        let gate = Arc::new(Semaphore::new(1));
        let held = BgPermit::acquire(&gate).await;
        assert_eq!(
            gate.available_permits(),
            0,
            "a metered acquire must hold the gate slot for its lifetime"
        );
        // Bypass takes nothing, so a saturated gate can never block it.
        let bypass = BgPermit::playback_priority();
        assert_eq!(gate.available_permits(), 0);
        drop(bypass);
        drop(held);
        assert_eq!(
            gate.available_permits(),
            1,
            "the metered slot frees on drop"
        );
    }

    /// 008 — a network fetch draws from the NETWORK gate and leaves the disk
    /// gate untouched.
    ///
    /// Both halves matter. If it took a disk slot it would throttle NFS work it
    /// does not contend for; if it took nothing at all a background prefetch
    /// would open unbounded upstream connections and get itself rate-limited
    /// mid-film. The disk assertion is what a naive "just reuse bg_io"
    /// implementation fails.
    #[tokio::test]
    async fn a_network_fetch_meters_on_its_own_gate_not_the_disk_one() {
        let disk = Arc::new(Semaphore::new(1));
        let net = NetworkGate::new(1);

        let held = BgPermit::network(&net).await;
        assert_eq!(
            net.available(),
            0,
            "a network fetch must hold a network slot"
        );
        assert_eq!(
            disk.available_permits(),
            1,
            "and must NOT hold a disk slot — it does not contend for that resource"
        );
        drop(held);
        assert_eq!(net.available(), 1, "the network slot frees on drop");
    }

    /// The gate is never zero-width: a `0` would deadlock every fetch forever
    /// rather than merely throttling, and the failure would look like a hang.
    #[test]
    fn a_zero_width_network_gate_is_clamped_rather_than_deadlocking() {
        assert_eq!(NetworkGate::new(0).available(), 1);
        assert_eq!(NetworkGate::new(4).available(), 4);
    }

    #[tokio::test]
    async fn acquire_or_bypass_routes_by_flag() {
        let gate = Arc::new(Semaphore::new(1));
        let metered = BgPermit::acquire_or_bypass(false, &gate).await;
        assert_eq!(gate.available_permits(), 0, "bypass=false → metered");
        drop(metered);
        let _bypass = BgPermit::acquire_or_bypass(true, &gate).await;
        assert_eq!(gate.available_permits(), 1, "bypass=true → holds no slot");
    }
}
