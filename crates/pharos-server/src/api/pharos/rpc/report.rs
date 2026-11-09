//! `Telemetry.report` and the calls that reach stored reports (spec 016 US5,
//! FR-008, ADR-0099). A report is sent only when its user presses "Report a
//! problem", so the diagnostics opt-in does not gate it. Its bundle is
//! bounded and redacted here before [`crate::client_reports`] stores it, and
//! one INFO line links the report id to the user, device and trace ids.

use std::rc::Rc;
use std::time::{Instant, SystemTime};

use pharos_api::pharos_capnp::telemetry;
use pharos_api::Denial;
use serde_json::Value;

use super::session::Session;
use super::telemetry::{is_credential_key, looks_like_credential};
use crate::client_reports::{NewReport, ReportId};

/// Largest bundle accepted: about 200 breadcrumbs, the last client events
/// and a player timeline fit well inside it.
pub(super) const MAX_BUNDLE_BYTES: usize = 256 * 1024;
/// Longest user note kept; a longer one is cut with its length noted.
const MAX_NOTE_BYTES: usize = 4 * 1024;
/// Longest string kept anywhere in a bundle.
const MAX_BUNDLE_STRING_BYTES: usize = 2 * 1024;
/// Deepest nesting walked; deeper values are replaced, so a hostile bundle
/// can't recurse the walk off the stack.
const MAX_BUNDLE_DEPTH: usize = 16;
/// Trace ids kept from a bundle's `traceIds`.
const MAX_TRACE_IDS: usize = 64;
/// Reports one session may send in a burst before the rate applies.
pub(super) const REPORT_BURST: u32 = 3;
/// Sustained reports per second one session may send. The per-user store
/// cap is the real bound; this only stops a loop.
pub(super) const REPORTS_PER_SECOND: u32 = 1;
/// Seconds a throttled reporter waits: one token refills in 1/rate.
const RETRY_AFTER_SECS: u64 = 1;

/// How one `report` call ended: the `outcome` label of
/// `pharos_ui_reports_total`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Outcome {
    Stored,
    InvalidBundle,
    TooLarge,
    RateLimited,
    NoStore,
    StoreFull,
    UserFull,
    StoreFailed,
}

impl Outcome {
    pub(super) fn label(self) -> &'static str {
        match self {
            Outcome::Stored => "stored",
            Outcome::InvalidBundle => "invalid_bundle",
            Outcome::TooLarge => "too_large",
            Outcome::RateLimited => "rate_limited",
            Outcome::NoStore => "no_store",
            Outcome::StoreFull => "store_full",
            Outcome::UserFull => "user_full",
            Outcome::StoreFailed => "store_failed",
        }
    }

    fn of(e: &crate::client_reports::SaveError) -> Self {
        use crate::client_reports::SaveError as E;
        match e {
            E::StoreFull { .. } => Outcome::StoreFull,
            E::UserFull { .. } => Outcome::UserFull,
            E::List { .. } | E::Random(_) | E::NoFreeId | E::Encode { .. } | E::Write { .. } => {
                Outcome::StoreFailed
            }
        }
    }
}

fn count(outcome: Outcome) {
    metrics::counter!("pharos_ui_reports_total", "outcome" => outcome.label()).increment(1);
}

/// A bundle made safe to keep: credentials replaced, strings cut, depth
/// bounded, and its trace ids read out.
#[derive(Debug, PartialEq)]
pub(super) struct Prepared {
    pub(super) bundle: Value,
    pub(super) trace_ids: Vec<String>,
    pub(super) route: String,
}

