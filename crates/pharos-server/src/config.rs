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
