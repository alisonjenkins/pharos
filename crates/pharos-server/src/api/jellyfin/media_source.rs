//! Which file a playback request reads: the item's own, or one of its grouped
//! editions (P34).
//!
//! PlaybackInfo advertises every edition as a `MediaSourceInfo` with Id
//! `{item}` for the primary and `{item}-{edition}` for each alternate, and
//! clients echo the chosen one back as `MediaSourceId`. Every byte route
//! classifies that value here, so "the client asked for an edition" and "we
//! served it" are recorded at one point, by
//! `pharos_media_source_selection_total{route, requested, served}`.

use pharos_core::{AlternateMediaSource, MediaItem, MediaStore};

/// The route family that read the request, for the metric's `route` label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Route {
    Stream,
    Hls,
    PlaybackInfo,
}

impl Route {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Stream => "stream",
            Self::Hls => "hls",
            Self::PlaybackInfo => "playback_info",
        }
    }
}

/// What the request's `MediaSourceId` named.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Requested {
    /// No `MediaSourceId`: the client means the primary file.
    Absent,
    Primary,
    Alternate,
    /// Names neither the primary nor any edition of this item.
    Unknown,
}

impl Requested {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Absent => "absent",
            Self::Primary => "primary",
            Self::Alternate => "alternate",
            Self::Unknown => "unknown",
        }
    }
}

/// Which file the route went on to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Served {
    Primary,
    Alternate,
}

impl Served {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Alternate => "alternate",
        }
    }
}

/// `MediaSourceId` from a query string. Matched case-insensitively: the SDK
/// sends `mediaSourceId`, the older apiclient `MediaSourceId`.
pub(crate) fn requested_id(qs: &str) -> Option<String> {
    qs.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k.eq_ignore_ascii_case("MediaSourceId") && !v.is_empty()).then(|| v.to_string())
    })
}

/// Classify `requested` against the ids PlaybackInfo advertises for `item`.
pub(crate) fn classify<'a>(
    item: &'a MediaItem,
    requested: Option<&str>,
) -> (Requested, Option<&'a AlternateMediaSource>) {
    let Some(raw) = requested else {
        return (Requested::Absent, None);
    };
    if pharos_jellyfin_api::dto::parse_item_id(raw) == Some(item.id) {
        return (Requested::Primary, None);
    }
    // `{item}-{edition}`: the item part is hex with no `-`, the edition slug
    // may contain one (`3d-hsbs`), so split on the first.
    let alt = raw.split_once('-').and_then(|(item_part, edition)| {
        (pharos_jellyfin_api::dto::parse_item_id(item_part) == Some(item.id))
            .then(|| {
                item.probe
                    .alternate_sources
                    .iter()
                    .find(|a| a.id == edition)
            })
            .flatten()
    });
    match alt {
        Some(a) => (Requested::Alternate, Some(a)),
        None => (Requested::Unknown, None),
    }
}

/// Record the selection a route made. Absent/primary is the every-request
/// case and logs at debug; an edition request, or an id we could not place,
/// logs at info with the raw value so the line alone says what was asked.
pub(crate) fn record(
    route: Route,
    item: &MediaItem,
    raw: Option<&str>,
    requested: Requested,
    served: Served,
) {
    metrics::counter!(
        "pharos_media_source_selection_total",
        "route" => route.label(),
        "requested" => requested.label(),
        "served" => served.label(),
    )
    .increment(1);
    match requested {
        Requested::Absent | Requested::Primary => tracing::debug!(
            media.id = item.id,
            route = route.label(),
            requested = requested.label(),
            served = served.label(),
            "media source selected"
        ),
        Requested::Alternate | Requested::Unknown => tracing::info!(
            media.id = item.id,
            route = route.label(),
            media_source_id = raw.unwrap_or_default(),
            requested = requested.label(),
            served = served.label(),
            path = %item.path.display(),
            "media source selected"
        ),
    }
}

