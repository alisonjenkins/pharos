//! Admin metadata edits (spec 015 US3, ADR-0097), shared by the admin REST
//! routes and Jellyfin's `POST /Items/{id}` (V172).
//!
//! Signals: `sum by (target, outcome, reason) (increase(pharos_metadata_edit_total[1d]))`
//! and `{namespace="pharos"} |= "metadata edit"`, which names the admin, the
//! target, the fields set and unlocked, and on a refusal the value refused.

use crate::state::AppState;
use pharos_core::{
    DomainError, EditRejected, EditTarget, LockedFields, MediaKind, MetadataEdit, MetadataEditStore,
};

/// How an edit ended. The `label()` strings are a dashboard contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Applied,
    Rejected,
    NotFound,
    Failed,
}

impl Outcome {
    pub const ALL: [Self; 4] = [Self::Applied, Self::Rejected, Self::NotFound, Self::Failed];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Applied => "applied",
            Self::Rejected => "rejected",
            Self::NotFound => "not_found",
            Self::Failed => "failed",
        }
    }
}

/// Why an edit was not applied.
#[derive(Debug, thiserror::Error)]
pub enum EditError {
    #[error(transparent)]
    Rejected(#[from] EditRejected),
    #[error("no item {0}")]
    NotFound(pharos_core::MediaId),
    #[error("store: {0}")]
    Store(String),
}

const fn target_label(target: &EditTarget) -> &'static str {
    match target {
        EditTarget::Item(_) => "item",
        EditTarget::Series { .. } => "series",
    }
}

fn names(fields: LockedFields) -> String {
    fields
        .iter()
        .map(|f| f.label())
        .collect::<Vec<_>>()
        .join(",")
}

fn record(target: &EditTarget, outcome: Outcome, reason: &'static str) {
    metrics::counter!(
        "pharos_metadata_edit_total",
        "target" => target_label(target),
        "outcome" => outcome.label(),
        "reason" => reason,
    )
    .increment(1);
}

/// The refusal label for a body the routes could not read as an edit (a
/// wrong type, an unknown field name): no `EditRejected` uses it.
pub const BAD_SHAPE: &str = "bad_shape";

/// Count and log a body the routes refused before it became an edit, so
/// every refusal is on the same counter and log line as the service's own.
pub fn record_unparsed(admin: &str, target: &EditTarget, why: &dyn std::fmt::Display) {
    record(target, Outcome::Rejected, BAD_SHAPE);
    tracing::warn!(admin, target = ?target, reason = %why, "metadata edit refused");
}

/// Check, write and lock `edit` on `target` for `admin`, returning the
/// target's locked fields afterwards. Lists and pages see the edit at once:
/// the item list cache is invalidated on success.
pub async fn apply(
    state: &AppState,
    admin: &str,
    target: &EditTarget,
    edit: &MetadataEdit,
) -> Result<LockedFields, EditError> {
    let edit = match edit.normalised(target) {
        Ok(e) => e,
        Err(rejected) => {
            record(target, Outcome::Rejected, rejected.label());
            tracing::warn!(
                admin,
                target = ?target,
                field = rejected.field(),
                reason = %rejected,
                "metadata edit refused"
            );
            return Err(rejected.into());
        }
    };
    let edit = match automatic_sort_name(state, target, edit).await {
        Ok(e) => e,
        Err(EditError::NotFound(id)) => {
            record(target, Outcome::NotFound, "none");
            tracing::warn!(admin, target = ?target, "metadata edit: no such item");
            return Err(EditError::NotFound(id));
        }
        Err(e) => {
            record(target, Outcome::Failed, "store");
            tracing::error!(admin, target = ?target, error = %e, "metadata edit: could not read the title for a sort title");
            return Err(e);
        }
    };
    match state.stores.edit_metadata(target, &edit).await {
        Ok(locked) => {
            record(target, Outcome::Applied, "none");
            tracing::info!(
                admin,
                target = ?target,
                set = %names(edit.fields()),
                unlocked = %names(edit.unlock),
                locked = %names(locked),
                "metadata edit applied"
            );
            state.notify_library_changed();
            Ok(locked)
        }
        Err(DomainError::NotFound(id)) => {
            record(target, Outcome::NotFound, "none");
            tracing::warn!(admin, target = ?target, "metadata edit: no such item");
            Err(EditError::NotFound(id))
        }
        Err(e) => {
            record(target, Outcome::Failed, "store");
            tracing::error!(admin, target = ?target, error = %e, "metadata edit failed");
            Err(EditError::Store(e.to_string()))
        }
    }
}

