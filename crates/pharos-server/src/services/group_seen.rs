//! What each member of a watch-together group has watched (spec 014 US6,
//! ADR-0054). A member reads their peers' played state only through the
//! `Membership` of the group they are in, and only for items they may see
//! themselves, so the group never widens what anyone is shown (V201).

use std::collections::{HashMap, HashSet};

use pharos_core::{MediaKind, User, UserDataStore, UserItemData};
use pharos_jellyfin_api::dto::{season_id_for_key, series_id_for_key, WireId};
use pharos_sync::group::GroupSnapshot;

use crate::state::AppState;

/// Ids one `seen` call may ask about.
pub const MAX_IDS: usize = 200;

/// How a `seen` call ended: the label set of `pharos_group_seen_total`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Outcome {
    Answered,
    OverLimit,
    NotInGroup,
    Unavailable,
}

impl Outcome {
    pub const ALL: [Self; 4] = [
        Self::Answered,
        Self::OverLimit,
        Self::NotInGroup,
        Self::Unavailable,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Answered => "answered",
            Self::OverLimit => "over_limit",
            Self::NotInGroup => "not_in_group",
            Self::Unavailable => "unavailable",
        }
    }
}

/// Counts one call and logs its inputs and verdict: `asked` ids came in,
/// `answered` of them were shown (the rest were hidden from the caller or do
/// not exist), for `members` members.
pub fn record(
    outcome: Outcome,
    caller: &User,
    group: &str,
    asked: usize,
    answered: usize,
    members: usize,
) {
    metrics::counter!("pharos_group_seen_total", "outcome" => outcome.label()).increment(1);
    tracing::info!(
        user = %caller.name,
        group,
        outcome = outcome.label(),
        asked,
        answered,
        omitted = asked.saturating_sub(answered),
        members,
        "group seen"
    );
}