/// Why a bundle was refused, naming what was wrong with it.
#[derive(Debug, thiserror::Error, PartialEq)]
pub(super) enum BundleError {
    #[error("bundle is {len} bytes, cap {MAX_BUNDLE_BYTES}")]
    TooLarge { len: usize },
    #[error("bundle is not JSON: {0}")]
    NotJson(String),
    #[error("bundle is JSON {found}, not an object")]
    NotObject { found: &'static str },
}

pub(super) fn prepare_bundle(bytes: &[u8]) -> Result<Prepared, BundleError> {
    if bytes.len() > MAX_BUNDLE_BYTES {
        return Err(BundleError::TooLarge { len: bytes.len() });
    }
    let value: Value =
        serde_json::from_slice(bytes).map_err(|e| BundleError::NotJson(e.to_string()))?;
    let Value::Object(_) = value else {
        return Err(BundleError::NotObject {
            found: json_kind(&value),
        });
    };
    // Read before scrubbing: a trace id is token-shaped, and so is a
    // Jellyfin API key, so `scrub` rightly redacts both inside the bundle.
    let trace_ids = value
        .get("traceIds")
        .and_then(Value::as_array)
        .map(|ids| {
            let mut out: Vec<String> = Vec::new();
            for id in ids
                .iter()
                .filter_map(Value::as_str)
                .filter(|s| is_trace_id(s))
            {
                if out.len() >= MAX_TRACE_IDS {
                    break;
                }
                if !out.iter().any(|seen| seen == id) {
                    out.push(id.to_owned());
                }
            }
            out
        })
        .unwrap_or_default();
    let bundle = scrub(value, 0);
    let route = bundle
        .get("route")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    Ok(Prepared {
        bundle,
        trace_ids,
        route,
    })
}

fn json_kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// `v` with every credential-named key's value and every credential-shaped
/// string replaced by `<redacted>` (V8), strings cut to
/// [`MAX_BUNDLE_STRING_BYTES`] and nesting past [`MAX_BUNDLE_DEPTH`] dropped.
fn scrub(v: Value, depth: usize) -> Value {
    if depth > MAX_BUNDLE_DEPTH {
        return Value::String("<too deep>".to_owned());
    }
    match v {
        Value::String(s) => Value::String(scrub_str(&s)),
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|item| scrub(item, depth.saturating_add(1)))
                .collect(),
        ),
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(key, value)| {
                    let value = if is_credential_key(&key) {
                        Value::String(pharos_api::redact::REDACTED.to_owned())
                    } else {
                        scrub(value, depth.saturating_add(1))
                    };
                    (cut(&key, MAX_BUNDLE_STRING_BYTES), value)
                })
                .collect(),
        ),
        other => other,
    }
}

fn scrub_str(s: &str) -> String {
    if looks_like_credential(s) || url_carries_credential(s) {
        pharos_api::redact::REDACTED.to_owned()
    } else {
        cut(s, MAX_BUNDLE_STRING_BYTES)
    }
}

/// Whether `s` is a URL whose query names a credential (`?api_key=`,
/// `&ApiKey=`, `X-Emby-Token=`, FR-013).
fn url_carries_credential(s: &str) -> bool {
    let Some((_, query)) = s.split_once('?') else {
        return false;
    };
    query
        .split('&')
        .filter_map(|pair| pair.split_once('=').map(|(k, _)| k))
        .any(|k| is_credential_key(k) || k.eq_ignore_ascii_case("tag"))
}

/// `s` cut to at most `max` bytes on a char boundary, its length noted when cut.
fn cut(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let end = (0..=max)
        .rev()
        .find(|i| s.is_char_boundary(*i))
        .unwrap_or(0);
    format!("{}…({} bytes)", s.get(..end).unwrap_or_default(), s.len())
}

/// A W3C trace id: 32 lowercase hex digits, not all zero.
fn is_trace_id(s: &str) -> bool {
    s.len() == 32
        && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        && s.bytes().any(|b| b != b'0')
}

