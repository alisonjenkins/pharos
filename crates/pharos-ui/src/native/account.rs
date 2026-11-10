//! The account button at the right of the top bar and the menu it opens:
//! the user's initials, then Account, Appearance, Manage (admins only) and
//! Sign out.

use dioxus::prelude::*;
use pharos_client::api::Me;

use super::route::Route;
use super::views::Nav;

/// Up to two letters for a name: the first letter of its first and last
/// words, upper-cased. A name with no letters at all gives `?`.
pub fn initials(name: &str) -> String {
    let mut words = name.split_whitespace();
    let first = words.next().and_then(|w| w.chars().next());
    let last = words.last().and_then(|w| w.chars().next());
    let letters: String = first
        .into_iter()
        .chain(last)
        .flat_map(char::to_uppercase)
        .collect();
    if letters.is_empty() {
        "?".to_owned()
    } else {
        letters
    }
}

/// The account button and its menu, open state kept here. `me` is `None`
/// until `Session.me` answers; the button then shows `?` and Manage stays
/// hidden. `kept_books` counts the books kept on this device, read as the
/// menu opens: signing out removes them, so Sign out asks first (FR-703).
#[component]
pub fn AccountMenu(
    me: Option<Me>,
    kept_books: Callback<(), Option<usize>>,
    on_sign_out: EventHandler<()>,
) -> Element {
    let mut open = use_signal(|| false);
    let mut kept = use_signal(|| Some(0usize));
    let mut confirming = use_signal(|| false);
    rsx! {
        AccountMenuView {
            me,
            open: open(),
            kept_books: kept(),
            confirming: confirming(),
            on_toggle: move |_| {
                if !*open.peek() {
                    kept.set(kept_books.call(()));
                }
                confirming.set(false);
                open.toggle();
            },
            on_close: move |_| {
                confirming.set(false);
                open.set(false);
            },
            on_confirm: move |ask: bool| confirming.set(ask),
            on_sign_out: move |()| {
                confirming.set(false);
                open.set(false);
                on_sign_out.call(());
            },
        }
    }
}

/// What Sign out says before removing `count` kept books; `None` when the
/// record could not be read, which may still leave books in the cache.
pub fn sign_out_warning(count: Option<usize>) -> String {
    match count {
        Some(1) => "Signing out removes the book kept on this device. You will need a connection to read it again.".to_owned(),
        Some(n) => format!("Signing out removes the {n} books kept on this device. You will need a connection to read them again."),
        None => "Signing out removes any books kept on this device. You will need a connection to read them again.".to_owned(),
    }
}

