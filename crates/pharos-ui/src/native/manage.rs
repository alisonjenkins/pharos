//! Manage inside the app (B921): the admin pages under the app's own header,
//! signed in by the browser's session. The admin endpoints are the Jellyfin
//! API's and take a header token, so the screen asks the session for a
//! short-lived manage token (ADR-0082) and asks again before it ends.

use dioxus::prelude::*;
use pharos_client::api::{MediaToken, NativeApi};
use pharos_client::ClientError;

use super::app::{call, AccountMe, Ctx};
use super::machine::{Event, Machine};
use super::platform;
use crate::views::{AdminPane, AdminTab};

/// How long before a manage token ends the screen asks for the next one.
const REFRESH_EARLY_MS: u64 = 5 * 60 * 1000;
/// The shortest wait between asks, so a token that is already near its end
/// (the session itself ends soon) or a server that keeps refusing cannot
/// spin the loop.
const REFRESH_FLOOR_MS: u64 = 30 * 1000;
/// The wait before asking again after the connection dropped mid-ask.
const RECONNECT_WAIT_MS: u32 = 5 * 1000;
/// The longest single sleep: a timer that a sleeping laptop held back is
/// caught by the next wall-clock look, at most this late.
const WAKE_CHECK_MS: u64 = 60 * 1000;

/// The least life the page gives a token it has just been handed. A device
/// clock ahead of the server's would otherwise see a fresh token as already
/// over and ask again on every round trip.
const SKEW_FLOOR_MS: u64 = 10 * 60 * 1000;

/// When the page treats a fresh token as ending, by its own clock: the
/// server's expiry, but never sooner than ten minutes after it arrived.
pub fn local_expiry(expires_at_ms: u64, now_ms: u64) -> u64 {
    expires_at_ms.max(now_ms.saturating_add(SKEW_FLOOR_MS))
}

/// The page's manage token, kept above the screen so going back to Manage
/// reuses it instead of minting another (B920). Cleared when the app gets a
/// new connection.
#[derive(Clone, Copy)]
pub struct HeldManageToken(pub Signal<Option<MediaToken>>);

/// The Manage tab to open on, set by a link elsewhere in the app (the
/// Check match badge) and taken by the next Manage screen.
#[derive(Clone, Copy)]
pub struct ManageTab(pub Signal<Option<AdminTab>>);

/// The show a "Fix identity" button asks the Show identity tab to open on,
/// by its 32-hex series id; taken by the next Manage screen, like
/// [`ManageTab`].
#[derive(Clone, Copy)]
pub struct ManageFocus(pub Signal<Option<String>>);

/// The flagged shows' item refs, asked for once by the first show page an
/// admin opens (`list` stays `None` until answered), and forgotten when
/// Manage closes, since a fix there changes the answer.
#[derive(Clone, Copy)]
pub struct FlaggedShows {
    pub list: Signal<Option<Vec<pharos_client::api::ItemRef>>>,
    pub asked: Signal<bool>,
}

/// When to ask for the next token, or `None` to ask now: five minutes
/// before the held one ends, but never sooner than thirty seconds after
/// the last ask.
pub fn next_ask_at(expires_at_ms: u64, now_ms: u64, last_ask_ms: Option<u64>) -> Option<u64> {
    let refresh_at = expires_at_ms.saturating_sub(REFRESH_EARLY_MS);
    let floor_at = last_ask_ms.map_or(0, |a| a.saturating_add(REFRESH_FLOOR_MS));
    let at = refresh_at.max(floor_at);
    (at > now_ms).then_some(at)
}

/// What the screen shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenState {
    Asking,
    /// A live token is in the screen's token signal.
    Held,
    /// The server said no: this user is not an admin.
    NotAdmin,
    Failed,
}

impl TokenState {
    /// The state a refused ask leaves; `None` when the refusal sends the app
    /// to sign-in instead.
    pub fn refused(error: &ClientError) -> Option<TokenState> {
        if error.needs_sign_in() {
            return None;
        }
        Some(match error {
            ClientError::Api {
                kind: "notPermitted",
                ..
            } => TokenState::NotAdmin,
            _ => TokenState::Failed,
        })
    }
}

pub(super) fn now_ms() -> u64 {
    u64::try_from(platform::unix_ms()).unwrap_or(0)
}

/// Sleeps until the wall clock reaches `at_ms`, looking at the clock at
/// least once a minute.
async fn sleep_until(at_ms: u64) {
    loop {
        let now = now_ms();
        if now >= at_ms {
            return;
        }
        let step = (at_ms - now).min(WAKE_CHECK_MS);
        platform::sleep_ms(u32::try_from(step).unwrap_or(u32::MAX)).await;
    }
}

