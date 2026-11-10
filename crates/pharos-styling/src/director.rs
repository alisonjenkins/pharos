//! The rule-based director: source lines, line signals and segments to an
//! [`EffectPlan`] (spec 028 FR-009 to FR-011).
//!
//! Pure and deterministic. Rules are ordered and the first match wins; each
//! line's `reason` names the rule and the numbers it used.

use std::collections::BTreeMap;

use crate::plan::{EffectPlan, Intent, Karaoke, LineEffect, Placement};
use crate::signals::{LineSignals, Segment, SegmentKind, TrackSignals};
use crate::source::SourceLine;
use crate::text::unwrap_marks;
use crate::version::DIRECTOR_VERSION;

/// What [`direct`] decided, with a tally for the per-item log line.
#[derive(Clone, Debug, PartialEq)]
pub struct Direction {
    /// One entry per source line.
    pub plan: EffectPlan,
    /// Lines per intent; intents with no line are absent.
    pub counts: BTreeMap<Intent, usize>,
    /// The dialogue loudness the rules measured against (lines outside an
    /// intro or outro), for the per-item log line.
    pub loudness: Loudness,
}

/// A class of lines' loudness: its median, its spread, and the factor the
/// loudness rules' margins were widened by.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct Loudness {
    pub median_lufs: Option<f32>,
    /// Twice the lower-quartile-to-median distance of the lines' loudness,
    /// LU (an interquartile range for a symmetric spread).
    pub spread_lu: Option<f32>,
    /// 1.0 for dialogue as tightly mixed as [`REFERENCE_SPREAD_LU`]; more for
    /// a wider mix, so a film's loud scenes do not all read as shouting.
    pub scale: f32,
}

/// Loudness at or below this is silence, not speech: no signal (matches the
/// signal pass's -70 dBFS silence floor).
/// Pitch from a line voiced less than this is not trusted.
const MIN_VOICED_RATIO: f32 = 0.3;
const SILENCE_LUFS: f32 = -70.0;
/// Fewest lines a segment kind needs for its own medians; fewer borrow the
/// whole track's.
const MIN_GROUP: usize = 3;
/// How far above its median a line must be to shout, for a track whose
/// dialogue spread is [`REFERENCE_SPREAD_LU`]. Every loudness margin below
/// is scaled by [`Loudness::scale`] (B1204).
const SHOUT_DLU: f32 = 6.0;
/// The dialogue loudness spread ([`Loudness::spread_lu`]) the margins were tuned
/// on: compressed TV dialogue. A film mix is often twice as wide.
const REFERENCE_SPREAD_LU: f32 = 3.0;
/// The widest scaling: past this a track's spread is noise, not dynamics.
const MAX_SCALE: f32 = 3.0;
/// A shout with no `!` and no shouted capitals must also be pitched up by
/// this much over the median: a loud scene raises loudness, a raised voice
/// raises pitch too (B1204).
const SHOUT_PITCH_RATIO: f32 = 1.1;
/// Extra margin inside an Intro or Outro, where music swells (plan.md Risks).
const MUSIC_SHOUT_EXTRA_DLU: f32 = 3.0;
/// Loudness step per Shout intensity level.
const SHOUT_STEP_DLU: f32 = 3.0;
const ATTACK_DLU: f32 = 9.0;
const WHISPER_DLU: f32 = -8.0;
/// "About the median" for Thought.
const NEAR_MEDIAN_DLU: f32 = 4.0;
/// Narration by italics needs a line longer than this inside a Recap.
const NARRATION_MS: i64 = 4000;
const CAPS_RATIO: f32 = 0.8;
/// Most words in a one-line battle call.
const ATTACK_MAX_WORDS: usize = 3;
/// Most words in a bracketed sound name.
const SFX_MAX_WORDS: usize = 4;
/// First-word stems that make a parenthesised line a sound cue rather than
/// a thought: non-speech voices first, then common sounds.
const SOUND_CUES: [&str; 38] = [
    "laugh", "chuckl", "giggl", "snicker", "cackl", "chortl", "guffaw", "sigh", "gasp", "groan",
    "grunt", "scream", "shriek", "sob", "crying", "cries", "cough", "sneez", "pant", "snor",
    "yawn", "whimper", "growl", "roar", "cheer", "applau", "scoff", "snort", "sniff", "knock",
    "bang", "crash", "thud", "ring", "buzz", "beep", "explo", "thunder",
];
const PHONE_DEVICES: [&str; 11] = [
    "phone",
    "telephone",
    "radio",
    "intercom",
    "comms",
    "comm",
    "speaker",
    "loudspeaker",
    "speakerphone",
    "tv",
    "walkie-talkie",
];

/// The [`PHONE_DEVICES`] a bracket may name on its own, without `on` or
/// `over`.
const BARE_PHONE_DEVICES: [&str; 7] = [
    "phone",
    "telephone",
    "radio",
    "intercom",
    "speakerphone",
    "tv",
    "walkie-talkie",
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    Intro,
    Outro,
    Rest,
}

/// A line's audio after dropping silence and non-finite values.
#[derive(Clone, Copy, Default)]
struct Audio {
    lufs: Option<f32>,
    pitch: Option<f32>,
}

