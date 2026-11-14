//! The recogniser (spec 042 slice 2, ADR-0125): whisper.cpp's tokens turned
//! into timed words, and the second decode of a window it lost to a caption.
//! The model itself runs in [`whisper`], behind the `recognise` feature;
//! everything here is plain data so it is tested without the model.

#[cfg(feature = "recognise")]
pub mod whisper;

use crate::obs::RECOGNISE_RETRY_TOTAL;
use crate::transcript::Word;

/// Seconds of audio whisper decodes at once.
pub const WINDOW_S: f64 = 30.0;
/// A segment this long with at most [`RETRY_MAX_WORDS`] words is whisper
/// putting a caption ("Thank you.", "I'm sorry.") over a whole window.
pub const RETRY_MIN_SPAN_S: f64 = 25.0;
pub const RETRY_MAX_WORDS: usize = 3;
/// A retry replaces the first decode only when it hears a passage; fewer
/// words is another caption, and keeping the first leaves the song as it was.
pub const RETRY_MIN_WORDS: usize = 8;
/// ...and only when at least this share of those words differ: a retry
/// can loop too (one phrase seven times over a spoken intro), and a loop
/// put there moves the lines it anchors. The passage it recovered had 0.68.
pub const RETRY_MIN_DISTINCT: f64 = 0.5;

/// How one decode runs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Decode {
    /// Beam width; `None` is greedy.
    pub beam: Option<i32>,
    /// whisper.cpp retries a window at a higher temperature when its tokens'
    /// entropy is under this. At the default 2.4 a chorus sung five times in
    /// one window counted as a loop and the retry kept one line of it; 2.3
    /// keeps the chorus, and 2.0 let a real loop of one caption through.
    pub entropy_thold: f32,
    /// Text tokens carried from one window into the next. Zero: carried
    /// text made whisper repeat lines into windows where they were not sung,
    /// and `no_context` clears the history only once per call.
    pub n_max_text_ctx: i32,
}

impl Decode {
    /// The decode of the whole song.
    pub const FIRST: Self = Self {
        beam: None,
        entropy_thold: 2.3,
        n_max_text_ctx: 0,
    };
    /// The second decode of a lost window. Beam search over the whole song
    /// changed most songs' answers; on the lost window alone it heard the
    /// passage greedy decoding missed.
    pub const RETRY: Self = Self {
        beam: Some(5),
        ..Self::FIRST
    };
}

/// One text token; times are seconds from the start of the song.
#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub text: String,
    pub prob: f32,
    /// Where whisper's timestamp rules put the token's start.
    pub t0_s: f64,
    /// Where DTW over the alignment heads puts the token's end.
    pub dtw_s: f64,
}

/// One decoded segment; times are seconds from the start of the song.
#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    pub start_s: f64,
    pub end_s: f64,
    pub text: String,
    pub tokens: Vec<Token>,
}

/// Groups tokens into words: a token that starts with a space opens one.
fn groups(tokens: &[Token]) -> Vec<&[Token]> {
    let mut out: Vec<&[Token]> = Vec::new();
    let mut start = 0;
    for (i, t) in tokens.iter().enumerate() {
        if i > start && t.text.starts_with(' ') {
            out.push(&tokens[start..i]);
            start = i;
        }
    }
    if start < tokens.len() {
        out.push(&tokens[start..]);
    }
    out.retain(|g| g.iter().any(|t| !t.text.trim().is_empty()));
    out
}

/// The words in `tokens`, in order. DTW times mark where a token ends, so a
/// word starts where the word before it ends (taking a word's own first DTW
/// time put starts ~380 ms late); the first word starts at its own `t0`.
pub fn words(tokens: &[Token]) -> Vec<Word> {
    let mut prev_end: Option<f64> = None;
    groups(tokens)
        .into_iter()
        .filter_map(|g| {
            let (first, last) = (g.first()?, g.last()?);
            let start_s = prev_end.unwrap_or(first.t0_s);
            let end_s = last.dtw_s.max(start_s);
            prev_end = Some(last.dtw_s);
            Some(Word {
                text: g.iter().map(|t| t.text.as_str()).collect(),
                start_s,
                end_s,
                prob: g.iter().map(|t| f64::from(t.prob)).sum::<f64>() / g.len() as f64,
            })
        })
        .collect()
}

/// Whether whisper put one caption over a whole window.
pub fn wants_retry(seg: &Segment) -> bool {
    seg.end_s - seg.start_s >= RETRY_MIN_SPAN_S && groups(&seg.tokens).len() <= RETRY_MAX_WORDS
}

