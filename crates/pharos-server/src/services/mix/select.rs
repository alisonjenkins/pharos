//! The mix engine: which songs come next, given a set of rules (spec 024).
//!
//! Pure on purpose. It sees songs as [`Song`] rows the service has already
//! scoped to what the user may hear, and takes the clock and the random
//! source as arguments, so every decision is reproducible in a test.
//!
//! One call picks up to `count` songs:
//!
//! 1. **Hard rules** (`Require`, `Exclude`) decide which songs are eligible.
//!    They are never relaxed: a mix that runs out says so (`exhausted`).
//! 2. **Freshness**: a song in `recent` (played or already queued) or picked
//!    earlier in this call is not picked again while a fresh one exists.
//! 3. **Shares** ("about 30% from the 90s"): for each share rule the engine
//!    looks at the last [`SHARE_WINDOW`] songs, sees how far the window is
//!    from the target, and decides whether the next song must match. The
//!    correction overshoots the target by the gap, so the proportion
//!    converges instead of drifting.
//! 4. **Spacing**: no song by an artist heard in the last `artist_gap`
//!    songs, nor from an album heard in the last `album_gap`.
//! 5. **Soft rules** (`Prefer`, `Avoid`, and the seed's likeness) weight a
//!    random draw among what is left.
//!
//! Steps 2 to 4 relax, in that order of last resort, when they would leave
//! nothing; each relaxation is reported, so a mix that bends is visible.

use std::collections::{HashMap, HashSet};

/// How many of the latest songs a share rule measures itself over.
pub const SHARE_WINDOW: usize = 20;
/// The most songs one call returns.
pub const MAX_COUNT: usize = 50;
/// The most rules one mix holds.
pub const MAX_RULES: usize = 32;
/// A `Prefer` match multiplies a song's weight by this; `Avoid` divides.
pub const PREFER_FACTOR: f64 = 4.0;
/// A seed likeness at or above this counts as "like the seed".
pub const LIKE_THRESHOLD: f64 = 0.35;

const DAY: i64 = 86_400;

/// One song, as the engine sees it. Text fields are compared
/// case-insensitively; the service fills them from the stored item.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Song {
    pub id: u64,
    pub artist: Option<String>,
    pub album_artist: Option<String>,
    pub album: Option<String>,
    pub genres: Vec<String>,
    pub year: Option<u16>,
    pub duration_s: Option<u32>,
    /// Unix seconds the song was added to the library.
    pub added_at: Option<i64>,
    pub play_count: u32,
    /// Unix seconds it was last played; 0 is never.
    pub last_played_at: i64,
    pub favorite: bool,
    /// Beats per minute: the file's tag, else the scanner's analysis.
    pub bpm: Option<f32>,
    /// How intense it sounds, 0 (calm) to 1, from the scanner's analysis.
    pub energy: Option<f32>,
}

impl Song {
    /// Who the song is by, for spacing and likeness: the track artist,
    /// else the album artist.
    fn by(&self) -> Option<&str> {
        self.artist.as_deref().or(self.album_artist.as_deref())
    }
}

/// How strongly a rule holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Weight {
    /// Only songs that match are eligible.
    Require,
    /// Songs that match are [`PREFER_FACTOR`] times as likely.
    Prefer,
    /// Songs that match are [`PREFER_FACTOR`] times less likely.
    Avoid,
    /// Songs that match are never picked.
    Exclude,
}

impl Weight {
    pub fn label(self) -> &'static str {
        match self {
            Weight::Require => "require",
            Weight::Prefer => "prefer",
            Weight::Avoid => "avoid",
            Weight::Exclude => "exclude",
        }
    }
}

/// What a rule tests a song for.
#[derive(Debug, Clone, PartialEq)]
pub enum Test {
    Genre(String),
    /// The track artist or the album artist.
    Artist(String),
    Album(String),
    /// Released in `from..=to`; a song with no year never matches.
    Years {
        from: u16,
        to: u16,
    },
    Favorite,
    NeverPlayed,
    /// Played `min..=max` times.
    PlayCount {
        min: u32,
        max: u32,
    },
    /// Not played in the last `n` days (never played counts).
    NotPlayedFor {
        days: u32,
    },
    /// Played in the last `n` days.
    PlayedWithin {
        days: u32,
    },
    /// Added to the library in the last `n` days.
    AddedWithin {
        days: u32,
    },
    /// Between `min_s` and `max_s` seconds long; unknown length never matches.
    Length {
        min_s: u32,
        max_s: u32,
    },
    /// Like the seed: same artist, or close in genre, era, tempo and
    /// energy. A `Prefer` weights by how alike, not just whether.
    LikeSeed,
    /// `min..=max` beats per minute; a song with no tempo never matches.
    Tempo {
        min: u16,
        max: u16,
    },
    /// Energy `min..=max` in percent (0 calm, 100 intense); a song not yet
    /// analysed never matches.
    Energy {
        min: u8,
        max: u8,
    },
}

