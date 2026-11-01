//! The library: what a user may browse (spec 017 US4). Both wire layers list
//! libraries through here, so the Jellyfin views and `Library.views` cannot
//! drift apart.

use pharos_core::{LibraryKind, UserPolicy};

use crate::state::AppState;

/// The id of the one placeholder view served when no library and no media
/// root is configured, so a client's sidebar still renders.
pub const ALL_MEDIA_ID: &str = "00000000000000000000000000000000";

/// One library as a user sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryView {
    /// The library's stable wire id (32 hex).
    pub id: String,
    pub name: String,
    /// `Mixed` for an untyped root and for the placeholder.
    pub kind: LibraryKind,
    /// The representative item whose Primary image stands for the library,
    /// when it has one.
    pub primary_tag: Option<String>,
}

/// The libraries `policy` lets its user see (V184). A hidden library is
/// dropped before its image lookup, so it costs nothing.
///
/// Typed libraries reconciled from config come first; without any, one
/// untyped view per configured media root; without those, the
/// [`ALL_MEDIA_ID`] placeholder.
pub async fn views(state: &AppState, policy: &UserPolicy) -> Vec<LibraryView> {
    // Cloned out of the guard: the loop awaits, and a std RwLock guard held
    // across an await can deadlock the executor.
    let libs: Vec<pharos_core::Library> = state.libraries().iter().cloned().collect();
    if !libs.is_empty() {
        // Sequential: the first image lookup warms the whole synth map, so
        // the rest are memo hits.
        let mut out = Vec::with_capacity(libs.len());
        for lib in libs
            .into_iter()
            .filter(|l| policy.allows_library(&l.wire_id))
        {
            let primary_tag = crate::api::jellyfin::images::synth_primary_tag(state, &lib.wire_id)
                .await
                .and_then(|mut tags| tags.remove("Primary"));
            out.push(LibraryView {
                id: lib.wire_id,
                name: lib.name,
                kind: lib.kind,
                primary_tag,
            });
        }
        return out;
    }
    if state.media_roots.is_empty() {
        return if policy.allows_library(ALL_MEDIA_ID) {
            vec![LibraryView {
                id: ALL_MEDIA_ID.to_owned(),
                name: "All Media".to_owned(),
                kind: LibraryKind::Mixed,
                primary_tag: None,
            }]
        } else {
            Vec::new()
        };
    }
    state
        .media_roots
        .iter()
        .filter_map(|root| {
            let id = crate::api::jellyfin::items::library_id_for_root(root);
            policy.allows_library(&id).then(|| LibraryView {
                id,
                name: root
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("Media")
                    .to_owned(),
                kind: LibraryKind::Mixed,
                primary_tag: None,
            })
        })
        .collect()
}
