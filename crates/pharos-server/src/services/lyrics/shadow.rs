//! The shadow aligner (spec 039, R21). Each request the primary aligner
//! answers, a song's lyrics or a video's sung subtitle lines, is sent again
//! to a second service once that answer is stored, and the two answers are
//! compared in full. The comparison is logged and counted; the
//! shadow's answer is never stored or served.

use std::sync::Arc;
use std::time::Instant;

use reqwest::Client;
use tokio::sync::Semaphore;

use super::align::{parse_answer, Aligned};

/// `pharos_lyrics_shadow_total{outcome,path}`: one per stored answer while a
/// shadow is configured.
pub const SHADOW_TOTAL: &str = "pharos_lyrics_shadow_total";

/// The only shadow `aligner_version` that can match: the Rust service's once
/// it answers each line's `score`. A shadow pointed at the Python service
/// (2, or 4 with scores) or at a Rust service without scores (3) cannot pass
/// the cutover gate.
pub const SHADOW_VERSION: u32 = 5;

/// How one song's comparison ended; `label()` is the metric's `outcome`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Every compared field is equal.
    Match,
    /// The answers differ; the log line names the first field that does.
    Mismatch,
    /// No usable answer: a refused connection, a non-2xx status, a body that
    /// does not parse or has the wrong line count, or a version other than 5.
    Error,
    /// The shadow took longer than the primary's 600 s budget.
    Timeout,
    /// Another shadow request was still in flight, so this song was not sent.
    SkippedBusy,
}

impl Outcome {
    pub const ALL: [Self; 5] = [
        Self::Match,
        Self::Mismatch,
        Self::Error,
        Self::Timeout,
        Self::SkippedBusy,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Match => "match",
            Self::Mismatch => "mismatch",
            Self::Error => "error",
            Self::Timeout => "timeout",
            Self::SkippedBusy => "skipped_busy",
        }
    }
}

/// Which aligner path asked; `label()` is the metric's `path`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// A song's synced lyrics, from the background align queue.
    Song,
    /// A video's sung subtitle lines (B1212), asked for and awaited.
    Lines,
}

impl Source {
    pub const ALL: [Self; 2] = [Self::Song, Self::Lines];

    pub fn label(self) -> &'static str {
        match self {
            Self::Song => "song",
            Self::Lines => "lines",
        }
    }
}

/// The first field where the shadow's answer differs from the primary's,
/// with both values. Fields and values are indices, counts and times only,
/// never lyric text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Difference {
    pub field: String,
    pub primary: String,
    pub shadow: String,
}

fn differs(field: String, primary: impl ToString, shadow: impl ToString) -> Option<Difference> {
    Some(Difference {
        field,
        primary: primary.to_string(),
        shadow: shadow.to_string(),
    })
}

/// Most two line scores, in thousandths, may differ and still match: each
/// side rounds its own mean to three places, so a mean near a .0005
/// boundary can land one thousandth apart.
pub const SCORE_TOLERANCE: u16 = 1;

/// A line score in thousandths as the aligner wrote it, or `null`.
fn show_score(score: Option<u16>) -> String {
    score.map_or_else(
        || "null".into(),
        |n| format!("{}.{:03}", n / 1000, n % 1000),
    )
}

/// Compares `offset_ms`, the line count, then line by line each line's word
/// count, each word's `at` and `start_ms`, and the line's score within
/// [`SCORE_TOLERANCE`], in that order; `None` when all are equal. A score on
/// one side only is a difference.
pub fn compare(primary: &Aligned, shadow: &Aligned) -> Option<Difference> {
    if primary.offset_ms != shadow.offset_ms {
        return differs("offset_ms".into(), primary.offset_ms, shadow.offset_ms);
    }
    if primary.lines.len() != shadow.lines.len() {
        return differs("lines".into(), primary.lines.len(), shadow.lines.len());
    }
    for (i, (p, s)) in primary.lines.iter().zip(&shadow.lines).enumerate() {
        if p.len() != s.len() {
            return differs(format!("lines[{i}].words"), p.len(), s.len());
        }
        for (j, (pw, sw)) in p.iter().zip(s).enumerate() {
            if pw.at != sw.at {
                return differs(format!("lines[{i}].words[{j}].at"), pw.at, sw.at);
            }
            if pw.start_ms != sw.start_ms {
                return differs(
                    format!("lines[{i}].words[{j}].start_ms"),
                    pw.start_ms,
                    sw.start_ms,
                );
            }
        }
        let score = |a: &Aligned| a.scores.get(i).copied().flatten();
        let (ps, ss) = (score(primary), score(shadow));
        let close = match (ps, ss) {
            (Some(p), Some(s)) => p.abs_diff(s) <= SCORE_TOLERANCE,
            (None, None) => true,
            _ => false,
        };
        if !close {
            return differs(format!("lines[{i}].score"), show_score(ps), show_score(ss));
        }
    }
    None
}

