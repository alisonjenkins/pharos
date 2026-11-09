//! HLS from the server (spec 032 slice 3, player step 4): fetch the master
//! the playback decision names, take its first (best) variant the feeder can
//! decode, and supply its segments to the feeder. The server sends a
//! non-browser client muxed MPEG-TS, and fMP4 only to browsers, so both are
//! read.
//!
//! Segments are fetched on their own thread, [`AHEAD`] in front of the
//! decoder, with the device token in the `X-Emby-Token` header (the server reads it there, not as a bearer) and never in a URL. Video
//! only: the audio rendition comes with the AAudio step.

use crate::feeder::{Segments, feedable};
use pharos_hls::playlist::{self, MediaPlaylist};
use std::sync::mpsc::{Receiver, sync_channel};

/// Segments held ready ahead of the decoder (about 18 s at 6 s each).
const AHEAD: usize = 3;
/// Fetch attempts per segment before the stream is given up.
const ATTEMPTS: u32 = 4;

/// How the stream's segments are packaged.
pub enum Packaging {
    /// fMP4, with the init segment its `EXT-X-MAP` names.
    Fmp4 { init: Vec<u8> },
    /// Muxed MPEG-TS (no `EXT-X-MAP`): the first segment, which carries the
    /// parameter sets, and the variant's `RESOLUTION`.
    Ts {
        first: Vec<u8>,
        size: Option<(u32, u32)>,
    },
}

/// What the player starts from: how it is packaged and the media segments
/// after the ones `packaging` holds.
pub struct Stream {
    pub packaging: Packaging,
    pub segments: Segments,
    pub variant: String,
}

struct Http {
    rt: tokio::runtime::Runtime,
    client: reqwest::Client,
    token: Option<String>,
}

impl Http {
    fn new() -> Result<Self, String> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("hls runtime: {e}"))?;
        Ok(Self {
            rt,
            client: reqwest::Client::new(),
            token: pharos_ui::native::android::token(),
        })
    }

    fn get(&self, url: &str) -> Result<Vec<u8>, String> {
        let mut last = String::new();
        for attempt in 0..ATTEMPTS {
            if attempt > 0 {
                std::thread::sleep(std::time::Duration::from_millis(500 << attempt));
            }
            let mut req = self.client.get(url);
            if let Some(token) = &self.token {
                req = req.header("X-Emby-Token", token);
            }
            let started = std::time::Instant::now();
            let result = self.rt.block_on(async {
                let resp = req.send().await.map_err(|e| e.to_string())?;
                let status = resp.status();
                if !status.is_success() {
                    return Err(format!("status {status}"));
                }
                resp.bytes()
                    .await
                    .map(|b| b.to_vec())
                    .map_err(|e| e.to_string())
            });
            match result {
                Ok(bytes) => {
                    log::info!(
                        "bench hls fetch bytes={} ms={} path={}",
                        bytes.len(),
                        started.elapsed().as_millis(),
                        url.split('?').next().unwrap_or(url),
                    );
                    return Ok(bytes);
                }
                Err(e) => last = e,
            }
        }
        Err(format!(
            "GET {}: {last}",
            url.split('?').next().unwrap_or(url)
        ))
    }

    fn text(&self, url: &str) -> Result<String, String> {
        String::from_utf8(self.get(url)?).map_err(|e| format!("{url}: not UTF-8: {e}"))
    }
}

/// Opens `stream_url` (the decision's same-origin path) on `server`.
pub fn open(server: &str, stream_url: &str) -> Result<Stream, String> {
    let http = Http::new()?;
    let master_url = format!("{}{stream_url}", server.trim_end_matches('/'));
    let master = playlist::parse_master(&http.text(&master_url)?, &master_url)
        .map_err(|e| format!("master: {e}"))?;
    // Variants are listed best first, and the first is the title at its own
    // resolution; never fall through to a smaller transcode rung.
    let variant = master
        .variants
        .iter()
        .find(|v| feedable(&v.codecs))
        .ok_or_else(|| {
            let seen: Vec<_> = master.variants.iter().map(|v| &v.codecs).collect();
            format!("no variant the feeder can decode: {seen:?}")
        })?;
    let media = playlist::parse_media(&http.text(&variant.uri)?, &variant.uri)
        .map_err(|e| format!("media playlist: {e}"))?;
    log::info!(
        "hls: variant codecs={:?} resolution={:?} bandwidth={} segments={} packaging={}",
        variant.codecs,
        variant.resolution,
        variant.bandwidth,
        media.segments.len(),
        if media.init_uri.is_some() { "fmp4" } else { "ts" },
    );
    let path = variant.uri.split('?').next().unwrap_or_default().to_owned();
    let (packaging, skip) = match media.init_uri.clone() {
        Some(init_uri) => (
            Packaging::Fmp4 {
                init: http.get(&init_uri)?,
            },
            0,
        ),
        None => {
            let first = media.segments.first().ok_or("media playlist has no segments")?;
            (
                Packaging::Ts {
                    first: http.get(&first.uri)?,
                    size: variant.resolution,
                },
                1,
            )
        }
    };
    let segments = spawn_fetcher(http, media, skip)?;
    Ok(Stream {
        packaging,
        segments: Box::new(move || segments.recv().ok()),
        variant: path,
    })
}

/// Fetches every segment in order on its own thread; the channel's bound
/// holds it [`AHEAD`] of the decoder. A failed segment ends the stream.
fn spawn_fetcher(
    http: Http,
    media: MediaPlaylist,
    skip: usize,
) -> Result<Receiver<Result<Vec<u8>, String>>, String> {
    let (tx, rx) = sync_channel(AHEAD);
    std::thread::Builder::new()
        .name("pharos-hls".into())
        .spawn(move || {
            for seg in media.segments.iter().skip(skip) {
                let got = http.get(&seg.uri);
                let failed = got.is_err();
                if tx.send(got).is_err() || failed {
                    return;
                }
            }
        })
        .map_err(|e| format!("hls thread: {e}"))?;
    Ok(rx)
}
