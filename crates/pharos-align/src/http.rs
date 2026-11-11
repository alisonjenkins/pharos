//! The HTTP face of the service: `/align` and `/healthz` with the Python
//! service's statuses and error bodies.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use actix_web::http::header::CONTENT_LENGTH;
use actix_web::http::StatusCode;
use actix_web::{web, HttpRequest, HttpResponse};
use serde::Serialize;

use crate::obs::{set_remote_parent, Metrics, SONGS_TOTAL, SONG_SECONDS};
use crate::plan::{parse_request, resolve, Line};
use crate::service::{Aligned, Engine, SongError};

/// Largest `/align` body read; larger ones answer 413 without being read.
pub const MAX_BODY_BYTES: usize = 1024 * 1024;

/// What every handler shares: one engine, so one lock, across all workers.
pub struct AppState {
    pub engine: Arc<Engine>,
    pub media_root: PathBuf,
    /// `None` answers `/metrics` with an empty body, as before a recorder is installed.
    pub metrics: Option<Metrics>,
}

fn json(status: StatusCode, body: &impl Serialize) -> HttpResponse {
    match serde_json::to_vec(body) {
        Ok(bytes) => HttpResponse::build(status)
            .content_type("application/json; charset=utf-8")
            .body(bytes),
        Err(e) => {
            tracing::error!(error = %e, "answer did not serialise");
            HttpResponse::InternalServerError().finish()
        }
    }
}

fn error(status: StatusCode, message: impl Into<String>) -> HttpResponse {
    #[derive(Serialize)]
    struct Error {
        error: String,
    }
    json(
        status,
        &Error {
            error: message.into(),
        },
    )
}

async fn healthz() -> HttpResponse {
    json(StatusCode::OK, &serde_json::json!({ "ok": true }))
}

async fn metrics(state: web::Data<AppState>) -> HttpResponse {
    let text = state
        .metrics
        .as_ref()
        .map(Metrics::render)
        .unwrap_or_default();
    HttpResponse::Ok()
        .content_type("text/plain; version=0.0.4")
        .body(text)
}

async fn not_found() -> HttpResponse {
    error(StatusCode::NOT_FOUND, "not found")
}