pub(super) async fn report(
    session: Rc<Session>,
    params: telemetry::ReportParams,
    results: &mut telemetry::ReportResults,
) -> Result<(), capnp::Error> {
    let params = params.get()?;
    let note_text = params.get_note()?;
    let bundle_bytes = params.get_bundle()?;
    let mut out = results.get().init_result();
    let user = match session.current_user().await {
        Ok(user) => user,
        Err(denial) => {
            denial.write(out.init_error());
            return Ok(());
        }
    };
    let Some(store) = session.state.client_reports.clone() else {
        count(Outcome::NoStore);
        tracing::warn!(
            user = %user.id.0,
            "client problem report refused: no report store ([obs].report_dir / log_dir unset)"
        );
        Denial::Unavailable {
            dependency: "report store",
        }
        .write(out.init_error());
        return Ok(());
    };
    if !session.report_bucket.borrow_mut().admit(Instant::now()) {
        count(Outcome::RateLimited);
        tracing::warn!(user = %user.id.0, "client problem report refused: rate limited");
        Denial::Throttled {
            name: "telemetry_reports",
            retry_after_secs: RETRY_AFTER_SECS,
        }
        .write(out.init_error());
        return Ok(());
    }
    let prepared = match prepare_bundle(bundle_bytes) {
        Ok(prepared) => prepared,
        Err(e) => {
            let (outcome, denial) = match e {
                BundleError::TooLarge { len } => (
                    Outcome::TooLarge,
                    Denial::Limit {
                        name: "report_bundle_bytes",
                        value: u64::try_from(len).unwrap_or(u64::MAX),
                        max: u64::try_from(MAX_BUNDLE_BYTES).unwrap_or(u64::MAX),
                    },
                ),
                BundleError::NotJson(_) | BundleError::NotObject { .. } => {
                    (Outcome::InvalidBundle, Denial::Invalid { field: "bundle" })
                }
            };
            count(outcome);
            tracing::warn!(
                user = %user.id.0,
                outcome = outcome.label(),
                error = %e,
                "client problem report refused"
            );
            denial.write(out.init_error());
            return Ok(());
        }
    };
    let note = pharos_api::redact::bounded_text(note_text, MAX_NOTE_BYTES);
    let device = session.device_id().await.map(|d| d.to_string());
    let trace_ids = prepared.trace_ids.join(",");
    let saved = store
        .save(
            NewReport {
                owner: user.id,
                user_name: user.name.clone(),
                device: device.clone(),
                note: note.clone(),
                trace_ids: prepared.trace_ids,
                bundle: prepared.bundle,
            },
            SystemTime::now(),
        )
        .await;
    match saved {
        Ok(id) => {
            count(Outcome::Stored);
            tracing::info!(
                report_id = %id,
                user = %user.id.0,
                device = device.as_deref().unwrap_or(""),
                route = %prepared.route,
                trace_ids = %trace_ids,
                note_bytes = note.len(),
                bundle_bytes = bundle_bytes.len(),
                "client problem report stored"
            );
            out.set_report_id(id.as_str());
        }
        Err(e) => {
            let outcome = Outcome::of(&e);
            count(outcome);
            tracing::warn!(
                user = %user.id.0,
                outcome = outcome.label(),
                trace_ids = %trace_ids,
                error = %e,
                "client problem report not stored"
            );
            let denial = match &e {
                crate::client_reports::SaveError::StoreFull { held, max } => Denial::Limit {
                    name: "stored_reports",
                    value: u64::try_from(*held).unwrap_or(u64::MAX),
                    max: u64::try_from(*max).unwrap_or(u64::MAX),
                },
                crate::client_reports::SaveError::UserFull { held, max, .. } => Denial::Limit {
                    name: "my_stored_reports",
                    value: u64::try_from(*held).unwrap_or(u64::MAX),
                    max: u64::try_from(*max).unwrap_or(u64::MAX),
                },
                _ => Denial::Unavailable {
                    dependency: "report store",
                },
            };
            denial.write(out.init_error());
        }
    }
    Ok(())
}

