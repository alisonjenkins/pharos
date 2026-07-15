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
//!
//! The token deliberately carries no methods: its VALUE is being un-forgeable
//! except through the gate, so its mere presence in a signature is the contract.

use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

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