impl Test {
    /// The bounded `test` label for metrics and logs.
    pub fn label(&self) -> &'static str {
        match self {
            Test::Genre(_) => "genre",
            Test::Artist(_) => "artist",
            Test::Album(_) => "album",
            Test::Years { .. } => "years",
            Test::Favorite => "favorite",
            Test::NeverPlayed => "never_played",
            Test::PlayCount { .. } => "play_count",
            Test::NotPlayedFor { .. } => "not_played_for",
            Test::PlayedWithin { .. } => "played_within",
            Test::AddedWithin { .. } => "added_within",
            Test::Length { .. } => "length",
            Test::LikeSeed => "like_seed",
            Test::Tempo { .. } => "tempo",
            Test::Energy { .. } => "energy",
        }
    }

    /// The words a listener sees for why a song was picked.
    pub fn describe(&self, seed: Option<&Song>) -> String {
        match self {
            Test::Genre(g) => g.clone(),
            Test::Artist(a) => a.clone(),
            Test::Album(a) => a.clone(),
            Test::Years { from, to } if from == to => from.to_string(),
            Test::Years { from, to } => format!("{from}–{to}"),
            Test::Favorite => "A favourite".to_owned(),
            Test::NeverPlayed => "Never played".to_owned(),
            Test::PlayCount { min, max } if *max == u32::MAX => format!("Played {min}+ times"),
            Test::PlayCount { min, max } => format!("Played {min}–{max} times"),
            Test::NotPlayedFor { days } => format!("Not played for {days} days"),
            Test::PlayedWithin { days } => format!("Played in the last {days} days"),
            Test::AddedWithin { days } => format!("Added in the last {days} days"),
            Test::Length { min_s, max_s } => format!("{}–{} long", mmss(*min_s), mmss(*max_s)),
            Test::Tempo { min, max } => format!("{min}–{max} BPM"),
            Test::Energy { min, max } => format!("Energy {min}–{max}%"),
            Test::LikeSeed => match seed.and_then(Song::by) {
                Some(by) => format!("Like {by}"),
                None => "Like the seed".to_owned(),
            },
        }
    }
}

fn mmss(s: u32) -> String {
    format!("{}:{:02}", s / 60, s % 60)
}

/// One rule of a mix.
#[derive(Debug, Clone, PartialEq)]
pub struct Rule {
    pub weight: Weight,
    /// `Some(p)`: aim for about `p`% of songs to match, instead of `weight`.
    pub share: Option<u8>,
    pub test: Test,
}

/// A whole mix: its rules, what it is like, and how it spaces songs.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Spec {
    pub rules: Vec<Rule>,
    /// What `LikeSeed` compares against. The service resolves it: a chosen
    /// song, or (following) the song heard last.
    pub seed: Option<Song>,
    /// No artist again within this many songs.
    pub artist_gap: u8,
    /// No album again within this many songs.
    pub album_gap: u8,
}

/// Why a spec cannot run, naming the offending rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpecError {
    TooManyRules(usize),
    /// A share outside 1..=100, or on a rule that is not `Prefer`.
    BadShare {
        rule: usize,
        share: u8,
        weight: &'static str,
    },
    /// A range whose end is before its start.
    BackwardsRange {
        rule: usize,
        test: &'static str,
    },
    /// Text that is empty after trimming.
    EmptyText {
        rule: usize,
        test: &'static str,
    },
    /// A `LikeSeed` rule with no seed to be like.
    NoSeed {
        rule: usize,
    },
}

impl SpecError {
    pub fn label(&self) -> &'static str {
        match self {
            SpecError::TooManyRules(_) => "too_many_rules",
            SpecError::BadShare { .. } => "bad_share",
            SpecError::BackwardsRange { .. } => "backwards_range",
            SpecError::EmptyText { .. } => "empty_text",
            SpecError::NoSeed { .. } => "no_seed",
        }
    }
}

impl std::fmt::Display for SpecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpecError::TooManyRules(n) => write!(f, "{n} rules; a mix holds at most {MAX_RULES}"),
            SpecError::BadShare {
                rule,
                share,
                weight,
            } => write!(
                f,
                "rule {rule}: share {share}% on a {weight} rule; a share is 1-100% on a prefer rule"
            ),
            SpecError::BackwardsRange { rule, test } => {
                write!(f, "rule {rule}: the {test} range ends before it starts")
            }
            SpecError::EmptyText { rule, test } => write!(f, "rule {rule}: the {test} is empty"),
            SpecError::NoSeed { rule } => write!(f, "rule {rule}: like-seed with no seed song"),
        }
    }
}

