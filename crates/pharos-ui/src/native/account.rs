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
/// hidden.
#[component]
pub fn AccountMenu(me: Option<Me>, on_sign_out: EventHandler<()>) -> Element {
    let mut open = use_signal(|| false);
    rsx! {
        AccountMenuView {
            me,
            open: open(),
            on_toggle: move |_| open.toggle(),
            on_close: move |_| open.set(false),
            on_sign_out,
        }
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
                if open && evt.key() == Key::Escape {
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
                    MenuLink { to: Route::Account, "Account" }
                    MenuLink { to: Route::Appearance, "Appearance" }
                    if is_admin {
                        MenuLink { to: Route::Manage, "Manage" }
                    }
                    button {
                        class: "pharos-account-item",
                        r#type: "button",
                        role: "menuitem",
                        onclick: move |_| on_sign_out.call(()),
                        "Sign out"
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
        let mut dom = VirtualDom::new_with_props(
            |(me, open): (Option<Me>, bool)| {
                use_context_provider(|| Nav(EventHandler::new(|_: Route| {})));
                rsx! {
                    AccountMenuView {
                        me,
                        open,
                        on_toggle: |_| {},
                        on_close: |_| {},
                        on_sign_out: |_| {},
                    }
                }
            },
            (me, open),
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
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
        assert!(!html.contains("Appearance"), "{html}");
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
        for item in ["Account", "Appearance", "Manage", "Sign out"] {
            assert!(html.contains(item), "{item}: {html}");
        }
    }

    #[test]
    fn a_user_who_is_not_an_admin_does_not_see_manage() {
        let html = render(me("guest", false), true);
        assert!(html.contains("Appearance"), "{html}");
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