/// The shadow service, and the one request it may have in flight.
pub struct Shadow {
    http: Client,
    url: String,
    slot: Arc<Semaphore>,
}

impl Shadow {
    /// `http` carries the primary's timeout, so both get the same budget.
    pub fn new(http: Client, base: &str) -> Self {
        Self {
            http,
            url: format!("{}/align", base.trim_end_matches('/')),
            slot: Arc::new(Semaphore::new(1)),
        }
    }

    /// Sends `body`, the request the primary answered with `primary`, unless
    /// a shadow request is still in flight, in which case it is counted as
    /// skipped. Both paths share the one slot. The handle is for tests;
    /// callers drop it.
    pub fn spawn(
        &self,
        media_id: u64,
        source: Source,
        body: Vec<u8>,
        primary: Aligned,
        traceparent: Option<String>,
    ) -> Option<tokio::task::JoinHandle<Outcome>> {
        let Ok(permit) = Arc::clone(&self.slot).try_acquire_owned() else {
            record(Outcome::SkippedBusy, source);
            tracing::info!(
                media.id = media_id,
                path = source.label(),
                outcome = Outcome::SkippedBusy.label(),
                "lyrics: shadow compared"
            );
            return None;
        };
        let (http, url) = (self.http.clone(), self.url.clone());
        Some(tokio::spawn(async move {
            let _permit = permit;
            let outcome =
                ask_and_compare(&http, &url, media_id, source, body, &primary, traceparent).await;
            record(outcome, source);
            outcome
        }))
    }
}

/// Puts every `outcome` × `path` series of [`SHADOW_TOTAL`] on `/metrics` at
/// zero, as `obs::register_static_series` does for the server's own, so
/// `increase()` and `rate()` see each series' first comparison.
pub fn register_series() {
    for source in Source::ALL {
        for outcome in Outcome::ALL {
            metrics::counter!(SHADOW_TOTAL, "outcome" => outcome.label(), "path" => source.label())
                .increment(0);
        }
    }
}

fn record(outcome: Outcome, source: Source) {
    metrics::counter!(SHADOW_TOTAL, "outcome" => outcome.label(), "path" => source.label())
        .increment(1);
}

/// Read apart from the answer: `parse_answer` keeps the answers it would
/// store, and the shadow must hold any of them to its own version.
#[derive(serde::Deserialize)]
struct Version {
    aligner_version: u32,
}