/// One item as one member has seen it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ItemSeen {
    pub id: WireId,
    pub played: bool,
    pub position_pct: u8,
    pub played_count: u32,
    pub total_count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberSeen {
    pub member_id: String,
    pub name: String,
    pub buffering: bool,
    pub items: Vec<ItemSeen>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SeenError {
    Unavailable,
}

/// What an id stands for once resolved: a stored item, or the episodes a
/// series or season holds.
enum Target {
    Item { id: u64, duration_ms: Option<u64> },
    Episodes(Vec<u64>),
}

/// Each member of `group`'s state on `ids`, as `caller` may see them. An id
/// the caller may not see, or that names nothing, is in no member's `items`.
pub async fn seen(
    state: &AppState,
    caller: &User,
    group: &GroupSnapshot,
    ids: &[WireId],
) -> Result<Vec<MemberSeen>, SeenError> {
    let targets = resolve(state, ids).await?;
    // Visibility first: what the caller may not see is dropped before any
    // other member's state is read for it.
    let mut candidates: Vec<u64> = targets
        .iter()
        .flat_map(|(_, t)| match t {
            Target::Item { id, .. } => vec![*id],
            Target::Episodes(eps) => eps.clone(),
        })
        .collect();
    candidates.sort_unstable();
    candidates.dedup();
    crate::services::access::retain_visible(state, caller, &mut candidates, |id| *id, "group_seen")
        .await
        .map_err(|_| SeenError::Unavailable)?;
    let visible: HashSet<u64> = candidates.iter().copied().collect();
    let shown: Vec<(WireId, Target)> = targets
        .into_iter()
        .filter_map(|(wire, target)| match target {
            Target::Item { id, .. } if !visible.contains(&id) => None,
            Target::Item { .. } => Some((wire, target)),
            Target::Episodes(eps) => {
                let eps: Vec<u64> = eps.into_iter().filter(|e| visible.contains(e)).collect();
                (!eps.is_empty()).then_some((wire, Target::Episodes(eps)))
            }
        })
        .collect();
    let lookup: Vec<u64> = candidates;
    let mut out = Vec::with_capacity(group.participant_ids.len());
    for (i, member_id) in group.participant_ids.iter().enumerate() {
        let user = group.participant_users.get(i).copied().flatten();
        let items = match user {
            Some(user) => match state.stores.user_data_bulk(user, &lookup).await {
                Ok(rows) => {
                    let by_id: HashMap<u64, UserItemData> =
                        lookup.iter().copied().zip(rows).collect();
                    shown
                        .iter()
                        .map(|(wire, t)| item_seen(*wire, t, &by_id))
                        .collect()
                }
                Err(e) => {
                    tracing::warn!(user = %user.0, error = %e, "group seen: a member's state is unreadable");
                    return Err(SeenError::Unavailable);
                }
            },
            None => Vec::new(),
        };
        out.push(MemberSeen {
            member_id: member_id.to_string(),
            name: group.participants.get(i).cloned().unwrap_or_default(),
            buffering: group.buffering_members.contains(member_id),
            items,
        });
    }
    Ok(out)
}

fn item_seen(wire: WireId, target: &Target, data: &HashMap<u64, UserItemData>) -> ItemSeen {
    let of = |id: &u64| data.get(id).copied().unwrap_or_default();
    match target {
        Target::Item { id, duration_ms } => {
            let d = of(id);
            ItemSeen {
                id: wire,
                played: d.played,
                position_pct: if d.played {
                    0
                } else {
                    position_pct(d.last_played_position_ticks, *duration_ms)
                },
                played_count: 0,
                total_count: 0,
            }
        }
        Target::Episodes(eps) => {
            let played = eps.iter().filter(|e| of(e).played).count();
            ItemSeen {
                id: wire,
                played: played == eps.len(),
                position_pct: 0,
                played_count: u32::try_from(played).unwrap_or(u32::MAX),
                total_count: u32::try_from(eps.len()).unwrap_or(u32::MAX),
            }
        }
    }
}

/// The resume point as a whole percent, 0 when the length is unknown and never
/// 100 (an item at 100 is played).
fn position_pct(ticks: u64, duration_ms: Option<u64>) -> u8 {
    let Some(total_ticks) = duration_ms.and_then(|ms| ms.checked_mul(10_000)) else {
        return 0;
    };
    if total_ticks == 0 {
        return 0;
    }
    let pct = (u128::from(ticks) * 100 / u128::from(total_ticks)).min(99);
    u8::try_from(pct).unwrap_or(99)
}

/// Resolves each id against the library: a stored item that exists, or the
/// episodes of a synthesised series or season. Unknown ids are left out.
async fn resolve(state: &AppState, ids: &[WireId]) -> Result<Vec<(WireId, Target)>, SeenError> {
    let real: HashSet<u64> = ids
        .iter()
        .filter_map(|w| match w {
            WireId::Real(id) => Some(*id),
            WireId::Synth(_) => None,
        })
        .collect();
    let synth: HashSet<String> = ids
        .iter()
        .filter(|w| matches!(w, WireId::Synth(_)))
        .map(ToString::to_string)
        .collect();
    let all = state
        .list_items_cached()
        .await
        .map_err(|_| SeenError::Unavailable)?;
    let mut items: HashMap<u64, Option<u64>> = HashMap::new();
    let mut episodes: HashMap<String, Vec<u64>> = HashMap::new();
    for item in all.iter() {
        if real.contains(&item.id) {
            items.insert(item.id, item.probe.duration_ms);
        }
        if synth.is_empty() || item.kind != MediaKind::Episode {
            continue;
        }
        let Some(series) = item.series.as_ref() else {
            continue;
        };
        let folder = series.series_folder.as_deref();
        let series_key = series_id_for_key(folder, &series.series_name);
        if synth.contains(&series_key) {
            episodes.entry(series_key).or_default().push(item.id);
        }
        if let Some(n) = series.season_number {
            let season_key = season_id_for_key(folder, &series.series_name, n);
            if synth.contains(&season_key) {
                episodes.entry(season_key).or_default().push(item.id);
            }
        }
    }
    Ok(ids
        .iter()
        .filter_map(|wire| match wire {
            WireId::Real(id) => items.get(id).map(|d| {
                (
                    *wire,
                    Target::Item {
                        id: *id,
                        duration_ms: *d,
                    },
                )
            }),
            WireId::Synth(_) => episodes
                .get(&wire.to_string())
                .map(|eps| (*wire, Target::Episodes(eps.clone()))),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_labels_are_distinct() {
        let labels: HashSet<_> = Outcome::ALL.iter().map(|o| o.label()).collect();
        assert_eq!(labels.len(), Outcome::ALL.len());
    }

    #[test]
    fn a_resume_point_is_a_percent_of_the_length_and_never_100() {
        assert_eq!(position_pct(5_000 * 10_000, Some(10_000)), 50);
        assert_eq!(position_pct(u64::MAX, Some(10_000)), 99);
        assert_eq!(position_pct(5, None), 0);
        assert_eq!(position_pct(5, Some(0)), 0);
    }

    #[test]
    fn a_series_is_played_when_every_episode_is() {
        let played = UserItemData {
            played: true,
            ..UserItemData::default()
        };
        let data: HashMap<u64, UserItemData> = [(1, played), (2, UserItemData::default())].into();
        let wire = WireId::Synth(9);
        let some = item_seen(wire, &Target::Episodes(vec![1, 2]), &data);
        assert_eq!(
            (some.played, some.played_count, some.total_count),
            (false, 1, 2)
        );
        let all = item_seen(wire, &Target::Episodes(vec![1]), &data);
        assert!(all.played);
    }
}
