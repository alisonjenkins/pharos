//! A show's or season's regular cast, ranked from its episodes' credits.

use std::collections::{HashMap, HashSet};

use crate::{ItemPerson, MediaId};

/// Placeholder billing for a credit with no order, so it ranks after billed ones.
const UNBILLED: u64 = 1_000;

/// One person's credits across the episodes seen so far.
struct Tally {
    person: ItemPerson,
    episodes: HashSet<MediaId>,
    billing: u64,
    characters: HashMap<String, u32>,
}

/// A show's regular cast from its episodes' actor credits: one entry per
/// person, most episodes first (ties by average billing, then name), each
/// with the character they are most often credited as. At most `limit`
/// entries, renumbered `sort_order` 0, 1, 2, ...
pub fn ensemble_cast(credits: Vec<(MediaId, ItemPerson)>, limit: usize) -> Vec<ItemPerson> {
    let mut by_person: HashMap<String, Tally> = HashMap::new();
    for (item, person) in credits {
        let tally = by_person
            .entry(person.wire_id.clone())
            .or_insert_with(|| Tally {
                person: ItemPerson {
                    character: None,
                    sort_order: None,
                    ..person.clone()
                },
                episodes: HashSet::new(),
                billing: 0,
                characters: HashMap::new(),
            });
        if tally.episodes.insert(item) {
            tally.billing += person.sort_order.map_or(UNBILLED, u64::from);
        }
        if tally.person.thumb_url.is_none() {
            tally.person.thumb_url = person.thumb_url;
        }
        if let Some(c) = person.character.filter(|c| !c.trim().is_empty()) {
            *tally.characters.entry(c).or_insert(0) += 1;
        }
    }
    let mut ranked: Vec<Tally> = by_person.into_values().collect();
    ranked.sort_by(|a, b| {
        let (na, nb) = (a.episodes.len() as u64, b.episodes.len() as u64);
        nb.cmp(&na)
            .then((a.billing / na).cmp(&(b.billing / nb)))
            .then_with(|| a.person.name.cmp(&b.person.name))
    });
    ranked
        .into_iter()
        .take(limit)
        .zip(0u32..)
        .map(|(t, order)| {
            // Most frequent character; ties go to the alphabetically first.
            let character = t
                .characters
                .into_iter()
                .max_by(|(ca, na), (cb, nb)| na.cmp(nb).then_with(|| cb.cmp(ca)))
                .map(|(c, _)| c);
            ItemPerson {
                character,
                sort_order: Some(order),
                ..t.person
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PersonKind;

    fn credit(name: &str, character: &str, order: u32) -> ItemPerson {
        ItemPerson {
            name: name.into(),
            wire_id: crate::person_wire_id(name),
            character: Some(character.into()).filter(|c: &String| !c.is_empty()),
            kind: PersonKind::Actor,
            sort_order: Some(order),
            ..ItemPerson::default()
        }
    }

    #[test]
    fn regulars_rank_by_episode_count_then_billing() {
        let credits = vec![
            (1, credit("Lead", "Hero", 0)),
            (2, credit("Lead", "Hero", 0)),
            (3, credit("Lead", "Hero", 0)),
            (1, credit("Second", "Sidekick", 1)),
            (2, credit("Second", "Sidekick", 1)),
            (3, credit("Second", "Sidekick", 1)),
            (2, credit("Once", "Villain", 0)),
        ];
        let cast = ensemble_cast(credits, 10);
        let names: Vec<_> = cast.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["Lead", "Second", "Once"]);
        assert_eq!(cast[1].sort_order, Some(1));
    }

    #[test]
    fn the_most_frequent_character_wins_and_blanks_are_ignored() {
        let credits = vec![
            (1, credit("A", "Doctor", 0)),
            (2, credit("A", "The Doctor", 0)),
            (3, credit("A", "The Doctor", 0)),
            (4, credit("A", "", 0)),
        ];
        let cast = ensemble_cast(credits, 10);
        assert_eq!(cast[0].character.as_deref(), Some("The Doctor"));
    }

    #[test]
    fn a_person_credited_twice_on_one_episode_counts_once() {
        let credits = vec![
            (1, credit("Twice", "X", 0)),
            (1, credit("Twice", "Y", 0)),
            (1, credit("Other", "Z", 1)),
            (2, credit("Other", "Z", 1)),
        ];
        let cast = ensemble_cast(credits, 10);
        assert_eq!(cast[0].name, "Other");
    }

    #[test]
    fn limit_caps_the_list() {
        let credits = (0..50)
            .map(|i| (1, credit(&format!("P{i:02}"), "", i)))
            .collect();
        assert_eq!(ensemble_cast(credits, 30).len(), 30);
    }
}
