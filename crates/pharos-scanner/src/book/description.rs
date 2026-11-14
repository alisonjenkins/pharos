//! A book's description as a reader sees it: the HTML a publisher wrote into
//! an EPUB's `dc:description` or an audiobook's tags, reduced to plain text.
//!
//! Descriptions arrive as markup (`<p>…</p>`), as escaped markup
//! (`&lt;p&gt;…`), or as plain text with entities. A detail page renders the
//! text as-is, so every one of those shapes is brought to the same plain text
//! here (B1440, V1414). A description shorter than [`MIN_DESCRIPTION_CHARS`]
//! is a stray character or a placeholder, not a synopsis, and is dropped.

/// Shortest text kept as a description. Placeholders such as `.` or
/// `Blurb` sit below it; the shortest real synopsis, a few words such as
/// "Alex returns.", sits above it.
pub const MIN_DESCRIPTION_CHARS: usize = 12;

/// What [`clean_description`] decided. Labels are a dashboard contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DescriptionOutcome {
    /// Plain text already; kept unchanged apart from whitespace.
    Kept,
    /// Markup or entities were removed.
    Cleaned,
    /// Too short once cleaned; dropped.
    TooShort,
}

impl DescriptionOutcome {
    pub const ALL: [Self; 3] = [Self::Kept, Self::Cleaned, Self::TooShort];

    pub fn label(self) -> &'static str {
        match self {
            Self::Kept => "kept",
            Self::Cleaned => "cleaned",
            Self::TooShort => "too_short",
        }
    }
}

/// `raw` as plain text with the decision, or `None` when it is empty.
/// Paragraphs are joined by a blank line.
pub fn clean_description(raw: &str) -> Option<(Option<String>, DescriptionOutcome)> {
    if raw.trim().is_empty() {
        return None;
    }
    // Escaped markup decodes to markup, so decode before stripping tags,
    // and decode again after for entities that sat inside the markup.
    let mut text = raw.to_owned();
    for _ in 0..2 {
        if !text.contains("&lt;") && !text.contains("&amp;") {
            break;
        }
        text = decode_entities(&text);
    }
    let text = decode_entities(&strip_tags(&text));
    let text = tidy(&text);
    let outcome = if text == tidy(raw) {
        DescriptionOutcome::Kept
    } else {
        DescriptionOutcome::Cleaned
    };
    if text.chars().count() < MIN_DESCRIPTION_CHARS {
        return Some((None, DescriptionOutcome::TooShort));
    }
    Some((Some(text), outcome))
}

/// [`clean_description`], counted on `pharos_book_description_total{source,
/// outcome}` and logged with the lengths it decided on.
pub fn record_description(raw: Option<&str>, source: &'static str) -> Option<String> {
    let raw = raw?;
    let (text, outcome) = clean_description(raw)?;
    metrics::counter!(
        "pharos_book_description_total",
        "source" => source,
        "outcome" => outcome.label(),
    )
    .increment(1);
    tracing::debug!(
        source,
        outcome = outcome.label(),
        raw_chars = raw.chars().count(),
        chars = text.as_deref().map_or(0, |t| t.chars().count()),
        "book description"
    );
    text
}

/// Block-level tags whose boundary is a paragraph break.
const BREAK_TAGS: &[&str] = &["p", "br", "div", "li", "h1", "h2", "h3", "h4", "h5", "h6"];

/// `s` with every `<…>` tag removed; block tags leave a paragraph break.
fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(open) = rest.find('<') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find('>') else {
            // A lone `<` is text, not a tag.
            out.push_str(&rest[open..]);
            return out;
        };
        let name: String = after[..close]
            .trim_start_matches('/')
            .chars()
            .take_while(char::is_ascii_alphanumeric)
            .collect::<String>()
            .to_ascii_lowercase();
        if name.is_empty() {
            out.push_str(&rest[open..open + 1 + close + 1]);
        } else if BREAK_TAGS.contains(&name.as_str()) {
            out.push_str("\n\n");
        }
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    out
}

