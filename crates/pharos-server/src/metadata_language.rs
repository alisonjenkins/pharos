//! The language show and episode text is fetched and shown in (ADR-0062).
//!
//! TVDB's base records are written in the language the show was made in, so
//! reading them as-is put Japanese and French descriptions in front of English
//! speakers (B564). Every provider text request now names a language, and its
//! result is counted in `pharos_metadata_translation_total{provider,language,
//! outcome}`.

/// The language stored in the default `overview` columns and shown to a user
/// who has not picked one.
pub const DEFAULT_LANGUAGE: &str = "eng";

/// The languages the description-language setting offers, as TVDB names them
/// (ISO 639-2/T). Anything else a client writes is still honoured, but is
/// counted under `other`.
pub const OFFERED: [&str; 9] = [
    "eng", "jpn", "fra", "deu", "spa", "ita", "por", "kor", "zho",
];

/// The setting value meaning "the language the show was made in". The same
/// literal the audio preference uses for the same idea.
pub const ORIGINAL: &str = "OriginalLanguage";

/// A user's description-language setting, parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DescriptionLanguage {
    /// Unset, or English: the default columns already hold this.
    Default,
    /// A specific language, as a TVDB three-letter code.
    Code(String),
    /// The provider's base-record text, whatever its language.
    Original,
}

impl DescriptionLanguage {
    /// Parse the stored setting. Two-letter and bibliographic codes map to the
    /// code TVDB uses (`fr`/`fre` → `fra`); `eng` and empty are the default.
    pub fn parse(raw: &str) -> Self {
        let raw = raw.trim();
        if raw.is_empty() {
            return Self::Default;
        }
        if raw.eq_ignore_ascii_case(ORIGINAL) || raw.eq_ignore_ascii_case("original") {
            return Self::Original;
        }
        match normalize(raw) {
            Some(code) if code == DEFAULT_LANGUAGE => Self::Default,
            Some(code) => Self::Code(code),
            None => Self::Default,
        }
    }
}

/// A language code in the three-letter form TVDB uses, or `None` when `raw`
/// is not shaped like a language code at all.
pub fn normalize(raw: &str) -> Option<String> {
    let lower = raw.trim().to_ascii_lowercase();
    let mapped = match lower.as_str() {
        "en" => "eng",
        "ja" => "jpn",
        "fr" | "fre" => "fra",
        "de" | "ger" => "deu",
        "es" => "spa",
        "it" => "ita",
        "pt" => "por",
        "ko" => "kor",
        "zh" | "chi" => "zho",
        other => other,
    };
    (mapped.len() == 3 && mapped.bytes().all(|b| b.is_ascii_lowercase()))
        .then(|| mapped.to_string())
}

/// Bounded metric label for a requested language: one of [`OFFERED`], else
/// `other`. Languages come from user settings any API client can write.
pub fn language_label(code: &str) -> &'static str {
    OFFERED
        .iter()
        .find(|o| o.eq_ignore_ascii_case(code))
        .copied()
        .unwrap_or("other")
}

/// What asking a provider for its text in one language came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranslationOutcome {
    /// The provider had text in that language.
    Translated,
    /// The record was written in that language; its base text is the answer.
    Original,
    /// The provider has no text in that language.
    Missing,
    /// The request failed; nothing was recorded, so it is asked again.
    Error,
}

impl TranslationOutcome {
    /// Stable, bounded-cardinality metric label. Asserted distinct in a test.
    pub fn label(self) -> &'static str {
        match self {
            Self::Translated => "translated",
            Self::Original => "original",
            Self::Missing => "missing",
            Self::Error => "error",
        }
    }
}

/// Count one language decision in
/// `pharos_metadata_translation_total{provider,language,outcome}`.
pub fn record_translation(provider: &'static str, language: &str, outcome: TranslationOutcome) {
    metrics::counter!(
        "pharos_metadata_translation_total",
        "provider" => provider,
        "language" => language_label(language),
        "outcome" => outcome.label(),
    )
    .increment(1);
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn outcome_labels_are_distinct() {
        let all = [
            TranslationOutcome::Translated,
            TranslationOutcome::Original,
            TranslationOutcome::Missing,
            TranslationOutcome::Error,
        ];
        let labels: std::collections::HashSet<_> = all.iter().map(|o| o.label()).collect();
        assert_eq!(labels.len(), all.len());
    }

    #[test]
    fn language_label_is_bounded() {
        assert_eq!(language_label("jpn"), "jpn");
        assert_eq!(language_label("ENG"), "eng");
        assert_eq!(language_label("xyz"), "other");
        assert_eq!(language_label("'; DROP TABLE"), "other");
        let labels: std::collections::HashSet<_> =
            OFFERED.iter().map(|c| language_label(c)).collect();
        assert_eq!(labels.len(), OFFERED.len());
    }

    #[test]
    fn setting_parses_to_tvdb_codes() {
        assert_eq!(DescriptionLanguage::parse(""), DescriptionLanguage::Default);
        assert_eq!(
            DescriptionLanguage::parse("eng"),
            DescriptionLanguage::Default
        );
        assert_eq!(
            DescriptionLanguage::parse("en"),
            DescriptionLanguage::Default
        );
        assert_eq!(
            DescriptionLanguage::parse("fre"),
            DescriptionLanguage::Code("fra".into())
        );
        assert_eq!(
            DescriptionLanguage::parse("ja"),
            DescriptionLanguage::Code("jpn".into())
        );
        assert_eq!(
            DescriptionLanguage::parse("OriginalLanguage"),
            DescriptionLanguage::Original
        );
        assert_eq!(
            DescriptionLanguage::parse("not a code"),
            DescriptionLanguage::Default
        );
    }

    /// The counter is the dashboard contract: one sample per decision, with
    /// the bounded labels. Fails if `record_translation` stops emitting.
    #[test]
    fn record_translation_emits_the_counter() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};
        let recorder = DebuggingRecorder::new();
        let snapshotter = pharos_test_metrics::snapshotter(&recorder);
        let _guard = metrics::set_default_local_recorder(&recorder);
        record_translation("tvdb", "jpn", TranslationOutcome::Missing);
        record_translation("tvdb", "klingon", TranslationOutcome::Translated);
        let mut seen = Vec::new();
        for (ck, _, _, v) in snapshotter.snapshot().into_vec() {
            if ck.key().name() != "pharos_metadata_translation_total" {
                continue;
            }
            let label = |n: &str| {
                ck.key()
                    .labels()
                    .find(|l| l.key() == n)
                    .map(|l| l.value().to_string())
                    .unwrap_or_default()
            };
            if let DebugValue::Counter(c) = v {
                seen.push((label("provider"), label("language"), label("outcome"), c));
            }
        }
        seen.sort();
        assert_eq!(
            seen,
            vec![
                ("tvdb".into(), "jpn".into(), "missing".into(), 1),
                ("tvdb".into(), "other".into(), "translated".into(), 1),
            ]
        );
    }
}