pub(super) async fn list_reports(
    session: Rc<Session>,
    params: telemetry::ListReportsParams,
    results: &mut telemetry::ListReportsResults,
) -> Result<(), capnp::Error> {
    let everyone = params.get()?.get_everyone();
    let out = results.get().init_result();
    let user = match session.current_user().await {
        Ok(user) => user,
        Err(denial) => {
            denial.write(out.init_error());
            return Ok(());
        }
    };
    if everyone && !user.policy.admin {
        Denial::NotPermitted {
            what: "every user's reports",
        }
        .write(out.init_error());
        return Ok(());
    }
    let Some(store) = session.state.client_reports.clone() else {
        Denial::Unavailable {
            dependency: "report store",
        }
        .write(out.init_error());
        return Ok(());
    };
    let summaries = match store.summaries().await {
        Ok(all) => all
            .into_iter()
            .filter(|s| everyone || s.owner == user.id)
            .collect::<Vec<_>>(),
        Err(e) => {
            tracing::warn!(dir = %store.dir().display(), error = %e, "client reports unlistable");
            Denial::Unavailable {
                dependency: "report store",
            }
            .write(out.init_error());
            return Ok(());
        }
    };
    let len = u32::try_from(summaries.len()).unwrap_or(u32::MAX);
    let mut list = out.init_reports(len);
    for (i, s) in (0..len).zip(summaries.iter()) {
        let mut row = list.reborrow().get(i);
        row.set_id(s.id.as_str());
        row.set_received_wall_ms(s.received_wall_ms);
        row.set_user_name(s.user_name.as_str());
        row.set_note(s.note.as_str());
        row.set_bytes(s.bytes);
    }
    Ok(())
}

/// The report `id` when the caller may see it: its reporter or an admin.
/// Anyone else is told it does not exist, so ids can't be probed.
async fn reachable(
    session: &Session,
    id_text: capnp::text::Reader<'_>,
) -> Result<
    (
        crate::client_reports::ReportStore,
        ReportId,
        crate::client_reports::StoredReport,
    ),
    Denial,
> {
    let user = session.current_user().await?;
    let id = id_text
        .to_str()
        .ok()
        .and_then(ReportId::parse)
        .ok_or(Denial::Invalid { field: "id" })?;
    let store = session
        .state
        .client_reports
        .clone()
        .ok_or(Denial::Unavailable {
            dependency: "report store",
        })?;
    let stored = match store.read(&id).await {
        Ok(Some(stored)) => stored,
        Ok(None) => return Err(Denial::NotFound { what: "report" }),
        Err(e) => {
            tracing::warn!(report_id = %id, error = %e, "client report unreadable");
            return Err(Denial::Unavailable {
                dependency: "report store",
            });
        }
    };
    if stored.owner != user.id && !user.policy.admin {
        tracing::info!(
            report_id = %id,
            user = %user.id.0,
            "client report refused: not the reporter or an admin"
        );
        return Err(Denial::NotFound { what: "report" });
    }
    Ok((store, id, stored))
}

pub(super) async fn get_report(
    session: Rc<Session>,
    params: telemetry::GetReportParams,
    results: &mut telemetry::GetReportResults,
) -> Result<(), capnp::Error> {
    let id = params.get()?.get_id()?;
    let mut out = results.get().init_result();
    match reachable(&session, id).await {
        Ok((_, _, stored)) => out.set_document(&stored.document),
        Err(denial) => denial.write(out.init_error()),
    }
    Ok(())
}

