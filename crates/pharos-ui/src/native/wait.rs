//! What the player says while it waits (038 FR-401, V1218).
//!
//! Every wait used to read "Waiting for the server…": the first frame, a
//! seek, a buffer that ran dry, a queue of busy encoders and a group member
//! still loading all looked the same. The caption now names the cause the
//! player can tell, most specific first.

use pharos_client::api::QueueHint;

use super::playback::clock;

/// A wait lasts this long before the player asks the server what it waits
/// behind (`PlaybackGrant.queueHint`): a wait that ends sooner costs no call.
pub const HINT_AFTER_MS: u64 = 2_000;
/// And asks again this often while the wait goes on.
pub const HINT_EVERY_MS: u64 = 2_000;

/// Why the player is waiting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitCause {
    /// The video has not shown its first frame.
    Starting,
    /// A seek waits on the frame it asked for.
    Seeking { to_ms: u64 },
    /// It was playing and ran out of video.
    Buffering,
    /// The server said this playback is queued for an encoder.
    ServerBusy { others: u32 },
    /// Other members of the group are still loading.
    Member { names: Vec<String> },
}

impl WaitCause {
    /// Stable label for the decision line.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Seeking { .. } => "seeking",
            Self::Buffering => "buffering",
            Self::ServerBusy { .. } => "server_busy",
            Self::Member { .. } => "member",
        }
    }

    /// What the viewer reads under the spinner.
    pub fn caption(&self) -> String {
        match self {
            Self::Starting => "Starting…".to_owned(),
            Self::Seeking { to_ms } => format!("Jumping to {}…", clock(*to_ms)),
            Self::Buffering => "Buffering…".to_owned(),
            Self::ServerBusy { others: 0 } => {
                "The server is busy. Your video is waiting for a free encoder…".to_owned()
            }
            Self::ServerBusy { others } => {
                let videos = if *others == 1 { "video" } else { "videos" };
                format!(
                    "The server is busy with {others} other {videos}. Yours is waiting for a free encoder…"
                )
            }
            Self::Member { names } => format!("Waiting for {}…", join(names)),
        }
    }
}

