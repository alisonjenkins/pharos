//! How many actix-web worker threads the HTTP server runs (ADR-0083, B909).
//!
//! actix builds a full `App` (every route's `ResourceDef` regex, every
//! middleware) per worker, about 5.6 MiB of heap each. Its default is one
//! worker per CPU, which on the 44-CPU production node held ~250 MiB at idle.
//! Above 16 workers no route got faster on the 2026-11-09 benchmark.

/// Default ceiling when `[server].http_workers` is unset (V870).
pub const DEFAULT_MAX: usize = 16;

/// Where the worker count came from, as the `source` label of
/// `pharos_http_workers`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Configured,
    Default,
}

impl Source {
    pub const ALL: [Source; 2] = [Source::Configured, Source::Default];

    pub const fn label(self) -> &'static str {
        match self {
            Source::Configured => "configured",
            Source::Default => "default",
        }
    }
}

/// The worker count for `configured` (`None` or `0` = unset) on a machine
/// with `cpus` usable CPUs.
pub fn resolve(configured: Option<usize>, cpus: usize) -> (usize, Source) {
    match configured {
        Some(n) if n > 0 => (n, Source::Configured),
        _ => (cpus.clamp(1, DEFAULT_MAX), Source::Default),
    }
}

/// Resolve against this machine's CPU count, publish the gauge and log the
/// choice.
pub fn choose(configured: Option<usize>) -> usize {
    let cpus = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
    let (workers, source) = resolve(configured, cpus);
    record(workers, source, cpus);
    workers
}

fn record(workers: usize, source: Source, cpus: usize) {
    metrics::gauge!("pharos_http_workers", "source" => source.label()).set(workers as f64);
    tracing::info!(
        workers,
        cpus,
        source = source.label(),
        default_max = DEFAULT_MAX,
        "http workers chosen"
    );
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};

    #[test]
    fn unset_follows_cpus_up_to_the_cap() {
        assert_eq!(resolve(None, 1), (1, Source::Default));
        assert_eq!(resolve(None, 8), (8, Source::Default));
        assert_eq!(resolve(None, 16), (16, Source::Default));
        assert_eq!(resolve(None, 44), (DEFAULT_MAX, Source::Default));
    }

    #[test]
    fn zero_is_unset_and_never_yields_zero_workers() {
        assert_eq!(resolve(Some(0), 44), (DEFAULT_MAX, Source::Default));
        assert_eq!(resolve(None, 0), (1, Source::Default));
    }

    #[test]
    fn configured_value_wins_even_above_the_cap() {
        assert_eq!(resolve(Some(4), 44), (4, Source::Configured));
        assert_eq!(resolve(Some(64), 8), (64, Source::Configured));
    }

    #[test]
    fn labels_are_distinct() {
        let labels: std::collections::HashSet<_> = Source::ALL.iter().map(|s| s.label()).collect();
        assert_eq!(labels.len(), Source::ALL.len());
    }

    #[test]
    fn record_publishes_the_gauge_with_its_source() {
        let recorder = DebuggingRecorder::new();
        let snap = recorder.snapshotter();
        metrics::with_local_recorder(&recorder, || record(16, Source::Default, 44));
        let got: Vec<_> = snap
            .snapshot()
            .into_vec()
            .into_iter()
            .filter(|(ck, ..)| ck.key().name() == "pharos_http_workers")
            .map(|(ck, .., v)| {
                let source = ck
                    .key()
                    .labels()
                    .find(|l| l.key() == "source")
                    .map(|l| l.value().to_owned());
                (source, v)
            })
            .collect();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0.as_deref(), Some("default"));
        assert!(matches!(got[0].1, DebugValue::Gauge(g) if g.into_inner() == 16.0));
    }
}