impl Spec {
    pub fn validate(&self) -> Result<(), SpecError> {
        if self.rules.len() > MAX_RULES {
            return Err(SpecError::TooManyRules(self.rules.len()));
        }
        for (rule, r) in self.rules.iter().enumerate() {
            if let Some(share) = r.share {
                if share == 0 || share > 100 || r.weight != Weight::Prefer {
                    return Err(SpecError::BadShare {
                        rule,
                        share,
                        weight: r.weight.label(),
                    });
                }
            }
            let test = r.test.label();
            match &r.test {
                Test::Genre(t) | Test::Artist(t) | Test::Album(t) if t.trim().is_empty() => {
                    return Err(SpecError::EmptyText { rule, test });
                }
                Test::Years { from, to } if to < from => {
                    return Err(SpecError::BackwardsRange { rule, test });
                }
                Test::PlayCount { min, max } if max < min => {
                    return Err(SpecError::BackwardsRange { rule, test });
                }
                Test::Length { min_s, max_s } if max_s < min_s => {
                    return Err(SpecError::BackwardsRange { rule, test });
                }
                Test::Tempo { min, max } if max < min => {
                    return Err(SpecError::BackwardsRange { rule, test });
                }
                Test::Energy { min, max } if max < min || *max > 100 => {
                    return Err(SpecError::BackwardsRange { rule, test });
                }
                Test::LikeSeed if self.seed.is_none() => return Err(SpecError::NoSeed { rule }),
                _ => {}
            }
        }
        Ok(())
    }
}

/// A step the engine had to bend to fill the batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Relax {
    /// A share rule's turn had no song to give; skipped for that pick.
    Share,
    /// Artist or album spacing would have left nothing.
    Spacing,
    /// Only recently heard songs were left, so one repeats.
    Repeat,
}

impl Relax {
    pub fn label(self) -> &'static str {
        match self {
            Relax::Share => "share",
            Relax::Spacing => "spacing",
            Relax::Repeat => "repeat",
        }
    }
}

/// One song picked, with the rules that drew it.
#[derive(Debug, Clone, PartialEq)]
pub struct Pick {
    pub id: u64,
    pub reasons: Vec<String>,
}

/// What one call produced, and what it took.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Batch {
    pub picks: Vec<Pick>,
    /// Songs the user may hear.
    pub pool: usize,
    /// Of those, the songs the hard rules allow.
    pub eligible: usize,
    /// Fewer picks than asked: the hard rules left too few songs.
    pub exhausted: bool,
    /// Each relaxation used, once, in the order first used.
    pub relaxed: Vec<Relax>,
}

/// A source of uniform draws in `[0, 1)`.
pub trait Draw {
    fn next_f64(&mut self) -> f64;
}

/// xorshift64*: small, fast, and good enough to shuffle a playlist.
pub struct XorShift(u64);

impl XorShift {
    pub fn new(seed: u64) -> Self {
        Self(seed | 1)
    }
}

impl Draw for XorShift {
    fn next_f64(&mut self) -> f64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        let x = self.0.wrapping_mul(0x2545_F491_4F6C_DD1D);
        (x >> 11) as f64 / (1u64 << 53) as f64
    }
}

fn eq_ci(a: &str, b: &str) -> bool {
    a.trim().to_lowercase() == b.trim().to_lowercase()
}

fn opt_eq(a: Option<&str>, b: &str) -> bool {
    a.is_some_and(|a| eq_ci(a, b))
}

/// How close two tempos are, 0 to 1, counting half and double time as the
/// same beat: 1 at the same tempo, 0 a quarter apart or more.
pub fn tempo_closeness(a: f32, b: f32) -> f64 {
    let (a, b) = (f64::from(a), f64::from(b));
    if a <= 0.0 || b <= 0.0 {
        return 0.0;
    }
    [b, b * 2.0, b / 2.0]
        .iter()
        .map(|b| (1.0 - ((a - b).abs() / a.max(*b)) / 0.25).max(0.0))
        .fold(0.0, f64::max)
}