fn join(names: &[String]) -> String {
    match names {
        [] => "the group".to_owned(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// What the player knows about a wait.
#[derive(Debug, Clone, Default)]
pub struct WaitFacts<'a> {
    /// The video has played at least once.
    pub started: bool,
    /// Where a seek still waiting is going.
    pub seeking_to: Option<u64>,
    /// The server's latest answer, while this wait goes on.
    pub hint: Option<QueueHint>,
    /// Group members other than this one who are still loading.
    pub members_waiting: &'a [String],
}

/// The cause to name, most specific first: someone else in the group is
/// what everyone waits on; the server's queue outranks the player's own
/// guess, since a seek or a start held behind busy encoders is waiting on
/// them; then the seek, the first frame, and a buffer that ran dry.
pub fn cause(facts: &WaitFacts<'_>) -> WaitCause {
    if !facts.members_waiting.is_empty() {
        return WaitCause::Member {
            names: facts.members_waiting.to_vec(),
        };
    }
    if let Some(hint) = facts.hint.filter(|h| h.queued > 0) {
        return WaitCause::ServerBusy {
            others: hint.others_queued,
        };
    }
    match (facts.seeking_to, facts.started) {
        (Some(to_ms), _) => WaitCause::Seeking { to_ms },
        (None, false) => WaitCause::Starting,
        (None, true) => WaitCause::Buffering,
    }
}

/// The decision line logged when the caption changes: the cause, and the
/// facts it was read from.
pub fn line(item: &str, cause: &WaitCause, facts: &WaitFacts<'_>) -> String {
    let hint = facts.hint.map_or_else(
        || "none".to_owned(),
        |h| {
            format!(
                "queued:{}/others:{}/busy:{}/waited_ms:{}",
                h.queued, h.others_queued, h.busy, h.waited_ms
            )
        },
    );
    format!(
        "pharos: wait caption item={item} cause={} started={} seeking_to={} hint={hint} members_waiting={}",
        cause.label(),
        facts.started,
        facts.seeking_to.map_or_else(|| "none".to_owned(), |ms| ms.to_string()),
        facts.members_waiting.len(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queued(others: u32) -> Option<QueueHint> {
        Some(QueueHint {
            queued: 1,
            waited_ms: 3_000,
            others_queued: others,
            busy: true,
        })
    }

    /// 038 FR-401, V1218: each wait names its own cause. Disarm check: with
    /// `cause` returning `Starting`, every other arm fails.
    #[test]
    fn each_wait_names_its_cause() {
        let facts = WaitFacts::default();
        assert_eq!(cause(&facts), WaitCause::Starting);
        let playing = WaitFacts {
            started: true,
            ..WaitFacts::default()
        };
        assert_eq!(cause(&playing), WaitCause::Buffering);
        let seek = WaitFacts {
            seeking_to: Some(600_000),
            ..playing.clone()
        };
        assert_eq!(cause(&seek), WaitCause::Seeking { to_ms: 600_000 });
        let busy = WaitFacts {
            hint: queued(2),
            ..seek.clone()
        };
        assert_eq!(cause(&busy), WaitCause::ServerBusy { others: 2 });
        let names = ["Sam".to_owned()];
        let group = WaitFacts {
            members_waiting: &names,
            ..busy.clone()
        };
        assert_eq!(
            cause(&group),
            WaitCause::Member {
                names: names.to_vec()
            }
        );
    }

    /// A hint saying nothing is queued leaves the player's own cause.
    #[test]
    fn an_empty_queue_is_not_server_busy() {
        let facts = WaitFacts {
            started: true,
            hint: Some(QueueHint::default()),
            ..WaitFacts::default()
        };
        assert_eq!(cause(&facts), WaitCause::Buffering);
    }

    #[test]
    fn captions_read_as_sentences() {
        assert_eq!(
            WaitCause::Seeking { to_ms: 754_000 }.caption(),
            "Jumping to 12:34…"
        );
        assert!(WaitCause::ServerBusy { others: 0 }
            .caption()
            .contains("free encoder"));
        assert!(WaitCause::ServerBusy { others: 1 }
            .caption()
            .contains("1 other video."));
        assert!(WaitCause::ServerBusy { others: 3 }
            .caption()
            .contains("3 other videos"));
        let names = |n: &[&str]| {
            WaitCause::Member {
                names: n.iter().map(|s| (*s).to_owned()).collect(),
            }
            .caption()
        };
        assert_eq!(names(&["Sam"]), "Waiting for Sam…");
        assert_eq!(names(&["Sam", "Ali", "Bo"]), "Waiting for Sam, Ali and Bo…");
        for c in [WaitCause::Starting, WaitCause::Buffering] {
            assert!(!c.caption().contains("server"), "{c:?}");
        }
    }

    #[test]
    fn labels_are_distinct() {
        let all = [
            WaitCause::Starting,
            WaitCause::Seeking { to_ms: 0 },
            WaitCause::Buffering,
            WaitCause::ServerBusy { others: 0 },
            WaitCause::Member { names: Vec::new() },
        ];
        let labels: std::collections::HashSet<_> = all.iter().map(WaitCause::label).collect();
        assert_eq!(labels.len(), all.len());
    }

    #[test]
    fn the_line_carries_the_cause_and_its_facts() {
        let facts = WaitFacts {
            started: true,
            hint: queued(4),
            ..WaitFacts::default()
        };
        let line = line("9", &cause(&facts), &facts);
        assert!(line.contains("cause=server_busy"), "{line}");
        assert!(
            line.contains("hint=queued:1/others:4/busy:true/waited_ms:3000"),
            "{line}"
        );
    }
}