#[derive(Clone, Copy, Default)]
struct Medians {
    lufs: Option<f32>,
    pitch: Option<f32>,
    spread: Option<f32>,
    scale: f32,
}

/// Directs `lines`: one [`LineEffect`] each, from the first rule that
/// matches. `signals.lines[i]` belongs to `lines[i]`; a missing entry means
/// no audio. A line the audio cannot speak for can only match text rules.
pub fn direct(lines: &[SourceLine], signals: &TrackSignals, segments: &[Segment]) -> Direction {
    let kinds: Vec<Option<SegmentKind>> = lines.iter().map(|l| segment_of(l, segments)).collect();
    let audio: Vec<Audio> = (0..lines.len())
        .map(|i| audio_of(signals.lines.get(i)))
        .collect();
    let medians = medians(lines, &kinds, &audio);
    let rest = medians[Class::Rest as usize];
    let caps_track = is_caps_track(lines);

    let mut out = Vec::with_capacity(lines.len());
    let mut counts: BTreeMap<Intent, usize> = BTreeMap::new();
    for (i, line) in lines.iter().enumerate() {
        let med = medians[class_of(kinds[i]) as usize];
        let mut fx = decide(line, kinds[i], audio[i], med, caps_track);
        fx.line = i;
        if spans_cut(line, &signals.cuts_ms)
            && (fx.intent != Intent::Plain || fx.karaoke != Karaoke::None)
        {
            fx.static_only = true;
            fx.reason.push_str(" static=cut");
        }
        *counts.entry(fx.intent).or_insert(0) += 1;
        out.push(fx);
    }
    Direction {
        plan: EffectPlan {
            director_version: DIRECTOR_VERSION,
            lines: out,
        },
        counts,
        loudness: Loudness {
            median_lufs: rest.lufs,
            spread_lu: rest.spread,
            scale: rest.scale,
        },
    }
}

fn effect(intent: Intent, intensity: u8, reason: String) -> LineEffect {
    LineEffect {
        line: 0,
        intent,
        intensity,
        placement: Placement::Default,
        karaoke: Karaoke::None,
        reason,
        static_only: false,
    }
}

fn decide(
    line: &SourceLine,
    kind: Option<SegmentKind>,
    a: Audio,
    med: Medians,
    caps_track: bool,
) -> LineEffect {
    if line.typeset {
        return effect(Intent::Plain, 0, "rule=typeset".into());
    }
    let text = line.text.trim();
    let dlu = match (a.lufs, med.lufs) {
        (Some(l), Some(m)) => Some(l - m),
        _ => None,
    };
    let dlu_s = dlu.map_or("na".to_owned(), |d| format!("{d:+.1}"));
    let caps = caps_ratio(text);
    let groups = groups(text);
    let phone_tag = groups.iter().any(|g| is_phone_tag(g));
    let whisper_tag = groups.iter().any(|g| is_whisper_tag(g));
    let in_music = matches!(kind, Some(SegmentKind::Intro | SegmentKind::Outro));

    // Not in plan.md's table: a source style named "flashback" is the
    // author's own statement (plan.md, below the table).
    if line.style.to_lowercase().contains("flashback") && !line.style.is_empty() {
        return effect(
            Intent::Flashback,
            1,
            format!("rule=flashback style={}", line.style),
        );
    }
    // 1
    if is_sfx(text) {
        return effect(Intent::Sfx, 1, "rule=sfx".into());
    }
    // 2
    if has_narrator_tag(text) {
        return effect(Intent::Narration, 1, "rule=narration tag=narrator".into());
    }
    if line.italic && kind == Some(SegmentKind::Recap) && line.end_ms - line.start_ms > NARRATION_MS
    {
        let secs = (line.end_ms - line.start_ms) as f32 / 1000.0;
        return effect(
            Intent::Narration,
            1,
            format!("rule=narration italic secs={secs:.1} seg=recap"),
        );
    }
    // 3. Only the author's own cue: a dub's mix, a cartoon's voice
    // processing or a dull microphone looks like a phone line to the audio
    // (B661), so the high-band share no longer decides it.
    if phone_tag {
        return effect(Intent::Phone, 1, "rule=phone tag".into());
    }
    // 4
    if whisper_tag {
        return effect(Intent::Whisper, 1, "rule=whisper tag".into());
    }
    let k = med.scale;
    if let Some(d) = dlu.filter(|&d| d <= WHISPER_DLU * k) {
        return effect(
            Intent::Whisper,
            1,
            format!("rule=whisper dlu={d:+.1} scale={k:.2}"),
        );
    }
    // 5
    let in_parens = text.starts_with(['(', '（']) && unwrap_marks(text) != text;
    if let Some(d) = dlu.filter(|d| d.abs() <= NEAR_MEDIAN_DLU) {
        if in_parens || line.italic {
            return effect(Intent::Thought, 1, format!("rule=thought dlu={d:+.1}"));
        }
    }
    // 6
    if let Some(d) = dlu.filter(|&d| d > 0.0) {
        if line.italic {
            return effect(Intent::OffScreen, 1, format!("rule=off_screen dlu={d:+.1}"));
        }
    }
    // 7
    let words = text.split_whitespace().count();
    if let (Some(d), Some(p), Some(mp)) = (dlu, a.pitch, med.pitch) {
        // p >= 1.2 * mp, in integers-friendly form to avoid rounding at the edge.
        if (1..=ATTACK_MAX_WORDS).contains(&words)
            && text.ends_with(['!', '！'])
            && d >= ATTACK_DLU * k
            && mp > 0.0
            && p * 5.0 >= mp * 6.0
        {
            return effect(
                Intent::AttackCall,
                2,
                format!("rule=attack_call dlu={d:+.1} pitch={p:.0} med={mp:.0} scale={k:.2}"),
            );
        }
    }
    // 8
    let threshold = (SHOUT_DLU + if in_music { MUSIC_SHOUT_EXTRA_DLU } else { 0.0 }) * k;
    let bang = text.contains(['!', '！']);
    let raised = match (a.pitch, med.pitch) {
        (Some(p), Some(mp)) if mp > 0.0 => p >= mp * SHOUT_PITCH_RATIO,
        _ => false,
    };
    let loud = dlu.filter(|&d| d >= threshold && (bang || raised));
    let shouting_caps = !caps_track && caps.mostly && bang;
    if loud.is_some() || shouting_caps {
        let intensity = match loud {
            Some(d) if d >= threshold + 3.0 * SHOUT_STEP_DLU * k => 3,
            Some(d) => 1 + ((d - threshold) / (SHOUT_STEP_DLU * k)).clamp(0.0, 2.0) as u8,
            None => 1,
        };
        let cue = if bang {
            "bang"
        } else if raised {
            "pitch"
        } else {
            "none"
        };
        return effect(
            Intent::Shout,
            intensity,
            format!(
                "rule=shout dlu={dlu_s} thr={threshold:.1} cue={cue} caps={:.2}",
                caps.ratio
            ),
        );
    }
    // 9
    if in_music && line.karaoke.as_ref().is_some_and(|k| !k.is_empty()) {
        let seg = if kind == Some(SegmentKind::Intro) {
            "intro"
        } else {
            "outro"
        };
        let mut fx = effect(Intent::Plain, 1, format!("rule=karaoke seg={seg}"));
        fx.karaoke = Karaoke::Sweep;
        return fx;
    }
    // 10
    let reason = if a.lufs.is_none() {
        format!("rule=plain signal=none caps={:.2}", caps.ratio)
    } else {
        format!("rule=plain dlu={dlu_s} caps={:.2}", caps.ratio)
    };
    effect(Intent::Plain, 0, reason)
}