/// Classify the request's `MediaSourceId` for `item` and return the file to
/// serve: the edition's path and probe under the item's own id, or the item
/// unchanged.
///
/// An id we cannot place, or an edition probed before its full probe was
/// kept (ADR-0030), is served the primary rather than refused: a stale id
/// after a rescan must not stop playback, and the `requested`/`served` pair
/// says it happened.
pub(crate) fn select(route: Route, mut item: MediaItem, raw: Option<&str>) -> MediaItem {
    let (requested, alt) = classify(&item, raw);
    let Some(alt) = alt else {
        record(route, &item, raw, requested, Served::Primary);
        return item;
    };
    let Some(probe) = alt.probe.as_deref().cloned() else {
        tracing::info!(
            media.id = item.id,
            edition = %alt.id,
            path = %alt.path.display(),
            "edition has no stored probe (probed before schema v9); serving the primary until a rescan"
        );
        record(route, &item, raw, requested, Served::Primary);
        return item;
    };
    let edition = alt.id.clone();
    item.path = alt.path.clone();
    item.probe = probe;
    item.edition = Some(edition);
    record(route, &item, raw, requested, Served::Alternate);
    item
}

/// Load item `id` for a playback route, as the file its query string's
/// `MediaSourceId` selects.
pub(crate) async fn load(
    state: &crate::state::AppState,
    route: Route,
    id: pharos_core::MediaId,
    qs: &str,
) -> Result<MediaItem, actix_web::Error> {
    let item = state.stores.get(id).await.map_err(|e| match e {
        pharos_core::DomainError::NotFound(_) => actix_web::error::ErrorNotFound("not found"),
        other => actix_web::error::ErrorInternalServerError(other.to_string()),
    })?;
    Ok(select(route, item, requested_id(qs).as_deref()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};
    use std::collections::HashSet;

    fn item_with_edition() -> MediaItem {
        let mut item = MediaItem {
            id: 0x2a,
            path: "/m/Film.mkv".into(),
            ..Default::default()
        };
        item.probe.alternate_sources = vec![AlternateMediaSource {
            id: "3d-hsbs".into(),
            path: "/m/Film - 3D.HSBS.mkv".into(),
            probe: Some(Box::new(pharos_core::MediaProbe {
                duration_ms: Some(5_000),
                video_3d_format: Some(pharos_core::Video3DFormat::HalfSideBySide),
                ..Default::default()
            })),
            ..Default::default()
        }];
        item
    }

    fn served_label(
        snapshotter: &pharos_test_metrics::CumulativeSnapshotter,
        requested: &str,
    ) -> Option<String> {
        snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .find_map(|(ck, _, _, _)| {
                let key = ck.key();
                let label = |n: &str| {
                    key.labels()
                        .find(|l| l.key() == n)
                        .map(|l| l.value().to_owned())
                };
                (key.name() == "pharos_media_source_selection_total"
                    && label("requested").as_deref() == Some(requested))
                .then(|| label("served"))
                .flatten()
            })
    }

    #[test]
    fn select_swaps_in_the_editions_file_and_probe() {
        let item = item_with_edition();
        let raw = format!(
            "{}-3d-hsbs",
            pharos_jellyfin_api::dto::wire_item_id(item.id)
        );
        let got = select(Route::Hls, item, Some(&raw));
        assert_eq!(got.id, 0x2a, "the item keeps its own id");
        assert_eq!(got.path, std::path::PathBuf::from("/m/Film - 3D.HSBS.mkv"));
        assert_eq!(got.probe.duration_ms, Some(5_000));
        assert_eq!(got.edition.as_deref(), Some("3d-hsbs"));
    }

    #[test]
    fn select_serves_the_primary_when_it_cannot_serve_the_edition() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = pharos_test_metrics::snapshotter(&recorder);
        let _guard = metrics::set_default_local_recorder(&recorder);
        let wire = pharos_jellyfin_api::dto::wire_item_id(0x2a);

        let mut pre_v9 = item_with_edition();
        pre_v9.probe.alternate_sources[0].probe = None;
        let got = select(Route::Stream, pre_v9, Some(&format!("{wire}-3d-hsbs")));
        assert_eq!(got.path, std::path::PathBuf::from("/m/Film.mkv"));
        assert_eq!(got.edition, None);
        assert_eq!(
            served_label(&snapshotter, "alternate").as_deref(),
            Some("primary")
        );

        let got = select(
            Route::Stream,
            item_with_edition(),
            Some(&format!("{wire}-extended")),
        );
        assert_eq!(got.path, std::path::PathBuf::from("/m/Film.mkv"));
        assert_eq!(
            served_label(&snapshotter, "unknown").as_deref(),
            Some("primary")
        );
    }

    #[test]
    fn requested_id_is_case_insensitive() {
        assert_eq!(
            requested_id("Static=true&mediaSourceId=2a-3d"),
            Some("2a-3d".into())
        );
        assert_eq!(requested_id("MediaSourceId=2a"), Some("2a".into()));
        assert_eq!(requested_id("MediaSourceId=&x=1"), None);
        assert_eq!(requested_id("Static=true"), None);
    }

    #[test]
    fn classify_places_every_advertised_id() {
        let item = item_with_edition();
        let wire = pharos_jellyfin_api::dto::wire_item_id(item.id);
        assert_eq!(classify(&item, None).0, Requested::Absent);
        assert_eq!(classify(&item, Some(&wire)).0, Requested::Primary);
        let (r, alt) = classify(&item, Some(&format!("{wire}-3d-hsbs")));
        assert_eq!(r, Requested::Alternate);
        assert_eq!(alt.map(|a| a.id.as_str()), Some("3d-hsbs"));
        assert_eq!(
            classify(&item, Some(&format!("{wire}-extended"))).0,
            Requested::Unknown
        );
        // An edition id under another item's prefix is not this item's.
        assert_eq!(classify(&item, Some("2b-3d-hsbs")).0, Requested::Unknown);
        // Some clients send the primary's id as a dashed GUID.
        assert_eq!(
            classify(&item, Some("00000000-0000-0000-0000-00000000002a")).0,
            Requested::Primary
        );
    }

    #[test]
    fn labels_are_distinct() {
        let r: HashSet<_> = [
            Requested::Absent,
            Requested::Primary,
            Requested::Alternate,
            Requested::Unknown,
        ]
        .map(Requested::label)
        .into();
        assert_eq!(r.len(), 4);
        let s: HashSet<_> = [Served::Primary, Served::Alternate]
            .map(Served::label)
            .into();
        assert_eq!(s.len(), 2);
        let rt: HashSet<_> = [Route::Stream, Route::Hls, Route::PlaybackInfo]
            .map(Route::label)
            .into();
        assert_eq!(rt.len(), 3);
    }

    #[test]
    fn select_counts_the_edition_request_and_what_was_served() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = pharos_test_metrics::snapshotter(&recorder);
        let _guard = metrics::set_default_local_recorder(&recorder);

        let item = item_with_edition();
        let raw = format!(
            "{}-3d-hsbs",
            pharos_jellyfin_api::dto::wire_item_id(item.id)
        );
        let _ = select(Route::Stream, item, Some(&raw));

        let hit = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .find_map(|(ck, _, _, v)| {
                let key = ck.key();
                let label = |n: &str| {
                    key.labels()
                        .find(|l| l.key() == n)
                        .map(|l| l.value().to_owned())
                };
                (key.name() == "pharos_media_source_selection_total"
                    && label("route").as_deref() == Some("stream")
                    && label("requested").as_deref() == Some("alternate"))
                .then(|| (label("served"), v))
            })
            .expect("selection counter recorded");
        assert_eq!(hit.0.as_deref(), Some("alternate"));
        assert!(matches!(hit.1, DebugValue::Counter(1)), "got {:?}", hit.1);
    }
}
