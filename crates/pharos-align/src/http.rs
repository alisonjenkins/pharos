//! The HTTP face of the service: `/align` and `/healthz` with the Python
//! service's statuses and error bodies.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use actix_web::http::header::CONTENT_LENGTH;
use actix_web::http::StatusCode;
use actix_web::{web, HttpRequest, HttpResponse};
use serde::Serialize;

use crate::obs::{Metrics, SONGS_TOTAL, SONG_SECONDS};
use crate::plan::{parse_request, resolve};
use crate::service::{Engine, SongError};

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
        aligner.path = tracing::field::Empty,
        aligner.lines = tracing::field::Empty,
    );
    let (rel_path, lines) = match parse_request(&body) {
        Ok(parsed) => parsed,
        Err(e) => return error(StatusCode::BAD_REQUEST, e.to_string()),
    };
    let full_path = match resolve(&state.media_root, &rel_path) {
        Ok(p) => p,
        Err(e) => return error(StatusCode::BAD_REQUEST, e.to_string()),
    };
    span.record("aligner.path", rel_path.as_str());
    span.record("aligner.lines", lines.len());
    // Checked here, not mapped from the decode: ffmpeg missing is a server
    // fault (500), not a missing file.
    if !full_path.is_file() {
        return error(StatusCode::NOT_FOUND, format!("file not found: {rel_path}"));
    }

    let engine = Arc::clone(&state.engine);
    // The song runs on a blocking thread so /healthz answers meanwhile; it
    // finishes and logs even if the caller stops waiting.
    let done = web::block(move || {
        span.in_scope(|| {
            let started = Instant::now();
            let n_lines = lines.len();
            let result = engine.align_song(&rel_path, &full_path, lines);
            let elapsed = started.elapsed();
            let duration_ms = elapsed.as_millis() as u64;
            let outcome = match &result {
                Ok(_) => "ok",
                Err(SongError::Ffmpeg(_)) => "ffmpeg",
                Err(SongError::Internal(_)) => "internal",
            };
            metrics::counter!(SONGS_TOTAL, "outcome" => outcome).increment(1);
            metrics::histogram!(SONG_SECONDS).record(elapsed.as_secs_f64());
            match &result {
                Ok(a) => tracing::info!(
                    path = %rel_path,
                    lines = n_lines,
                    words_aligned = a.words_aligned,
                    offset_ms = a.answer.offset_ms,
                    duration_ms,
                    decode_ms = a.phases.decode_ms(),
                    model_ms = a.phases.model_ms(),
                    "song aligned"
                ),
                Err(e @ SongError::Ffmpeg(_)) => {
                    tracing::warn!(path = %rel_path, failed = "ffmpeg", duration_ms, error = %e, "song failed");
                }
                Err(e @ SongError::Internal(_)) => {
                    tracing::error!(path = %rel_path, failed = "internal", duration_ms, error = %e, "song failed");
                }
            }
            result
        })
    })
    .await;
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
