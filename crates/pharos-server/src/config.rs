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
    /// Seek timestamp (seconds) for poster / thumb frame extraction from
    /// video sources. Default 30 suits real movies; lower it for short
    /// test fixtures so the seek lands inside the clip (a seek past EOF
    /// yields no frame and the image 404s).
    #[serde(default = "default_image_seek_seconds")]
    pub image_seek_seconds: u32,
    /// Soft cap on the extracted-image cache (posters / backdrops / thumbs /
    /// scaled sidecar artwork / chapter thumbs), in bytes. `0` = unbounded
    /// (no janitor — the historical behaviour). When non-zero a periodic
    /// sweep recounts the cache tree and deletes the oldest files once it
    /// exceeds the cap; evicted images are re-extracted on next request
    /// (V6: never fatal). Default 0. Set it on large libraries so the image
    /// cache can't slowly fill the shared cache volume.
    #[serde(default)]
    pub image_cache_max_bytes: u64,
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
    /// Concurrent encode sessions to allow per hardware device (per GPU
    /// / render node). The load-balancing transcode scheduler caps each
    /// GPU at this many simultaneous segment encodes; the CPU device gets
    /// one permit per logical core. Default 2 (a safe value for consumer
    /// GPUs; raise for server-class cards). Set to 0 to disable the
    /// scheduler and use the legacy single-ffmpeg path.
    #[serde(default = "default_transcode_hw_session_cap")]
    pub transcode_hw_session_cap: usize,
    /// Auto-probe each hardware device's real concurrent-session cap at
    /// boot (ramp trial encodes until one fails). When false, every GPU
    /// uses `transcode_hw_session_cap`. Probing adds a few seconds to
    /// startup but learns true caps (e.g. consumer NVENC's session limit)
    /// instead of guessing. Default true.
    #[serde(default = "default_true")]
    pub transcode_probe_caps: bool,
    /// In-process subtitle cache cap in bytes. P5 — keeps WebVTT
    /// extraction results so subsequent fetches skip the ffmpeg
    /// spawn. Default 64 MiB.
    #[serde(default = "default_subtitle_cache_bytes")]
    pub subtitle_cache_max_bytes: u64,
    /// Maximum subtitle cache entry count. Default 1024 — generous
    /// for the largest realistic library; pairs with byte cap.
    #[serde(default = "default_subtitle_cache_entries")]
    pub subtitle_cache_max_entries: usize,
    /// Directory to PERSIST extracted subtitles under (the cache PVC). A
    /// subtitle extraction demuxes the whole source (~tens of seconds over
    /// NFS), so persisting it makes that a once-ever cost instead of
    /// re-incurring on every pod restart. When unset, pharos derives a
    /// `subtitles` sibling of `transcode_cache_dir` / `image_cache_dir` if
    /// either is set; otherwise the cache stays memory-only.
    #[serde(default)]
    pub subtitle_cache_dir: Option<PathBuf>,
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
    /// P36 — percentage-of-runtime threshold for marking an item
    /// `played=true` when `POST /Sessions/Playing/Stopped` fires.
    /// Real-world tuning: 80 lets the credits-skipper crowd flip
    /// before the recap; 95 keeps documentary watchers from auto-
    /// completing during the credits roll. Clamped to `[50, 100]`
    /// on read so a typo doesn't make every touch mark played.
    #[serde(default = "default_played_threshold_pct")]
    pub played_threshold_pct: u32,
    /// P43 — inter-probe sleep in milliseconds for the scanner +
    /// `/Library/Refresh` background pass. 0 (default) keeps the
    /// CLI scan behaviour unchanged. Production deployments with
    /// active streaming during re-scan can set this to 50–500 ms
    /// so ffprobe spawns don't saturate disk + CPU during the
    /// catalog walk.
    #[serde(default)]
    pub scan_rate_limit_ms: u64,
    /// #11 — cap on concurrent per-file probes during a scan. `0` (default)
    /// auto-sizes conservatively (leaves shared-storage I/O headroom so a scan
    /// doesn't starve foreground reads — subtitle extraction, HLS segments,
    /// trickplay generation). Raise it (e.g. 8) on local-SSD deployments that
    /// want faster catalog walks; lower it further on a slow NFS/SMB link.
    #[serde(default)]
    pub scan_probe_concurrency: usize,
    /// LIB-A9 — enable native filesystem watching (inotify / kqueue /
    /// ReadDirectoryChangesW) for media roots that support it, so the
    /// library index stays live between full scans without polling.
    /// Default `true`, but the watch tier only engages when the binary was
    /// built with the `watch` feature *and* the root's filesystem can
    /// deliver events (local fs — not NFS / SMB / FUSE, which always fall
    /// back to the periodic rescan below). With the `watch` feature off this
    /// flag is a no-op; periodic rescan still applies.
    #[serde(default = "default_true")]
    pub library_watch_enabled: bool,
    /// LIB-A9 — interval, in seconds, for the periodic incremental rescan
    /// that backstops every media root (and is the primary detector for
    /// network / fuse roots, or when the `watch` feature is off). The rescan
    /// reuses the cheap incremental `scan_into` path (unchanged files cost
    /// only a stat). Default `300` (5 min). Set to `0` to disable periodic
    /// rescans entirely — roots then rely solely on a native watch (if
    /// eligible + built) or on manual `/Library/Refresh` (the floor tier).
    #[serde(default = "default_library_poll_interval_secs")]
    pub library_poll_interval_secs: u64,
    /// Phase B3 (graceful drain) — seconds to keep serving in-flight
    /// requests after SIGTERM flips `/readyz` unready, before the HTTP
    /// server begins its graceful stop. This window lets the load balancer
    /// observe the unready probe and stop routing new requests, so a rolling
    /// deploy doesn't cut a viewer mid-segment. Should be ≥ the k8s readiness
    /// probe period; the pod's `terminationGracePeriodSeconds` must exceed
    /// this plus the in-flight drain. Default `10`.
    #[serde(default = "default_drain_grace_secs")]
    pub drain_grace_secs: u64,
}