/// [`AccountMenu`] with the open state passed in, so tests render it open.
#[component]
pub fn AccountMenuView(
    me: Option<Me>,
    open: bool,
    on_toggle: EventHandler<()>,
    on_close: EventHandler<()>,
    on_sign_out: EventHandler<()>,
    /// Books kept on this device; with any, Sign out asks first.
    #[props(default = Some(0))]
    kept_books: Option<usize>,
    /// Sign out was pressed once and is asking.
    #[props(default)]
    confirming: bool,
    /// `true` to ask, `false` to cancel the ask.
    #[props(default)]
    on_confirm: EventHandler<bool>,
) -> Element {
    let name = me.as_ref().map(|me| me.name.clone()).unwrap_or_default();
    let is_admin = me.as_ref().is_some_and(|me| me.is_admin);
    // The name stays out of the button's accessible name: a name such as
    // "playwright" would otherwise match a search for a "Play" button.
    let title = if name.is_empty() {
        "Account".to_owned()
    } else {
        format!("Account: {name}")
    };
    rsx! {
        div {
            class: "pharos-account",
            onkeydown: move |evt: KeyboardEvent| {
                if open && evt.key() == Key::Named(NamedKey::Escape) {
                    on_close.call(());
                }
            },
            button {
                class: "pharos-account-button",
                r#type: "button",
                aria_label: "Account",
                title: "{title}",
                aria_haspopup: "menu",
                aria_expanded: "{open}",
                aria_controls: MENU_ID,
                onclick: move |_| on_toggle.call(()),
                Avatar { me: me.clone() }
            }
            if open {
                // Catches a click anywhere else on the page and closes the menu.
                div { class: "pharos-account-scrim", onclick: move |_| on_close.call(()) }
                div {
                    id: MENU_ID,
                    class: "pharos-account-menu",
                    role: "menu",
                    aria_label: "Account",
                    onclick: move |_| on_close.call(()),
                    if !name.is_empty() {
                        p { class: "pharos-account-name", "{name}" }
                    }
                    // One way into the settings pages (Appearance, Playback,
                    // Account, Devices, Privacy); their sidebar does the rest.
                    MenuLink { to: Route::Appearance, "Settings" }
                    MenuLink { to: Route::ApproveDevice, "Approve a device" }
                    if is_admin {
                        MenuLink { to: Route::Manage, "Manage" }
                    }
                    if confirming {
                        div {
                            class: "pharos-account-confirm",
                            role: "group",
                            aria_label: "Sign out",
                            // A click on the warning must not close the menu.
                            onclick: move |e: MouseEvent| e.stop_propagation(),
                            p { class: "pharos-account-warning", "{sign_out_warning(kept_books)}" }
                            button {
                                class: "pharos-account-item pharos-account-danger",
                                r#type: "button",
                                role: "menuitem",
                                // The pressed Sign out is gone: keep keyboard focus in the ask.
                                onmounted: move |e: MountedEvent| async move {
                                    let _ = e.set_focus(true).await;
                                },
                                onclick: move |e: MouseEvent| {
                                    e.stop_propagation();
                                    on_sign_out.call(());
                                },
                                "Sign out and remove"
                            }
                            button {
                                class: "pharos-account-item",
                                r#type: "button",
                                role: "menuitem",
                                onclick: move |e: MouseEvent| {
                                    e.stop_propagation();
                                    on_confirm.call(false);
                                },
                                "Cancel"
                            }
                        }
                    } else {
                        button {
                            class: "pharos-account-item",
                            r#type: "button",
                            role: "menuitem",
                            onclick: move |e: MouseEvent| {
                                if kept_books != Some(0) {
                                    e.stop_propagation();
                                    on_confirm.call(true);
                                } else {
                                    on_sign_out.call(());
                                }
                            },
                            "Sign out"
                        }
                    }
                }
            }
        }
    }
}

const MENU_ID: &str = "account-menu";

/// The user's picture when they have one, else their initials. Decorative:
/// whatever holds it names the account.
#[component]
pub fn Avatar(me: Option<Me>) -> Element {
    let name = me.as_ref().map(|me| me.name.as_str()).unwrap_or_default();
    match me.as_ref().and_then(|me| me.avatar_url.as_deref()) {
        Some(url) => rsx! {
            img { class: "pharos-avatar pharos-avatar-picture", src: "{url}", alt: "", "aria-hidden": "true" }
        },
        None => rsx! {
            span { class: "pharos-avatar", "aria-hidden": "true", {initials(name)} }
        },
    }
}

