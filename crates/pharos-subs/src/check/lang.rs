//! FR-002 / FR-003: which tracks the aligner can judge at all.
//!
//! The aligner only knows `a`-`z` and `'`, and it scores text against speech,
//! so a track is checked only when it is in the audio's language and in the
//! Latin alphabet.

/// ISO 639-1 code for each language, with its 639-2 B and T forms and English name.
const LANGS: &[(&str, &[&str])] = &[
    ("ar", &["ara", "arabic"]),
    ("bg", &["bul", "bulgarian"]),
    ("ca", &["cat", "catalan"]),
    ("cs", &["cze", "ces", "czech"]),
    ("cy", &["wel", "cym", "welsh"]),
    ("da", &["dan", "danish"]),
    ("de", &["ger", "deu", "german"]),
    ("el", &["gre", "ell", "greek"]),
    ("en", &["eng", "english"]),
    ("es", &["spa", "spanish"]),
    ("et", &["est", "estonian"]),
    ("eu", &["baq", "eus", "basque"]),
    ("fa", &["per", "fas", "persian"]),
    ("fi", &["fin", "finnish"]),
    ("fr", &["fre", "fra", "french"]),
    ("ga", &["gle", "irish"]),
    ("gl", &["glg", "galician"]),
    ("he", &["heb", "hebrew"]),
    ("hi", &["hin", "hindi"]),
    ("hr", &["hrv", "croatian"]),
    ("hu", &["hun", "hungarian"]),
    ("id", &["ind", "indonesian"]),
    ("is", &["ice", "isl", "icelandic"]),
    ("it", &["ita", "italian"]),
    ("ja", &["jpn", "japanese"]),
    ("ko", &["kor", "korean"]),
    ("lt", &["lit", "lithuanian"]),
    ("lv", &["lav", "latvian"]),
    ("ms", &["may", "msa", "malay"]),
    ("nl", &["dut", "nld", "dutch"]),
    ("no", &["nor", "nob", "nno", "nb", "nn", "norwegian"]),
    ("pl", &["pol", "polish"]),
    ("pt", &["por", "portuguese"]),
    ("ro", &["rum", "ron", "romanian"]),
    ("ru", &["rus", "russian"]),
    ("sk", &["slo", "slk", "slovak"]),
    ("sl", &["slv", "slovenian"]),
    ("sr", &["srp", "serbian"]),
    ("sv", &["swe", "swedish"]),
    ("th", &["tha", "thai"]),
    ("tl", &["tgl", "fil", "tagalog", "filipino"]),
    ("tr", &["tur", "turkish"]),
    ("uk", &["ukr", "ukrainian"]),
    ("vi", &["vie", "vietnamese"]),
    ("zh", &["chi", "zho", "chinese"]),
];

/// Tags that say "no language": undetermined, multiple, none, uncoded.
const UNTAGGED: &[&str] = &["", "und", "mul", "zxx", "mis", "unknown"];

