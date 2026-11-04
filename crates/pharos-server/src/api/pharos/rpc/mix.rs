//! `Library.mix` on the wire: reads a `MixSpec` into the engine's rules and
//! writes a `MixBatch` back (spec 024). The decisions live in
//! `services::mix`; this file only translates.

use pharos_api::pharos_capnp::{mix_batch, mix_rule, mix_spec, MixWeight};
use pharos_jellyfin_api::dto::WireId;

use super::library::{read_item_id, write_item};
use crate::services::mix::{self, Mixed, Rule, Seed, Test, Weight};

/// Why a `MixSpec` cannot be read; the field it names is a fixed string.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct BadField(pub &'static str);

fn text(t: capnp::Result<capnp::text::Reader<'_>>) -> Result<String, capnp::Error> {
    Ok(t?.to_str()?.trim().to_owned())
}

fn read_rule(r: mix_rule::Reader<'_>) -> Result<Result<Rule, BadField>, capnp::Error> {
    use mix_rule::test::Which as T;
    let weight = match r.get_weight() {
        Ok(MixWeight::Require) => Weight::Require,
        Ok(MixWeight::Prefer) => Weight::Prefer,
        Ok(MixWeight::Avoid) => Weight::Avoid,
        Ok(MixWeight::Exclude) => Weight::Exclude,
        // A weight from a newer schema: refuse rather than guess its strength.
        Err(_) => return Ok(Err(BadField("rule.weight"))),
    };
    let share = Some(r.get_share()).filter(|&s| s > 0);
    let range = |m: pharos_api::pharos_capnp::mix_range::Reader<'_>| (m.get_from(), m.get_to());
    let u16_of = |v: u32| u16::try_from(v).unwrap_or(u16::MAX);
    let test = match r.get_test().which() {
        Ok(T::Genre(t)) => Test::Genre(text(t)?),
        Ok(T::Artist(t)) => Test::Artist(text(t)?),
        Ok(T::Album(t)) => Test::Album(text(t)?),
        Ok(T::Years(m)) => {
            let (from, to) = range(m?);
            Test::Years {
                from: u16_of(from),
                to: u16_of(to),
            }
        }
        Ok(T::Favorite(())) => Test::Favorite,
        Ok(T::NeverPlayed(())) => Test::NeverPlayed,
        Ok(T::PlayCount(m)) => {
            let (min, max) = range(m?);
            Test::PlayCount { min, max }
        }
        Ok(T::NotPlayedForDays(days)) => Test::NotPlayedFor { days },
        Ok(T::PlayedWithinDays(days)) => Test::PlayedWithin { days },
        Ok(T::AddedWithinDays(days)) => Test::AddedWithin { days },
        Ok(T::LengthSecs(m)) => {
            let (min_s, max_s) = range(m?);
            Test::Length { min_s, max_s }
        }
        Ok(T::LikeSeed(())) => Test::LikeSeed,
        Ok(T::TempoBpm(m)) => {
            let (min, max) = range(m?);
            Test::Tempo {
                min: u16_of(min),
                max: u16_of(max),
            }
        }
        Ok(T::Energy(m)) => {
            let (min, max) = range(m?);
            let pct = |v: u32| u8::try_from(v).unwrap_or(u8::MAX);
            Test::Energy {
                min: pct(min),
                max: pct(max),
            }
        }
        Err(_) => return Ok(Err(BadField("rule.test"))),
    };
    Ok(Ok(Rule {
        weight,
        share,
        test,
    }))
}

/// The ask a `MixSpec`, `recent` and `count` make, or the field that is
/// unusable. A synthetic id in `recent` names no song and is skipped.
pub(super) fn read_ask(
    spec: mix_spec::Reader<'_>,
    recent: capnp::struct_list::Reader<'_, pharos_api::pharos_capnp::item_id::Owned>,
    count: u32,
) -> Result<Result<mix::MixAsk, BadField>, capnp::Error> {
    let rules_in = spec.get_rules()?;
    if rules_in.len() as usize > mix::select::MAX_RULES {
        return Ok(Err(BadField("spec.rules")));
    }
    let mut rules = Vec::with_capacity(rules_in.len() as usize);
    for r in rules_in.iter() {
        match read_rule(r)? {
            Ok(rule) => rules.push(rule),
            Err(bad) => return Ok(Err(bad)),
        }
    }
    let seed = match spec.get_seed().which() {
        Ok(mix_spec::seed::None(())) => Seed::None,
        Ok(mix_spec::seed::Follow(())) => Seed::Follow,
        Ok(mix_spec::seed::Song(id)) => match read_item_id(id?)? {
            WireId::Real(id) => Seed::Song(id),
            WireId::Synth(_) => return Ok(Err(BadField("spec.seed"))),
        },
        Err(_) => return Ok(Err(BadField("spec.seed"))),
    };
    // Read only the tail the service keeps, so a huge list costs nothing.
    let skip = (recent.len() as usize).saturating_sub(mix::MAX_RECENT);
    let mut ids = Vec::with_capacity(recent.len() as usize - skip);
    for id in recent.iter().skip(skip) {
        if let WireId::Real(id) = read_item_id(id)? {
            ids.push(id);
        }
    }
    Ok(Ok(mix::MixAsk {
        rules,
        seed,
        artist_gap: spec.get_artist_gap(),
        album_gap: spec.get_album_gap(),
        recent: ids,
        count: count.min(mix::select::MAX_COUNT as u32),
    }))
}

pub(super) fn write_batch(mut out: mix_batch::Builder<'_>, mixed: &Mixed, server_id: &str) {
    let len = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
    out.set_pool(len(mixed.pool));
    out.set_eligible(len(mixed.eligible));
    out.set_exhausted(mixed.exhausted);
    {
        let mut relaxed = out.reborrow().init_relaxed(len(mixed.relaxed.len()));
        for (i, r) in (0..).zip(&mixed.relaxed) {
            relaxed.set(i, r.label());
        }
    }
    let mut picks = out.init_picks(len(mixed.picks.len()));
    for (i, p) in (0..).zip(&mixed.picks) {
        let mut pick = picks.reborrow().get(i);
        write_item(pick.reborrow().init_item(), &p.item, p.user_data, server_id);
        let mut reasons = pick.init_reasons(len(p.reasons.len()));
        for (j, why) in (0..).zip(&p.reasons) {
            reasons.set(j, why.as_str());
        }
    }
}