fn is_sfx(text: &str) -> bool {
    let inner = unwrap_marks(text);
    // Parentheses usually hold a thought; only a sound cue there is a sound
    // (B1203: `(laughs)` was drawn as a thought).
    if text.starts_with(['(', '（']) && !is_sound_cue(inner) {
        return false;
    }
    inner != text
        && !inner.is_empty()
        && inner.split_whitespace().count() <= SFX_MAX_WORDS
        && !inner.contains([',', '?', '.'])
        && !is_phone_tag(&inner.to_lowercase())
        && !is_whisper_tag(&inner.to_lowercase())
}

/// Whether a bracket's contents name a sound or a non-speech voice: its
/// first word starts with a [`SOUND_CUES`] stem (`laughs`, `sighing`,
/// `door creaks` has no stem and is not one; `knocking on door` is).
fn is_sound_cue(inner: &str) -> bool {
    let lower = inner.to_lowercase();
    let Some(first) = lower.split_whitespace().next() else {
        return false;
    };
    let first = first.trim_matches(|c: char| !c.is_alphabetic());
    SOUND_CUES.iter().any(|stem| first.starts_with(stem))
}

fn has_narrator_tag(text: &str) -> bool {
    let t = text.trim_start_matches(['-', ' ']).to_lowercase();
    t.starts_with("narrator:") || t.starts_with("narration:")
}

/// The lowercased contents of each bracket or parenthesis pair.
fn groups(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    for (i, c) in text.char_indices() {
        match c {
            '(' | '[' | '（' | '［' => start = Some(i + c.len_utf8()),
            ')' | ']' | '）' | '］' => {
                if let Some(s) = start.take() {
                    out.push(text[s..i].trim().to_lowercase());
                }
            }
            _ => {}
        }
    }
    out
}

/// A bracket naming a device the voice comes through: `[phone]`, `(radio)`,
/// `(over radio)`, `(on the phone)`, `[through the intercom]`.
fn is_phone_tag(group: &str) -> bool {
    let g = group.trim();
    let Some(rest) = ["on ", "over ", "via ", "through ", "into "]
        .iter()
        .find_map(|p| g.strip_prefix(p))
    else {
        // A bare word only for a device that never names a sound or a
        // speaker on its own: `[speaker]` and `(comms)` stay what they were.
        return BARE_PHONE_DEVICES.contains(&g);
    };
    let rest = rest.trim_start_matches("the ").trim_start_matches("a ");
    PHONE_DEVICES.contains(&rest.trim())
}

fn is_whisper_tag(group: &str) -> bool {
    let g = group.trim();
    g.starts_with("whisper") || matches!(g, "softly" | "quietly" | "hushed")
}

struct Caps {
    ratio: f32,
    /// At least two cased letters and a ratio of [`CAPS_RATIO`] or more.
    mostly: bool,
}

