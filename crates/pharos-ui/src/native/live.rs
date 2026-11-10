//! Live updates from the server (017 US2, T050, ADR-0094): one `Events`
//! subscription per connection, resumed from the last `seq` after a
//! reconnect, that reloads the screens whose data an event changed.
//!
//! Screens reload by revision signals: `marks` (a user's watched or
//! favourite state, which the title, browse, person and home screens read)
//! and [`LibraryRev`] (what the library holds: home, browse and the library
//! buttons). Events are coalesced for [`COALESCE_MS`], and a user data event
//! that repeats a state already seen is ignored, so a playing song's
//! progress saves do not reload the page every few seconds.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use dioxus::prelude::*;
use futures_util::StreamExt as _;
use pharos_client::api::{ItemRef, NativeApi};
use pharos_client::events::{EventCursor, EventSubscription, PushEvent};

use super::platform;
use crate::views::scan_progress::{ScanCounts, ScanView};

/// How long events gather before one reload: a folder marked watched sends
/// one event per episode, and a scan sends a burst of library changes.
pub const COALESCE_MS: u32 = 750;

/// Item states remembered to tell a real change from a repeat. Past this the
/// memory starts again, which costs at most one extra reload.
const KNOWN_MAX: usize = 4096;

/// The topics the UI follows; scan progress has no screen yet.
const TOPICS: [&str; 2] = ["userData", "library"];

/// Bumped when the library changed, so screens listing it reload.
#[derive(Clone, Copy)]
pub struct LibraryRev(pub Signal<u32>);

/// Which revisions a batch of events moves.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Refresh {
    pub user_data: bool,
    pub library: bool,
}

impl Refresh {
    fn merge(&mut self, other: Refresh) {
        self.user_data |= other.user_data;
        self.library |= other.library;
    }

    pub fn any(self) -> bool {
        self.user_data || self.library
    }
}

/// The last watched, favourite and play count state seen for each item.
#[derive(Debug, Default)]
pub struct Known(HashMap<ItemRef, (bool, bool, u32)>);

impl Known {
    /// What `batch` changes, remembering its states.
    pub fn plan(&mut self, batch: &[PushEvent]) -> Refresh {
        let mut refresh = Refresh::default();
        for event in batch {
            match event {
                PushEvent::UserData {
                    id,
                    played,
                    favorite,
                    play_count,
                    ..
                } => {
                    if self.0.len() >= KNOWN_MAX {
                        self.0.clear();
                    }
                    let state = (*played, *favorite, *play_count);
                    if self.0.insert(*id, state) != Some(state) {
                        refresh.user_data = true;
                    }
                }
                PushEvent::Library { .. } => refresh.library = true,
                PushEvent::Resync => {
                    self.0.clear();
                    refresh.user_data = true;
                    refresh.library = true;
                }
                PushEvent::Task { .. } => {}
            }
        }
        refresh
    }
}

/// Folds a batch's task events into `scan`, writing only when what Manage
/// shows changed.
fn follow_scan(scan: &mut Signal<ScanView>, batch: &[PushEvent]) {
    let mut next = scan.peek().clone();
    if fold_tasks(&mut next, batch) {
        scan.set(next);
    }
}

fn fold_tasks(view: &mut ScanView, batch: &[PushEvent]) -> bool {
    let mut changed = false;
    for event in batch {
        if let PushEvent::Task {
            id,
            progress,
            state,
            counts,
        } = event
        {
            let counts = ScanCounts {
                added: counts.added,
                changed: counts.changed,
                removed: counts.removed,
                failed_roots: counts.failed_roots,
            };
            changed |= view.apply(id, *progress, state, counts);
        }
    }
    changed
}

