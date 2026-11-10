//! The "Check match" badge on a show's page (spec 033 §3): an admin sees
//! which shows the matcher flagged as a close call, and one click opens
//! Manage on the Show identity tab. Every other show gets a "Fix identity"
//! button instead (V943), which opens the same tab focused on that show;
//! the two are never shown together. The flagged list is asked for once,
//! when the first show page of an admin opens.

use dioxus::prelude::*;
use pharos_client::api::ItemRef;

use super::app::{call, AccountMe, Ctx};
use super::manage::{local_expiry, now_ms, FlaggedShows, HeldManageToken, ManageFocus, ManageTab};
use super::platform;
use super::route::Route;
use super::views::Nav;
use crate::client::series_item_ref;
use crate::views::AdminTab;

/// The shows a flagged answer names; an id this build cannot read names
/// none.
pub fn flagged_refs(series_ids: &[String]) -> Vec<ItemRef> {
    series_ids
        .iter()
        .filter_map(|id| series_item_ref(id))
        .collect()
}

/// Whether the badge shows: an admin, on a show in the answered list.
pub fn badge_shown(admin: bool, flagged: Option<&[ItemRef]>, series: ItemRef) -> bool {
    admin && flagged.is_some_and(|list| list.contains(&series))
}

/// Whether the "Fix identity" button shows: an admin, on a show that is not
/// flagged (a flagged one has the badge). Until the flagged list answers it
/// shows, so a failed answer never hides the way in.
pub fn fix_identity_shown(admin: bool, flagged: Option<&[ItemRef]>, series: ItemRef) -> bool {
    admin && !badge_shown(admin, flagged, series)
}

/// The 32-hex series id the server names a show by: the 63-bit hash in both
/// halves. `None` for any other kind of item.
pub fn series_id_hex(series: ItemRef) -> Option<String> {
    match series {
        ItemRef::Synth(hash) => Some(format!("{hash:016x}{hash:016x}")),
        _ => None,
    }
}

#[cfg(feature = "web")]
async fn fetch_flagged(base: &str, token: &str) -> Result<Vec<String>, String> {
    crate::client::web::flagged_series_ids(base, token)
        .await
        .map_err(|e| e.to_string())
}

#[cfg(not(feature = "web"))]
async fn fetch_flagged(_base: &str, _token: &str) -> Result<Vec<String>, String> {
    Err("flagged shows are only read in the web build".into())
}

#[component]
pub fn CheckMatchBadge(series: ItemRef) -> Element {
    let me = try_use_context::<AccountMe>();
    let ctx = try_use_context::<Ctx>();
    let flagged = try_use_context::<FlaggedShows>();
    let held = try_use_context::<HeldManageToken>();
    let tab = try_use_context::<ManageTab>();
    let focus = try_use_context::<ManageFocus>();
    let nav = try_use_context::<Nav>();
    let is_admin =
        move || me.is_some_and(|me| me.0.read().clone().flatten().is_some_and(|m| m.is_admin));
    use_effect(move || {
        let (Some(ctx), Some(flagged), Some(held)) = (ctx, flagged, held) else {
            return;
        };
        if !is_admin() || flagged.asked.peek().to_owned() {
            return;
        }
        let (mut list, mut asked, mut held) = (flagged.list, flagged.asked, held.0);
        asked.set(true);
        spawn(async move {
            let Some(api) = ctx.api.peek().clone() else {
                asked.set(false);
                return;
            };
            let now = now_ms();
            let live = held.peek().clone().filter(|t| t.expires_at_ms > now);
            let token = match live {
                Some(t) => t.token,
                None => match api.manage_token(&call("title", "flagged_token")).await {
                    Ok(mut t) => {
                        t.expires_at_ms = local_expiry(t.expires_at_ms, now);
                        let token = t.token.clone();
                        held.set(Some(t));
                        token
                    }
                    Err(e) => {
                        platform::warn(&format!("pharos: check match token refused: {e}"));
                        return;
                    }
                },
            };
            match fetch_flagged(&platform::page_origin(), &token).await {
                Ok(ids) => list.set(Some(flagged_refs(&ids))),
                Err(e) => platform::warn(&format!("pharos: flagged shows unreadable: {e}")),
            }
        });
    });
    let list = flagged.and_then(|f| f.list.read().clone());
    let flagged_here = badge_shown(is_admin(), list.as_deref(), series);
    let fix_here = fix_identity_shown(is_admin(), list.as_deref(), series);
    let (Some(tab), Some(focus), Some(nav), true) = (tab, focus, nav, flagged_here || fix_here)
    else {
        return rsx! {};
    };
    let href = Route::Manage.path();
    let (label, title, class) = if flagged_here {
        (
            "Check match",
            "The matcher was not sure of this show. Open Manage to check it.",
            "pharos-button pharos-button-secondary pharos-check-match",
        )
    } else {
        (
            "Fix identity",
            "Open Manage to pick this show's match or move its episodes to another show.",
            "pharos-button pharos-button-secondary pharos-fix-identity",
        )
    };
    rsx! {
        a {
            class: "{class}",
            href: "{href}",
            title: "{title}",
            onclick: move |evt: MouseEvent| {
                if evt.modifiers().is_empty() {
                    evt.prevent_default();
                    let (mut tab, mut focus) = (tab.0, focus.0);
                    tab.set(Some(AdminTab::ShowIdentity));
                    focus.set(if flagged_here { None } else { series_id_hex(series) });
                    nav.0.call(Route::Manage);
                }
            },
            "{label}"
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn flagged_ids_become_the_shows_item_refs_and_unreadable_ones_are_dropped() {
        let ids = vec![
            "000000000000002a000000000000002a".to_owned(),
            "not an id".to_owned(),
        ];
        assert_eq!(flagged_refs(&ids), vec![ItemRef::Synth(42)]);
    }

    #[test]
    fn only_an_admin_sees_the_badge_and_only_on_a_flagged_show() {
        let list = [ItemRef::Synth(42)];
        assert!(badge_shown(true, Some(&list), ItemRef::Synth(42)));
        assert!(!badge_shown(false, Some(&list), ItemRef::Synth(42)));
        assert!(!badge_shown(true, Some(&list), ItemRef::Synth(43)));
        assert!(!badge_shown(true, None, ItemRef::Synth(42)));
    }

    #[test]
    fn an_admin_gets_fix_identity_on_every_show_the_badge_is_not_on() {
        let list = [ItemRef::Synth(42)];
        assert!(!fix_identity_shown(true, Some(&list), ItemRef::Synth(42)));
        assert!(fix_identity_shown(true, Some(&list), ItemRef::Synth(43)));
        assert!(fix_identity_shown(true, None, ItemRef::Synth(42)));
        assert!(!fix_identity_shown(false, Some(&list), ItemRef::Synth(43)));
    }

    #[test]
    fn a_show_ref_names_its_series_id_the_way_the_server_does() {
        let hex = series_id_hex(ItemRef::Synth(42)).unwrap();
        assert_eq!(hex, "000000000000002a000000000000002a");
        assert_eq!(series_item_ref(&hex), Some(ItemRef::Synth(42)));
        assert_eq!(series_id_hex(ItemRef::Real(1)), None);
    }
}