/// A blank sort title means "sort by the title again": the sort title is
/// derived from the new (or stored) title and left unlocked, so a later
/// title change moves it, rather than stored empty and locked.
async fn automatic_sort_name(
    state: &AppState,
    target: &EditTarget,
    mut edit: MetadataEdit,
) -> Result<MetadataEdit, EditError> {
    let (EditTarget::Item(id), Some(None)) = (target, &edit.sort_name) else {
        return Ok(edit);
    };
    let title = match &edit.title {
        Some(t) => t.clone(),
        None => match pharos_core::MediaStore::get(&state.stores, *id).await {
            Ok(item) => item.title,
            Err(DomainError::NotFound(id)) => return Err(EditError::NotFound(id)),
            Err(e) => return Err(EditError::Store(e.to_string())),
        },
    };
    edit.sort_name = Some(Some(pharos_core::sort_name(&title)));
    edit.unlock.insert(pharos_core::EditableField::SortName);
    Ok(edit)
}

/// Resolve an admin-supplied id to what an edit writes: a real item's own
/// row, or a show's metadata row when the id is a show's synthesised id.
/// `None` when the id names neither.
pub async fn resolve_target(state: &AppState, raw: &str) -> Result<Option<EditTarget>, String> {
    if let Some(id) = pharos_jellyfin_api::dto::parse_item_id(raw) {
        return Ok(Some(EditTarget::Item(id)));
    }
    let wanted = crate::api::jellyfin::items::canonical_wire_id(raw).to_ascii_lowercase();
    let all = state.list_items_cached().await.map_err(|e| e.to_string())?;
    Ok(all
        .iter()
        .filter(|i| i.kind == MediaKind::Episode)
        .filter_map(|i| i.series.as_ref())
        .find(|s| {
            pharos_jellyfin_api::dto::series_id_for_key(s.identity_folder(), &s.series_name)
                .eq_ignore_ascii_case(&wanted)
        })
        .map(|s| EditTarget::Series {
            key: s.series_key().to_string(),
            name: s.series_name.clone(),
        }))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[tokio::test]
    async fn every_outcome_is_counted_with_its_reason() {
        use metrics_util::debugging::DebuggingRecorder;
        use pharos_core::MediaStore;
        let recorder = DebuggingRecorder::new();
        let snapshotter = pharos_test_metrics::snapshotter(&recorder);
        let _guard = metrics::set_default_local_recorder(&recorder);
        let stores = crate::state::Stores::connect("sqlite::memory:")
            .await
            .unwrap();
        stores
            .put(pharos_core::MediaItem {
                id: 7,
                path: "/m/a.mkv".into(),
                title: "A".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        let state = AppState::new(stores, "t".into());
        let ok = MetadataEdit {
            title: Some("B".into()),
            ..Default::default()
        };
        apply(&state, "ali", &EditTarget::Item(7), &ok)
            .await
            .unwrap();
        let bad = MetadataEdit {
            production_year: Some(Some(3)),
            ..Default::default()
        };
        assert!(matches!(
            apply(&state, "ali", &EditTarget::Item(7), &bad).await,
            Err(EditError::Rejected(EditRejected::Year(3)))
        ));
        assert!(matches!(
            apply(&state, "ali", &EditTarget::Item(8), &ok).await,
            Err(EditError::NotFound(8))
        ));
        let blank_sort_title = MetadataEdit {
            sort_name: Some(None),
            ..Default::default()
        };
        assert!(
            matches!(
                apply(&state, "ali", &EditTarget::Item(9), &blank_sort_title).await,
                Err(EditError::NotFound(9))
            ),
            "a miss while deriving the sort title is a not-found, not a store failure"
        );
        record_unparsed("ali", &EditTarget::Item(7), &"Fields must be an object");
        let counts: Vec<((String, String), u64)> = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .filter(|(k, ..)| k.key().name() == "pharos_metadata_edit_total")
            .map(|(k, _, _, v)| {
                let label = |n: &str| {
                    k.key()
                        .labels()
                        .find(|l| l.key() == n)
                        .map(|l| l.value().to_string())
                        .unwrap_or_default()
                };
                let n = match v {
                    metrics_util::debugging::DebugValue::Counter(n) => n,
                    _ => 0,
                };
                ((label("outcome"), label("reason")), n)
            })
            .collect();
        let seen: Vec<(String, String)> = counts.iter().map(|(k, _)| k.clone()).collect();
        let not_found = counts
            .iter()
            .find(|(k, _)| k.0 == "not_found")
            .map_or(0, |(_, n)| *n);
        assert_eq!(
            not_found, 2,
            "items 8 and 9 are each one not_found, the sort-title miss included: {counts:?}"
        );
        for want in [
            ("applied", "none"),
            ("rejected", "year"),
            ("not_found", "none"),
            ("rejected", BAD_SHAPE),
        ] {
            assert!(
                seen.contains(&(want.0.to_string(), want.1.to_string())),
                "missing {want:?} in {seen:?}"
            );
        }
        assert!(
            !seen.contains(&("failed".to_string(), "store".to_string())),
            "no call here failed in the store, so a miss while deriving the sort title must not count as one: {seen:?}"
        );
    }

    #[test]
    fn outcome_labels_are_distinct() {
        let mut seen = std::collections::HashSet::new();
        for o in Outcome::ALL {
            assert!(seen.insert(o.label()));
        }
    }
}
