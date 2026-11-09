//! The "Check match" badge on a show's page (spec 033 §3): an admin sees
//! which shows the matcher flagged as a close call, and one click opens
//! Manage on the Show identity tab. The flagged list is asked for once,
//! when the first show page of an admin opens.

use dioxus::prelude::*;
use pharos_client::api::ItemRef;

use super::app::{call, AccountMe, Ctx};
use super::manage::{local_expiry, now_ms, FlaggedShows, HeldManageToken, ManageTab};
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
    let shown = badge_shown(
        is_admin(),
        flagged.and_then(|f| f.list.read().clone()).as_deref(),
        series,
    );
    let (Some(tab), Some(nav), true) = (tab, nav, shown) else {
        return rsx! {};
    };
    let href = Route::Manage.path();
    rsx! {
        a {
            class: "pharos-button pharos-button-secondary pharos-check-match",
            href: "{href}",
            title: "The matcher was not sure of this show. Open Manage to check it.",
            onclick: move |evt: MouseEvent| {
                if evt.modifiers().is_empty() {
                    evt.prevent_default();
                    let mut tab = tab.0;
                    tab.set(Some(AdminTab::ShowIdentity));
                    nav.0.call(Route::Manage);
                }
            },
            "Check match"
        }
    }
}

#[cfg(test)]
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
}