/// How alike `s` is to `seed`, from 0 to 1: the same artist is 1; else a
/// weighted mean of shared genres (Jaccard, 0.5), closeness in year (0.2),
/// in tempo (0.15) and in energy (0.15). Tempo and energy count only when
/// the seed has them (a song without scores halfway), so an unanalysed
/// library scores as genre 0.7 / era 0.3.
pub fn likeness(seed: &Song, s: &Song) -> f64 {
    if let (Some(a), Some(b)) = (seed.by(), s.by()) {
        if eq_ci(a, b) {
            return 1.0;
        }
    }
    let sg: HashSet<String> = seed
        .genres
        .iter()
        .map(|g| g.trim().to_lowercase())
        .collect();
    let tg: HashSet<String> = s.genres.iter().map(|g| g.trim().to_lowercase()).collect();
    let union = sg.union(&tg).count();
    let genre = if union == 0 {
        0.0
    } else {
        sg.intersection(&tg).count() as f64 / union as f64
    };
    let era = match (seed.year, s.year) {
        (Some(a), Some(b)) => (1.0 - f64::from(a.abs_diff(b)) / 20.0).max(0.0),
        _ => 0.0,
    };
    let mut sum = 0.5 * genre + 0.2 * era;
    let mut weight = 0.7;
    // A part counts when the seed has it; a song the scanner has not yet
    // analysed scores halfway on it, neither like nor unlike.
    if let Some(a) = seed.bpm {
        sum += 0.15 * s.bpm.map_or(0.5, |b| tempo_closeness(a, b));
        weight += 0.15;
    }
    if let Some(a) = seed.energy {
        sum += 0.15
            * s.energy
                .map_or(0.5, |b| (1.0 - f64::from((a - b).abs())).clamp(0.0, 1.0));
        weight += 0.15;
    }
    sum / weight
}

/// How strongly `test` holds for `s`, from 0 to 1. Every test but
/// `LikeSeed` is 0 or 1.
fn degree(test: &Test, s: &Song, seed: Option<&Song>, now: i64) -> f64 {
    let yes = |b: bool| if b { 1.0 } else { 0.0 };
    match test {
        Test::Genre(g) => yes(s.genres.iter().any(|x| eq_ci(x, g))),
        Test::Artist(a) => {
            yes(opt_eq(s.artist.as_deref(), a) || opt_eq(s.album_artist.as_deref(), a))
        }
        Test::Album(a) => yes(opt_eq(s.album.as_deref(), a)),
        Test::Years { from, to } => yes(s.year.is_some_and(|y| (*from..=*to).contains(&y))),
        Test::Favorite => yes(s.favorite),
        Test::NeverPlayed => yes(s.play_count == 0 && s.last_played_at == 0),
        Test::PlayCount { min, max } => yes((*min..=*max).contains(&s.play_count)),
        Test::NotPlayedFor { days } => {
            yes(s.last_played_at == 0 || now - s.last_played_at >= i64::from(*days) * DAY)
        }
        Test::PlayedWithin { days } => {
            yes(s.last_played_at != 0 && now - s.last_played_at < i64::from(*days) * DAY)
        }
        Test::AddedWithin { days } => {
            yes(s.added_at.is_some_and(|t| now - t < i64::from(*days) * DAY))
        }
        Test::Length { min_s, max_s } => {
            yes(s.duration_s.is_some_and(|d| (*min_s..=*max_s).contains(&d)))
        }
        Test::LikeSeed => seed.map_or(0.0, |seed| likeness(seed, s)),
        Test::Tempo { min, max } => yes(s
            .bpm
            .is_some_and(|b| (f32::from(*min)..=f32::from(*max)).contains(&b))),
        Test::Energy { min, max } => yes(s
            .energy
            .is_some_and(|e| (f32::from(*min) / 100.0..=f32::from(*max) / 100.0).contains(&e))),
    }
}

fn matches(test: &Test, s: &Song, seed: Option<&Song>, now: i64) -> bool {
    match test {
        Test::LikeSeed => degree(test, s, seed, now) >= LIKE_THRESHOLD,
        _ => degree(test, s, seed, now) >= 1.0,
    }
}