fn default_played_threshold_pct() -> u32 {
    90
}

fn default_drain_grace_secs() -> u64 {
    10
}

fn default_library_poll_interval_secs() -> u64 {
    300
}

fn default_image_seek_seconds() -> u32 {
    30
}

fn default_transcode_cache_bytes() -> u64 {
    1024 * 1024 * 1024
}

fn default_transcode_hw_session_cap() -> usize {
    2
}

fn default_true() -> bool {
    true
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
    /// Back-compat plain root list. Each entry becomes a `mixed` library
    /// named after the directory basename. Still the simplest config;
    /// existing files keep working unchanged.
    #[serde(default)]
    pub roots: Vec<PathBuf>,
    /// LIB-C1 — richer per-library declaration. When present, each entry
    /// becomes a typed library with an explicit `kind` + optional display
    /// `name`. `[[media.libraries]]` and `roots` coexist: the union of
    /// both is reconciled into the `libraries` table at boot (a path
    /// appearing in both is treated as one library, the typed entry
    /// winning its kind/name).
    #[serde(default)]
    pub libraries: Vec<LibraryConfig>,
}

/// LIB-C1 — one typed library in `[[media.libraries]]`.
///
/// ```toml
/// [[media.libraries]]
/// path = "/srv/Movies"
/// name = "Movies"
/// kind = "movies"   # movies | tvshows | music | mixed (default)
/// ```
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct LibraryConfig {
    pub path: PathBuf,
    /// Display name. Defaults to the directory basename when omitted.
    #[serde(default)]
    pub name: Option<String>,
    /// `movies` | `tvshows` | `music` | `mixed`. Unknown / omitted →
    /// `mixed` (parsed leniently by `LibraryKind::parse`).
    #[serde(default)]
    pub kind: Option<String>,
}

impl MediaConfig {
    /// The full set of filesystem roots pharos scans — the union of the
    /// plain `roots` list and any `[[media.libraries]]` paths, de-duped
    /// preserving order (plain roots first, then typed-only paths). Used
    /// by the scanner + watcher so a path declared only under
    /// `[[media.libraries]]` is still walked.
    pub fn scan_roots(&self) -> Vec<PathBuf> {
        let mut out = self.roots.clone();
        for lib in &self.libraries {
            if !out.iter().any(|r| r == &lib.path) {
                out.push(lib.path.clone());
            }
        }
        out
    }