async fn ask_and_compare(
    http: &Client,
    url: &str,
    media_id: u64,
    source: Source,
    body: Vec<u8>,
    primary: &Aligned,
    traceparent: Option<String>,
) -> Outcome {
    let started = Instant::now();
    let failed = |outcome: Outcome, error: String| {
        tracing::warn!(
            media.id = media_id,
            path = source.label(),
            outcome = outcome.label(),
            error = %error,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "lyrics: shadow compared"
        );
        outcome
    };
    let mut request = http
        .post(url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body);
    if let Some(tp) = traceparent {
        request = request.header("traceparent", tp);
    }
    let reply = match request.send().await {
        Ok(r) => r,
        Err(e) if e.is_timeout() => return failed(Outcome::Timeout, format!("request: {e}")),
        Err(e) => return failed(Outcome::Error, format!("request: {e}")),
    };
    let status = reply.status();
    if !status.is_success() {
        return failed(Outcome::Error, format!("status {status}"));
    }
    let text = match reply.text().await {
        Ok(t) => t,
        Err(e) if e.is_timeout() => return failed(Outcome::Timeout, format!("body: {e}")),
        Err(e) => return failed(Outcome::Error, format!("body: {e}")),
    };
    let version = match serde_json::from_str::<Version>(&text) {
        Ok(v) => v.aligner_version,
        Err(e) => return failed(Outcome::Error, format!("invalid aligner response: {e}")),
    };
    if version != SHADOW_VERSION {
        return failed(
            Outcome::Error,
            format!("aligner_version {version}, not {SHADOW_VERSION}"),
        );
    }
    let shadow = match parse_answer(&text, primary.lines.len()) {
        Ok(a) => a,
        Err(e) => return failed(Outcome::Error, e),
    };
    let words: usize = primary.lines.iter().map(Vec::len).sum();
    let elapsed_ms = started.elapsed().as_millis() as u64;
    match compare(primary, &shadow) {
        None => {
            tracing::info!(
                media.id = media_id,
                path = source.label(),
                outcome = Outcome::Match.label(),
                lines = primary.lines.len(),
                words,
                offset_ms = primary.offset_ms,
                elapsed_ms,
                "lyrics: shadow compared"
            );
            Outcome::Match
        }
        Some(d) => {
            tracing::warn!(
                media.id = media_id,
                path = source.label(),
                outcome = Outcome::Mismatch.label(),
                lines = primary.lines.len(),
                words,
                field = %d.field,
                primary = %d.primary,
                shadow = %d.shadow,
                elapsed_ms,
                "lyrics: shadow compared"
            );
            Outcome::Mismatch
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::time::Duration;

    use metrics_util::debugging::{DebugValue, DebuggingRecorder, Snapshotter};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;

    use super::*;
    use crate::services::lyrics::WordStamp;

    fn answer(offset_ms: i64, lines: &[&[(u32, u64)]]) -> Aligned {
        Aligned {
            lines: lines
                .iter()
                .map(|l| {
                    l.iter()
                        .map(|&(at, start_ms)| WordStamp { at, start_ms })
                        .collect()
                })
                .collect(),
            offset_ms,
            scores: vec![None; lines.len()],
        }
    }

    pub(crate) fn body(a: &Aligned, version: u32) -> String {
        let lines: Vec<serde_json::Value> = a
            .lines
            .iter()
            .enumerate()
            .map(|(i, l)| {
                let words: Vec<_> = l
                    .iter()
                    .map(|w| serde_json::json!({"at": w.at, "start_ms": w.start_ms}))
                    .collect();
                let score = a
                    .scores
                    .get(i)
                    .copied()
                    .flatten()
                    .map(|n| f64::from(n) / 1000.0);
                serde_json::json!({ "words": words, "score": score })
            })
            .collect();
        serde_json::json!({"lines": lines, "offset_ms": a.offset_ms, "aligner_version": version})
            .to_string()
    }

    /// Answers one request with `status` and `reply` once `hold` (if any)
    /// fires; the request body comes back on the receiver.
    pub(crate) async fn fake(
        status: u16,
        reply: String,
        hold: Option<oneshot::Receiver<()>>,
    ) -> (String, oneshot::Receiver<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = oneshot::channel();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut got = Vec::new();
            let mut buf = [0u8; 4096];
            let body = loop {
                let n = sock.read(&mut buf).await.unwrap();
                got.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&got);
                if let Some((head, rest)) = text.split_once("\r\n\r\n") {
                    let len: usize = head
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse().unwrap())
                        })
                        .unwrap_or(0);
                    if rest.len() >= len {
                        break rest.as_bytes().to_vec();
                    }
                }
            };
            let _ = tx.send(body);
            if let Some(hold) = hold {
                let _ = hold.await;
            }
            let response = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                reply.len()
            );
            let _ = sock.write_all(response.as_bytes()).await;
        });
        (base, rx)
    }

    pub(crate) fn counts(snapshotter: &Snapshotter) -> Vec<(String, u64)> {
        let mut out: Vec<(String, u64)> = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .filter(|(k, ..)| k.key().name() == SHADOW_TOTAL)
            .filter_map(|(k, _, _, v)| {
                let outcome = k
                    .key()
                    .labels()
                    .find(|l| l.key() == "outcome")?
                    .value()
                    .to_owned();
                match v {
                    DebugValue::Counter(n) => Some((outcome, n)),
                    _ => None,
                }
            })
            .collect();
        out.sort();
        out
    }

    async fn run(base: &str, primary: &Aligned) -> Outcome {
        let shadow = Shadow::new(crate::http_client::client(), base);
        shadow
            .spawn(7, Source::Song, b"{}".to_vec(), primary.clone(), None)
            .unwrap()
            .await
            .unwrap()
    }

    /// `(path, outcome, count)` per series, sorted.
    pub(crate) fn by_path(snapshotter: &Snapshotter) -> Vec<(String, String, u64)> {
        let mut out: Vec<_> = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .filter(|(k, ..)| k.key().name() == SHADOW_TOTAL)
            .filter_map(|(k, _, _, v)| {
                let label = |name: &str| {
                    k.key()
                        .labels()
                        .find(|l| l.key() == name)
                        .map(|l| l.value().to_owned())
                };
                match v {
                    DebugValue::Counter(n) => Some((label("path")?, label("outcome")?, n)),
                    _ => None,
                }
            })
            .collect();
        out.sort();
        out
    }

    #[test]
    fn path_labels_are_distinct() {
        let mut labels: Vec<_> = Source::ALL.iter().map(|s| s.label()).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), Source::ALL.len());
    }

    #[test]
    fn outcome_labels_are_distinct() {
        let mut labels: Vec<_> = Outcome::ALL.iter().map(|o| o.label()).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), Outcome::ALL.len());
    }

    #[test]
    fn compare_names_the_first_differing_field_with_both_values() {
        let a = answer(120, &[&[(0, 1000), (6, 1400)], &[(0, 3000)]]);
        assert_eq!(compare(&a, &a.clone()), None);
        let cases: [(Aligned, &str, &str, &str); 5] = [
            (
                answer(80, &[&[(0, 1000), (6, 1400)], &[(0, 3000)]]),
                "offset_ms",
                "120",
                "80",
            ),
            (answer(120, &[&[(0, 1000), (6, 1400)]]), "lines", "2", "1"),
            (
                answer(120, &[&[(0, 1000)], &[(0, 3000)]]),
                "lines[0].words",
                "2",
                "1",
            ),
            (
                answer(120, &[&[(0, 1000), (5, 1400)], &[(0, 3000)]]),
                "lines[0].words[1].at",
                "6",
                "5",
            ),
            (
                answer(120, &[&[(0, 1000), (6, 1400)], &[(0, 3020)]]),
                "lines[1].words[0].start_ms",
                "3000",
                "3020",
            ),
        ];
        for (shadow, field, p, s) in cases {
            assert_eq!(
                compare(&a, &shadow),
                Some(Difference {
                    field: field.into(),
                    primary: p.into(),
                    shadow: s.into()
                })
            );
        }
    }

    fn scored(mut a: Aligned, scores: &[Option<u16>]) -> Aligned {
        a.scores = scores.to_vec();
        a
    }

    #[test]
    fn line_scores_match_within_a_thousandth_and_only_when_both_sides_have_one() {
        let words: &[&[(u32, u64)]] = &[&[(0, 1000)], &[(0, 3000)], &[]];
        let a = scored(answer(0, words), &[Some(812), Some(1000), None]);
        let near = scored(answer(0, words), &[Some(811), Some(999), None]);
        assert_eq!(compare(&a, &near), None, "one thousandth apart matches");
        let cases: [(&[Option<u16>], &str, &str, &str); 3] = [
            (
                &[Some(814), Some(1000), None],
                "lines[0].score",
                "0.812",
                "0.814",
            ),
            (&[Some(812), None, None], "lines[1].score", "1.000", "null"),
            (
                &[Some(812), Some(1000), Some(0)],
                "lines[2].score",
                "null",
                "0.000",
            ),
        ];
        for (scores, field, p, s) in cases {
            assert_eq!(
                compare(&a, &scored(answer(0, words), scores)),
                Some(Difference {
                    field: field.into(),
                    primary: p.into(),
                    shadow: s.into()
                })
            );
        }
    }

    #[tokio::test]
    async fn a_line_score_two_thousandths_apart_is_a_mismatch_over_http() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _guard = metrics::set_default_local_recorder(&recorder);
        let primary = scored(answer(0, &[&[(0, 900)]]), &[Some(700)]);
        let near = scored(answer(0, &[&[(0, 900)]]), &[Some(701)]);
        let far = scored(answer(0, &[&[(0, 900)]]), &[Some(702)]);
        let (base, _sent) = fake(200, body(&near, SHADOW_VERSION), None).await;
        assert_eq!(run(&base, &primary).await, Outcome::Match);
        let (base, _sent) = fake(200, body(&far, SHADOW_VERSION), None).await;
        assert_eq!(run(&base, &primary).await, Outcome::Mismatch);
        assert_eq!(
            counts(&snapshotter),
            vec![("match".into(), 1), ("mismatch".into(), 1)]
        );
    }

    #[tokio::test]
    async fn an_equal_version_3_answer_is_a_match_and_the_request_is_the_primary_s() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _guard = metrics::set_default_local_recorder(&recorder);
        let primary = answer(-40, &[&[(0, 900), (4, 1300)]]);
        let (base, sent) = fake(200, body(&primary, SHADOW_VERSION), None).await;
        let shadow = Shadow::new(crate::http_client::client(), &base);
        let handle = shadow.spawn(
            7,
            Source::Song,
            br#"{"path":"a"}"#.to_vec(),
            primary.clone(),
            None,
        );
        assert_eq!(handle.unwrap().await.unwrap(), Outcome::Match);
        assert_eq!(sent.await.unwrap(), br#"{"path":"a"}"#.to_vec());
        assert_eq!(counts(&snapshotter), vec![("match".into(), 1)]);
    }

    #[tokio::test]
    async fn a_different_word_time_is_a_mismatch() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _guard = metrics::set_default_local_recorder(&recorder);
        let primary = answer(0, &[&[(0, 900), (4, 1300)]]);
        let other = answer(0, &[&[(0, 900), (4, 1320)]]);
        let (base, _sent) = fake(200, body(&other, SHADOW_VERSION), None).await;
        assert_eq!(run(&base, &primary).await, Outcome::Mismatch);
        assert_eq!(counts(&snapshotter), vec![("mismatch".into(), 1)]);
    }

    #[tokio::test]
    async fn an_equal_answer_from_any_other_version_is_an_error_not_a_match() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _guard = metrics::set_default_local_recorder(&recorder);
        let primary = answer(0, &[&[(0, 900)]]);
        // Python (2), the Rust service before scores (3), Python with them (4).
        for version in [2, 3, 4] {
            let (base, _sent) = fake(200, body(&primary, version), None).await;
            assert_eq!(
                run(&base, &primary).await,
                Outcome::Error,
                "version {version}"
            );
        }
        assert_eq!(counts(&snapshotter), vec![("error".into(), 3)]);
    }

    #[tokio::test]
    async fn a_shadow_that_is_down_or_failing_is_an_error() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _guard = metrics::set_default_local_recorder(&recorder);
        let primary = answer(0, &[&[(0, 900)]]);
        // Bound then dropped: nothing listens there.
        let down = {
            let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
            format!("http://{}", l.local_addr().unwrap())
        };
        assert_eq!(run(&down, &primary).await, Outcome::Error);
        let (base, _sent) = fake(500, r#"{"error":"internal error"}"#.into(), None).await;
        assert_eq!(run(&base, &primary).await, Outcome::Error);
        assert_eq!(counts(&snapshotter), vec![("error".into(), 2)]);
    }

    #[tokio::test]
    async fn a_shadow_slower_than_the_budget_is_a_timeout() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _guard = metrics::set_default_local_recorder(&recorder);
        let primary = answer(0, &[&[(0, 900)]]);
        let (_release, hold) = oneshot::channel();
        let (base, _sent) = fake(200, body(&primary, SHADOW_VERSION), Some(hold)).await;
        let http = crate::http_client::builder()
            .timeout(Duration::from_millis(200))
            .build()
            .unwrap();
        let shadow = Shadow::new(http, &base);
        let handle = shadow
            .spawn(7, Source::Song, b"{}".to_vec(), primary, None)
            .unwrap();
        assert_eq!(handle.await.unwrap(), Outcome::Timeout);
        assert_eq!(counts(&snapshotter), vec![("timeout".into(), 1)]);
    }

    #[tokio::test]
    async fn a_song_finished_while_the_shadow_is_busy_is_skipped_and_counted() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _guard = metrics::set_default_local_recorder(&recorder);
        let primary = answer(0, &[&[(0, 900)]]);
        let (release, hold) = oneshot::channel();
        let (base, sent) = fake(200, body(&primary, SHADOW_VERSION), Some(hold)).await;
        let shadow = Shadow::new(crate::http_client::client(), &base);
        let first = shadow
            .spawn(7, Source::Song, b"{}".to_vec(), primary.clone(), None)
            .unwrap();
        sent.await.unwrap();
        assert!(
            shadow
                .spawn(8, Source::Song, b"{}".to_vec(), primary.clone(), None)
                .is_none(),
            "one in flight"
        );
        release.send(()).unwrap();
        assert_eq!(first.await.unwrap(), Outcome::Match);
        assert_eq!(
            counts(&snapshotter),
            vec![("match".into(), 1), ("skipped_busy".into(), 1)]
        );
        // The slot is free again once the first has finished.
        let (base2, _sent) = fake(200, body(&primary, SHADOW_VERSION), None).await;
        let again = Shadow {
            url: format!("{base2}/align"),
            ..shadow
        };
        assert!(again
            .spawn(9, Source::Song, b"{}".to_vec(), primary, None)
            .is_some());
    }
}