fn caps_ratio(text: &str) -> Caps {
    let (mut upper, mut cased) = (0u32, 0u32);
    for c in text.chars() {
        if c.is_uppercase() {
            upper += 1;
            cased += 1;
        } else if c.is_lowercase() {
            cased += 1;
        }
    }
    let ratio = if cased == 0 {
        0.0
    } else {
        upper as f32 / cased as f32
    };
    Caps {
        ratio,
        mostly: cased >= 2 && ratio >= CAPS_RATIO,
    }
}

/// At least 80% of the lines that have letters are mostly capitals: the
/// capitals rule would fire on everything, so it is off.
fn is_caps_track(lines: &[SourceLine]) -> bool {
    let (mut mostly, mut total) = (0usize, 0usize);
    for l in lines.iter().filter(|l| !l.typeset) {
        let c = caps_ratio(&l.text);
        if c.ratio > 0.0 || l.text.chars().any(char::is_lowercase) {
            total += 1;
            mostly += usize::from(c.mostly);
        }
    }
    total > 0 && mostly * 10 >= total * 8
}

fn spans_cut(line: &SourceLine, cuts_ms: &[u64]) -> bool {
    cuts_ms.iter().any(|&c| {
        let c = i64::try_from(c).unwrap_or(i64::MAX);
        line.start_ms < c && c < line.end_ms
    })
}

/// The kind of the first segment holding the line's midpoint.
fn segment_of(line: &SourceLine, segments: &[Segment]) -> Option<SegmentKind> {
    let mid = line.start_ms + (line.end_ms - line.start_ms) / 2;
    segments
        .iter()
        .find(|s| {
            let start = i64::try_from(s.start_ms).unwrap_or(i64::MAX);
            let end = i64::try_from(s.end_ms).unwrap_or(i64::MAX);
            start <= mid && mid < end
        })
        .map(|s| s.kind)
}

fn class_of(kind: Option<SegmentKind>) -> Class {
    match kind {
        Some(SegmentKind::Intro) => Class::Intro,
        Some(SegmentKind::Outro) => Class::Outro,
        _ => Class::Rest,
    }
}

fn audio_of(s: Option<&LineSignals>) -> Audio {
    let Some(s) = s else { return Audio::default() };
    Audio {
        lufs: s
            .loudness_lufs
            .filter(|l| l.is_finite() && *l > SILENCE_LUFS),
        pitch: s
            .pitch_hz
            .filter(|p| p.is_finite() && *p > 0.0 && s.voiced_ratio >= MIN_VOICED_RATIO),
    }
}

/// Medians and loudness spread per class (Intro, Outro, Rest), indexed by
/// `Class as usize`. A class with under [`MIN_GROUP`] values uses the whole
/// track's.
fn medians(lines: &[SourceLine], kinds: &[Option<SegmentKind>], audio: &[Audio]) -> [Medians; 3] {
    let pick = |class: Class, f: fn(&Audio) -> Option<f32>| -> Vec<f32> {
        let mut group = Vec::new();
        let mut all = Vec::new();
        for (i, a) in audio.iter().enumerate() {
            if lines[i].typeset {
                continue;
            }
            if let Some(v) = f(a) {
                all.push(v);
                if class_of(kinds[i]) == class {
                    group.push(v);
                }
            }
        }
        if group.len() >= MIN_GROUP {
            group
        } else {
            all
        }
    };
    [Class::Intro, Class::Outro, Class::Rest].map(|c| {
        let mut lufs = pick(c, |a| a.lufs);
        let spread = spread(&mut lufs);
        Medians {
            lufs: median(&mut lufs),
            pitch: median(&mut pick(c, |a| a.pitch)),
            spread,
            scale: spread.map_or(1.0, |s| (s / REFERENCE_SPREAD_LU).clamp(1.0, MAX_SCALE)),
        }
    })
}

/// The loudness spread: twice the distance from the lower quartile to the
/// median (nearest rank), or `None` under [`MIN_GROUP`] values. Only the
/// quieter half counts, so a track full of real shouts does not widen its
/// own margins; a wide mix is wide on both sides.
fn spread(v: &mut [f32]) -> Option<f32> {
    if v.len() < MIN_GROUP {
        return None;
    }
    v.sort_by(f32::total_cmp);
    let at = |q: usize| v[(v.len() - 1) * q / 4];
    Some(2.0 * (at(2) - at(1)))
}