/// Keeps `token` holding a live manage token for as long as the screen is
/// up; Dioxus ends the task with the screen. A refusal while the held token
/// still works keeps the pages up and asks again later; only a refusal with
/// nothing live left, or one that says this user is no longer an admin,
/// takes them down.
async fn keep_token(
    api: Signal<Option<NativeApi>>,
    mut machine: Signal<Machine>,
    mut held: Signal<Option<MediaToken>>,
    mut state: Signal<TokenState>,
    mut token: Signal<String>,
    mut why: Signal<String>,
) {
    let mut last_ask: Option<u64> = None;
    loop {
        let now = now_ms();
        let live = held.peek().clone().filter(|t| t.expires_at_ms > now);
        if let Some(t) = &live {
            if *token.peek() != t.token {
                token.set(t.token.clone());
            }
            if *state.peek() != TokenState::Held {
                state.set(TokenState::Held);
            }
            if let Some(at) = next_ask_at(t.expires_at_ms, now, last_ask) {
                sleep_until(at).await;
                continue;
            }
        }
        let Some(session) = api.peek().clone() else {
            platform::sleep_ms(RECONNECT_WAIT_MS).await;
            continue;
        };
        last_ask = Some(now);
        match session.manage_token(&call("manage", "token")).await {
            Ok(mut t) => {
                let skewed = t.expires_at_ms < now.saturating_add(SKEW_FLOOR_MS);
                platform::info(&format!(
                    "pharos: manage token outcome=ok expires_at_ms={} clock_skewed={skewed}",
                    t.expires_at_ms
                ));
                t.expires_at_ms = local_expiry(t.expires_at_ms, now);
                held.set(Some(t));
            }
            Err(e) if e.is_disconnected() => {
                platform::warn(&format!(
                    "pharos: manage token outcome=disconnected error={e}"
                ));
                platform::sleep_ms(RECONNECT_WAIT_MS).await;
            }
            Err(e) => {
                let refused = TokenState::refused(&e);
                let keep = live.is_some() && refused == Some(TokenState::Failed);
                platform::warn(&format!(
                    "pharos: manage token outcome=refused keeps_live_token={keep} error={e}"
                ));
                if keep {
                    continue;
                }
                held.set(None);
                match refused {
                    Some(next) => {
                        why.set(e.to_string());
                        state.set(next);
                    }
                    None => machine.write().apply(Event::NeedsSignIn),
                }
                return;
            }
        }
    }
}

