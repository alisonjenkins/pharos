use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Config {
    pub server: ServerConfig,
    pub obs: ObsConfig,
    pub media: MediaConfig,
    #[serde(default)]
    pub database: DatabaseConfig,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ServerConfig {
    pub bind: String,
    #[serde(default = "default_server_name")]
    pub name: String,
    /// Directory containing the built Dioxus UI bundle. When set, the
    /// server serves index.html + assets at `/ui/*`. Run `dx build` in
    /// `crates/pharos-ui` to produce one.
    #[serde(default)]
    pub ui_dir: Option<PathBuf>,
    /// Directory pharos writes extracted poster images to. When unset,
    /// /Items/{id}/Images/Primary returns 404.
    #[serde(default)]
    pub image_cache_dir: Option<PathBuf>,
    /// Directory used to cache transcoded HLS segments (T42). When
    /// unset, segments stream live without persisting — every request
    /// spawns ffmpeg.
    #[serde(default)]
    pub transcode_cache_dir: Option<PathBuf>,
    /// Soft cap on the HLS segment cache, in bytes. Once exceeded,
    /// least-recently-used segments are evicted. Default 1 GiB.
    #[serde(default = "default_transcode_cache_bytes")]
    pub transcode_cache_max_bytes: u64,
    /// Directory used to cache Trickplay sprite sheets. When unset,
    /// trickplay generation is disabled and BaseItemDto.Trickplay emits
    /// an empty map.
    #[serde(default)]
    pub trickplay_cache_dir: Option<PathBuf>,
    /// Soft cap on the Trickplay cache, in bytes. LRU eviction kicks in
    /// once exceeded. Default 256 MiB.
    #[serde(default = "default_trickplay_cache_bytes")]
    pub trickplay_cache_max_bytes: u64,
    /// Milliseconds between thumbnails. Default 10000 (one thumb per
    /// 10s). Lower = denser sprite + more disk + more ffmpeg work.
    #[serde(default = "default_trickplay_interval_ms")]
    pub trickplay_interval_ms: u32,
    /// Sprite widths to generate. Empty = trickplay disabled even when
    /// `trickplay_cache_dir` is set. Default `[320]`.
    #[serde(default = "default_trickplay_widths")]
    pub trickplay_widths: Vec<u32>,
    /// P14 — hardware encoder selection. `"auto"` probes
    /// `ffmpeg -hwaccels` at boot and prefers VideoToolbox →
    /// NVENC → QSV → VAAPI in that order. `"off"` keeps the
    /// software libx264 / libx265 path. Default `"auto"`.
    #[serde(default)]
    pub hwaccel: pharos_transcode::HwAccel,
    /// In-process subtitle cache cap in bytes. P5 — keeps WebVTT
    /// extraction results so subsequent fetches skip the ffmpeg
    /// spawn. Default 64 MiB.
    #[serde(default = "default_subtitle_cache_bytes")]
    pub subtitle_cache_max_bytes: u64,
    /// Maximum subtitle cache entry count. Default 1024 — generous
    /// for the largest realistic library; pairs with byte cap.
    #[serde(default = "default_subtitle_cache_entries")]
    pub subtitle_cache_max_entries: usize,
    /// Live-TV M3U playlist path (T47). When set, /LiveTv/Channels
    /// + /LiveTv/Programs serve channels + EPG from this backend.
    #[serde(default)]
    pub live_tv_m3u: Option<PathBuf>,
    /// Optional XMLTV file for the EPG side of the live-TV backend.
    #[serde(default)]
    pub live_tv_xmltv: Option<PathBuf>,
    /// T48 phase 2 — enable SSDP UDP-multicast responder on
    /// 239.255.255.250:1900 so DLNA / UPnP control points discover
    /// pharos without manual configuration. Default false; flip on
    /// per-deployment when you actually want LAN discovery.
    #[serde(default)]
    pub ssdp_enabled: bool,
    /// Externally-reachable origin pharos publishes in SSDP NOTIFY +
    /// M-SEARCH replies (`LOCATION:` field points at
    /// `{advertise_url}/Dlna/{server_id}/description.xml`). Falls
    /// back to a synthesised `http://{first_lan_ip}:{port}` when
    /// unset.
    #[serde(default)]
    pub ssdp_advertise_url: Option<String>,
}

fn default_transcode_cache_bytes() -> u64 {
    1024 * 1024 * 1024
}

fn default_trickplay_cache_bytes() -> u64 {
    256 * 1024 * 1024
}

fn default_trickplay_interval_ms() -> u32 {
    10_000
}

fn default_trickplay_widths() -> Vec<u32> {
    // P21 — three rungs cover mobile (320) + desktop hover (640)
    // + TV-overlay (1280). Default cap is 256 MiB which handles
    // all three for typical libraries.
    vec![320, 640, 1280]
}

fn default_subtitle_cache_bytes() -> u64 {
    64 * 1024 * 1024
}

fn default_subtitle_cache_entries() -> usize {
    1024
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct DatabaseConfig {
    #[serde(default = "default_db_url")]
    pub url: String,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            url: default_db_url(),
        }
    }
}