/// `s` with HTML entities decoded. An unknown entity is left as written.
fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];
        let decoded = after
            .find(';')
            .filter(|&end| end <= 10)
            .and_then(|end| entity(&after[..end]).map(|c| (c, end)));
        match decoded {
            Some((c, end)) => {
                out.push(c);
                rest = &after[end + 1..];
            }
            None => {
                out.push('&');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

fn entity(name: &str) -> Option<char> {
    if let Some(num) = name.strip_prefix('#') {
        let code = match num.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
            None => num.parse().ok()?,
        };
        return char::from_u32(code);
    }
    Some(match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => ' ',
        "lsquo" => '\u{2018}',
        "rsquo" => '\u{2019}',
        "ldquo" => '\u{201C}',
        "rdquo" => '\u{201D}',
        "ndash" => '\u{2013}',
        "mdash" => '\u{2014}',
        "hellip" => '\u{2026}',
        "copy" => '\u{A9}',
        "eacute" => '\u{E9}',
        _ => return None,
    })
}

/// Runs of spaces collapsed within each paragraph; empty paragraphs dropped.
fn tidy(s: &str) -> String {
    s.replace("\r\n", "\n")
        .split("\n\n")
        .map(|p| p.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    const LONG: &str = "A sailor crosses an ocean alone and learns what the sea keeps.";

    fn text(raw: &str) -> Option<String> {
        clean_description(raw).and_then(|(t, _)| t)
    }

    #[test]
    fn plain_text_is_kept() {
        assert_eq!(
            clean_description(LONG),
            Some((Some(LONG.to_owned()), DescriptionOutcome::Kept))
        );
    }

    #[test]
    fn markup_becomes_paragraphs() {
        let raw = format!("<div><p>{LONG}</p><p>Second <b>part</b>.</p></div>");
        let (t, outcome) = clean_description(&raw).unwrap();
        assert_eq!(t.unwrap(), format!("{LONG}\n\nSecond part."));
        assert_eq!(outcome, DescriptionOutcome::Cleaned);
    }

    #[test]
    fn escaped_markup_is_decoded_then_stripped() {
        let raw = format!("&lt;p&gt;{LONG} &amp;amp; more&lt;/p&gt;");
        assert_eq!(text(&raw).unwrap(), format!("{LONG} & more"));
    }

    #[test]
    fn entities_decode() {
        let raw = format!("{LONG} It&rsquo;s &#8220;fine&#x201D; &mdash; mostly.");
        assert_eq!(
            text(&raw).unwrap(),
            format!("{LONG} It\u{2019}s \u{201C}fine\u{201D} \u{2014} mostly.")
        );
    }

    #[test]
    fn a_lone_angle_bracket_or_ampersand_is_text() {
        let raw = format!("{LONG} 3 < 4 & R&D");
        assert_eq!(text(&raw).unwrap(), raw);
    }

    #[test]
    fn a_short_description_is_dropped() {
        assert_eq!(
            clean_description("."),
            Some((None, DescriptionOutcome::TooShort))
        );
        assert_eq!(
            clean_description("<p>Blurb</p>"),
            Some((None, DescriptionOutcome::TooShort))
        );
        assert_eq!(clean_description("   "), None);
    }

    #[test]
    fn outcome_labels_are_distinct() {
        let labels: std::collections::HashSet<_> =
            DescriptionOutcome::ALL.iter().map(|o| o.label()).collect();
        assert_eq!(labels.len(), DescriptionOutcome::ALL.len());
    }

    #[test]
    fn the_decision_is_counted() {
        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let snap = recorder.snapshotter();
        metrics::with_local_recorder(&recorder, || {
            assert_eq!(record_description(Some("."), "book"), None);
            assert!(record_description(Some(LONG), "book").is_some());
        });
        let seen: Vec<String> = snap
            .snapshot()
            .into_vec()
            .into_iter()
            .filter(|(k, ..)| k.key().name() == "pharos_book_description_total")
            .map(|(k, ..)| {
                k.key()
                    .labels()
                    .map(|l| format!("{}={}", l.key(), l.value()))
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .collect();
        assert!(
            seen.contains(&"source=book,outcome=too_short".to_owned()),
            "{seen:?}"
        );
        assert!(
            seen.contains(&"source=book,outcome=kept".to_owned()),
            "{seen:?}"
        );
    }
}