async fn align(
    req: HttpRequest,
    payload: web::Payload,
    state: web::Data<AppState>,
) -> HttpResponse {
    let Some(length) = req.headers().get(CONTENT_LENGTH) else {
        return error(StatusCode::BAD_REQUEST, "Content-Length header required");
    };
    let Some(length) = length
        .to_str()
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
    else {
        return error(StatusCode::BAD_REQUEST, "invalid Content-Length");
    };
    if length > MAX_BODY_BYTES {
        return error(StatusCode::PAYLOAD_TOO_LARGE, "payload too large");
    }
    let raw = match payload.to_bytes_limited(MAX_BODY_BYTES).await {
        Ok(Ok(raw)) => raw,
        Ok(Err(e)) => return error(StatusCode::BAD_REQUEST, format!("reading body: {e}")),
        Err(_) => return error(StatusCode::PAYLOAD_TOO_LARGE, "payload too large"),
    };
    let body: serde_json::Value = match serde_json::from_slice(&raw) {
        Ok(v) => v,
        Err(e) => return error(StatusCode::BAD_REQUEST, format!("invalid JSON: {e}")),
    };

    let span = tracing::info_span!(
        "aligner.align",
        otel.kind = "server",
        otel.status_code = tracing::field::Empty,
        otel.status_message = tracing::field::Empty,
        aligner.path = tracing::field::Empty,
        aligner.lines = tracing::field::Empty,
        aligner.offset_ms = tracing::field::Empty,
        aligner.words_aligned = tracing::field::Empty,
        aligner.decode_ms = tracing::field::Empty,
        aligner.model_ms = tracing::field::Empty,
        aligner.device = tracing::field::Empty,
        aligner.device_reason = tracing::field::Empty,
        aligner.rss_mib = tracing::field::Empty,
    );
    set_remote_parent(&span, req.headers());
    let fail = |status: StatusCode, message: String| {
        span.record("otel.status_code", "error");
        span.record("otel.status_message", message.as_str());
        error(status, message)
    };
    let (rel_path, lines) = match parse_request(&body) {
        Ok(parsed) => parsed,
        Err(e) => return fail(StatusCode::BAD_REQUEST, e.to_string()),
    };
    let full_path = match resolve(&state.media_root, &rel_path) {
        Ok(p) => p,
        Err(e) => return fail(StatusCode::BAD_REQUEST, e.to_string()),
    };
    span.record("aligner.path", rel_path.as_str());
    span.record("aligner.lines", lines.len());
    // Checked here, not mapped from the decode: ffmpeg missing is a server
    // fault (500), not a missing file.
    if !full_path.is_file() {
        return fail(StatusCode::NOT_FOUND, format!("file not found: {rel_path}"));
    }

    let engine = Arc::clone(&state.engine);
    // The song runs on a blocking thread so /healthz answers meanwhile; it
    // finishes and logs even if the caller stops waiting.
    let done =
        web::block(move || span.in_scope(|| run_song(&engine, &rel_path, &full_path, lines))).await;
    match done {
        Ok(Ok(aligned)) => json(StatusCode::OK, &aligned.answer),
        Ok(Err(e @ SongError::Ffmpeg(_))) => error(StatusCode::UNPROCESSABLE_ENTITY, e.to_string()),
        Ok(Err(SongError::Internal(_))) => {
            error(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
        }
        Err(e) => {
            tracing::error!(error = %e, "alignment thread failed");
            error(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
        }
    }
}

/// One song on the blocking pool, inside its `aligner.align` span: the
/// alignment, then its counters, span attributes and log line.
fn run_song(
    engine: &Engine,
    rel_path: &str,
    full_path: &Path,
    lines: Vec<Line>,
) -> Result<Aligned, SongError> {
    let started = Instant::now();
    let n_lines = lines.len();
    let (result, used, _gpu) = engine.align_song_on(rel_path, full_path, lines);
    let (device, device_reason) = used.map_or(("none", String::new()), |u| (u.device, u.reason));
    let elapsed = started.elapsed();
    let duration_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
    metrics::histogram!(SONG_SECONDS).record(elapsed.as_secs_f64());
    // 0 where /proc cannot be read, so the field is always there to query.
    let rss_mib = crate::obs::rss_mib().unwrap_or(0);
    let span = tracing::Span::current();
    span.record("aligner.rss_mib", rss_mib);
    let outcome = match &result {
        Ok(a) => {
            span.record("aligner.offset_ms", a.answer.offset_ms);
            span.record("aligner.words_aligned", a.words_aligned);
            span.record("aligner.decode_ms", a.phases.decode_ms());
            span.record("aligner.model_ms", a.phases.model_ms());
            span.record("aligner.device", device);
            span.record("aligner.device_reason", device_reason.as_str());
            tracing::info!(
                path = %rel_path,
                lines = n_lines,
                words_aligned = a.words_aligned,
                offset_ms = a.answer.offset_ms,
                duration_ms,
                decode_ms = a.phases.decode_ms(),
                model_ms = a.phases.model_ms(),
                device = %device,
                device_reason = %device_reason,
                rss_mib,
                "song aligned"
            );
            "ok"
        }
        Err(e) => {
            span.record("otel.status_code", "error");
            span.record("otel.status_message", e.to_string().as_str());
            let failed = match e {
                SongError::Ffmpeg(_) => "ffmpeg",
                SongError::Internal(_) => "internal",
            };
            span.record("aligner.device", device);
            span.record("aligner.device_reason", device_reason.as_str());
            tracing::warn!(
                path = %rel_path,
                failed = %failed,
                duration_ms,
                device = %device,
                device_reason = %device_reason,
                rss_mib,
                error = %e,
                "song failed"
            );
            failed
        }
    };
    metrics::counter!(SONGS_TOTAL, "outcome" => outcome).increment(1);
    result
}

/// Registers the routes; `state` must be built once, outside the
/// `HttpServer::new` factory, so every worker shares one engine and one lock.
pub fn configure(state: web::Data<AppState>) -> impl FnOnce(&mut web::ServiceConfig) {
    move |cfg| {
        cfg.app_data(state)
            .service(
                web::resource("/healthz")
                    .route(web::get().to(healthz))
                    .default_service(web::to(not_found)),
            )
            .service(
                web::resource("/metrics")
                    .route(web::get().to(metrics))
                    .default_service(web::to(not_found)),
            )
            .service(
                web::resource("/align")
                    .route(web::post().to(align))
                    .default_service(web::to(not_found)),
            )
            .default_service(web::to(not_found));
    }
}
