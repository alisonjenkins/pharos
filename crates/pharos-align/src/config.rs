//! The service's environment, with the Python service's names and defaults,
//! plus `PHAROS_ALIGNER_WEIGHTS`, the converted weights the image ships.

use std::path::PathBuf;
use std::time::Duration;

use crate::device::DeviceMode;
use crate::pin::PinMode;

pub const DEFAULT_LISTEN: &str = "0.0.0.0:8790";
const DEFAULT_PORT: u16 = 8790;
/// The model holds over a GiB; reloading it costs seconds, so it stays a
/// while after the last song.
pub const DEFAULT_IDLE_UNLOAD: Duration = Duration::from_secs(600);
/// On a busy node, threads beyond this wait on each other more than they work.
pub const DEFAULT_THREADS: usize = 8;
/// About 62 s of 16 kHz audio a forward pass: every offset probe of a
/// typical song in one pass, with peak RSS inside 2.6 GiB (spec 039, R19).
pub const DEFAULT_BATCH_SAMPLES: usize = 1_000_000;

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub media_root: PathBuf,
    pub host: String,
    pub port: u16,
    pub otlp_endpoint: Option<String>,
    pub threads: usize,
    /// `None` keeps the model loaded.
    pub idle_unload: Option<Duration>,
    /// The `alignerWeights` output: `weights.bin` and `weights.json`.
    pub weights: PathBuf,
    pub pin: PinMode,
    /// Most samples one forward pass holds.
    pub batch_samples: usize,
    pub device: DeviceMode,
    /// Where the GPU pass keeps compiled kernels between restarts; `None`
    /// compiles them afresh in every process.
    pub cache_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("PHAROS_ALIGNER_MEDIA_ROOT environment variable is required")]
    NoMediaRoot,
    #[error("PHAROS_ALIGNER_WEIGHTS environment variable is required")]
    NoWeights,
    #[error("{name} must be {want}, got {value:?}")]
    Invalid {
        name: &'static str,
        want: &'static str,
        value: String,
    },
}

fn invalid(name: &'static str, want: &'static str, value: &str) -> ConfigError {
    ConfigError::Invalid {
        name,
        want,
        value: value.to_owned(),
    }
}

/// Unset and blank both mean the default, as in server.py.
fn set(var: Option<String>) -> Option<String> {
    var.filter(|v| !v.trim().is_empty())
}

impl Config {
    /// Reads the variables through `get`, `std::env::var` outside tests.
    pub fn from_env(get: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let media_root = set(get("PHAROS_ALIGNER_MEDIA_ROOT")).ok_or(ConfigError::NoMediaRoot)?;
        let weights = set(get("PHAROS_ALIGNER_WEIGHTS")).ok_or(ConfigError::NoWeights)?;

        let listen = set(get("PHAROS_ALIGNER_LISTEN")).unwrap_or_else(|| DEFAULT_LISTEN.to_owned());
        let (host, port) = match listen.rsplit_once(':') {
            Some((host, port)) => {
                let port = port
                    .parse()
                    .map_err(|_| invalid("PHAROS_ALIGNER_LISTEN", "host:port", &listen))?;
                (host.to_owned(), port)
            }
            None => (listen.clone(), DEFAULT_PORT),
        };

        let threads = match set(get("PHAROS_ALIGNER_THREADS")) {
            None => DEFAULT_THREADS,
            Some(raw) => match raw.trim().parse::<usize>() {
                Ok(n) if n >= 1 => n,
                _ => return Err(invalid("PHAROS_ALIGNER_THREADS", "an integer >= 1", &raw)),
            },
        };

        let idle_unload = match set(get("PHAROS_ALIGNER_IDLE_UNLOAD_SECS")) {
            None => Some(DEFAULT_IDLE_UNLOAD),
            Some(raw) => match raw.trim().parse::<f64>() {
                Ok(0.0) => None,
                Ok(s) if s > 0.0 && s.is_finite() => Some(Duration::from_secs_f64(s)),
                _ => {
                    return Err(invalid(
                        "PHAROS_ALIGNER_IDLE_UNLOAD_SECS",
                        "seconds >= 0",
                        &raw,
                    ))
                }
            },
        };

        let pin = match set(get("PHAROS_ALIGNER_PIN")) {
            None => PinMode::Auto,
            Some(raw) => raw
                .trim()
                .parse()
                .map_err(|()| invalid("PHAROS_ALIGNER_PIN", "auto, on or off", &raw))?,
        };

        let batch_samples = match set(get("PHAROS_ALIGNER_BATCH_SAMPLES")) {
            None => DEFAULT_BATCH_SAMPLES,
            Some(raw) => match raw.trim().parse::<usize>() {
                Ok(n) if n >= 1 => n,
                _ => {
                    return Err(invalid(
                        "PHAROS_ALIGNER_BATCH_SAMPLES",
                        "an integer >= 1",
                        &raw,
                    ))
                }
            },
        };

        let device = match set(get("PHAROS_ALIGNER_DEVICE")) {
            None => DeviceMode::Auto,
            Some(raw) => raw
                .trim()
                .parse()
                .map_err(|()| invalid("PHAROS_ALIGNER_DEVICE", "auto, vulkan or cpu", &raw))?,
        };

        Ok(Self {
            media_root: PathBuf::from(media_root),
            host,
            port,
            otlp_endpoint: set(get("PHAROS_ALIGNER_OTLP_ENDPOINT")),
            threads,
            idle_unload,
            weights: PathBuf::from(weights),
            pin,
            batch_samples,
            device,
            cache_dir: set(get("PHAROS_ALIGNER_CACHE_DIR")).map(PathBuf::from),
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| ((*k).into(), (*v).into()))
            .collect();
        move |k| pairs.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())
    }