/// Pick up to `count` songs from `pool` for `spec`. `recent` is what was
/// heard or queued, oldest first; it feeds freshness, spacing and shares.
/// The spec must have passed [`Spec::validate`].
pub fn select(
    pool: &[Song],
    spec: &Spec,
    recent: &[u64],
    count: usize,
    now: i64,
    draw: &mut dyn Draw,
) -> Batch {
    let count = count.min(MAX_COUNT);
    let seed = spec.seed.as_ref();
    let by_id: HashMap<u64, &Song> = pool.iter().map(|s| (s.id, s)).collect();
    let eligible: Vec<&Song> = pool
        .iter()
        .filter(|s| {
            spec.rules.iter().all(|r| match (r.share, r.weight) {
                (None, Weight::Require) => matches(&r.test, s, seed, now),
                (None, Weight::Exclude) => !matches(&r.test, s, seed, now),
                _ => true,
            })
        })
        .collect();
    // Each song's base weight from the soft rules, computed once.
    let base: Vec<f64> = eligible
        .iter()
        .map(|s| {
            spec.rules
                .iter()
                .fold(1.0, |w, r| match (r.share, r.weight) {
                    (None, Weight::Prefer) => match r.test {
                        Test::LikeSeed => {
                            w * (1.0 + (PREFER_FACTOR - 1.0) * degree(&r.test, s, seed, now))
                        }
                        _ if matches(&r.test, s, seed, now) => w * PREFER_FACTOR,
                        _ => w,
                    },
                    (None, Weight::Avoid) if matches(&r.test, s, seed, now) => w / PREFER_FACTOR,
                    _ => w,
                })
        })
        .collect();

    let mut batch = Batch {
        pool: pool.len(),
        eligible: eligible.len(),
        ..Batch::default()
    };
    let relax = |batch: &mut Batch, r: Relax| {
        if !batch.relaxed.contains(&r) {
            batch.relaxed.push(r);
        }
    };
    // The history the window rules read: recent, then this call's picks.
    let mut history: Vec<&Song> = recent
        .iter()
        .filter_map(|id| by_id.get(id).copied())
        .collect();
    let mut taken: HashSet<u64> = HashSet::new();
    let heard: HashSet<u64> = recent.iter().copied().collect();
    let shares: Vec<&Rule> = spec.rules.iter().filter(|r| r.share.is_some()).collect();

    while batch.picks.len() < count {
        let not_taken: Vec<usize> = (0..eligible.len())
            .filter(|&i| !taken.contains(&eligible[i].id))
            .collect();
        if not_taken.is_empty() {
            batch.exhausted = true;
            break;
        }
        // Freshness: unheard first; else the older half of what was heard;
        // else anything but the song heard last.
        let mut cands: Vec<usize> = not_taken
            .iter()
            .copied()
            .filter(|&i| !heard.contains(&eligible[i].id))
            .collect();
        if cands.is_empty() {
            relax(&mut batch, Relax::Repeat);
            let newest: HashSet<u64> = recent
                .iter()
                .rev()
                .take(recent.len() / 2)
                .copied()
                .collect();
            cands = not_taken
                .iter()
                .copied()
                .filter(|&i| !newest.contains(&eligible[i].id))
                .collect();
            if cands.is_empty() {
                let last = history.last().map(|s| s.id);
                cands = not_taken
                    .iter()
                    .copied()
                    .filter(|&i| Some(eligible[i].id) != last)
                    .collect();
            }
            if cands.is_empty() {
                cands = not_taken;
            }
        }
        // Shares: decide whether this turn must match, toward the target.
        for rule in &shares {
            let target = f64::from(rule.share.unwrap_or(0)) / 100.0;
            let window: Vec<&&Song> = history.iter().rev().take(SHARE_WINDOW).collect();
            let observed = if window.is_empty() {
                target
            } else {
                window
                    .iter()
                    .filter(|s| matches(&rule.test, s, seed, now))
                    .count() as f64
                    / window.len() as f64
            };
            let want = (2.0 * target - observed).clamp(0.0, 1.0);
            let must = draw.next_f64() < want;
            let kept: Vec<usize> = cands
                .iter()
                .copied()
                .filter(|&i| matches(&rule.test, eligible[i], seed, now) == must)
                .collect();
            if kept.is_empty() {
                relax(&mut batch, Relax::Share);
            } else {
                cands = kept;
            }
        }
        // Spacing: no artist or album heard too lately.
        let lately = |gap: u8| history.iter().rev().take(usize::from(gap));
        let artists: HashSet<String> = lately(spec.artist_gap)
            .filter_map(|s| s.by().map(|a| a.trim().to_lowercase()))
            .collect();
        let albums: HashSet<String> = lately(spec.album_gap)
            .filter_map(|s| s.album.as_deref().map(|a| a.trim().to_lowercase()))
            .collect();
        let spaced: Vec<usize> = cands
            .iter()
            .copied()
            .filter(|&i| {
                let s = eligible[i];
                !s.by()
                    .is_some_and(|a| artists.contains(&a.trim().to_lowercase()))
                    && !s
                        .album
                        .as_deref()
                        .is_some_and(|a| albums.contains(&a.trim().to_lowercase()))
            })
            .collect();
        if spaced.is_empty() {
            relax(&mut batch, Relax::Spacing);
        } else {
            cands = spaced;
        }
        // The weighted draw.
        let total: f64 = cands.iter().map(|&i| base[i]).sum();
        let mut at = draw.next_f64() * total;
        let mut chosen = cands[cands.len() - 1];
        for &i in &cands {
            at -= base[i];
            if at < 0.0 {
                chosen = i;
                break;
            }
        }
        let song = eligible[chosen];
        taken.insert(song.id);
        history.push(song);
        batch.picks.push(Pick {
            id: song.id,
            reasons: reasons(spec, song, now),
        });
    }
    batch
}

