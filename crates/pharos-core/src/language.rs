//! Language-code equivalence: ISO 639-2/B (what Jellyfin reports), 639-2/T and
//! 639-1 spellings of one language compare equal. Probe tags carry whichever
//! form the muxer wrote, so `fre` and `fra` must be one language to a filter.

/// `(639-2/B, 639-2/T, 639-1)` per language. Languages whose B and T codes
/// coincide repeat the code in the second slot.
const LANGUAGES: &[(&str, &str, &str)] = &[
    ("alb", "sqi", "sq"),
    ("arm", "hye", "hy"),
    ("baq", "eus", "eu"),
    ("bur", "mya", "my"),
    ("chi", "zho", "zh"),
    ("cze", "ces", "cs"),
    ("dut", "nld", "nl"),
    ("fre", "fra", "fr"),
    ("geo", "kat", "ka"),
    ("ger", "deu", "de"),
    ("gre", "ell", "el"),
    ("ice", "isl", "is"),
    ("mac", "mkd", "mk"),
    ("mao", "mri", "mi"),
    ("may", "msa", "ms"),
    ("per", "fas", "fa"),
    ("rum", "ron", "ro"),
    ("slo", "slk", "sk"),
    ("tib", "bod", "bo"),
    ("wel", "cym", "cy"),
    ("ara", "ara", "ar"),
    ("cat", "cat", "ca"),
    ("dan", "dan", "da"),
    ("eng", "eng", "en"),
    ("fin", "fin", "fi"),
    ("heb", "heb", "he"),
    ("hin", "hin", "hi"),
    ("hun", "hun", "hu"),
    ("ind", "ind", "id"),
    ("ita", "ita", "it"),
    ("jpn", "jpn", "ja"),
    ("kor", "kor", "ko"),
    ("nor", "nor", "no"),
    ("pol", "pol", "pl"),
    ("por", "por", "pt"),
    ("rus", "rus", "ru"),
    ("spa", "spa", "es"),
    ("swe", "swe", "sv"),
    ("tha", "tha", "th"),
    ("tur", "tur", "tr"),
    ("ukr", "ukr", "uk"),
    ("vie", "vie", "vi"),
];

/// Every spelling the table knows, 639-2/B, 639-2/T and 639-1, so a copy of the
/// table elsewhere (the `language_codes` SQL table) can be checked against it.
pub fn known_codes() -> impl Iterator<Item = &'static str> {
    LANGUAGES.iter().flat_map(|(b, t, a)| [*b, *t, *a])
}

fn entry(code: &str) -> Option<&'static (&'static str, &'static str, &'static str)> {
    LANGUAGES
        .iter()
        .find(|(b, t, a)| code == *b || code == *t || code == *a)
}

/// The ISO 639-2/B form of `code` (trimmed, lower-cased). A code outside the
/// table is returned lower-cased as is.
pub fn canonical_language(code: &str) -> String {
    let c = code.trim().to_ascii_lowercase();
    entry(&c).map_or(c, |(b, _, _)| (*b).to_string())
}

/// Every spelling that names the same language as `code` (itself included),
/// lower-cased and de-duplicated, for matching against tags stored verbatim.
pub fn language_variants(code: &str) -> Vec<String> {
    let c = code.trim().to_ascii_lowercase();
    let mut out = vec![c.clone()];
    if let Some((b, t, a)) = entry(&c) {
        for v in [*b, *t, *a] {
            if !out.iter().any(|o| o == v) {
                out.push(v.to_string());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn b_t_and_alpha2_spellings_share_a_canonical_form() {
        for code in ["fre", "fra", "FR", " Fre "] {
            assert_eq!(canonical_language(code), "fre");
        }
        for code in ["ger", "deu", "de"] {
            assert_eq!(canonical_language(code), "ger");
        }
        assert_eq!(canonical_language("en"), "eng");
    }

    #[test]
    fn unknown_codes_pass_through_lower_cased() {
        assert_eq!(canonical_language("ZZZ"), "zzz");
        assert_eq!(language_variants("ZZZ"), ["zzz"]);
    }

    #[test]
    fn variants_cover_every_spelling_once() {
        assert_eq!(language_variants("fra"), ["fra", "fre", "fr"]);
        assert_eq!(language_variants("eng"), ["eng", "en"]);
    }
}