#[component]
pub fn ManageScreen() -> Element {
    let ctx = use_context::<Ctx>();
    let account = use_context::<AccountMe>();
    let local = use_signal(|| None::<MediaToken>);
    let held = try_use_context::<HeldManageToken>().map_or(local, |h| h.0);
    let manage_tab = try_use_context::<ManageTab>();
    let initial_tab = use_hook(|| {
        manage_tab.and_then(|tab| {
            let mut tab = tab.0;
            let wanted = *tab.peek();
            tab.set(None);
            wanted
        })
    })
    .unwrap_or_default();
    let manage_focus = try_use_context::<ManageFocus>();
    let initial_focus = use_hook(|| {
        manage_focus.and_then(|focus| {
            let mut focus = focus.0;
            let wanted = focus.peek().clone();
            focus.set(None);
            wanted
        })
    });
    let flagged_shows = try_use_context::<FlaggedShows>();
    use_drop(move || {
        if let Some(flagged) = flagged_shows {
            let (mut list, mut asked) = (flagged.list, flagged.asked);
            list.set(None);
            asked.set(false);
        }
    });
    let state = use_signal(|| TokenState::Asking);
    let token = use_signal(String::new);
    let why = use_signal(String::new);
    let task = use_signal(|| None::<dioxus::core::Task>);
    let start = move || {
        let mut task = task;
        let next = spawn(keep_token(ctx.api, ctx.machine, held, state, token, why));
        if let Some(old) = task.replace(Some(next)) {
            old.cancel();
        }
    };
    use_hook(start);
    // A new connection may be another account (a sign-in in another tab):
    // the token this screen holds is the old session's, so drop it and ask
    // the new session before the pages send anything else (V880).
    let mut first = use_signal(|| true);
    use_effect(move || {
        let _ = ctx.api.read();
        if *first.peek() {
            first.set(false);
            return;
        }
        platform::info("pharos: manage token restart reason=connection_changed");
        let (mut held, mut token, mut state) = (held, token, state);
        held.set(None);
        token.set(String::new());
        state.set(TokenState::Asking);
        start();
    });
    // `None` while the lookup runs, `Some(None)` when it failed. The pages
    // wait for it: without the admin's own id their card would offer Delete
    // and the admin switch (V881).
    let me = account.0.read().clone();
    let me_id = me.clone().flatten().map(|me| me.id).unwrap_or_default();
    let current = *state.read();
    match current {
        TokenState::Asking => rsx! {
            div { class: "pharos-manage-gate", super::views::Loading {} }
        },
        TokenState::NotAdmin => rsx! {
            div { class: "pharos-manage-gate",
                h2 { "Manage is for admins" }
                p { class: "pharos-notice",
                    "Your account can't manage this server. Ask an admin if you need something changed."
                }
            }
        },
        TokenState::Failed => rsx! {
            div { class: "pharos-manage-gate",
                h2 { "Couldn't open Manage" }
                p { class: "pharos-error", "{why}" }
                button {
                    class: "pharos-button",
                    r#type: "button",
                    onclick: move |_| {
                        let mut state = state;
                        state.set(TokenState::Asking);
                        start();
                    },
                    "Try again"
                }
            }
        },
        TokenState::Held if matches!(me, Some(None)) => rsx! {
            div { class: "pharos-manage-gate",
                h2 { "Couldn't open Manage" }
                p { class: "pharos-error", "Couldn't find out who is signed in." }
                button {
                    class: "pharos-button",
                    r#type: "button",
                    onclick: move |_| {
                        let mut me = account.0;
                        me.restart();
                    },
                    "Try again"
                }
            }
        },
        TokenState::Held if me_id.is_empty() => rsx! {
            div { class: "pharos-manage-gate", super::views::Loading {} }
        },
        TokenState::Held => rsx! {
            AdminPane {
                access_token: token,
                server_base: platform::page_origin(),
                current_user_id: me_id,
                initial_tab,
                initial_focus,
            }
        },
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    const HOUR: u64 = 60 * 60 * 1000;

    #[test]
    fn the_next_token_is_asked_for_five_minutes_before_this_one_ends() {
        assert_eq!(
            next_ask_at(1_000 + HOUR, 1_000, None),
            Some(1_000 + HOUR - 5 * 60 * 1000)
        );
        assert_eq!(
            next_ask_at(1_000 + HOUR, 1_000 + HOUR - 5 * 60 * 1000, None),
            None
        );
    }

    #[test]
    fn a_token_near_its_end_waits_thirty_seconds_after_the_last_ask() {
        assert_eq!(
            next_ask_at(1_000 + 60_000, 1_000, Some(1_000)),
            Some(31_000)
        );
        assert_eq!(next_ask_at(1_000 + 60_000, 31_000, Some(1_000)), None);
    }

    #[test]
    fn a_device_clock_ahead_of_the_server_still_waits_before_asking_again() {
        // The server says the token ended an hour ago by this device's clock:
        // the page keeps it ten minutes and asks again at five, not at once.
        let now = 5 * HOUR;
        let exp = local_expiry(4 * HOUR, now);
        assert_eq!(exp, now + 10 * 60 * 1000);
        assert_eq!(next_ask_at(exp, now, Some(now)), Some(now + 5 * 60 * 1000));
        assert_eq!(local_expiry(now + HOUR, now), now + HOUR);
    }

    #[test]
    fn a_refused_refresh_retries_after_thirty_seconds_while_the_token_lives() {
        // Asked at the refresh point and refused: the held token still has
        // five minutes, so the next ask waits for the floor, not for ever.
        let exp = 10 * 60 * 1000;
        let asked = exp - 5 * 60 * 1000;
        assert_eq!(next_ask_at(exp, asked, Some(asked)), Some(asked + 30_000));
    }

    #[test]
    fn a_refusal_for_a_non_admin_says_so_and_a_revoked_one_signs_in() {
        let api = |kind: &'static str| ClientError::Api {
            kind,
            reason: String::new(),
            trace_id: String::new(),
        };
        assert_eq!(
            TokenState::refused(&api("notPermitted")),
            Some(TokenState::NotAdmin)
        );
        assert_eq!(TokenState::refused(&api("revoked")), None);
        assert_eq!(
            TokenState::refused(&api("unavailable")),
            Some(TokenState::Failed)
        );
    }
}