    #[test]
    fn defaults_match_server_py() {
        let c = Config::from_env(env(&[
            ("PHAROS_ALIGNER_MEDIA_ROOT", "/media"),
            ("PHAROS_ALIGNER_WEIGHTS", "/w"),
        ]))
        .unwrap();
        assert_eq!(c.media_root, PathBuf::from("/media"));
        assert_eq!((c.host.as_str(), c.port), ("0.0.0.0", 8790));
        assert_eq!(c.threads, 8);
        assert_eq!(c.idle_unload, Some(Duration::from_secs(600)));
        assert_eq!(c.otlp_endpoint, None);
        assert_eq!(c.weights, PathBuf::from("/w"));
        assert_eq!(c.pin, PinMode::Auto);
        assert_eq!(c.batch_samples, 1_000_000);
        assert_eq!(c.device, DeviceMode::Auto);
        assert_eq!(c.cache_dir, None);
    }

    #[test]
    fn values_are_read() {
        let c = Config::from_env(env(&[
            ("PHAROS_ALIGNER_MEDIA_ROOT", "/m"),
            ("PHAROS_ALIGNER_WEIGHTS", "/w"),
            ("PHAROS_ALIGNER_LISTEN", "127.0.0.1:9000"),
            ("PHAROS_ALIGNER_THREADS", "4"),
            ("PHAROS_ALIGNER_IDLE_UNLOAD_SECS", "0"),
            ("PHAROS_ALIGNER_OTLP_ENDPOINT", "http://otel:4317"),
            ("PHAROS_ALIGNER_PIN", "off"),
            ("PHAROS_ALIGNER_BATCH_SAMPLES", "48000"),
            ("PHAROS_ALIGNER_DEVICE", "cpu"),
            ("PHAROS_ALIGNER_CACHE_DIR", "/var/cache/aligner"),
        ]))
        .unwrap();
        assert_eq!(c.device, DeviceMode::Cpu);
        assert_eq!(c.cache_dir, Some(PathBuf::from("/var/cache/aligner")));
        assert_eq!(c.pin, PinMode::Off);
        assert_eq!(c.batch_samples, 48_000);
        assert_eq!((c.host.as_str(), c.port, c.threads), ("127.0.0.1", 9000, 4));
        assert_eq!(c.idle_unload, None, "0 keeps the model loaded");
        assert_eq!(c.otlp_endpoint.as_deref(), Some("http://otel:4317"));
        let bare = Config::from_env(env(&[
            ("PHAROS_ALIGNER_MEDIA_ROOT", "/m"),
            ("PHAROS_ALIGNER_WEIGHTS", "/w"),
            ("PHAROS_ALIGNER_LISTEN", "::1x"),
        ]));
        assert!(bare.is_err());
        let host_only = Config::from_env(env(&[
            ("PHAROS_ALIGNER_MEDIA_ROOT", "/m"),
            ("PHAROS_ALIGNER_WEIGHTS", "/w"),
            ("PHAROS_ALIGNER_LISTEN", "localhost"),
        ]))
        .unwrap();
        assert_eq!(
            (host_only.host.as_str(), host_only.port),
            ("localhost", 8790)
        );
        let half = Config::from_env(env(&[
            ("PHAROS_ALIGNER_MEDIA_ROOT", "/m"),
            ("PHAROS_ALIGNER_WEIGHTS", "/w"),
            ("PHAROS_ALIGNER_IDLE_UNLOAD_SECS", "1.5"),
        ]))
        .unwrap();
        assert_eq!(half.idle_unload, Some(Duration::from_millis(1500)));
    }

