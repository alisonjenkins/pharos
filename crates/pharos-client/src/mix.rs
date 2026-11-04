//! Dynamic mixes (spec 024): the rules a client builds, the presets every
//! first-party client offers, and their wire form for `Library.mix` and for
//! the `mix.saved` pref. Plain Rust in and out; nothing above sees capnp.

use pharos_api::pharos_capnp::{
    mix_batch, mix_rule, mix_spec, saved_mixes, MixWeight as WireWeight,
};

use crate::api::{read_item, read_item_ref, set_item_ref, Item, ItemRef};

/// The pref key a user's saved mixes live under.
pub const SAVED_KEY: &str = "mix.saved";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Weight {
    /// Only songs that match.
    Require,
    /// Matching songs are likelier.
    Prefer,
    /// Matching songs are less likely.
    Avoid,
    /// Never songs that match.
    Exclude,
}

impl Weight {
    pub const ALL: [Weight; 4] = [
        Weight::Require,
        Weight::Prefer,
        Weight::Avoid,
        Weight::Exclude,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Weight::Require => "Only",
            Weight::Prefer => "More",
            Weight::Avoid => "Less",
            Weight::Exclude => "Never",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Test {
    Genre(String),
    Artist(String),
    Album(String),
    Years { from: u16, to: u16 },
    Favorite,
    NeverPlayed,
    PlayCount { min: u32, max: u32 },
    NotPlayedForDays(u32),
    PlayedWithinDays(u32),
    AddedWithinDays(u32),
    LengthSecs { min: u32, max: u32 },
    LikeSeed,
}

impl Test {
    /// What the rule editor calls each kind of test, in its menu order.
    pub fn kind_label(&self) -> &'static str {
        match self {
            Test::Genre(_) => "Genre",
            Test::Artist(_) => "Artist",
            Test::Album(_) => "Album",
            Test::Years { .. } => "Years",
            Test::Favorite => "Favourites",
            Test::NeverPlayed => "Never played",
            Test::PlayCount { .. } => "Play count",
            Test::NotPlayedForDays(_) => "Not played for",
            Test::PlayedWithinDays(_) => "Played in the last",
            Test::AddedWithinDays(_) => "Added in the last",
            Test::LengthSecs { .. } => "Length",
            Test::LikeSeed => "Like the seed",
        }
    }

    /// One fresh test of each kind, as the editor's "add rule" menu offers.
    pub fn kinds() -> Vec<Test> {
        vec![
            Test::Genre(String::new()),
            Test::Artist(String::new()),
            Test::Album(String::new()),
            Test::Years {
                from: 1990,
                to: 1999,
            },
            Test::Favorite,
            Test::NeverPlayed,
            Test::PlayCount { min: 0, max: 2 },
            Test::NotPlayedForDays(30),
            Test::PlayedWithinDays(30),
            Test::AddedWithinDays(30),
            Test::LengthSecs { min: 120, max: 360 },
            Test::LikeSeed,
        ]
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub weight: Weight,
    /// 1-100: aim for about this % of songs to match (a `Prefer` rule
    /// only). 0: no share.
    pub share: u8,
    pub test: Test,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Seed {
    #[default]
    None,
    Song(ItemRef),
    /// Like whatever played last, drifting as the mix goes.
    Follow,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MixSpec {
    pub rules: Vec<Rule>,
    pub seed: Seed,
    pub artist_gap: u8,
    pub album_gap: u8,
}

/// A mix with the name the user gave it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedMix {
    pub name: String,
    pub spec: MixSpec,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MixPick {
    pub item: Item,
    /// The soft rules it matched, in words.
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct MixBatch {
    pub picks: Vec<MixPick>,
    pub pool: u32,
    pub eligible: u32,
    /// Fewer songs than asked: the mix has run out.
    pub exhausted: bool,
    /// Steps the server bent to fill the batch: `repeat`, `share`, `spacing`.
    pub relaxed: Vec<String>,
}

const SPACED: (u8, u8) = (3, 5);

fn rule(weight: Weight, test: Test) -> Rule {
    Rule {
        weight,
        share: 0,
        test,
    }
}

/// A mix like `song`: the same artist and neighbours in genre and era.
pub fn radio(song: ItemRef) -> MixSpec {
    MixSpec {
        rules: vec![
            rule(Weight::Require, Test::LikeSeed),
            rule(Weight::Prefer, Test::LikeSeed),
        ],
        seed: Seed::Song(song),
        artist_gap: SPACED.0,
        album_gap: SPACED.1,
    }
}

/// The built-in mixes, in the order the picker lists them.
pub fn presets() -> Vec<SavedMix> {
    let spaced = |rules: Vec<Rule>, seed: Seed| MixSpec {
        rules,
        seed,
        artist_gap: SPACED.0,
        album_gap: SPACED.1,
    };
    vec![
        SavedMix {
            name: "Drift".into(),
            spec: spaced(vec![rule(Weight::Prefer, Test::LikeSeed)], Seed::Follow),
        },
        SavedMix {
            name: "Favourites".into(),
            spec: spaced(vec![rule(Weight::Require, Test::Favorite)], Seed::None),
        },
        SavedMix {
            name: "Rediscover".into(),
            spec: spaced(
                vec![
                    rule(
                        Weight::Require,
                        Test::PlayCount {
                            min: 1,
                            max: u32::MAX,
                        },
                    ),
                    rule(Weight::Require, Test::NotPlayedForDays(90)),
                    rule(Weight::Prefer, Test::Favorite),
                ],
                Seed::None,
            ),
        },
        SavedMix {
            name: "Deep cuts".into(),
            spec: spaced(
                vec![
                    rule(Weight::Prefer, Test::NeverPlayed),
                    rule(
                        Weight::Avoid,
                        Test::PlayCount {
                            min: 5,
                            max: u32::MAX,
                        },
                    ),
                ],
                Seed::None,
            ),
        },
        SavedMix {
            name: "New arrivals".into(),
            spec: spaced(
                vec![Rule {
                    weight: Weight::Prefer,
                    share: 60,
                    test: Test::AddedWithinDays(30),
                }],
                Seed::None,
            ),
        },
    ]
}

fn set_weight(w: Weight) -> WireWeight {
    match w {
        Weight::Require => WireWeight::Require,
        Weight::Prefer => WireWeight::Prefer,
        Weight::Avoid => WireWeight::Avoid,
        Weight::Exclude => WireWeight::Exclude,
    }
}

fn write_rule(mut out: mix_rule::Builder<'_>, r: &Rule) {
    out.set_weight(set_weight(r.weight));
    out.set_share(r.share);
    let mut t = out.init_test();
    match &r.test {
        Test::Genre(s) => t.set_genre(s.as_str()),
        Test::Artist(s) => t.set_artist(s.as_str()),
        Test::Album(s) => t.set_album(s.as_str()),
        Test::Years { from, to } => {
            let mut m = t.init_years();
            m.set_from(u32::from(*from));
            m.set_to(u32::from(*to));
        }
        Test::Favorite => t.set_favorite(()),
        Test::NeverPlayed => t.set_never_played(()),
        Test::PlayCount { min, max } => {
            let mut m = t.init_play_count();
            m.set_from(*min);
            m.set_to(*max);
        }
        Test::NotPlayedForDays(d) => t.set_not_played_for_days(*d),
        Test::PlayedWithinDays(d) => t.set_played_within_days(*d),
        Test::AddedWithinDays(d) => t.set_added_within_days(*d),
        Test::LengthSecs { min, max } => {
            let mut m = t.init_length_secs();
            m.set_from(*min);
            m.set_to(*max);
        }
        Test::LikeSeed => t.set_like_seed(()),
    }
}

pub(crate) fn write_spec(mut out: mix_spec::Builder<'_>, spec: &MixSpec) {
    out.set_artist_gap(spec.artist_gap);
    out.set_album_gap(spec.album_gap);
    match spec.seed {
        Seed::None => out.reborrow().init_seed().set_none(()),
        Seed::Follow => out.reborrow().init_seed().set_follow(()),
        Seed::Song(id) => set_item_ref(out.reborrow().init_seed().init_song(), id),
    }
    let mut rules = out.init_rules(u32::try_from(spec.rules.len()).unwrap_or(u32::MAX));
    for (i, r) in (0..rules.len()).zip(&spec.rules) {
        write_rule(rules.reborrow().get(i), r);
    }
}

fn text(t: capnp::Result<capnp::text::Reader<'_>>) -> Result<String, capnp::Error> {
    Ok(t?.to_string()?)
}

fn read_rule(r: mix_rule::Reader<'_>) -> Result<Option<Rule>, capnp::Error> {
    use mix_rule::test::Which as T;
    let weight = match r.get_weight() {
        Ok(WireWeight::Require) => Weight::Require,
        Ok(WireWeight::Prefer) => Weight::Prefer,
        Ok(WireWeight::Avoid) => Weight::Avoid,
        Ok(WireWeight::Exclude) => Weight::Exclude,
        Err(_) => return Ok(None),
    };
    let u16_of = |v: u32| u16::try_from(v).unwrap_or(u16::MAX);
    let test = match r.get_test().which() {
        Ok(T::Genre(t)) => Test::Genre(text(t)?),
        Ok(T::Artist(t)) => Test::Artist(text(t)?),
        Ok(T::Album(t)) => Test::Album(text(t)?),
        Ok(T::Years(m)) => {
            let m = m?;
            Test::Years {
                from: u16_of(m.get_from()),
                to: u16_of(m.get_to()),
            }
        }
        Ok(T::Favorite(())) => Test::Favorite,
        Ok(T::NeverPlayed(())) => Test::NeverPlayed,
        Ok(T::PlayCount(m)) => {
            let m = m?;
            Test::PlayCount {
                min: m.get_from(),
                max: m.get_to(),
            }
        }
        Ok(T::NotPlayedForDays(d)) => Test::NotPlayedForDays(d),
        Ok(T::PlayedWithinDays(d)) => Test::PlayedWithinDays(d),
        Ok(T::AddedWithinDays(d)) => Test::AddedWithinDays(d),
        Ok(T::LengthSecs(m)) => {
            let m = m?;
            Test::LengthSecs {
                min: m.get_from(),
                max: m.get_to(),
            }
        }
        Ok(T::LikeSeed(())) => Test::LikeSeed,
        // A test from a newer schema: this client cannot show or keep it.
        Err(_) => return Ok(None),
    };
    Ok(Some(Rule {
        weight,
        share: r.get_share(),
        test,
    }))
}

fn read_spec(s: mix_spec::Reader<'_>) -> Result<MixSpec, capnp::Error> {
    let seed = match s.get_seed().which() {
        Ok(mix_spec::seed::Song(id)) => Seed::Song(read_item_ref(id?)?),
        Ok(mix_spec::seed::Follow(())) => Seed::Follow,
        Ok(mix_spec::seed::None(())) | Err(_) => Seed::None,
    };
    let mut rules = Vec::new();
    for r in s.get_rules()?.iter() {
        if let Some(r) = read_rule(r)? {
            rules.push(r);
        }
    }
    Ok(MixSpec {
        rules,
        seed,
        artist_gap: s.get_artist_gap(),
        album_gap: s.get_album_gap(),
    })
}

fn texts(list: capnp::Result<capnp::text_list::Reader<'_>>) -> Result<Vec<String>, capnp::Error> {
    list?.iter().map(text).collect()
}

pub(crate) fn read_batch(b: mix_batch::Reader<'_>) -> Result<MixBatch, capnp::Error> {
    let picks = b
        .get_picks()?
        .iter()
        .map(|p| {
            Ok(MixPick {
                item: read_item(p.get_item()?)?,
                reasons: texts(p.get_reasons())?,
            })
        })
        .collect::<Result<_, capnp::Error>>()?;
    Ok(MixBatch {
        picks,
        pool: b.get_pool(),
        eligible: b.get_eligible(),
        exhausted: b.get_exhausted(),
        relaxed: texts(b.get_relaxed())?,
    })
}

/// `mixes` as the bytes stored under [`SAVED_KEY`].
pub fn encode_saved(mixes: &[SavedMix]) -> Vec<u8> {
    let mut message = capnp::message::Builder::new_default();
    {
        let root = message.init_root::<saved_mixes::Builder<'_>>();
        let mut list = root.init_mixes(u32::try_from(mixes.len()).unwrap_or(u32::MAX));
        for (i, m) in (0..list.len()).zip(mixes) {
            let mut out = list.reborrow().get(i);
            out.set_name(m.name.as_str());
            write_spec(out.init_spec(), &m.spec);
        }
    }
    capnp::serialize::write_message_to_words(&message)
}

/// The saved mixes in `bytes`; rules this client does not know are dropped.
pub fn decode_saved(bytes: &[u8]) -> Result<Vec<SavedMix>, capnp::Error> {
    let message = capnp::serialize::read_message_from_flat_slice(
        &mut &bytes[..],
        capnp::message::ReaderOptions::new(),
    )?;
    let root = message.get_root::<saved_mixes::Reader<'_>>()?;
    root.get_mixes()?
        .iter()
        .map(|m| {
            Ok(SavedMix {
                name: text(m.get_name())?,
                spec: read_spec(m.get_spec()?)?,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn saved_mixes_round_trip_every_test_and_seed() {
        let rules: Vec<Rule> = Test::kinds()
            .into_iter()
            .zip(Weight::ALL.iter().cycle())
            .map(|(test, &weight)| Rule {
                weight,
                share: 0,
                test,
            })
            .collect();
        let mixes = vec![
            SavedMix {
                name: "All".into(),
                spec: MixSpec {
                    rules,
                    seed: Seed::Song(ItemRef::Real(42)),
                    artist_gap: 2,
                    album_gap: 4,
                },
            },
            SavedMix {
                name: "Follow".into(),
                spec: MixSpec {
                    seed: Seed::Follow,
                    ..MixSpec::default()
                },
            },
        ];
        let mut all = mixes.clone();
        all.extend(presets());
        assert_eq!(decode_saved(&encode_saved(&all)).unwrap(), all);
    }

    #[test]
    fn presets_have_distinct_names_and_space_their_songs() {
        let p = presets();
        let names: std::collections::HashSet<&str> = p.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names.len(), p.len());
        assert!(p.iter().all(|m| m.spec.artist_gap > 0));
    }
}