/// How a retried window ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The retry heard a passage and took the window's place.
    Replaced,
    /// The retry heard too little; the first decode stands.
    Kept,
    /// The retry heard one phrase over and over; the first decode stands.
    Looped,
}

impl Outcome {
    pub const ALL: [Self; 3] = [Self::Replaced, Self::Kept, Self::Looped];

    /// The `outcome` label on [`RECOGNISE_RETRY_TOTAL`].
    pub fn label(self) -> &'static str {
        match self {
            Self::Replaced => "replaced",
            Self::Kept => "kept",
            Self::Looped => "looped",
        }
    }
}

/// One window decoded a second time.
#[derive(Debug, Clone, PartialEq)]
pub struct Retry {
    pub start_s: f64,
    pub end_s: f64,
    /// What the first decode heard over the window.
    pub first: String,
    /// Words the retry heard inside the window.
    pub words: usize,
    /// The share of those words that differ from each other.
    pub distinct: f64,
    pub outcome: Outcome,
}

impl Retry {
    /// Logs the retry with its window and why, and counts it.
    pub fn record(&self) {
        tracing::info!(
            start_s = self.start_s,
            end_s = self.end_s,
            first = %self.first,
            words = self.words,
            min_words = RETRY_MIN_WORDS,
            distinct = self.distinct,
            min_distinct = RETRY_MIN_DISTINCT,
            outcome = self.outcome.label(),
            reason = "one short segment covered the window",
            "recogniser window retried"
        );
        metrics::counter!(RECOGNISE_RETRY_TOTAL, "outcome" => self.outcome.label()).increment(1);
    }
}

/// The retry's segments cut to the first segment's span: past its end the
/// first decode's next segments already cover the audio.
fn within(first: &Segment, retry: Vec<Segment>) -> Vec<Segment> {
    retry
        .into_iter()
        .filter_map(|mut s| {
            s.tokens.retain(|t| t.dtw_s <= first.end_s);
            s.end_s = s.end_s.min(first.end_s);
            (!s.tokens.is_empty()).then_some(s)
        })
        .collect()
}

/// The share of `segments`' words that differ, ignoring case and
/// punctuation; 1.0 when there are none.
fn distinct_share(segments: &[Segment]) -> f64 {
    let words: Vec<String> = segments
        .iter()
        .flat_map(|s| groups(&s.tokens))
        .map(|g| {
            g.iter()
                .flat_map(|t| t.text.chars())
                .filter(|c| c.is_alphanumeric() || *c == '\'')
                .flat_map(char::to_lowercase)
                .collect()
        })
        .collect();
    if words.is_empty() {
        return 1.0;
    }
    let mut seen = words.clone();
    seen.sort_unstable();
    seen.dedup();
    seen.len() as f64 / words.len() as f64
}

/// Decodes again each segment that [`wants_retry`], over [`WINDOW_S`] from
/// its start, and takes the retry in its place when it hears at least
/// [`RETRY_MIN_WORDS`] words there, [`RETRY_MIN_DISTINCT`] of them distinct. `decode` gets the window's start and
/// end in seconds and returns segments in song time.
pub fn retry_windows<E>(
    segments: Vec<Segment>,
    song_s: f64,
    mut decode: impl FnMut(f64, f64) -> Result<Vec<Segment>, E>,
) -> Result<(Vec<Segment>, Vec<Retry>), E> {
    let mut out = Vec::with_capacity(segments.len());
    let mut retries = Vec::new();
    for seg in segments {
        if !wants_retry(&seg) {
            out.push(seg);
            continue;
        }
        let heard = within(
            &seg,
            decode(seg.start_s, (seg.start_s + WINDOW_S).min(song_s))?,
        );
        let words = heard.iter().map(|s| groups(&s.tokens).len()).sum();
        let distinct = distinct_share(&heard);
        let outcome = if words < RETRY_MIN_WORDS {
            Outcome::Kept
        } else if distinct < RETRY_MIN_DISTINCT {
            Outcome::Looped
        } else {
            Outcome::Replaced
        };
        retries.push(Retry {
            start_s: seg.start_s,
            end_s: seg.end_s,
            first: seg.text.trim().to_owned(),
            words,
            distinct,
            outcome,
        });
        match outcome {
            Outcome::Replaced => out.extend(heard),
            Outcome::Kept | Outcome::Looped => out.push(seg),
        }
    }
    Ok((out, retries))
}

#[cfg(test)]
mod tests;