fn default_db_url() -> String {
    "sqlite::memory:".into()
}

fn default_server_name() -> String {
    "pharos".into()
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ObsConfig {
    #[serde(default)]
    pub otlp_endpoint: Option<String>,
    #[serde(default = "default_log_level")]
    pub log_level: String,
    /// Directory pharos writes / surfaces log files from. When set,
    /// `/System/Logs` lists every regular file in it with size + mtime
    /// and `/System/Logs/Log?name=…` serves the file body. Pharos does
    /// not write to it itself today — operators point this at the dir
    /// their log shipper (journald-to-file, supervisor, etc.) populates.
    #[serde(default)]
    pub log_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct MediaConfig {
    #[serde(default)]
    pub roots: Vec<PathBuf>,
}

fn default_log_level() -> String {
    "info".into()
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("parse: {0}")]
    Parse(#[from] toml::de::Error),
}

impl Config {
    pub fn from_toml_str(s: &str) -> Result<Self, ConfigError> {
        Ok(toml::from_str(s)?)
    }

    pub fn from_path(path: &Path) -> Result<Self, ConfigError> {
        let body = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_toml_str(&body)
    }

    /// Override fields from environment vars. Prefix `PHAROS_`.
    /// Recognized: `PHAROS_BIND`, `PHAROS_LOG_LEVEL`, `PHAROS_OTLP_ENDPOINT`.
    pub fn apply_env(mut self) -> Self {
        if let Ok(v) = std::env::var("PHAROS_BIND") {
            self.server.bind = v;
        }
        if let Ok(v) = std::env::var("PHAROS_LOG_LEVEL") {
            self.obs.log_level = v;
        }
        if let Ok(v) = std::env::var("PHAROS_OTLP_ENDPOINT") {
            self.obs.otlp_endpoint = Some(v);
        }
        self
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    const SAMPLE: &str = r#"
        [server]
        bind = "0.0.0.0:8096"
        name = "pharos-test"

        [obs]
        log_level = "debug"

        [media]
        roots = ["/srv/media"]

        [database]
        url = "sqlite::memory:"
    "#;

    #[test]
    fn parses_minimal_toml() {
        let c = Config::from_toml_str(SAMPLE).unwrap();
        assert_eq!(c.server.bind, "0.0.0.0:8096");
        assert_eq!(c.obs.log_level, "debug");
        assert_eq!(c.media.roots, vec![PathBuf::from("/srv/media")]);
        assert!(c.obs.otlp_endpoint.is_none());
    }

    #[test]
    fn defaults_log_level() {
        let s = r#"
            [server]
            bind = "127.0.0.1:0"
            [obs]
            [media]
        "#;
        let c = Config::from_toml_str(s).unwrap();
        assert_eq!(c.obs.log_level, "info");
        assert_eq!(c.server.name, "pharos");
        assert_eq!(c.database.url, "sqlite::memory:");
    }

    #[test]
    fn env_overrides_bind_and_log_level() {
        // Serial via unique var names per test; clear after.
        std::env::set_var("PHAROS_BIND", "1.2.3.4:9000");
        std::env::set_var("PHAROS_LOG_LEVEL", "trace");
        std::env::set_var("PHAROS_OTLP_ENDPOINT", "http://otel:4317");
        let c = Config::from_toml_str(SAMPLE).unwrap().apply_env();
        assert_eq!(c.server.bind, "1.2.3.4:9000");
        assert_eq!(c.obs.log_level, "trace");
        assert_eq!(c.obs.otlp_endpoint.as_deref(), Some("http://otel:4317"));
        std::env::remove_var("PHAROS_BIND");
        std::env::remove_var("PHAROS_LOG_LEVEL");
        std::env::remove_var("PHAROS_OTLP_ENDPOINT");
    }
}