/// A menu item that navigates in place, as [`super::views::Link`] does.
#[component]
fn MenuLink(to: Route, children: Element) -> Element {
    let nav = use_context::<Nav>();
    let href = to.path();
    rsx! {
        a {
            href: "{href}",
            class: "pharos-account-item",
            role: "menuitem",
            onclick: move |evt: MouseEvent| {
                if evt.modifiers().is_empty() {
                    evt.prevent_default();
                    nav.0.call(to.clone());
                }
            },
            {children}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(me: Option<Me>, open: bool) -> String {
        render_kept(me, open, Some(0), false)
    }

    fn render_kept(
        me: Option<Me>,
        open: bool,
        kept_books: Option<usize>,
        confirming: bool,
    ) -> String {
        let mut dom = VirtualDom::new_with_props(
            |(me, open, kept_books, confirming): (Option<Me>, bool, Option<usize>, bool)| {
                use_context_provider(|| Nav(EventHandler::new(|_: Route| {})));
                rsx! {
                    AccountMenuView {
                        me,
                        open,
                        kept_books,
                        confirming,
                        on_toggle: |_| {},
                        on_close: |_| {},
                        on_sign_out: |_| {},
                    }
                }
            },
            (me, open, kept_books, confirming),
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    /// FR-703: with books kept, the asking menu names their count and offers
    /// Cancel. Disarm check: rendering the plain Sign out while confirming
    /// fails the first assertion.
    #[test]
    fn sign_out_asks_first_naming_the_kept_books() {
        let html = render_kept(me("ali", false), true, Some(3), true);
        assert!(
            html.contains("Signing out removes the 3 books kept on this device."),
            "{html}"
        );
        assert!(html.contains(">Sign out and remove<"), "{html}");
        assert!(html.contains(">Cancel<"), "{html}");
        let one = render_kept(me("ali", false), true, Some(1), true);
        assert!(
            one.contains("removes the book kept on this device"),
            "{one}"
        );
        // An unreadable record may still leave books cached: ask anyway.
        let unknown = render_kept(me("ali", false), true, None, true);
        assert!(
            unknown.contains("removes any books kept on this device"),
            "{unknown}"
        );
    }

    #[test]
    fn with_nothing_kept_sign_out_is_one_step() {
        let html = render_kept(me("ali", false), true, Some(0), false);
        assert!(html.contains(">Sign out<"), "{html}");
        assert!(!html.contains("Signing out removes"), "{html}");
    }

    fn me(name: &str, is_admin: bool) -> Option<Me> {
        Some(Me {
            id: "0123".into(),
            name: name.into(),
            is_admin,
            avatar_url: None,
        })
    }

    #[test]
    fn initials_take_the_first_and_last_words() {
        assert_eq!(initials("alison jenkins"), "AJ");
        assert_eq!(initials("  Mary Ann  Smith "), "MS");
        assert_eq!(initials("ali"), "A");
        assert_eq!(initials("élodie"), "É");
        assert_eq!(initials(""), "?");
        assert_eq!(initials("   "), "?");
    }

    #[test]
    fn the_menu_starts_closed_with_the_initials_on_the_button() {
        let html = render(me("alison jenkins", true), false);
        assert!(html.contains(">AJ<"), "{html}");
        assert!(html.contains("aria-expanded=\"false\""), "{html}");
        assert!(!html.contains("Sign out"), "{html}");
        assert!(!html.contains("Settings"), "{html}");
    }

    #[test]
    fn a_user_with_an_avatar_sees_it_on_the_button() {
        let mut with = me("alison jenkins", false);
        if let Some(me) = &mut with {
            me.avatar_url = Some("/UserImage?userId=0123&tag=ab".into());
        }
        let html = render(with, false);
        assert!(
            html.contains("src=\"/UserImage?userId=0123&#38;tag=ab\""),
            "{html}"
        );
        assert!(!html.contains(">AJ<"), "{html}");
    }

    #[test]
    fn an_admin_sees_manage() {
        let html = render(me("ali", true), true);
        assert!(html.contains("aria-expanded=\"true\""), "{html}");
        for item in ["Settings", "Approve a device", "Manage", "Sign out"] {
            assert!(html.contains(item), "{item}: {html}");
        }
    }

    #[test]
    fn a_user_who_is_not_an_admin_does_not_see_manage() {
        let html = render(me("guest", false), true);
        assert!(html.contains("Approve a device"), "{html}");
        assert!(html.contains("Settings"), "{html}");
        assert!(html.contains("Sign out"), "{html}");
        assert!(!html.contains("Manage"), "{html}");
    }

    #[test]
    fn before_me_answers_the_button_shows_a_question_mark_and_no_manage() {
        let html = render(None, true);
        assert!(html.contains(">?<"), "{html}");
        assert!(!html.contains("Manage"), "{html}");
        assert!(html.contains("Sign out"), "{html}");
    }
}
