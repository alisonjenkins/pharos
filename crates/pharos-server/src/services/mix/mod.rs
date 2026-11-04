//! Dynamic mixes (spec 024): the next songs for a set of rules, for the
//! native `Library.mix` call. The engine is [`select`], pure; this module
//! scopes the pool to what the user may hear, resolves the seed, runs the
//! engine, and records what it decided (ODD: every call leaves one log line
//! and one `pharos_mix_total` sample, failures included).

pub mod select;

use std::collections::HashMap;
use std::time::Instant;

use pharos_core::{
    split_genre_field, MediaItem, MediaKind, User, UserDataStore as _, UserItemData,
};

use crate::services::library::{self, ItemQuery, KindFilter};
use crate::state::AppState;
pub use select::{Batch, Relax, Rule, Spec, SpecError, Test, Weight};

/// The most `recent` ids a call reads; older ones are dropped.
pub const MAX_RECENT: usize = 200;

/// What the `LikeSeed` rules compare against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Seed {
    /// Nothing: a mix with a `LikeSeed` rule is refused.
    None,
    /// This song.
    Song(u64),
    /// The song heard last, so the mix drifts as it plays. Before anything
    /// has played the like-seed rules sit out.
    Follow,
}

impl Seed {
    pub fn label(self) -> &'static str {
        match self {
            Seed::None => "none",
            Seed::Song(_) => "song",
            Seed::Follow => "follow",
        }
    }
}

/// One call's ask.
#[derive(Debug, Clone)]
pub struct MixAsk {
    pub rules: Vec<Rule>,
    pub seed: Seed,
    pub artist_gap: u8,
    pub album_gap: u8,
    /// Heard or queued, oldest first.
    pub recent: Vec<u64>,
    pub count: u32,
}

/// One picked song, ready to encode.
#[derive(Debug)]
pub struct Picked {
    pub item: MediaItem,
    pub user_data: UserItemData,
    pub reasons: Vec<String>,
}

#[derive(Debug)]
pub struct Mixed {
    pub picks: Vec<Picked>,
    pub pool: usize,
    pub eligible: usize,
    pub exhausted: bool,
    pub relaxed: Vec<Relax>,
}

#[derive(Debug)]
pub enum MixError {
    Invalid(SpecError),
    /// The seed song is not one the user may hear (or does not exist).
    SeedNotFound(u64),
    Store(String),
}

impl MixError {
    pub fn label(&self) -> &'static str {
        match self {
            MixError::Invalid(_) => "invalid",
            MixError::SeedNotFound(_) => "seed_not_found",
            MixError::Store(_) => "unavailable",
        }
    }
}

impl std::fmt::Display for MixError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MixError::Invalid(e) => write!(f, "{e}"),
            MixError::SeedNotFound(id) => write!(f, "seed song {id} is not in the user's music"),
            MixError::Store(e) => write!(f, "media store: {e}"),
        }
    }
}

/// How a call ended, as the `outcome` label of `pharos_mix_total`.
pub fn outcome_label(result: &Result<Mixed, MixError>) -> &'static str {
    match result {
        Ok(m) if m.picks.is_empty() => "empty",
        Ok(m) if m.exhausted => "short",
        Ok(_) => "full",
        Err(e) => e.label(),
    }
}

