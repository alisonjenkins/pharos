//! Counters and language mapping for the OpenSubtitles feature (V571).
//!
//! `pharos_opensubtitles_total{op,outcome}`: every provider call is counted by
//! operation and outcome, failures as well as successes. Label values come
//! from the `label()` methods below so the dashboard contract is one place.

use super::{Downloaded, ProviderError, SubtitleHit};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Search,
    Download,
}

impl Op {
    pub const ALL: [Op; 2] = [Op::Search, Op::Download];

    pub fn label(self) -> &'static str {
        match self {
            Op::Search => "search",
            Op::Download => "download",
        }
    }
}

impl ProviderError {
    pub const LABELS: [&'static str; 6] = [
        "quota",
        "rate_limited",
        "auth",
        "http",
        "transport",
        "parse",
    ];

    pub fn label(&self) -> &'static str {
        match self {
            ProviderError::QuotaExceeded { .. } => "quota",
            ProviderError::RateLimited { .. } => "rate_limited",
            ProviderError::Auth { .. } => "auth",
            ProviderError::Http { .. } => "http",
            ProviderError::Transport { .. } => "transport",
            ProviderError::Parse { .. } => "parse",
        }
    }
}

fn count(op: Op, outcome: &'static str) {
    metrics::counter!("pharos_opensubtitles_total", "op" => op.label(), "outcome" => outcome)
        .increment(1);
}

/// Count a search: `ok` with results, `empty` with none, else the error label.
pub fn record_search(r: &Result<Vec<SubtitleHit>, ProviderError>) {
    let outcome = match r {
        Ok(hits) if hits.is_empty() => "empty",
        Ok(_) => "ok",
        Err(e) => e.label(),
    };
    count(Op::Search, outcome);
}

/// Count a download: `ok`, else the error label.
pub fn record_download(r: &Result<Downloaded, ProviderError>) {
    count(
        Op::Download,
        match r {
            Ok(_) => "ok",
            Err(e) => e.label(),
        },
    );
}

/// OpenSubtitles language code for a Jellyfin language argument: the
/// three-letter ISO 639 code jellyfin-web sends (`eng`, `fre`/`fra`), or a
/// two-letter code or `pt-br`-style locale passed through. `None` for a code
/// this table does not know, so the search answers empty instead of asking the
/// provider for a language it will reject.
pub fn provider_language(code: &str) -> Option<String> {
    let lower = code.trim().to_ascii_lowercase();
    let two = match lower.as_str() {
        "eng" => "en",
        "jpn" => "ja",
        "fra" | "fre" => "fr",
        "deu" | "ger" => "de",
        "spa" => "es",
        "ita" => "it",
        "por" => "pt-pt",
        "kor" => "ko",
        "zho" | "chi" => "zh-cn",
        "rus" => "ru",
        "nld" | "dut" => "nl",
        "pol" => "pl",
        "swe" => "sv",
        "nor" | "nob" => "no",
        "dan" => "da",
        "fin" => "fi",
        "tur" => "tr",
        "ara" => "ar",
        "heb" => "he",
        "hin" => "hi",
        "ces" | "cze" => "cs",
        "hun" => "hu",
        "ell" | "gre" => "el",
        "ron" | "rum" => "ro",
        "tha" => "th",
        "vie" => "vi",
        "ukr" => "uk",
        "ind" => "id",
        other
            if (2..=5).contains(&other.len())
                && other.bytes().all(|b| b.is_ascii_lowercase() || b == b'-')
                && other.len() != 3 =>
        {
            return Some(other.to_string());
        }
        _ => return None,
    };
    Some(two.to_string())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use metrics_util::debugging::DebuggingRecorder;

    #[test]
    fn labels_are_distinct() {
        let ops: std::collections::HashSet<_> = Op::ALL.iter().map(|o| o.label()).collect();
        assert_eq!(ops.len(), Op::ALL.len());
        let errs: std::collections::HashSet<_> = ProviderError::LABELS.iter().collect();
        assert_eq!(errs.len(), ProviderError::LABELS.len());
        for reserved in ["ok", "empty"] {
            assert!(!ProviderError::LABELS.contains(&reserved));
        }
    }

    #[test]
    fn provider_language_maps_three_letter_codes() {
        assert_eq!(provider_language("eng").as_deref(), Some("en"));
        assert_eq!(provider_language("FRE").as_deref(), Some("fr"));
        assert_eq!(provider_language("pt-br").as_deref(), Some("pt-br"));
        assert_eq!(provider_language("de").as_deref(), Some("de"));
        assert_eq!(provider_language("xyz"), None);
        assert_eq!(provider_language(""), None);
        assert_eq!(provider_language("../x"), None);
    }

    #[test]
    fn search_and_download_outcomes_are_counted_with_op_and_outcome() {
        let recorder = DebuggingRecorder::new();
        let snapshotter = pharos_test_metrics::snapshotter(&recorder);
        let _guard = metrics::set_default_local_recorder(&recorder);
        record_search(&Ok(Vec::new()));
        record_search(&Err(ProviderError::Http {
            op: "search",
            status: 500,
        }));
        record_download(&Err(ProviderError::QuotaExceeded { reset_unix: 1 }));
        let seen: Vec<(String, String)> = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .filter(|(k, ..)| k.key().name() == "pharos_opensubtitles_total")
            .map(|(k, ..)| {
                let label = |n: &str| {
                    k.key()
                        .labels()
                        .find(|l| l.key() == n)
                        .map(|l| l.value().to_string())
                        .unwrap_or_default()
                };
                (label("op"), label("outcome"))
            })
            .collect();
        for want in [
            ("search", "empty"),
            ("search", "http"),
            ("download", "quota"),
        ] {
            assert!(
                seen.contains(&(want.0.to_string(), want.1.to_string())),
                "missing {want:?} in {seen:?}"
            );
        }
    }
}
