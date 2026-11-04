//! What the members of a watch-together group have seen (014 US6): the
//! server's answer as the screens read it, and how it is worded. Pure, so the
//! marks are tested without a browser.

use super::together::Member;
use pharos_client::api::ItemRef;

/// Ids one `seen` call may ask about, the server's cap.
pub const MAX_IDS: usize = 200;

/// One item as one member has seen it. A film or episode has `played` and
/// `position_pct`; a series or season has `played_count` of `total_count`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ItemSeen {
    pub id: ItemRef,
    pub played: bool,
    pub position_pct: u8,
    pub played_count: u32,
    pub total_count: u32,
}

/// What one member has seen of the items asked about. An item the viewer may
/// not see is missing, and reads as not seen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberSeen {
    pub member: Member,
    pub items: Vec<ItemSeen>,
}

/// How a member stands on one item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    Seen,
    /// Part-way, as a percent.
    Partway(u8),
    NotSeen,
    /// Episodes seen of the episodes there are.
    Episodes {
        seen: u32,
        of: u32,
    },
}

impl Mark {
    /// The mark for `item`; no entry is not seen.
    pub fn of(item: Option<&ItemSeen>) -> Self {
        let Some(item) = item else {
            return Self::NotSeen;
        };
        if item.total_count > 0 {
            return Self::Episodes {
                seen: item.played_count,
                of: item.total_count,
            };
        }
        if item.played {
            Self::Seen
        } else if item.position_pct > 0 {
            Self::Partway(item.position_pct)
        } else {
            Self::NotSeen
        }
    }

    pub fn label(self) -> String {
        match self {
            Self::Seen => "seen".to_owned(),
            Self::Partway(p) => format!("{p}%"),
            Self::NotSeen => "not seen".to_owned(),
            Self::Episodes { seen, of } => format!("{seen}/{of} episodes"),
        }
    }

    /// The CSS state: everything seen, some of it, or none.
    pub fn state(self) -> &'static str {
        match self {
            Self::Seen => "is-seen",
            Self::Partway(_) => "is-partway",
            Self::NotSeen => "is-unseen",
            Self::Episodes { seen, of } if of > 0 && seen >= of => "is-seen",
            Self::Episodes { seen, .. } if seen > 0 => "is-partway",
            Self::Episodes { .. } => "is-unseen",
        }
    }
}

/// A member and their mark on one item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeenRow {
    pub member_id: String,
    pub name: String,
    pub mark: Mark,
}

/// Every member's mark on `id`.
pub fn rows(seen: &[MemberSeen], id: ItemRef) -> Vec<SeenRow> {
    seen.iter()
        .map(|m| SeenRow {
            member_id: m.member.id.clone(),
            name: m.member.name.clone(),
            mark: Mark::of(m.items.iter().find(|i| i.id == id)),
        })
        .collect()
}

/// The first letter of a name, for a dot.
pub fn initial(name: &str) -> String {
    name.chars()
        .find(|c| c.is_alphanumeric())
        .map(|c| c.to_uppercase().collect())
        .unwrap_or_else(|| "?".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(id: u64, played: bool, pct: u8, count: u32, of: u32) -> ItemSeen {
        ItemSeen {
            id: ItemRef::Real(id),
            played,
            position_pct: pct,
            played_count: count,
            total_count: of,
        }
    }

    #[test]
    fn a_film_is_seen_partway_or_not_seen() {
        assert_eq!(Mark::of(Some(&seen(1, true, 0, 0, 0))), Mark::Seen);
        assert_eq!(Mark::of(Some(&seen(1, false, 40, 0, 0))), Mark::Partway(40));
        assert_eq!(Mark::of(Some(&seen(1, false, 0, 0, 0))), Mark::NotSeen);
        assert_eq!(Mark::of(None), Mark::NotSeen);
    }

    #[test]
    fn labels_say_what_the_mark_means() {
        assert_eq!(Mark::Seen.label(), "seen");
        assert_eq!(Mark::Partway(40).label(), "40%");
        assert_eq!(Mark::NotSeen.label(), "not seen");
        assert_eq!(
            Mark::of(Some(&seen(1, false, 0, 6, 10))).label(),
            "6/10 episodes"
        );
    }

    #[test]
    fn a_season_is_seen_only_when_every_episode_is() {
        assert_eq!(Mark::Episodes { seen: 3, of: 3 }.state(), "is-seen");
        assert_eq!(Mark::Episodes { seen: 1, of: 3 }.state(), "is-partway");
        assert_eq!(Mark::Episodes { seen: 0, of: 3 }.state(), "is-unseen");
    }

    #[test]
    fn rows_pair_each_member_with_their_own_mark() {
        let member = |id: &str, name: &str| Member {
            id: id.into(),
            name: name.into(),
            buffering: false,
        };
        let answer = vec![
            MemberSeen {
                member: member("a", "Ada"),
                items: vec![seen(1, true, 0, 0, 0)],
            },
            MemberSeen {
                member: member("b", "Bo"),
                items: vec![],
            },
        ];
        let rows = rows(&answer, ItemRef::Real(1));
        assert_eq!(
            rows.iter()
                .map(|r| (r.name.as_str(), r.mark))
                .collect::<Vec<_>>(),
            vec![("Ada", Mark::Seen), ("Bo", Mark::NotSeen)]
        );
    }

    #[test]
    fn an_initial_is_the_first_letter_upper_cased() {
        assert_eq!(initial("ada"), "A");
        assert_eq!(initial("  "), "?");
    }
}