pub(super) async fn delete_report(
    session: Rc<Session>,
    params: telemetry::DeleteReportParams,
    results: &mut telemetry::DeleteReportResults,
) -> Result<(), capnp::Error> {
    let id = params.get()?.get_id()?;
    let mut out = results.get().init_result();
    let (store, id, _) = match reachable(&session, id).await {
        Ok(found) => found,
        Err(denial) => {
            denial.write(out.init_error());
            return Ok(());
        }
    };
    match store.delete(&id).await {
        Ok(_) => {
            tracing::info!(report_id = %id, "client problem report deleted");
            out.set_ok(());
        }
        Err(e) => {
            tracing::warn!(report_id = %id, error = %e, "client report delete failed");
            Denial::Unavailable {
                dependency: "report store",
            }
            .write(out.init_error());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::collections::HashSet;

    use super::*;

    const ALL_OUTCOMES: [Outcome; 8] = [
        Outcome::Stored,
        Outcome::InvalidBundle,
        Outcome::TooLarge,
        Outcome::RateLimited,
        Outcome::NoStore,
        Outcome::StoreFull,
        Outcome::UserFull,
        Outcome::StoreFailed,
    ];

    /// Disarm check: a repeated label would merge two outcomes under one
    /// dashboard series.
    #[test]
    fn outcome_labels_are_distinct() {
        let labels: HashSet<_> = ALL_OUTCOMES.iter().map(|o| o.label()).collect();
        assert_eq!(labels.len(), ALL_OUTCOMES.len());
    }

    #[test]
    fn credentials_anywhere_in_a_bundle_are_redacted() {
        let bundle = serde_json::json!({
            "route": "/play/42",
            "accessToken": "plain-but-named-token",
            "breadcrumbs": [
                "GET /Videos/1/master.m3u8?api_key=abc123&static=true",
                "Bearer eyJhbGciOi",
                "clicked play"
            ],
            "player": {"nested": {"X-Emby-Token": "t0k"}, "src": "/Items/1/Images/Primary?tag=ab12"},
            "traceIds": []
        });
        let p = prepare_bundle(bundle.to_string().as_bytes()).unwrap();
        let text = p.bundle.to_string();
        for leaked in [
            "plain-but-named-token",
            "abc123",
            "eyJhbGciOi",
            "t0k",
            "ab12",
        ] {
            assert!(!text.contains(leaked), "{leaked} survived: {text}");
        }
        assert!(text.contains("clicked play"));
        assert_eq!(p.route, "/play/42");
    }

    #[test]
    fn only_wellformed_distinct_trace_ids_are_kept() {
        let good = "4bf92f3577b34da6a3ce929d0e0e4736";
        let bundle = serde_json::json!({
            "traceIds": [good, good, "00000000000000000000000000000000", "XYZ", 7, "4BF92F3577B34DA6A3CE929D0E0E4736"]
        });
        let p = prepare_bundle(bundle.to_string().as_bytes()).unwrap();
        assert_eq!(p.trace_ids, vec![good.to_owned()]);
    }

    #[test]
    fn a_bundle_that_is_not_a_json_object_is_refused_with_what_it_was() {
        assert_eq!(
            prepare_bundle(b"[1,2]"),
            Err(BundleError::NotObject { found: "an array" })
        );
        assert!(matches!(
            prepare_bundle(b"{nope"),
            Err(BundleError::NotJson(_))
        ));
        let big = vec![b' '; MAX_BUNDLE_BYTES + 1];
        assert_eq!(
            prepare_bundle(&big),
            Err(BundleError::TooLarge {
                len: MAX_BUNDLE_BYTES + 1
            })
        );
    }

    #[test]
    fn long_strings_are_cut_and_deep_nesting_is_bounded() {
        let long = "é".repeat(MAX_BUNDLE_STRING_BYTES);
        let mut deep = serde_json::json!("leaf");
        for _ in 0..(MAX_BUNDLE_DEPTH + 4) {
            deep = serde_json::json!([deep]);
        }
        let bundle = serde_json::json!({"long": long, "deep": deep});
        let p = prepare_bundle(bundle.to_string().as_bytes()).unwrap();
        let cut = p.bundle["long"].as_str().unwrap();
        assert!(cut.len() < long.len());
        assert!(cut.ends_with(&format!("…({} bytes)", long.len())));
        assert!(p.bundle.to_string().contains("<too deep>"));
        assert!(!p.bundle.to_string().contains("leaf"));
    }
}
