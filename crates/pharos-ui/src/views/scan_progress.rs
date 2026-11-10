//! A library scan as the server's task events tell it (FR-701): the bar on
//! Manage's Libraries and Tasks tabs while one runs, and what the last one
//! did once it ends. Pure: the native app feeds [`ScanView::apply`] from its
//! event subscription and Manage renders [`ScanBar`].

use std::collections::BTreeMap;

use dioxus::prelude::*;

/// The task id the server sends a whole-library scan under; anything else
/// is one library's item id.
pub const LIBRARY_SCAN_TASK: &str = "refresh-library";

/// How a finished scan ended, from `TaskProgress.state`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanEnd {
    Done,
    /// Some folders could not be read.
    Partial,
    /// No folder could be read.
    Failed,
    Cancelled,
}

impl ScanEnd {
    /// `None` while the scan runs, or for a state this build does not know.
    pub fn parse(state: &str) -> Option<Self> {
        match state {
            "done" => Some(Self::Done),
            "partial" => Some(Self::Partial),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }
}

/// What a finished scan did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ScanCounts {
    pub added: u32,
    pub changed: u32,
    pub removed: u32,
    pub failed_roots: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanResult {
    pub end: ScanEnd,
    pub counts: ScanCounts,
}

impl ScanResult {
    /// The line shown once the scan ends.
    pub fn line(&self) -> String {
        let ScanCounts {
            added,
            changed,
            removed,
            failed_roots,
        } = self.counts;
        let tally = format!("added {added}, changed {changed}, removed {removed}");
        let folders = if failed_roots == 1 {
            "1 folder".to_owned()
        } else {
            format!("{failed_roots} folders")
        };
        match self.end {
            ScanEnd::Done => format!("Scan finished: {tally}."),
            ScanEnd::Partial => {
                format!("Scan finished, but {folders} couldn't be read: {tally}.")
            }
            ScanEnd::Failed => format!(
                "Scan failed: {folders} couldn't be read. Check the server log for the reason."
            ),
            ScanEnd::Cancelled => format!("Scan stopped: {tally} before it stopped."),
        }
    }
}

/// The library scan as last heard.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ScanView {
    /// The whole scan's progress, 0 to 1, while one runs.
    pub running: Option<f32>,
    /// One library's progress while its own scan runs, by item id.
    pub libraries: BTreeMap<String, f32>,
    /// How the last scan ended, once one has.
    pub last: Option<ScanResult>,
}

/// The app's [`ScanView`], provided by the native app and read by Manage.
#[derive(Clone, Copy)]
pub struct ScanWatch(pub Signal<ScanView>);

impl ScanView {
    /// Folds one task event in; `false` when nothing shown changed.
    pub fn apply(&mut self, id: &str, progress: f32, state: &str, counts: ScanCounts) -> bool {
        let before = self.clone();
        let end = ScanEnd::parse(state);
        let progress = progress.clamp(0.0, 1.0);
        if id == LIBRARY_SCAN_TASK {
            match end {
                Some(end) => {
                    self.running = None;
                    self.libraries.clear();
                    self.last = Some(ScanResult { end, counts });
                }
                None => self.running = Some(progress),
            }
        } else if end.is_some() {
            self.libraries.remove(id);
        } else {
            self.libraries.insert(id.to_owned(), progress);
        }
        *self != before
    }
}

fn percent(fraction: f32) -> u32 {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let p = (fraction.clamp(0.0, 1.0) * 100.0).round() as u32;
    p
}

/// The running scan's bar, or the last scan's result.
#[component]
pub fn ScanBar(scan: ScanView) -> Element {
    if let Some(fraction) = scan.running {
        let p = percent(fraction);
        return rsx! {
            div { class: "pharos-scan-status", role: "status",
                progress {
                    class: "pharos-scan-bar",
                    max: "100",
                    value: "{p}",
                    aria_label: "Library scan",
                }
                p { "Scanning libraries… {p}%" }
            }
        };
    }
    match scan.last {
        Some(result) => rsx! {
            div {
                class: "pharos-scan-status pharos-scan-result",
                role: "status",
                "data-end": match result.end {
                    ScanEnd::Done => "done",
                    ScanEnd::Partial => "partial",
                    ScanEnd::Failed => "failed",
                    ScanEnd::Cancelled => "cancelled",
                },
                p { "{result.line()}" }
            }
        },
        None => rsx! {},
    }
}

/// One library's bar while its own scan runs.
#[component]
pub fn LibraryScanBar(name: String, fraction: f32) -> Element {
    let p = percent(fraction);
    rsx! {
        div { class: "pharos-scan-status pharos-library-scan", role: "status",
            progress {
                class: "pharos-scan-bar",
                max: "100",
                value: "{p}",
                aria_label: "Scanning {name}",
            }
            p { "Scanning… {p}%" }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counts(added: u32, changed: u32, removed: u32, failed_roots: u32) -> ScanCounts {
        ScanCounts {
            added,
            changed,
            removed,
            failed_roots,
        }
    }

    #[test]
    fn a_scan_runs_then_ends_with_its_counts() {
        let mut v = ScanView::default();
        assert!(v.apply(LIBRARY_SCAN_TASK, 0.4, "running", ScanCounts::default()));
        assert_eq!(v.running, Some(0.4));
        assert!(!v.apply(LIBRARY_SCAN_TASK, 0.4, "running", ScanCounts::default()));
        assert!(v.apply(LIBRARY_SCAN_TASK, 1.0, "done", counts(3, 2, 1, 0)));
        assert_eq!(v.running, None);
        assert_eq!(
            v.last.map(|r| r.line()).as_deref(),
            Some("Scan finished: added 3, changed 2, removed 1.")
        );
    }

    #[test]
    fn a_library_scan_shows_on_its_library_until_it_ends() {
        let mut v = ScanView::default();
        v.apply("lib1", 0.5, "running", ScanCounts::default());
        assert_eq!(v.libraries.get("lib1"), Some(&0.5));
        v.apply("lib1", 1.0, "done", ScanCounts::default());
        assert!(v.libraries.is_empty());
        assert_eq!(v.last, None, "the result comes on the whole scan's task");
    }

    #[test]
    fn a_result_says_when_folders_could_not_be_read() {
        let line = |end, c| ScanResult { end, counts: c }.line();
        assert_eq!(
            line(ScanEnd::Partial, counts(1, 0, 0, 1)),
            "Scan finished, but 1 folder couldn't be read: added 1, changed 0, removed 0."
        );
        assert_eq!(
            line(ScanEnd::Failed, counts(0, 0, 0, 2)),
            "Scan failed: 2 folders couldn't be read. Check the server log for the reason."
        );
        assert_eq!(
            line(ScanEnd::Cancelled, counts(4, 0, 0, 0)),
            "Scan stopped: added 4, changed 0, removed 0 before it stopped."
        );
    }

    #[test]
    fn an_unknown_state_counts_as_running() {
        let mut v = ScanView::default();
        v.apply(LIBRARY_SCAN_TASK, 0.2, "paused", ScanCounts::default());
        assert_eq!(v.running, Some(0.2));
    }
}