/// Normalises a language tag (`en`, `eng`, `en-GB`, `pt_BR`, `English`) to a
/// lower-case ISO 639-1 code where one is known, else to the lower-case primary
/// subtag. `None` for an absent or "undetermined" tag.
pub fn normalise_lang(tag: &str) -> Option<String> {
    let primary = tag
        .trim()
        .split(['-', '_'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if UNTAGGED.contains(&primary.as_str()) || !primary.chars().all(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    let code = LANGS
        .iter()
        .find(|(one, other)| *one == primary || other.contains(&primary.as_str()))
        .map_or(primary, |(one, _)| (*one).to_owned());
    Some(code)
}

/// A Latin letter: ASCII, Latin-1 Supplement, Latin Extended-A/B, IPA
/// Extensions (lower-case `ə` beside Extended-B's `Ə`), Latin Extended
/// Additional and Latin Extended-C/D/E. All fold into `a`-`z` with marks
/// dropped, or are Latin letters the aligner reads as their base.
fn folds_to_latin(c: char) -> bool {
    c.is_ascii_alphabetic()
        || matches!(
            c,
            '\u{00C0}'..='\u{02AF}'
                | '\u{1E00}'..='\u{1EFF}'
                | '\u{2C60}'..='\u{2C7F}'
                | '\u{A720}'..='\u{A7FF}'
                | '\u{AB30}'..='\u{AB6F}'
        )
}

/// Share of the letters in `texts` that fold into `a`-`z`; 0 when there are none.
pub fn latin_share<'a>(texts: impl IntoIterator<Item = &'a str>) -> f32 {
    let (mut latin, mut letters) = (0u32, 0u32);
    for c in texts
        .into_iter()
        .flat_map(str::chars)
        .filter(|c| c.is_alphabetic())
    {
        letters += 1;
        if folds_to_latin(c) {
            latin += 1;
        }
    }
    if letters == 0 {
        0.0
    } else {
        latin as f32 / letters as f32
    }
}

/// The FR-002 floor for [`latin_share`].
pub const MIN_LATIN_SHARE: f32 = 0.8;

/// Most frequent short words, for FR-003's guess at an untagged track.
const STOPWORDS: &[(&str, &[&str])] = &[
    (
        "en",
        &[
            "the", "a", "and", "to", "of", "you", "i", "it", "is", "that", "in", "we", "what",
            "this", "me", "for", "on", "my", "your", "have", "be", "do", "not", "are", "was", "he",
            "she", "they", "with", "just", "no", "don't", "i'm", "it's", "so", "can",
        ],
    ),
    (
        "fr",
        &[
            "le", "la", "les", "de", "des", "et", "un", "une", "je", "tu", "il", "elle", "est",
            "pas", "que", "qui", "vous", "nous", "on", "ce", "ça", "en", "à", "du", "mais",
            "c'est", "suis", "moi", "pour", "dans",
        ],
    ),
    (
        "de",
        &[
            "der", "die", "das", "und", "ich", "du", "er", "sie", "es", "ist", "nicht", "ein",
            "eine", "zu", "mit", "wir", "was", "den", "dem", "ja", "nein", "auf", "für", "mich",
            "dich", "hast", "habe", "sind", "aber", "wie",
        ],
    ),
    (
        "es",
        &[
            "el", "la", "los", "las", "de", "que", "y", "en", "un", "una", "no", "es", "lo", "me",
            "se", "te", "por", "con", "para", "qué", "yo", "tú", "está", "pero", "mi", "su", "al",
            "del", "muy", "eso",
        ],
    ),
    (
        "it",
        &[
            "il", "lo", "la", "i", "gli", "le", "di", "che", "e", "un", "una", "non", "è", "mi",
            "ti", "si", "per", "con", "ma", "sono", "io", "tu", "lui", "lei", "del", "della",
            "questo", "cosa", "ho", "hai",
        ],
    ),
    (
        "pt",
        &[
            "o", "a", "os", "as", "de", "que", "e", "um", "uma", "não", "é", "do", "da", "em",
            "eu", "você", "se", "me", "por", "para", "com", "mas", "isso", "está", "no", "na",
            "meu", "ele", "ela", "sim",
        ],
    ),
    (
        "nl",
        &[
            "de", "het", "een", "en", "ik", "je", "jij", "is", "niet", "dat", "van", "wat", "we",
            "hij", "zij", "ze", "in", "op", "met", "voor", "maar", "ja", "nee", "zijn", "heb",
            "hebt", "mijn", "dit", "er", "naar",
        ],
    ),
];

/// FR-003: at least this many words, and this share of them stopwords.
const STOPWORD_MIN_WORDS: usize = 20;
const STOPWORD_MIN_SHARE: f32 = 0.2;

/// Whether `texts` read as `lang` by a stopword count. `false` for a language
/// with no list, so an untagged track is only guessed where the guess is cheap
/// and safe.
pub fn looks_like<'a>(lang: &str, texts: impl IntoIterator<Item = &'a str>) -> bool {
    let Some((_, words)) = STOPWORDS.iter().find(|(l, _)| *l == lang) else {
        return false;
    };
    let (mut total, mut hits) = (0usize, 0usize);
    for w in texts
        .into_iter()
        .flat_map(str::split_whitespace)
        .map(|w| {
            w.trim_matches(|c: char| !c.is_alphanumeric() && c != '\'')
                .to_lowercase()
        })
        .filter(|w| !w.is_empty())
    {
        total += 1;
        if words.contains(&w.as_str()) {
            hits += 1;
        }
    }
    total >= STOPWORD_MIN_WORDS && hits as f32 >= STOPWORD_MIN_SHARE * total as f32
}

/// Why a track is not sent to the aligner. The `Display` form is the stored
/// reason (FR-002 / FR-003).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Skip {
    /// The track's language is not the audio's: `language_mismatch:<sub>/<audio>`.
    LanguageMismatch { sub: String, audio: String },
    /// Not in the Latin alphabet: `script:<sub>`.
    Script { sub: String },
    /// No usable tag on the track (or on the audio) and no stopword match.
    LanguageUntagged,
}

