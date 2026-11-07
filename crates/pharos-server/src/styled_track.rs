//! The generated "English (Pharos styled)" subtitle track (spec 028 FR-017).
//!
//! A background job directs a source subtitle into an effect plan and stores
//! it per item ([`pharos_core::SubtitleStyling`]). This module decides whether
//! that row may still be served, and where the synthetic track sits in the
//! subtitle index space.

use pharos_core::{MediaItem, SubtitleStyling, SubtitleStylingStore};
use pharos_jellyfin_api::dto::sidecar_base_index;
use pharos_styling::DIRECTOR_VERSION;

use crate::api::jellyfin::subtitles::discover_sidecars;
use crate::subtitle_styling::signature;

/// The styled track's subtitle index: one past the last sidecar (FR-017), so
/// it never collides with a real stream and B71's contiguous rule holds.
pub fn styled_index(probe: &pharos_core::MediaProbe, sidecar_count: u32) -> u32 {
    sidecar_base_index(probe).saturating_add(sidecar_count)
}

/// Whether `stream_index` is the item's styled slot, row or no row: the slot
/// is a fixed place in the index space, so every plain subtitle route can
/// refuse it by name.
pub async fn is_styled_index(item: &MediaItem, stream_index: u32) -> bool {
    if stream_index < sidecar_base_index(&item.probe) {
        return false;
    }
    let count = discover_sidecars(&item.path).await.len();
    stream_index == styled_index(&item.probe, u32::try_from(count).unwrap_or(u32::MAX))
}

/// A native player's subtitle pick as negotiation sees it. The styled track
/// is drawn by the client from `stream.ass`, never burned, so naming it
/// negotiates as its source track (text, drawn by the client). The second
/// value is the styled index when that substitution happened, so the grant
/// can echo the player's own pick. A styled pick whose plan is no longer
/// current (the file changed and the job has not restyled it yet) negotiates
/// as no pick, so the server's default choice applies rather than none.
pub async fn negotiation_choice<S: SubtitleStylingStore>(
    store: &S,
    item: &MediaItem,
    requested: Option<i64>,
) -> (Option<i64>, Option<u32>) {
    let Some(n) = requested.and_then(|n| u32::try_from(n).ok()) else {
        return (requested, None);
    };
    if !is_styled_index(item, n).await {
        return (requested, None);
    }
    match current(store, item).await {
        Some(row) => (Some(i64::from(row.source_index)), Some(n)),
        None => {
            tracing::info!(
                item = %item.id,
                requested = n,
                "styled track asked for without a current plan; negotiating the default subtitle"
            );
            (None, None)
        }
    }
}

/// The item's styling row when its plan can still be served: it has a plan,
/// its director is this one or older, the video file is the one the plan was
/// made from, and a sidecar source still has the signature it had then.
/// A read error is logged and answers `None`.
pub async fn current<S: SubtitleStylingStore>(
    store: &S,
    item: &MediaItem,
) -> Option<SubtitleStyling> {
    let row = match store.subtitle_styling(item.id).await {
        Ok(Some(row)) => row,
        Ok(None) => return None,
        Err(e) => {
            tracing::warn!(item = %item.id, error = %e, "styled subtitle row unreadable");
            return None;
        }
    };
    // A row without a plan is the job's own verdict (undecodable, no source),
    // logged when it was made; every other rejection withdraws a track the
    // viewer may have seen before, so it is logged at info.
    let rejected = |reason: &str, stored: String, found: String| {
        tracing::info!(
            item = %item.id,
            source_index = row.source_index,
            reason,
            stored,
            found,
            "styled track: plan not current"
        );
    };
    row.plan_json.as_ref()?;
    // A plan from an older director is still served until the job replaces
    // it (V613): a director bump re-styles every item in the background, and
    // withdrawing each track until then would hide styled subtitles for
    // hours. Only a plan from a newer director (a rollback) is refused, as
    // this build may not read it.
    if row.director_version > DIRECTOR_VERSION {
        rejected(
            "director version",
            row.director_version.to_string(),
            DIRECTOR_VERSION.to_string(),
        );
        return None;
    }
    if row.director_version < DIRECTOR_VERSION {
        tracing::debug!(
            item = %item.id,
            stored = row.director_version,
            current = DIRECTOR_VERSION,
            "styled track: serving a plan from an older director until it is re-styled"
        );
    }
    let video = signature(&item.path).await;
    if video != Some((row.source_mtime, row.source_size)) {
        rejected(
            "video signature",
            format!("{:?}", (row.source_mtime, row.source_size)),
            format!("{video:?}"),
        );
        return None;
    }
    if let (Some(mtime), Some(size)) = (row.sidecar_mtime, row.sidecar_size) {
        let sidecars = discover_sidecars(&item.path).await;
        let base = sidecar_base_index(&item.probe);
        let path = row
            .source_index
            .checked_sub(base)
            .and_then(|offset| usize::try_from(offset).ok())
            .and_then(|offset| sidecars.get(offset));
        let Some((path, _)) = path else {
            rejected(
                "sidecar missing",
                row.source_index.to_string(),
                format!("{} sidecars from {base}", sidecars.len()),
            );
            return None;
        };
        let found = signature(path).await;
        if found != Some((mtime, size)) {
            rejected(
                "sidecar signature",
                format!("{:?}", (mtime, size)),
                format!("{found:?}"),
            );
            return None;
        }
    }
    Some(row)
}
