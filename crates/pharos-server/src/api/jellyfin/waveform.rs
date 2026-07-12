//! P42 — audio waveform endpoint.
//!
//! `GET /Items/{id}/Waveform.json?bins=N` returns a normalised peak
//! array suitable for scrubber rendering. Finamp + custom HTML5
//! players read the JSON shape `{ duration_ms, bins, peaks: [f32; N] }`
//! where each peak is a `[0.0, 1.0]` linear amplitude (re-converted
//! from ffmpeg's dB RMS readings).
//!
//! The endpoint streams ffmpeg `-af astats` lines from stderr and
//! parses the `lavfi.astats.Overall.RMS_level` metadata key — one
//! reading per `asetnsamples` window. Window count == bin count so
//! the output is exactly `bins` long (truncated / zero-padded when
//! ffmpeg emits an unexpected count).
//!
//! No persistent cache: a typical 4-minute song hits the audio
//! decoder once + emits 256 floats in well under a second. Heavy
//! libraries get the speed-up by hitting the endpoint from the
//! client cache instead.

use crate::{api::jellyfin::auth_extractor::AuthUser, state::AppState};
use actix_web::{error, web, HttpRequest, HttpResponse};
use pharos_core::{MediaKind, MediaStore};
use tokio::process::Command;

pub fn register(cfg: &mut web::ServiceConfig) {
    cfg.route("/items/{id}/waveform", web::get().to(get_waveform))
        .route("/items/{id}/waveform.json", web::get().to(get_waveform));
}

const DEFAULT_BINS: u32 = 256;
const MAX_BINS: u32 = 2048;

async fn get_waveform(
    state: web::Data<AppState>,
    _user: AuthUser,
    req: HttpRequest,
    path: web::Path<String>,
) -> Result<HttpResponse, actix_web::Error> {
    let id_str = path.into_inner();
    let id: u64 = pharos_jellyfin_api::dto::parse_item_id(&id_str)
        .ok_or_else(|| error::ErrorBadRequest("invalid id"))?;
    let item = state.stores.get(id).await.map_err(|e| match e {
        pharos_core::DomainError::NotFound(_) => error::ErrorNotFound("not found"),
        other => error::ErrorInternalServerError(other.to_string()),
    })?;
    if !matches!(item.kind, MediaKind::Audio) {
        return Err(error::ErrorBadRequest(
            "waveform only available for Audio items",
        ));
    }
    let duration_ms = item.probe.duration_ms.unwrap_or(0);
    if duration_ms == 0 {
        return Err(error::ErrorBadRequest("unknown duration; cannot bin"));
    }
    let bins = parse_bins(req.query_string()).clamp(8, MAX_BINS);
    let input = item
        .path
        .to_str()
        .ok_or_else(|| error::ErrorInternalServerError("non-utf8 path"))?;
    let sample_rate: u32 = item.probe.sample_rate.unwrap_or(44_100);
    let total_samples = (duration_ms as u64).saturating_mul(sample_rate as u64) / 1_000;
    let samples_per_bin = (total_samples / bins as u64).max(1);

    let peaks = run_astats(input, samples_per_bin, bins).await?;
    Ok(HttpResponse::Ok().json(serde_json::json!({
        "DurationMs": duration_ms,
        "Bins": peaks.len(),
        "Peaks": peaks,
    })))
}

fn parse_bins(qs: &str) -> u32 {
    for kv in qs.split('&') {
        if let Some((k, v)) = kv.split_once('=') {
            if k.eq_ignore_ascii_case("bins") {
                if let Ok(n) = v.parse::<u32>() {
                    return n;
                }
            }
        }
    }
    DEFAULT_BINS
}

/// Spawn ffmpeg with `astats` per-window. Emits `lavfi.astats.…RMS_level`
/// lines on stderr; we grep for the key, parse the trailing float, and
/// convert dB → linear amplitude.
async fn run_astats(
    input: &str,
    samples_per_bin: u64,
    target_bins: u32,
) -> Result<Vec<f32>, actix_web::Error> {
    let filter = format!(
        "aresample=async=1,aformat=channel_layouts=mono,asetnsamples={samples_per_bin},astats=metadata=1:reset=1,ametadata=print:key=lavfi.astats.Overall.RMS_level"
    );
    let out = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-nostdin",
            "-loglevel",
            "info",
            "-i",
            input,
            "-vn",
            "-af",
            &filter,
            "-f",
            "null",
            "-",
        ])
        .output()
        .await
        .map_err(|e| error::ErrorInternalServerError(format!("ffmpeg spawn: {e}")))?;
    if !out.status.success() {
        return Err(error::ErrorInternalServerError(format!(
            "ffmpeg astats: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let mut peaks: Vec<f32> = Vec::with_capacity(target_bins as usize);
    for line in stderr.lines() {
        if let Some(idx) = line.find("RMS_level=") {
            let tail = &line[idx + "RMS_level=".len()..];
            let val = tail.split_whitespace().next().unwrap_or("");
            if val == "-inf" || val.is_empty() {
                peaks.push(0.0);
                continue;
            }
            if let Ok(db) = val.parse::<f32>() {
                let linear = 10f32.powf(db / 20.0);
                peaks.push(linear.clamp(0.0, 1.0));
            } else {
                peaks.push(0.0);
            }
        }
    }
    while peaks.len() < target_bins as usize {
        peaks.push(0.0);
    }
    peaks.truncate(target_bins as usize);
    Ok(peaks)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn parse_bins_picks_query_value() {
        assert_eq!(parse_bins("bins=512"), 512);
        assert_eq!(parse_bins("Bins=64&other=zz"), 64);
        assert_eq!(parse_bins(""), DEFAULT_BINS);
        assert_eq!(parse_bins("bins=notanumber"), DEFAULT_BINS);
    }
}