impl std::fmt::Display for Skip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LanguageMismatch { sub, audio } => write!(f, "language_mismatch:{sub}/{audio}"),
            Self::Script { sub } => write!(f, "script:{sub}"),
            Self::LanguageUntagged => f.write_str("language_untagged"),
        }
    }
}

/// FR-002 / FR-003 gate. Returns the language the track is checked in.
///
/// `audio_lang` is the tag of the audio stream the track plays over;
/// `texts` the track's cue texts.
pub fn gate<'a, I>(
    sub_lang: Option<&str>,
    audio_lang: Option<&str>,
    texts: I,
) -> Result<String, Skip>
where
    I: IntoIterator<Item = &'a str> + Clone,
{
    let sub = sub_lang.and_then(normalise_lang);
    let audio = audio_lang.and_then(normalise_lang);
    let lang = match (sub, audio) {
        (Some(sub), Some(audio)) if sub == audio => sub,
        (Some(sub), Some(audio)) => return Err(Skip::LanguageMismatch { sub, audio }),
        (None, Some(audio)) if looks_like(&audio, texts.clone()) => audio,
        _ => return Err(Skip::LanguageUntagged),
    };
    if latin_share(texts) < MIN_LATIN_SHARE {
        return Err(Skip::Script { sub: lang });
    }
    Ok(lang)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn tags_normalise_across_639_forms_regions_and_names() {
        for tag in ["en", "eng", "EN-gb", "en_US", "English"] {
            assert_eq!(normalise_lang(tag).as_deref(), Some("en"), "{tag}");
        }
        assert_eq!(normalise_lang("fre").as_deref(), Some("fr"));
        assert_eq!(normalise_lang("fra").as_deref(), Some("fr"));
        assert_eq!(normalise_lang("nob").as_deref(), Some("no"));
        assert_eq!(normalise_lang("xyz").as_deref(), Some("xyz"));
        for tag in ["", "und", "mul", "  ", "1x"] {
            assert_eq!(normalise_lang(tag), None, "{tag:?}");
        }
    }

    #[test]
    fn accented_latin_counts_as_latin_and_other_scripts_do_not() {
        assert!((latin_share(["Ça va très bien, señor"]) - 1.0).abs() < f32::EPSILON);
        assert!(latin_share(["Đây là tiếng Việt"]) > 0.99);
        assert!((latin_share(["Ə ə"]) - 1.0).abs() < f32::EPSILON);
        assert!(latin_share(["Привет, как дела"]) < 0.01);
        assert!(latin_share(["こんにちは"]) < 0.01);
        assert_eq!(latin_share(["123 ... !!"]), 0.0);
    }

    const ENGLISH: &[&str] = &[
        "I don't know what you want from me.",
        "It's the last time we do this, and you know it.",
        "Is that your car on the corner?",
        "No, we have to go to the station first.",
        "She said they were in the house with him.",
    ];

    #[test]
    fn same_language_latin_track_passes() {
        assert_eq!(
            gate(Some("eng"), Some("en"), ENGLISH.iter().copied()),
            Ok("en".to_owned())
        );
    }

    #[test]
    fn other_language_is_a_mismatch_named_by_both_codes() {
        let err = gate(Some("eng"), Some("jpn"), ENGLISH.iter().copied()).unwrap_err();
        assert_eq!(err.to_string(), "language_mismatch:en/ja");
    }

    #[test]
    fn same_language_other_script_is_skipped_by_script() {
        let err = gate(Some("rus"), Some("ru"), ["Привет, как дела"]).unwrap_err();
        assert_eq!(err.to_string(), "script:ru");
    }

    #[test]
    fn untagged_track_takes_the_audio_language_only_on_a_stopword_match() {
        assert_eq!(
            gate(None, Some("eng"), ENGLISH.iter().copied()),
            Ok("en".to_owned())
        );
        let err = gate(Some("und"), Some("deu"), ENGLISH.iter().copied()).unwrap_err();
        assert_eq!(err, Skip::LanguageUntagged);
        // Audio with no stopword list: no guess.
        assert_eq!(
            gate(None, Some("fin"), ENGLISH.iter().copied()),
            Err(Skip::LanguageUntagged)
        );
        // Untagged audio: nothing to compare with.
        assert_eq!(
            gate(Some("en"), None, ENGLISH.iter().copied()),
            Err(Skip::LanguageUntagged)
        );
    }

    #[test]
    fn a_few_words_are_not_enough_for_a_stopword_guess() {
        assert!(!looks_like("en", ["the and you"]));
    }
}