    #[test]
    fn errors_name_the_variable_and_value() {
        assert_eq!(Config::from_env(env(&[])), Err(ConfigError::NoMediaRoot));
        let no_weights = Config::from_env(env(&[("PHAROS_ALIGNER_MEDIA_ROOT", "/m")]));
        assert_eq!(no_weights, Err(ConfigError::NoWeights));
        let e = Config::from_env(env(&[
            ("PHAROS_ALIGNER_MEDIA_ROOT", "/m"),
            ("PHAROS_ALIGNER_WEIGHTS", "/w"),
            ("PHAROS_ALIGNER_THREADS", "0"),
        ]))
        .unwrap_err();
        assert_eq!(
            e.to_string(),
            r#"PHAROS_ALIGNER_THREADS must be an integer >= 1, got "0""#
        );
        let e = Config::from_env(env(&[
            ("PHAROS_ALIGNER_MEDIA_ROOT", "/m"),
            ("PHAROS_ALIGNER_WEIGHTS", "/w"),
            ("PHAROS_ALIGNER_IDLE_UNLOAD_SECS", "-1"),
        ]))
        .unwrap_err();
        assert_eq!(
            e.to_string(),
            r#"PHAROS_ALIGNER_IDLE_UNLOAD_SECS must be seconds >= 0, got "-1""#
        );
        let e = Config::from_env(env(&[
            ("PHAROS_ALIGNER_MEDIA_ROOT", "/m"),
            ("PHAROS_ALIGNER_WEIGHTS", "/w"),
            ("PHAROS_ALIGNER_LISTEN", "h:port"),
        ]))
        .unwrap_err();
        assert_eq!(
            e.to_string(),
            r#"PHAROS_ALIGNER_LISTEN must be host:port, got "h:port""#
        );
        let e = Config::from_env(env(&[
            ("PHAROS_ALIGNER_MEDIA_ROOT", "/m"),
            ("PHAROS_ALIGNER_WEIGHTS", "/w"),
            ("PHAROS_ALIGNER_PIN", "yes"),
        ]))
        .unwrap_err();
        assert_eq!(
            e.to_string(),
            r#"PHAROS_ALIGNER_PIN must be auto, on or off, got "yes""#
        );
        let e = Config::from_env(env(&[
            ("PHAROS_ALIGNER_MEDIA_ROOT", "/m"),
            ("PHAROS_ALIGNER_WEIGHTS", "/w"),
            ("PHAROS_ALIGNER_BATCH_SAMPLES", "0"),
        ]))
        .unwrap_err();
        assert_eq!(
            e.to_string(),
            r#"PHAROS_ALIGNER_BATCH_SAMPLES must be an integer >= 1, got "0""#
        );
        let e = Config::from_env(env(&[
            ("PHAROS_ALIGNER_MEDIA_ROOT", "/m"),
            ("PHAROS_ALIGNER_WEIGHTS", "/w"),
            ("PHAROS_ALIGNER_DEVICE", "gpu"),
        ]))
        .unwrap_err();
        assert_eq!(
            e.to_string(),
            r#"PHAROS_ALIGNER_DEVICE must be auto, vulkan or cpu, got "gpu""#
        );
    }
}
