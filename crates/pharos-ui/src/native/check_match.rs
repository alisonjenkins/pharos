//! The "Check match" badge on a show's page (spec 033 §3): an admin sees
//! which shows the matcher flagged as a close call, and one click opens
//! Manage on the Show identity tab. Every other show gets a "Fix identity"
//! button instead (V943), which opens the same tab focused on that show;
//! the two are never shown together. The flagged list is asked for once,
//! when the first show page of an admin opens.

use dioxus::prelude::*;
use pharos_client::api::{ItemRef, MediaToken};

use super::app::{call, AccountMe, Ctx};
use super::manage::{local_expiry, now_ms, FlaggedShows, HeldManageToken, ManageFocus, ManageTab};
use super::platform;
use super::route::Route;
use super::views::Nav;
use crate::client::{series_item_ref, FixDone};
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

/// The held manage token if it is still live, else a fresh one, kept for
/// the next caller. `Err("")` when there is no connection yet.
async fn held_token(
    ctx: Ctx,
    mut held: Signal<Option<MediaToken>>,
    action: &str,
) -> Result<String, String> {
    let Some(api) = ctx.api.peek().clone() else {
        return Err(String::new());
    };
    let now = now_ms();
    if let Some(t) = held.peek().clone().filter(|t| t.expires_at_ms > now) {
        return Ok(t.token);
    }
    let mut t = api
        .manage_token(&call("title", action))
        .await
        .map_err(|e| e.to_string())?;
    t.expires_at_ms = local_expiry(t.expires_at_ms, now);
    let token = t.token.clone();
    held.set(Some(t));
    Ok(token)
}

#[cfg(feature = "web")]
async fn rebuild_one(base: &str, token: &str, id: u64) -> Result<FixDone, String> {
    crate::client::web::rebuild_episodes(base, token, &[id])
        .await
        .map_err(|e| e.to_string())
}

#[cfg(not(feature = "web"))]
async fn rebuild_one(_base: &str, _token: &str, _id: u64) -> Result<FixDone, String> {
    Err("episodes are only rebuilt in the web build".into())
}

/// What the row says after a Re-match: the server's count, or its reason.
pub fn rematch_status(done: Result<FixDone, String>) -> String {
    match done {
        Ok(done) if done.rebuilding == 0 => "Nothing to rebuild.".to_owned(),
        Ok(_) => "Re-matching from this show in the background.".to_owned(),
        Err(e) => format!("Couldn't re-match: {e}"),
    }
}

/// "Re-match" on an episode row, for admins: rebuild this one episode from
/// its show's stored match (V943), for an episode that kept another show's
/// title or art after the show was fixed.
#[component]
pub fn RematchEpisode(id: ItemRef) -> Element {
    let me = try_use_context::<AccountMe>();
    let ctx = try_use_context::<Ctx>();
    let held = try_use_context::<HeldManageToken>();
    let mut status = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);
    let admin = me.is_some_and(|me| me.0.read().clone().flatten().is_some_and(|m| m.is_admin));
    let (ItemRef::Real(media_id), Some(ctx), Some(held), true) = (id, ctx, held, admin) else {
        return rsx! {};
    };
    rsx! {
        button {
            r#type: "button",
            class: "pharos-button pharos-button-secondary pharos-episode-rematch",
            title: "Rebuild this episode from its show's match",
            "aria-label": "Re-match from this show",
            disabled: *busy.read(),
            onclick: move |_| {
                if *busy.peek() {
                    return;
                }
                busy.set(true);
                spawn(async move {
                    let done = match held_token(ctx, held.0, "rematch_token").await {
                        Ok(token) => rebuild_one(&platform::page_origin(), &token, media_id).await,
                        Err(e) if e.is_empty() => Err("not connected".to_owned()),
                        Err(e) => Err(e),
                    };
                    if let Err(e) = &done {
                        platform::warn(&format!("pharos: re-match of {media_id} refused: {e}"));
                    }
                    status.set(Some(rematch_status(done)));
                    busy.set(false);
                });
            },
            "Re-match"
        }
        if let Some(line) = status.read().as_ref() {
            span { class: "pharos-muted pharos-episode-rematch-status", role: "status", "{line}" }
        }
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
        let (mut list, mut asked, held) = (flagged.list, flagged.asked, held.0);
        asked.set(true);
        spawn(async move {
            let token = match held_token(ctx, held, "flagged_token").await {
                Ok(token) => token,
                Err(e) => {
                    if e.is_empty() {
                        asked.set(false);
                    } else {
                        platform::warn(&format!("pharos: check match token refused: {e}"));
                    }
                    return;
                }
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
    fn a_rematch_says_what_the_server_did_or_why_it_refused() {
        let done = |n| {
            Ok(FixDone {
                rebuilding: n,
                ..FixDone::default()
            })
        };
        assert_eq!(
            rematch_status(done(1)),
            "Re-matching from this show in the background."
        );
        assert_eq!(rematch_status(done(0)), "Nothing to rebuild.");
        assert_eq!(
            rematch_status(Err("item 9 is a Movie, not an episode".into())),
            "Couldn't re-match: item 9 is a Movie, not an episode"
        );
    }

    #[test]
    fn a_show_ref_names_its_series_id_the_way_the_server_does() {
        let hex = series_id_hex(ItemRef::Synth(42)).unwrap();
        assert_eq!(hex, "000000000000002a000000000000002a");
        assert_eq!(series_item_ref(&hex), Some(ItemRef::Synth(42)));
        assert_eq!(series_id_hex(ItemRef::Real(1)), None);
    }
}