    /// LIB-C1 — reconcile config into `(name, root_path, kind)` tuples for
    /// the `libraries` table. A path appearing in both `roots` and
    /// `[[media.libraries]]` yields a single typed entry (the typed kind
    /// wins); a plain-only root yields a `mixed` library named after its
    /// basename.
    pub fn library_specs(&self) -> Vec<(String, PathBuf, pharos_core::LibraryKind)> {
        let mut out: Vec<(String, PathBuf, pharos_core::LibraryKind)> = Vec::new();
        let basename = |p: &Path| -> String {
            p.file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("Media")
                .to_string()
        };
        // Plain roots first (mixed unless overridden by a typed entry below).
        for root in &self.roots {
            out.push((
                basename(root),
                root.clone(),
                pharos_core::LibraryKind::Mixed,
            ));
        }
        for lib in &self.libraries {
            let kind = lib
                .kind
                .as_deref()
                .map(pharos_core::LibraryKind::parse)
                .unwrap_or_default();
            let name = lib.name.clone().unwrap_or_else(|| basename(&lib.path));
            if let Some(existing) = out.iter_mut().find(|(_, p, _)| p == &lib.path) {
                // Typed entry wins for a path also listed under `roots`.
                existing.0 = name;
                existing.2 = kind;
            } else {
                out.push((name, lib.path.clone(), kind));
            }
        }
        out
    }
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
    /// Recognized: `PHAROS_BIND`, `PHAROS_LOG_LEVEL`, `PHAROS_OTLP_ENDPOINT`,
    /// `PHAROS_DATABASE_URL`.
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
        // Lets the DB connection string (incl. a generated Postgres password)
        // be injected from a Secret at runtime instead of baked into config.toml
        // — so no credential lands in git. Overrides `[database].url`.
        if let Ok(v) = std::env::var("PHAROS_DATABASE_URL") {
            self.database.url = v;
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
        std::env::set_var(
            "PHAROS_DATABASE_URL",
            "postgresql://u:p@pharos-db-rw:5432/pharos",
        );
        let c = Config::from_toml_str(SAMPLE).unwrap().apply_env();
        assert_eq!(c.server.bind, "1.2.3.4:9000");
        assert_eq!(c.obs.log_level, "trace");
        assert_eq!(c.obs.otlp_endpoint.as_deref(), Some("http://otel:4317"));
        assert_eq!(c.database.url, "postgresql://u:p@pharos-db-rw:5432/pharos");
        std::env::remove_var("PHAROS_BIND");
        std::env::remove_var("PHAROS_LOG_LEVEL");
        std::env::remove_var("PHAROS_OTLP_ENDPOINT");
        std::env::remove_var("PHAROS_DATABASE_URL");
    }

    #[test]
    fn plain_roots_synthesise_mixed_libraries_back_compat() {
        // LIB-C1 — a legacy config with only `roots` keeps working: each
        // root becomes one `mixed` library named after its basename.
        let s = r#"
            [server]
            bind = "127.0.0.1:0"
            [obs]
            [media]
            roots = ["/srv/Movies", "/srv/TV"]
        "#;
        let c = Config::from_toml_str(s).unwrap();
        assert!(c.media.libraries.is_empty());
        let specs = c.media.library_specs();
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].0, "Movies");
        assert_eq!(specs[0].2, pharos_core::LibraryKind::Mixed);
        assert_eq!(specs[1].0, "TV");
        // scan_roots = the plain roots (no extra typed paths).
        assert_eq!(
            c.media.scan_roots(),
            vec![PathBuf::from("/srv/Movies"), PathBuf::from("/srv/TV")]
        );
    }

    #[test]
    fn typed_libraries_array_drives_kind_and_name() {
        let s = r#"
            [server]
            bind = "127.0.0.1:0"
            [obs]
            [media]
            roots = ["/srv/Shared"]

            [[media.libraries]]
            path = "/srv/Films"
            name = "Films"
            kind = "movies"

            [[media.libraries]]
            path = "/srv/Shows"
            kind = "tvshows"
        "#;
        let c = Config::from_toml_str(s).unwrap();
        let specs = c.media.library_specs();
        // Shared (mixed, from roots) + Films (movies) + Shows (tvshows,
        // name defaulted to basename).
        assert_eq!(specs.len(), 3);
        assert_eq!(specs[0].0, "Shared");
        assert_eq!(specs[0].2, pharos_core::LibraryKind::Mixed);
        assert_eq!(specs[1].0, "Films");
        assert_eq!(specs[1].2, pharos_core::LibraryKind::Movies);
        assert_eq!(specs[2].0, "Shows");
        assert_eq!(specs[2].2, pharos_core::LibraryKind::TvShows);
        // scan_roots unions both lists.
        assert_eq!(
            c.media.scan_roots(),
            vec![
                PathBuf::from("/srv/Shared"),
                PathBuf::from("/srv/Films"),
                PathBuf::from("/srv/Shows"),
            ]
        );
    }

    #[test]
    fn typed_entry_wins_for_path_listed_under_both_roots_and_libraries() {
        let s = r#"
            [server]
            bind = "127.0.0.1:0"
            [obs]
            [media]
            roots = ["/srv/Movies"]

            [[media.libraries]]
            path = "/srv/Movies"
            name = "My Movies"
            kind = "movies"
        "#;
        let c = Config::from_toml_str(s).unwrap();
        let specs = c.media.library_specs();
        // One entry — the typed declaration overrides the plain root's
        // mixed/basename defaults, not a duplicate.
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].0, "My Movies");
        assert_eq!(specs[0].2, pharos_core::LibraryKind::Movies);
        assert_eq!(c.media.scan_roots(), vec![PathBuf::from("/srv/Movies")]);
    }
}
