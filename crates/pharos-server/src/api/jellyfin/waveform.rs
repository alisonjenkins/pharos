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
//! reading per `asetnsamples` window. The window size is derived from the
//! PROBED sample rate while `astats` counts the REAL decoded stream, so the
//! window count only matches the bin count when the probe was right;
//! [`resample_peaks`] groups whatever ffmpeg produced onto exactly `bins`
//! (see its docs — the previous truncate silently dropped the end of the
//! track whenever the rates disagreed).
//!
//! T97/B72 — the `astats` pass decodes the WHOLE audio stream, which over an
//! NFS-backed lossless track is far from the "well under a second" once assumed.
//! A [`WaveformCache`] memoises the computed peaks per `(item, mtime, bins)` and
//! single-flights the miss, so a re-open / re-seek storm never re-decodes the
//! source. It is in-memory (session-scoped), NOT disk-backed like the subtitle /
//! image caches: this endpoint is client-optional (Finamp / custom players; the
//! deployed jellyfin-web + android-tv never call it), so the recompute-on-boot
//! cost is negligible and does not justify a config-plumbed disk layer.

use crate::{api::jellyfin::auth_extractor::AuthUser, state::AppState};
use actix_web::{error, web, HttpRequest, HttpResponse};
use pharos_cache::KeyedLocks;
use pharos_core::{MediaKind, MediaStore};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::Mutex;

/// Cache key: item id + source mtime (a re-encode invalidates) + bin count
/// (different resolutions are different outputs).
type WaveKey = (u64, i64, u32);

/// In-memory memo + per-key single-flight for computed waveform peaks (T97). A
/// hit skips the whole-file `astats` decode; a miss runs exactly once even under
/// a burst of concurrent first-fetchers. Cheap to clone-share (all `Arc`).
#[derive(Clone, Default)]
pub struct WaveformCache {
    memo: Arc<Mutex<HashMap<WaveKey, Arc<Vec<f32>>>>>,
    locks: Arc<KeyedLocks<WaveKey>>,
}

impl WaveformCache {
    async fn get(&self, key: &WaveKey) -> Option<Arc<Vec<f32>>> {
        self.memo.lock().await.get(key).cloned()
    }
    async fn store(&self, key: WaveKey, peaks: Vec<f32>) -> Arc<Vec<f32>> {
        let shared = Arc::new(peaks);
        self.memo.lock().await.insert(key, shared.clone());
        shared
    }
}

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

    // T97/B72 — memo + single-flight so a re-open / re-seek burst never
    // re-decodes the whole file. `mtime` in the key invalidates on a re-encode.
    let mtime = pharos_cache::subtitle_cache::mtime_secs(&item.path).await;
    let key: WaveKey = (id, mtime, bins);
    let cache = &state.waveform;
    let peaks = if let Some(hit) = cache.get(&key).await {
        hit
    } else {
        let lock = cache.locks.lock(key).await;
        let _guard = lock.lock().await;
        if let Some(hit) = cache.get(&key).await {
            hit
        } else {
            // On-demand waveform for the item being viewed — playback priority
            // (V34). Runs exactly once per key under a stampede.
            let computed = run_astats(
                input,
                samples_per_bin,
                bins,
                &crate::bg_io::BgPermit::playback_priority(),
            )
            .await?;
            cache.store(key, computed).await
        }
    };
    let peaks: &[f32] = &peaks;
    Ok(crate::api::jellyfin::wire::json(&serde_json::json!({
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
    _permit: &crate::bg_io::BgPermit,
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
    Ok(resample_peaks(peaks, target_bins as usize))
}

/// Fit however many `astats` windows ffmpeg actually produced onto exactly
/// `target_bins` output bins, taking the peak of each group.
///
/// The window size handed to `asetnsamples` is derived from the PROBED sample
/// rate, but `astats` counts the REAL decoded stream. When the probe carries no
/// sample rate the code assumes 44100, so a 48 kHz file yields
/// `48000/44100 ≈ 1.088x` as many windows as bins. The previous code padded
/// short and then `truncate`d long — and since the windows arrive in time
/// order, truncating threw away the LAST ~8% of the track: the returned array
/// claimed to span `DurationMs` but stopped early. Grouping instead keeps the
/// array spanning the whole track whatever the real rate turns out to be.
///
/// Still pads with silence when ffmpeg produced FEWER windows than requested
/// (a short or partly-unreadable file), so the array is always exactly
/// `target_bins` long.
fn resample_peaks(peaks: Vec<f32>, target_bins: usize) -> Vec<f32> {
    if target_bins == 0 {
        return Vec::new();
    }
    if peaks.len() <= target_bins {
        let mut out = peaks;
        out.resize(target_bins, 0.0);
        return out;
    }
    let n = peaks.len();
    (0..target_bins)
        .map(|i| {
            // Half-open [lo, hi) over the input, so every input window lands in
            // exactly one output bin and none is dropped.
            let lo = i * n / target_bins;
            let hi = ((i + 1) * n / target_bins).max(lo + 1);
            peaks[lo..hi.min(n)].iter().copied().fold(0.0f32, f32::max)
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn surplus_windows_are_grouped_not_truncated_off_the_end() {
        // A 48 kHz file whose probe carries no sample rate is binned as if it
        // were 44100, so ffmpeg emits ~8.8% more windows than bins. Truncating
        // dropped that many windows off the END of the track. Grouping keeps
        // the last window represented.
        let peaks: Vec<f32> = (0..279).map(|i| i as f32 / 278.0).collect();
        let out = resample_peaks(peaks, 256);
        assert_eq!(out.len(), 256);
        // The final bin must carry the track's final window (1.0), not the
        // value 256/278 that truncation would have left there.
        assert!(
            (out[255] - 1.0).abs() < 1e-6,
            "last bin must reach the end of the track, got {}",
            out[255]
        );
        assert!(out[0] < 0.01, "first bin still comes from the start");
        // Monotonic input stays monotonic — no bin is skipped or reordered.
        assert!(out.windows(2).all(|w| w[1] >= w[0]));
    }

    #[test]
    fn short_or_exact_window_counts_are_padded_to_the_bin_count() {
        assert_eq!(
            resample_peaks(vec![0.5, 0.25], 4),
            vec![0.5, 0.25, 0.0, 0.0]
        );
        assert_eq!(resample_peaks(vec![0.5, 0.25], 2), vec![0.5, 0.25]);
        assert!(resample_peaks(vec![0.5], 0).is_empty());
    }

    #[tokio::test]
    async fn waveform_cache_memoises_by_key() {
        // T97: a computed waveform is returned from the memo on the next fetch
        // of the same (id, mtime, bins), and a different key still misses.
        let cache = WaveformCache::default();
        let key: WaveKey = (42, 1000, 256);
        assert!(cache.get(&key).await.is_none(), "cold key must miss");
        let stored = cache.store(key, vec![0.1, 0.2, 0.3]).await;
        let hit = cache.get(&key).await.unwrap(); // warm key must hit
        assert_eq!(hit.as_slice(), stored.as_slice());
        // A different bin count is a distinct output → still a miss.
        assert!(cache.get(&(42, 1000, 128)).await.is_none());
        // A re-encode (new mtime) invalidates.
        assert!(cache.get(&(42, 1001, 256)).await.is_none());
    }

    #[test]
    fn parse_bins_picks_query_value() {
        assert_eq!(parse_bins("bins=512"), 512);
        assert_eq!(parse_bins("Bins=64&other=zz"), 64);
        assert_eq!(parse_bins(""), DEFAULT_BINS);
        assert_eq!(parse_bins("bins=notanumber"), DEFAULT_BINS);
    }
}