/// Follows the server's events for as long as the app lives: subscribes on
/// each signed-in connection `api` brings, from where the last one stopped,
/// and bumps `marks` and `library` when an event changes what they cover.
pub fn use_live_updates(
    api: Signal<Option<NativeApi>>,
    marks: Signal<u32>,
    library: Signal<u32>,
    scan: Signal<ScanView>,
) {
    let cursor = use_hook(EventCursor::default);
    let held = use_hook(|| Rc::new(RefCell::new(None::<EventSubscription>)));
    let reload = use_coroutine(move |mut rx: UnboundedReceiver<Vec<PushEvent>>| {
        let (mut marks, mut library, mut scan) = (marks, library, scan);
        async move {
            let mut known = Known::default();
            while let Some(batch) = rx.next().await {
                follow_scan(&mut scan, &batch);
                let mut refresh = known.plan(&batch);
                platform::sleep_ms(COALESCE_MS).await;
                while let Ok(more) = rx.try_recv() {
                    follow_scan(&mut scan, &more);
                    refresh.merge(known.plan(&more));
                }
                if !refresh.any() {
                    continue;
                }
                platform::info(&format!(
                    "pharos: live reload user_data={} library={}",
                    refresh.user_data, refresh.library
                ));
                if refresh.user_data {
                    marks += 1;
                }
                if refresh.library {
                    library += 1;
                }
            }
        }
    });
    use_effect(move || {
        let Some(api) = api.read().clone() else {
            held.borrow_mut().take();
            return;
        };
        let (cursor, held) = (cursor.clone(), Rc::clone(&held));
        spawn(async move {
            let after = cursor.seq();
            let call = super::app::call("live", "subscribe");
            let made = api
                .subscribe_events(&call, &TOPICS, &cursor, move |batch| reload.send(batch))
                .await;
            match made {
                Ok(sub) => {
                    platform::info(&format!("pharos: live subscribed after_seq={after}"));
                    *held.borrow_mut() = Some(sub);
                }
                // A server from before push: the screens still load as before.
                Err(e) if e.is_unimplemented() => {
                    platform::info("pharos: live updates unavailable: server has no Events");
                }
                Err(e) => platform::warn(&format!(
                    "pharos: live subscribe failed after_seq={after} error={e}"
                )),
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_data(id: u64, played: bool, position_ms: u64) -> PushEvent {
        PushEvent::UserData {
            id: ItemRef::Real(id),
            played,
            favorite: false,
            position_ms,
            play_count: u32::from(played),
        }
    }

    #[test]
    fn a_new_state_reloads_and_a_repeat_does_not() {
        let mut known = Known::default();
        assert_eq!(
            known.plan(&[user_data(1, false, 1_000)]),
            Refresh {
                user_data: true,
                library: false
            }
        );
        // Progress saves of a playing item: only the position moved.
        assert!(!known.plan(&[user_data(1, false, 20_000)]).any());
        assert!(!known.plan(&[user_data(1, false, 40_000)]).any());
        // Finishing it flips the watched mark.
        assert!(known.plan(&[user_data(1, true, 0)]).user_data);
    }

    #[test]
    fn a_library_change_reloads_the_library_only() {
        let mut known = Known::default();
        let refresh = known.plan(&[PushEvent::Library {
            added: vec![ItemRef::Real(9)],
            updated: vec![],
            removed: vec![],
        }]);
        assert_eq!(
            refresh,
            Refresh {
                user_data: false,
                library: true
            }
        );
    }

    #[test]
    fn a_resync_reloads_everything_and_forgets_states() {
        let mut known = Known::default();
        known.plan(&[user_data(1, true, 0)]);
        let refresh = known.plan(&[PushEvent::Resync]);
        assert!(refresh.user_data && refresh.library);
        // Forgotten: the same state counts as new again.
        assert!(known.plan(&[user_data(1, true, 0)]).user_data);
    }

    #[test]
    fn task_events_move_the_scan_view() {
        let mut view = ScanView::default();
        let task = |progress, state: &str, added| PushEvent::Task {
            id: crate::views::scan_progress::LIBRARY_SCAN_TASK.into(),
            progress,
            state: state.into(),
            counts: pharos_client::events::TaskCounts {
                added,
                ..Default::default()
            },
        };
        assert!(fold_tasks(&mut view, &[task(0.3, "running", 0)]));
        assert_eq!(view.running, Some(0.3));
        assert!(!fold_tasks(&mut view, &[user_data(1, true, 0)]));
        assert!(fold_tasks(&mut view, &[task(1.0, "done", 2)]));
        assert_eq!(view.last.map(|r| r.counts.added), Some(2));
    }

    #[test]
    fn task_progress_reloads_nothing() {
        let mut known = Known::default();
        let refresh = known.plan(&[PushEvent::Task {
            id: "lib".into(),
            progress: 0.5,
            state: "running".into(),
            counts: Default::default(),
        }]);
        assert!(!refresh.any());
    }
}