/// The soft and share rules `s` matches, in the words a listener sees.
/// Hard rules are left out: every song in the mix matches them.
fn reasons(spec: &Spec, s: &Song, now: i64) -> Vec<String> {
    let seed = spec.seed.as_ref();
    spec.rules
        .iter()
        .filter(|r| r.share.is_some() || r.weight == Weight::Prefer)
        .filter(|r| matches(&r.test, s, seed, now))
        .map(|r| r.test.describe(seed))
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    const NOW: i64 = 1_800_000_000;

    fn song(id: u64, artist: &str, genre: &str, year: u16) -> Song {
        Song {
            id,
            artist: Some(artist.to_owned()),
            album: Some(format!("{artist} album {}", id % 3)),
            genres: vec![genre.to_owned()],
            year: Some(year),
            duration_s: Some(200),
            ..Song::default()
        }
    }

    /// 10 artists x 10 songs; artists 0-4 rock, 5-9 jazz; years 1970+id%40.
    fn library() -> Vec<Song> {
        (0..100)
            .map(|id| {
                let a = id / 10;
                let genre = if a < 5 { "Rock" } else { "Jazz" };
                song(id, &format!("Artist {a}"), genre, 1970 + (id % 40) as u16)
            })
            .collect()
    }

    fn rule(weight: Weight, test: Test) -> Rule {
        Rule {
            weight,
            share: None,
            test,
        }
    }

    fn by_id(lib: &[Song], id: u64) -> &Song {
        lib.iter().find(|s| s.id == id).expect("in library")
    }

    #[test]
    fn a_required_genre_is_never_broken() {
        let lib = library();
        let spec = Spec {
            rules: vec![rule(Weight::Require, Test::Genre("rock".into()))],
            ..Spec::default()
        };
        let b = select(&lib, &spec, &[], 30, NOW, &mut XorShift::new(7));
        assert_eq!(b.picks.len(), 30);
        assert_eq!(b.eligible, 50);
        assert!(b.picks.iter().all(|p| by_id(&lib, p.id).genres == ["Rock"]));
    }

    #[test]
    fn an_excluded_artist_never_plays() {
        let lib = library();
        let spec = Spec {
            rules: vec![rule(Weight::Exclude, Test::Artist("ARTIST 3".into()))],
            ..Spec::default()
        };
        let b = select(&lib, &spec, &[], 50, NOW, &mut XorShift::new(1));
        assert!(b.picks.iter().all(|p| !(30..40).contains(&p.id)));
    }

    #[test]
    fn a_mix_that_runs_dry_says_so_and_never_bends_a_hard_rule() {
        let lib = library();
        let spec = Spec {
            rules: vec![rule(Weight::Require, Test::Artist("Artist 2".into()))],
            ..Spec::default()
        };
        let b = select(&lib, &spec, &[], 15, NOW, &mut XorShift::new(3));
        assert_eq!(b.picks.len(), 10);
        assert!(b.exhausted);
        let ids: HashSet<u64> = b.picks.iter().map(|p| p.id).collect();
        assert_eq!(ids.len(), 10, "no song twice in one batch");
    }

    #[test]
    fn no_song_heard_lately_comes_back_while_a_fresh_one_exists() {
        let lib = library();
        let recent: Vec<u64> = (0..90).collect();
        let b = select(
            &lib,
            &Spec::default(),
            &recent,
            10,
            NOW,
            &mut XorShift::new(9),
        );
        assert!(b.picks.iter().all(|p| p.id >= 90));
        assert!(b.relaxed.is_empty());
    }

    #[test]
    fn when_only_heard_songs_are_left_the_oldest_repeat_and_it_is_reported() {
        let lib: Vec<Song> = library().into_iter().take(10).collect();
        let recent: Vec<u64> = (0..10).collect();
        let b = select(
            &lib,
            &Spec::default(),
            &recent,
            3,
            NOW,
            &mut XorShift::new(9),
        );
        assert_eq!(b.picks.len(), 3);
        assert!(
            b.picks.iter().all(|p| p.id < 5),
            "only the older half repeats"
        );
        assert_eq!(b.relaxed, vec![Relax::Repeat]);
    }

    #[test]
    fn artist_spacing_holds_when_it_can_and_bends_out_loud_when_it_cannot() {
        let lib = library();
        let spec = Spec {
            artist_gap: 3,
            ..Spec::default()
        };
        let b = select(&lib, &spec, &[], 40, NOW, &mut XorShift::new(11));
        let artists: Vec<&str> = b
            .picks
            .iter()
            .map(|p| by_id(&lib, p.id).by().unwrap_or(""))
            .collect();
        for w in artists.windows(4) {
            let set: HashSet<&&str> = w.iter().collect();
            assert_eq!(set.len(), 4, "an artist came back within 3: {w:?}");
        }
        assert!(b.relaxed.is_empty());

        let one_artist = Spec {
            artist_gap: 3,
            rules: vec![rule(Weight::Require, Test::Artist("Artist 1".into()))],
            ..Spec::default()
        };
        let b = select(&lib, &one_artist, &[], 4, NOW, &mut XorShift::new(11));
        assert_eq!(b.picks.len(), 4);
        assert_eq!(b.relaxed, vec![Relax::Spacing]);
    }

    #[test]
    fn a_share_converges_on_its_target() {
        let lib = library();
        let spec = Spec {
            rules: vec![Rule {
                weight: Weight::Prefer,
                share: Some(30),
                test: Test::Genre("Jazz".into()),
            }],
            ..Spec::default()
        };
        // 200 songs over many calls, feeding each batch back as history.
        let mut recent: Vec<u64> = Vec::new();
        let mut draw = XorShift::new(5);
        let mut jazz = 0;
        let mut total = 0;
        for _ in 0..40 {
            let b = select(&lib, &spec, &recent, 5, NOW, &mut draw);
            for p in &b.picks {
                total += 1;
                if by_id(&lib, p.id).genres == ["Jazz"] {
                    jazz += 1;
                }
                recent.push(p.id);
            }
            if recent.len() > 60 {
                recent.drain(..recent.len() - 60);
            }
        }
        let share = f64::from(jazz) / f64::from(total);
        assert!((0.22..=0.38).contains(&share), "jazz share {share}");
    }

    #[test]
    fn prefer_tilts_the_draw_and_avoid_tilts_it_away() {
        let lib = library();
        let count = |spec: &Spec| {
            let mut draw = XorShift::new(21);
            let mut rock = 0;
            for _ in 0..50 {
                let b = select(&lib, spec, &[], 1, NOW, &mut draw);
                if b.picks.iter().any(|p| by_id(&lib, p.id).genres == ["Rock"]) {
                    rock += 1;
                }
            }
            rock
        };
        let prefer = count(&Spec {
            rules: vec![rule(Weight::Prefer, Test::Genre("Rock".into()))],
            ..Spec::default()
        });
        let avoid = count(&Spec {
            rules: vec![rule(Weight::Avoid, Test::Genre("Rock".into()))],
            ..Spec::default()
        });
        assert!(prefer > 30, "prefer rock picked rock {prefer}/50");
        assert!(avoid < 20, "avoid rock picked rock {avoid}/50");
    }

    #[test]
    fn like_seed_scores_the_same_artist_highest_then_genre_and_era() {
        let seed = song(1, "A", "Trip-hop", 1994);
        assert_eq!(likeness(&seed, &song(2, "a", "Rock", 1960)), 1.0);
        let near = likeness(&seed, &song(3, "B", "Trip-hop", 1996));
        let far = likeness(&seed, &song(4, "C", "Trip-hop", 2024));
        let other = likeness(&seed, &song(5, "D", "Polka", 1994));
        assert!(near > far && far > other, "{near} {far} {other}");
        assert!(near >= LIKE_THRESHOLD && other < LIKE_THRESHOLD);
    }

    #[test]
    fn tempo_and_energy_rules_read_the_analysis_and_skip_songs_without_it() {
        let mut s = song(1, "A", "House", 2000);
        let tempo = Test::Tempo { min: 120, max: 130 };
        let calm = Test::Energy { min: 0, max: 30 };
        assert!(!matches(&tempo, &s, None, NOW), "no tempo, no match");
        assert!(!matches(&calm, &s, None, NOW), "not analysed, no match");
        s.bpm = Some(124.0);
        s.energy = Some(0.2);
        assert!(matches(&tempo, &s, None, NOW));
        assert!(matches(&calm, &s, None, NOW));
        s.energy = Some(0.31);
        assert!(!matches(&calm, &s, None, NOW));
        assert_eq!(tempo.describe(None), "120–130 BPM");
    }

    #[test]
    fn like_seed_hears_tempo_and_energy_when_the_seed_has_them() {
        let mut seed = song(1, "A", "House", 2000);
        seed.bpm = Some(124.0);
        seed.energy = Some(0.8);
        let mut near = song(2, "B", "House", 2000);
        near.bpm = Some(62.0); // half time: the same beat
        near.energy = Some(0.75);
        let mut far = song(3, "C", "House", 2000);
        far.bpm = Some(90.0);
        far.energy = Some(0.1);
        let unknown = song(4, "D", "House", 2000);
        assert!(likeness(&seed, &near) > likeness(&seed, &unknown));
        assert!(likeness(&seed, &unknown) > likeness(&seed, &far));
        assert!((tempo_closeness(124.0, 62.0) - 1.0).abs() < 1e-9);
        assert_eq!(tempo_closeness(124.0, 0.0), 0.0);
        // No analysis on either side scores as before: genre 0.7, era 0.3.
        let a = song(5, "E", "House", 2000);
        assert!((likeness(&a, &unknown) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn play_history_rules_read_the_clock() {
        let mut s = song(1, "A", "Rock", 2000);
        assert!(matches(&Test::NeverPlayed, &s, None, NOW));
        assert!(matches(&Test::NotPlayedFor { days: 30 }, &s, None, NOW));
        s.play_count = 4;
        s.last_played_at = NOW - 10 * DAY;
        assert!(!matches(&Test::NeverPlayed, &s, None, NOW));
        assert!(!matches(&Test::NotPlayedFor { days: 30 }, &s, None, NOW));
        assert!(matches(&Test::PlayedWithin { days: 30 }, &s, None, NOW));
        assert!(matches(
            &Test::PlayCount {
                min: 3,
                max: u32::MAX
            },
            &s,
            None,
            NOW
        ));
        s.added_at = Some(NOW - 2 * DAY);
        assert!(matches(&Test::AddedWithin { days: 7 }, &s, None, NOW));
        assert!(!matches(&Test::AddedWithin { days: 1 }, &s, None, NOW));
    }

    #[test]
    fn reasons_name_the_soft_rules_a_song_matched() {
        let lib = library();
        let spec = Spec {
            rules: vec![
                rule(Weight::Require, Test::Genre("Rock".into())),
                rule(
                    Weight::Prefer,
                    Test::Years {
                        from: 1970,
                        to: 1979,
                    },
                ),
            ],
            ..Spec::default()
        };
        let s = by_id(&lib, 5); // Rock, 1975
        assert_eq!(reasons(&spec, s, NOW), vec!["1970–1979".to_owned()]);
    }

    #[test]
    fn a_bad_spec_names_the_rule_and_the_value() {
        let bad_share = Spec {
            rules: vec![Rule {
                weight: Weight::Require,
                share: Some(30),
                test: Test::Favorite,
            }],
            ..Spec::default()
        };
        let e = bad_share.validate().expect_err("share on require");
        assert_eq!(e.label(), "bad_share");
        assert!(e.to_string().contains("30%") && e.to_string().contains("require"));
        let backwards = Spec {
            rules: vec![rule(
                Weight::Prefer,
                Test::Years {
                    from: 2000,
                    to: 1990,
                },
            )],
            ..Spec::default()
        };
        assert_eq!(
            backwards.validate().map_err(|e| e.label()),
            Err("backwards_range")
        );
        let no_seed = Spec {
            rules: vec![rule(Weight::Prefer, Test::LikeSeed)],
            ..Spec::default()
        };
        assert_eq!(no_seed.validate().map_err(|e| e.label()), Err("no_seed"));
    }

    #[test]
    fn labels_are_distinct() {
        let tests = [
            Test::Genre(String::new()),
            Test::Artist(String::new()),
            Test::Album(String::new()),
            Test::Years { from: 0, to: 0 },
            Test::Favorite,
            Test::NeverPlayed,
            Test::PlayCount { min: 0, max: 0 },
            Test::NotPlayedFor { days: 0 },
            Test::PlayedWithin { days: 0 },
            Test::AddedWithin { days: 0 },
            Test::Length { min_s: 0, max_s: 0 },
            Test::LikeSeed,
            Test::Tempo { min: 0, max: 0 },
            Test::Energy { min: 0, max: 0 },
        ];
        let labels: HashSet<&str> = tests.iter().map(Test::label).collect();
        assert_eq!(labels.len(), tests.len());
        let relax: HashSet<&str> = [Relax::Share, Relax::Spacing, Relax::Repeat]
            .iter()
            .map(|r| r.label())
            .collect();
        assert_eq!(relax.len(), 3);
    }

    #[test]
    fn the_same_draws_give_the_same_mix() {
        let lib = library();
        let a = select(&lib, &Spec::default(), &[], 20, NOW, &mut XorShift::new(42));
        let b = select(&lib, &Spec::default(), &[], 20, NOW, &mut XorShift::new(42));
        assert_eq!(a, b);
    }
}