/// The song the engine sees for a stored item.
fn song(item: &MediaItem, ud: UserItemData) -> select::Song {
    let text = |s: &Option<String>| {
        s.as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    select::Song {
        id: item.id,
        artist: text(&item.probe.artist),
        album_artist: text(&item.probe.album_artist),
        album: text(&item.probe.album),
        genres: item
            .probe
            .genre
            .as_deref()
            .map(split_genre_field)
            .unwrap_or_default(),
        year: item
            .probe
            .year
            .or(item.metadata.production_year)
            .and_then(|y| u16::try_from(y).ok()),
        duration_s: item
            .probe
            .duration_ms
            .map(|ms| u32::try_from(ms / 1000).unwrap_or(u32::MAX)),
        added_at: item.created_at,
        play_count: ud.play_count,
        last_played_at: ud.last_played_at,
        favorite: ud.is_favorite,
        // Filled from `probe.bpm` / `probe.energy` once the scanner reads
        // tempo tags and analyses energy (B445); until then the tempo and
        // energy rules match nothing and likeness ignores them.
        bpm: None,
        energy: None,
    }
}

/// The next songs for `ask`, from the music `user` may hear.
pub async fn mix(state: &AppState, user: &User, ask: MixAsk, now: i64) -> Result<Mixed, MixError> {
    let started = Instant::now();
    let tests: Vec<&'static str> = ask.rules.iter().map(|r| r.test.label()).collect();
    let seed_label = ask.seed.label();
    let asked = ask.count;
    let result = run(state, user, ask, now).await;
    let outcome = outcome_label(&result);
    let elapsed = started.elapsed();
    metrics::counter!("pharos_mix_total", "outcome" => outcome).increment(1);
    metrics::histogram!("pharos_mix_seconds").record(elapsed.as_secs_f64());
    match &result {
        Ok(m) => {
            for r in &m.relaxed {
                metrics::counter!("pharos_mix_relaxed_total", "step" => r.label()).increment(1);
            }
            let relaxed: Vec<&str> = m.relaxed.iter().map(|r| r.label()).collect();
            tracing::info!(
                user.id = %user.id,
                outcome,
                rules = ?tests,
                seed = seed_label,
                asked,
                picked = m.picks.len(),
                pool = m.pool,
                eligible = m.eligible,
                exhausted = m.exhausted,
                relaxed = ?relaxed,
                elapsed_ms = elapsed.as_millis() as u64,
                "mix: batch picked"
            );
        }
        Err(e) => tracing::warn!(
            user.id = %user.id,
            outcome,
            rules = ?tests,
            seed = seed_label,
            asked,
            error = %e,
            elapsed_ms = elapsed.as_millis() as u64,
            "mix: no batch"
        ),
    }
    result
}

async fn run(state: &AppState, user: &User, mut ask: MixAsk, now: i64) -> Result<Mixed, MixError> {
    if ask.recent.len() > MAX_RECENT {
        ask.recent.drain(..ask.recent.len() - MAX_RECENT);
    }
    // Validate before the store is touched: a bad spec costs nothing.
    let mut spec = Spec {
        rules: ask.rules,
        seed: None,
        artist_gap: ask.artist_gap,
        album_gap: ask.album_gap,
    };
    // A song or follow seed is resolved from the pool below, so validate as
    // if it were there; with no seed, a like-seed rule is refused.
    let checked = match ask.seed {
        Seed::None => spec.clone(),
        Seed::Song(_) | Seed::Follow => Spec {
            seed: Some(select::Song::default()),
            ..spec.clone()
        },
    };
    checked.validate().map_err(MixError::Invalid)?;

    let q = ItemQuery {
        kinds: KindFilter::Kinds(vec![MediaKind::Audio]),
        limit: None,
        ..ItemQuery::default()
    };
    let page = library::query(state, user, None, &q)
        .await
        .map_err(|e| MixError::Store(e.to_string()))?;
    let ids: Vec<u64> = page.items.iter().map(|i| i.id).collect();
    let user_data = state
        .stores
        .user_data_bulk(user.id, &ids)
        .await
        .map_err(|e| MixError::Store(e.to_string()))?;
    let pool: Vec<select::Song> = page
        .items
        .iter()
        .enumerate()
        .map(|(i, item)| song(item, user_data.get(i).copied().unwrap_or_default()))
        .collect();

    spec.seed = match ask.seed {
        Seed::None => None,
        Seed::Song(id) => Some(
            pool.iter()
                .find(|s| s.id == id)
                .cloned()
                .ok_or(MixError::SeedNotFound(id))?,
        ),
        Seed::Follow => ask
            .recent
            .iter()
            .rev()
            .find_map(|id| pool.iter().find(|s| s.id == *id))
            .cloned(),
    };
    if spec.seed.is_none() {
        // Following with nothing heard yet: nothing to be like.
        spec.rules.retain(|r| r.test != Test::LikeSeed);
    }

    let mut draw = select::XorShift::new(fresh_seed());
    let count = usize::try_from(ask.count).unwrap_or(select::MAX_COUNT);
    let batch = select::select(&pool, &spec, &ask.recent, count, now, &mut draw);

    let mut by_id: HashMap<u64, (MediaItem, UserItemData)> = page
        .items
        .into_iter()
        .zip(
            user_data
                .into_iter()
                .chain(std::iter::repeat(UserItemData::default())),
        )
        .map(|(item, ud)| (item.id, (item, ud)))
        .collect();
    let picks = batch
        .picks
        .into_iter()
        .filter_map(|p| {
            by_id.remove(&p.id).map(|(item, user_data)| Picked {
                item,
                user_data,
                reasons: p.reasons,
            })
        })
        .collect();
    Ok(Mixed {
        picks,
        pool: batch.pool,
        eligible: batch.eligible,
        exhausted: batch.exhausted,
        relaxed: batch.relaxed,
    })
}

/// A fresh random seed per call; a clock-derived one if the OS has none.
fn fresh_seed() -> u64 {
    let mut buf = [0u8; 8];
    match getrandom::fill(&mut buf) {
        Ok(()) => u64::from_le_bytes(buf),
        Err(_) => std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0x9E37_79B9_7F4A_7C15, |d| d.as_nanos() as u64),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn outcome_labels_are_distinct() {
        let ok = |picks: usize, exhausted: bool| -> Result<Mixed, MixError> {
            Ok(Mixed {
                picks: (0..picks)
                    .map(|_| Picked {
                        item: MediaItem::default(),
                        user_data: UserItemData::default(),
                        reasons: vec![],
                    })
                    .collect(),
                pool: 0,
                eligible: 0,
                exhausted,
                relaxed: vec![],
            })
        };
        let labels = [
            outcome_label(&ok(0, true)),
            outcome_label(&ok(1, true)),
            outcome_label(&ok(1, false)),
            outcome_label(&Err(MixError::Invalid(SpecError::TooManyRules(99)))),
            outcome_label(&Err(MixError::SeedNotFound(1))),
            outcome_label(&Err(MixError::Store(String::new()))),
        ];
        let set: std::collections::HashSet<_> = labels.iter().collect();
        assert_eq!(set.len(), labels.len(), "{labels:?}");
    }

    #[test]
    fn a_stored_song_carries_its_tags_and_play_state_into_the_engine() {
        let mut item = MediaItem {
            id: 9,
            ..MediaItem::default()
        };
        item.probe.artist = Some(" Portishead ".into());
        item.probe.album = Some("Dummy".into());
        item.probe.genre = Some("Trip-hop, Electronic".into());
        item.probe.year = Some(1994);
        item.probe.duration_ms = Some(241_500);
        item.created_at = Some(5);
        let ud = UserItemData {
            play_count: 3,
            last_played_at: 77,
            is_favorite: true,
            ..UserItemData::default()
        };
        let s = song(&item, ud);
        assert_eq!(s.artist.as_deref(), Some("Portishead"));
        assert_eq!(s.genres, vec!["Trip-hop", "Electronic"]);
        assert_eq!(s.year, Some(1994));
        assert_eq!(s.duration_s, Some(241));
        assert_eq!((s.play_count, s.last_played_at, s.favorite), (3, 77, true));
        assert_eq!(s.added_at, Some(5));
    }
}