fn median(v: &mut [f32]) -> Option<f32> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(f32::total_cmp);
    let n = v.len();
    Some(if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::source::{SourceFormat, Syllable};

    const MEDIAN_LUFS: f32 = -23.0;
    const MEDIAN_PITCH: f32 = 150.0;
    const MEDIAN_HB: f32 = 0.4;

    fn line(text: &str) -> SourceLine {
        SourceLine {
            start_ms: 10_000,
            end_ms: 13_000,
            text: text.to_owned(),
            raw: text.to_owned(),
            format: SourceFormat::Srt,
            style: "Default".into(),
            layer: 0,
            italic: false,
            typeset: false,
            karaoke: None,
            style_def: None,
        }
    }

    fn sig(lufs: f32) -> LineSignals {
        LineSignals {
            loudness_lufs: Some(lufs),
            peak_dbfs: Some(-3.0),
            pitch_hz: Some(MEDIAN_PITCH),
            voiced_ratio: 0.8,
            high_band_share: Some(MEDIAN_HB),
        }
    }

    /// One line under test.
    struct Case {
        line: SourceLine,
        sig: LineSignals,
        segs: Vec<Segment>,
        cuts: Vec<u64>,
    }

    fn case(text: &str) -> Case {
        Case {
            line: line(text),
            sig: sig(MEDIAN_LUFS),
            segs: Vec::new(),
            cuts: Vec::new(),
        }
    }

    impl Case {
        fn italic(mut self) -> Case {
            self.line.italic = true;
            self
        }
        fn secs(mut self, s: i64) -> Case {
            self.line.end_ms = self.line.start_ms + s * 1000;
            self
        }
        fn dlu(mut self, d: f32) -> Case {
            self.sig.loudness_lufs = Some(MEDIAN_LUFS + d);
            self
        }
        fn pitch(mut self, p: f32) -> Case {
            self.sig.pitch_hz = Some(p);
            self
        }
        fn hb(mut self, h: f32) -> Case {
            self.sig.high_band_share = Some(h);
            self
        }
        fn silent(mut self) -> Case {
            self.sig = LineSignals::NONE;
            self
        }
        fn seg(mut self, kind: SegmentKind) -> Case {
            self.segs.push(Segment {
                kind,
                start_ms: 0,
                end_ms: 60_000,
            });
            self
        }
        fn cut(mut self, ms: u64) -> Case {
            self.cuts.push(ms);
            self
        }
        fn karaoke(mut self) -> Case {
            self.line.karaoke = Some(vec![Syllable {
                dur_ms: 500,
                text: "la".into(),
            }]);
            self
        }

        /// Directs the line among ten unremarkable lines that set the
        /// track medians; returns the decision for the line under test.
        fn run(self) -> LineEffect {
            let mut lines: Vec<SourceLine> = (0..10).map(|_| line("Hello there")).collect();
            let mut sigs: Vec<LineSignals> = (0..10).map(|_| sig(MEDIAN_LUFS)).collect();
            lines.push(self.line);
            sigs.push(self.sig);
            let d = direct(
                &lines,
                &TrackSignals {
                    lines: sigs,
                    cuts_ms: self.cuts,
                },
                &self.segs,
            );
            d.plan.lines[10].clone()
        }
    }

    fn intent(c: Case) -> Intent {
        c.run().intent
    }

    #[test]
    fn sfx() {
        assert_eq!(intent(case("[explosion]")), Intent::Sfx);
        assert_eq!(intent(case("*bang*")), Intent::Sfx);
        assert_eq!(intent(case("(explosion)").silent()), Intent::Sfx);
        assert_eq!(intent(case("[Hello] there")), Intent::Plain);
        assert_eq!(intent(case("[on phone]")), Intent::Phone);
        assert_eq!(intent(case("*bang!*")), Intent::Sfx);
        assert_eq!(
            intent(case("*I love you so much, Mom*").silent()),
            Intent::Plain
        );
        assert_eq!(intent(case("[Is that you?]").silent()), Intent::Plain);
        assert_eq!(
            intent(case("*One two three four five*").silent()),
            Intent::Plain
        );
        assert_eq!(intent(case("*It ends here.*").silent()), Intent::Plain);
    }

    /// B1203: a parenthesised sound is a sound, not a thought.
    #[test]
    fn a_parenthesised_sound_cue_is_a_sound() {
        for cue in ["(sighs)", "(knocking on door)", "(EXPLOSION)", "（gasps）"] {
            assert_eq!(intent(case(cue)), Intent::Sfx, "{cue}");
        }
        assert_eq!(intent(case("(What is that)")), Intent::Thought);
        assert_eq!(intent(case("(sighs, then stops)")), Intent::Thought);
        assert_eq!(intent(case("(Laughing at me, are you?)")), Intent::Thought);
        assert_eq!(intent(case("(whispers)")), Intent::Whisper);
    }

    #[test]
    fn pitch_of_a_barely_voiced_line_is_not_used() {
        let mut c = case("Fire!").dlu(9.0).pitch(300.0);
        c.sig.voiced_ratio = 0.05;
        assert_eq!(intent(c), Intent::Shout);
        let mut c = case("Fire!").dlu(9.0).pitch(300.0);
        c.sig.voiced_ratio = 0.3;
        assert_eq!(intent(c), Intent::AttackCall);
    }

    #[test]
    fn narration() {
        assert_eq!(intent(case("NARRATOR: Long ago")), Intent::Narration);
        let recap = || case("It began.").italic().silent().seg(SegmentKind::Recap);
        assert_eq!(intent(recap().secs(5)), Intent::Narration);
        assert_eq!(intent(recap().secs(4)), Intent::Plain);
        let other = case("It began.").italic().silent().secs(5);
        assert_eq!(intent(other), Intent::Plain);
    }

    #[test]
    fn phone() {
        for cue in [
            "(over radio) Come in",
            "(on the phone) Come in",
            "[phone] Come in",
            "(radio) Come in",
            "[TV] Come in",
            "(intercom) Come in",
            "(through the intercom) Come in",
            "[on phone]",
            "[radio]",
            "[TV]",
        ] {
            assert_eq!(intent(case(cue)), Intent::Phone, "{cue}");
        }
        assert_eq!(intent(case("[speaker]")), Intent::Sfx);
        for not_a_cue in [
            "(phone rings)",
            "[radio static]",
            "(telephoto) Come in",
            "(speaker) Come in",
        ] {
            assert_ne!(intent(case(not_a_cue)), Intent::Phone, "{not_a_cue}");
        }
    }

    /// B661: a muffled voice is not a phone without the author's cue; a dub's
    /// mix, a cartoon's processing or a dull microphone all read as one.
    #[test]
    fn a_dull_voice_without_a_cue_is_not_a_phone() {
        for c in [
            case("Come in").hb(0.10),
            case("Come in").hb(0.01).dlu(-4.0),
            case("Come in").hb(0.0).dlu(0.0),
        ] {
            assert_eq!(intent(c), Intent::Plain);
        }
    }

    #[test]
    fn whisper() {
        assert_eq!(intent(case("(whispering) Quiet")), Intent::Whisper);
        assert_eq!(intent(case("Quiet").dlu(-8.0)), Intent::Whisper);
        assert_eq!(intent(case("Quiet").dlu(-7.5)), Intent::Plain);
    }

    #[test]
    fn thought() {
        assert_eq!(intent(case("(What is that)")), Intent::Thought);
        assert_eq!(
            intent(case("What is that").italic().dlu(2.0)),
            Intent::Thought
        );
        assert_eq!(intent(case("(What is that)").dlu(5.0)), Intent::Plain);
        assert_eq!(intent(case("(What is that)").silent()), Intent::Plain);
    }

    #[test]
    fn off_screen() {
        assert_eq!(
            intent(case("Over here").italic().dlu(5.0)),
            Intent::OffScreen
        );
        assert_eq!(
            intent(case("Over here").italic().dlu(-2.0)),
            Intent::Thought
        );
        assert_eq!(intent(case("Over here").italic().dlu(-5.0)), Intent::Plain);
        assert_eq!(intent(case("Over here").dlu(5.0)), Intent::Plain);
    }

    #[test]
    fn attack_call() {
        let c = || case("Fire!").dlu(9.0);
        assert_eq!(intent(c().pitch(180.0)), Intent::AttackCall);
        assert_eq!(intent(c().pitch(170.0)), Intent::Shout);
        assert_eq!(intent(case("Fire!").dlu(8.5).pitch(200.0)), Intent::Shout);
        assert_eq!(
            intent(case("Fire it now please!").dlu(9.0).pitch(200.0)),
            Intent::Shout
        );
        assert_eq!(intent(case("Fire").dlu(9.0).pitch(200.0)), Intent::Shout);
    }

    #[test]
    fn shout_and_its_intensity() {
        let shout = |d: f32| case("Stop right there!").dlu(d).run();
        assert_eq!(shout(5.9).intent, Intent::Plain);
        let s = shout(6.0);
        assert_eq!((s.intent, s.intensity), (Intent::Shout, 1));
        assert_eq!(shout(9.0).intensity, 2);
        assert_eq!(shout(12.0).intensity, 3);
        assert_eq!(shout(15.0).intensity, 3);
        assert_eq!(shout(30.0).intensity, 3);
    }

    /// B1204: a loud line with no `!` shouts only when its pitch is up too.
    #[test]
    fn a_loud_line_without_a_bang_needs_a_raised_voice() {
        let loud = || case("Stop right there").dlu(9.0);
        assert_eq!(intent(loud()), Intent::Plain, "a loud scene, a calm voice");
        assert_eq!(intent(loud().pitch(MEDIAN_PITCH * 1.05)), Intent::Plain);
        let s = loud().pitch(MEDIAN_PITCH * 1.1).run();
        assert_eq!(s.intent, Intent::Shout);
        assert!(s.reason.contains("cue=pitch"), "{}", s.reason);
        let mut unvoiced = loud();
        unvoiced.sig.voiced_ratio = 0.0;
        assert_eq!(intent(unvoiced), Intent::Plain, "no pitch, no bang");
    }

    /// B1204: the loudness margins widen with the track's dialogue spread,
    /// so a wide film mix shouts only at its own extremes.
    #[test]
    fn a_wide_mix_widens_the_loudness_margins() {
        // Dialogue spread over +-6 LU around -23: an interquartile range of
        // 6 LU, twice the reference, so every margin doubles.
        let run = |d: f32, text: &str| {
            let mut lines = Vec::new();
            let mut sigs = Vec::new();
            for i in 0..20 {
                lines.push(line("Hello there"));
                sigs.push(sig(MEDIAN_LUFS - 6.0 + (i % 5) as f32 * 3.0));
            }
            lines.push(line(text));
            sigs.push(sig(MEDIAN_LUFS + d));
            direct(
                &lines,
                &TrackSignals {
                    lines: sigs,
                    cuts_ms: vec![],
                },
                &[],
            )
        };
        let d = run(9.0, "Stop right there!");
        assert_eq!(d.loudness.median_lufs, Some(MEDIAN_LUFS));
        assert_eq!(d.loudness.spread_lu, Some(6.0));
        assert_eq!(d.loudness.scale, 2.0);
        assert_eq!(
            d.plan.lines[20].intent,
            Intent::Plain,
            "+9 is not a shout here"
        );
        let s = run(12.0, "Stop right there!").plan.lines[20].clone();
        assert_eq!((s.intent, s.intensity), (Intent::Shout, 1));
        assert!(s.reason.contains("thr=12.0"), "{}", s.reason);
        assert_eq!(run(-9.0, "Hello").plan.lines[20].intent, Intent::Plain);
        assert_eq!(run(-16.0, "Hello").plan.lines[20].intent, Intent::Whisper);
        assert!(run(-16.0, "Hello")
            .plan
            .lines
            .iter()
            .take(20)
            .all(|l| l.intent == Intent::Plain));
    }

    /// A track where a third of the lines really are shouted keeps the
    /// reference margins: only the quieter half sets the spread.
    #[test]
    fn many_real_shouts_do_not_widen_the_margins() {
        let mut lines = Vec::new();
        let mut sigs = Vec::new();
        for i in 0..30 {
            let loud = i % 3 == 0;
            lines.push(line(if loud { "Get down!" } else { "Hello there" }));
            sigs.push(sig(if loud {
                MEDIAN_LUFS + 10.0
            } else {
                MEDIAN_LUFS
            }));
        }
        let d = direct(
            &lines,
            &TrackSignals {
                lines: sigs,
                cuts_ms: vec![],
            },
            &[],
        );
        assert_eq!(d.loudness.scale, 1.0);
        assert_eq!(d.counts.get(&Intent::Shout), Some(&10));
    }

    #[test]
    fn a_tight_mix_keeps_the_reference_margins() {
        let d = case("Hi").run();
        let d2 = direct(
            &[line("a"), line("b"), line("c")],
            &TrackSignals {
                lines: vec![sig(-23.0), sig(-22.0), sig(-21.0)],
                cuts_ms: vec![],
            },
            &[],
        );
        assert_eq!(d.intent, Intent::Plain);
        assert_eq!(d2.loudness.scale, 1.0);
        let few = direct(
            &[line("a")],
            &TrackSignals {
                lines: vec![sig(-23.0)],
                cuts_ms: vec![],
            },
            &[],
        );
        assert_eq!((few.loudness.spread_lu, few.loudness.scale), (None, 1.0));
    }

    #[test]
    fn shout_by_capitals_needs_a_bang() {
        let s = case("STOP RIGHT NOW!").run();
        assert_eq!((s.intent, s.intensity), (Intent::Shout, 1));
        assert_eq!(intent(case("STOP RIGHT NOW")), Intent::Plain);
        assert_eq!(intent(case("Stop right now!")), Intent::Plain);
        assert_eq!(intent(case("STOP right now please!")), Intent::Plain);
    }

    #[test]
    fn capitals_work_without_audio() {
        assert_eq!(intent(case("STOP RIGHT NOW!").silent()), Intent::Shout);
    }

    #[test]
    fn an_all_caps_track_turns_the_capitals_rule_off() {
        let run = |caps: usize| {
            let lines: Vec<SourceLine> = (0..10)
                .map(|i| {
                    line(if i < caps {
                        "HELLO THERE!"
                    } else {
                        "Hello there"
                    })
                })
                .collect();
            direct(&lines, &TrackSignals::default(), &[])
        };
        assert!(run(8).plan.lines.iter().all(|l| l.intent == Intent::Plain));
        assert!(run(9).plan.lines.iter().all(|l| l.intent == Intent::Plain));
        let seven = run(7);
        assert_eq!(seven.counts.get(&Intent::Shout), Some(&7));
    }

    #[test]
    fn inside_an_intro_or_outro_shout_needs_three_more_lu() {
        for kind in [SegmentKind::Intro, SegmentKind::Outro] {
            let at = |d: f32| case("Stop right there!").dlu(d).seg(kind).run();
            assert_eq!(at(8.9).intent, Intent::Plain, "{kind:?}");
            let s = at(9.0);
            assert_eq!((s.intent, s.intensity), (Intent::Shout, 1), "{kind:?}");
            assert_eq!(at(12.0).intensity, 2);
        }
        let rec = case("Stop right there!")
            .dlu(6.0)
            .seg(SegmentKind::Recap)
            .run();
        assert_eq!(rec.intent, Intent::Shout);
    }

    #[test]
    fn medians_are_per_segment_kind() {
        let mut lines = Vec::new();
        let mut sigs = Vec::new();
        for i in 0..6 {
            let mut l = line("Hello there");
            l.start_ms = i * 1000;
            l.end_ms = i * 1000 + 900;
            lines.push(l);
            sigs.push(sig(-23.0));
        }
        for i in 0..4 {
            let mut l = line("La la!");
            l.start_ms = 100_000 + i * 1000;
            l.end_ms = 100_000 + i * 1000 + 900;
            lines.push(l);
            sigs.push(sig(-10.0));
        }
        let intro = [Segment {
            kind: SegmentKind::Intro,
            start_ms: 100_000,
            end_ms: 200_000,
        }];
        let track = TrackSignals {
            lines: sigs,
            cuts_ms: vec![],
        };
        let with = direct(&lines, &track, &intro);
        assert!(with.plan.lines.iter().all(|l| l.intent == Intent::Plain));
        let without = direct(&lines, &track, &[]);
        assert_eq!(without.counts.get(&Intent::Shout), Some(&4));
    }

    #[test]
    fn karaoke_sweep_only_inside_an_intro_or_outro() {
        for kind in [SegmentKind::Intro, SegmentKind::Outro] {
            let fx = case("la la").karaoke().seg(kind).run();
            assert_eq!((fx.intent, fx.karaoke), (Intent::Plain, Karaoke::Sweep));
            assert!(fx.reason.starts_with("rule=karaoke"), "{}", fx.reason);
        }
        assert_eq!(case("la la").karaoke().run().karaoke, Karaoke::None);
        assert_eq!(
            case("la la")
                .karaoke()
                .seg(SegmentKind::Recap)
                .run()
                .karaoke,
            Karaoke::None
        );
        assert_eq!(
            case("la la").seg(SegmentKind::Intro).run().karaoke,
            Karaoke::None
        );
    }

    #[test]
    fn no_audio_signal_is_plain_unless_a_text_rule_matches() {
        let quiet = || case("Something ordinary");
        assert_eq!(intent(quiet().silent()), Intent::Plain);
        assert_eq!(intent(case("Over here").italic().silent()), Intent::Plain);
        assert_eq!(intent(case("Fire!").silent()), Intent::Plain);
        assert_eq!(intent(case("(whispering) hi").silent()), Intent::Whisper);
        assert_eq!(intent(case("[boom]").silent()), Intent::Sfx);
        let mut sil = case("Stop right there");
        sil.sig.loudness_lufs = Some(-120.0);
        assert_eq!(sil.run().intent, Intent::Plain);
        let mut nan = case("Stop right there");
        nan.sig.loudness_lufs = Some(f32::NAN);
        assert_eq!(nan.run().intent, Intent::Plain);
    }

    #[test]
    fn a_line_past_the_end_of_the_signals_has_none() {
        let lines = vec![line("Hello"), line("Over here")];
        let d = direct(&lines, &TrackSignals::default(), &[]);
        assert_eq!(d.plan.lines.len(), 2);
        assert!(d.plan.lines.iter().all(|l| l.intent == Intent::Plain));
    }

    #[test]
    fn a_line_that_spans_a_cut_keeps_its_intent_but_stays_still() {
        let fx = case("Stop right there!").dlu(9.0).cut(11_000).run();
        assert_eq!(fx.intent, Intent::Shout);
        assert!(fx.static_only);
        assert!(fx.reason.contains("cut"), "{}", fx.reason);
        for edge in [10_000, 13_000, 5_000, 20_000] {
            let fx = case("Stop right there!").dlu(9.0).cut(edge).run();
            assert!(!fx.static_only, "cut at {edge}");
        }
        assert!(!case("Hello again").cut(11_000).run().static_only);
        let k = case("la")
            .karaoke()
            .seg(SegmentKind::Intro)
            .cut(11_000)
            .run();
        assert_eq!(k.karaoke, Karaoke::Sweep);
        assert!(k.static_only);
    }

    #[test]
    fn typeset_lines_are_plain_and_pass_through() {
        let mut c = case("[boom]").dlu(15.0);
        c.line.typeset = true;
        let fx = c.run();
        assert_eq!((fx.intent, fx.intensity), (Intent::Plain, 0));
        assert!(fx.reason.starts_with("rule=typeset"), "{}", fx.reason);
    }

    #[test]
    fn a_flashback_style_is_a_flashback() {
        let mut c = case("Back then");
        c.line.style = "Flashback-Main".into();
        assert_eq!(intent(c), Intent::Flashback);
    }

    #[test]
    fn reasons_name_the_rule_and_its_inputs() {
        let fx = case("Stop right there!").dlu(9.0).run();
        assert_eq!(fx.reason, "rule=shout dlu=+9.0 thr=6.0 cue=bang caps=0.07");
        assert_eq!(case("[boom]").run().reason, "rule=sfx");
        let plain = case("Hello again").run();
        assert!(plain.reason.starts_with("rule=plain"), "{}", plain.reason);
    }

    #[test]
    fn counts_add_up_and_the_plan_covers_every_line() {
        let lines: Vec<SourceLine> = ["[boom]", "Hi", "Hi", "(whispering) x"]
            .into_iter()
            .map(line)
            .collect();
        let d = direct(&lines, &TrackSignals::default(), &[]);
        assert_eq!(d.plan.director_version, DIRECTOR_VERSION);
        assert_eq!(d.counts.values().sum::<usize>(), lines.len());
        assert_eq!(d.counts.get(&Intent::Plain), Some(&2));
        for (i, l) in d.plan.lines.iter().enumerate() {
            assert_eq!(l.line, i);
        }
    }

    #[test]
    fn same_inputs_same_plan() {
        let mut lines: Vec<SourceLine> = (0..12).map(|_| line("Hello there")).collect();
        lines[3] = line("STOP!");
        lines[5] = line("(whispering) hi");
        let sigs = (0..12).map(|i| sig(-30.0 + i as f32 * 2.0)).collect();
        let t = TrackSignals {
            lines: sigs,
            cuts_ms: vec![11_000],
        };
        assert_eq!(direct(&lines, &t, &[]), direct(&lines, &t, &[]));
    }
}
