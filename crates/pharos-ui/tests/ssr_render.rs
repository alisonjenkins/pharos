#![allow(clippy::unwrap_used, clippy::expect_used)]
//! T51 phase 1 — DOM-level snapshot tests via `dioxus_ssr`.
//!
//! Each view is rendered through a tiny root component inside a
//! `VirtualDom`, then `dioxus_ssr::render` serialises the result to
//! an HTML string we assert against. Hooks (`use_signal`, etc.) and
//! `EventHandler` closures both need the dioxus runtime to be
//! active, which means rsx! must be invoked inside a component body
//! — not directly in the `#[test]` fn. The `dom!` helper below wraps
//! that ceremony.

use dioxus::prelude::*;
use pharos_ui::api_types::{ItemKind, LibraryItem};
use pharos_ui::client::AdminUser;
use pharos_ui::client::{ItemDetail, SearchHint};
use pharos_ui::views::{
    AdminView, GroupMember, GroupSessionPanel, GroupSnapshot, ItemDetailView, LibraryView,
    LoginForm, PlayerView, SearchStatus, SearchView,
};

fn render_root(root: fn() -> Element) -> String {
    let mut dom = VirtualDom::new(root);
    dom.rebuild_in_place();
    dioxus_ssr::render(&dom)
}

// ---- LoginForm ---------------------------------------------------

fn login_form_no_error() -> Element {
    rsx! {
        LoginForm {
            on_submit: move |_| {},
            error: None,
        }
    }
}

fn login_form_with_error() -> Element {
    rsx! {
        LoginForm {
            on_submit: move |_| {},
            error: Some("invalid credentials".to_string()),
        }
    }
}

#[test]
fn login_form_renders_username_password_inputs_and_submit() {
    let html = render_root(login_form_no_error);
    assert!(html.contains("Sign in to pharos"), "{html}");
    assert!(html.contains(r#"type="text""#), "missing username input: {html}");
    assert!(
        html.contains(r#"type="password""#),
        "missing password input: {html}"
    );
    assert!(html.contains(r#"autocomplete="username""#), "{html}");
    assert!(html.contains(r#"autocomplete="current-password""#), "{html}");
    assert!(html.contains("Sign in</button>"), "{html}");
    assert!(!html.contains("pharos-error"), "error class leaked: {html}");
}

#[test]
fn login_form_renders_error_banner_when_present() {
    let html = render_root(login_form_with_error);
    assert!(html.contains("pharos-error"), "{html}");
    assert!(html.contains("invalid credentials"), "{html}");
}

// ---- LibraryView -------------------------------------------------

fn library_with_items() -> Element {
    let items = vec![
        LibraryItem {
            id: "1".into(),
            name: "Movie One".into(),
            kind: ItemKind::Movie,
        },
        LibraryItem {
            id: "5".into(),
            name: "Track Two".into(),
            kind: ItemKind::Audio,
        },
    ];
    rsx! {
        LibraryView {
            items: items,
            on_play: move |_: String| {},
        }
    }
}

fn library_empty() -> Element {
    rsx! {
        LibraryView {
            items: Vec::<LibraryItem>::new(),
            on_play: move |_: String| {},
        }
    }
}

#[test]
fn library_view_renders_tile_per_item() {
    let html = render_root(library_with_items);
    assert!(html.contains("Movie One"), "{html}");
    assert!(html.contains("Track Two"), "{html}");
}

#[test]
fn library_view_empty_state_renders_no_item_titles() {
    let html = render_root(library_empty);
    assert!(!html.contains("Movie One"), "{html}");
    assert!(!html.contains("Track Two"), "{html}");
}

// ---- PlayerView --------------------------------------------------

fn player_movie() -> Element {
    rsx! {
        PlayerView {
            item_id: "1".to_string(),
            kind: ItemKind::Movie,
            access_token: "tok".to_string(),
            server_base: "http://x".to_string(),
            on_event: move |_| {},
        }
    }
}

fn player_audio() -> Element {
    rsx! {
        PlayerView {
            item_id: "2".to_string(),
            kind: ItemKind::Audio,
            access_token: "tok".to_string(),
            server_base: "http://x".to_string(),
            on_event: move |_| {},
        }
    }
}

#[test]
fn player_view_renders_video_for_movie_kind() {
    let html = render_root(player_movie);
    assert!(html.contains("<video"), "video element missing: {html}");
    // Direct-play URL with api_key.
    assert!(html.contains("/Videos/1/stream"), "{html}");
    assert!(html.contains("api_key=tok"), "{html}");
}

#[test]
fn player_view_renders_audio_for_audio_kind() {
    let html = render_root(player_audio);
    assert!(html.contains("<audio"), "audio element missing: {html}");
    assert!(html.contains("/Audio/2/universal"), "{html}");
}

// ---- GroupSessionPanel ------------------------------------------

fn group_panel_two_members() -> Element {
    let snap = GroupSnapshot {
        group_id: Some("g-1".into()),
        members: vec![
            GroupMember {
                member_id: "m-1".into(),
                name: "ali".into(),
                is_leader: true,
                is_buffering: false,
            },
            GroupMember {
                member_id: "m-2".into(),
                name: "ben".into(),
                is_leader: false,
                is_buffering: false,
            },
        ],
    };
    rsx! {
        GroupSessionPanel {
            snapshot: snap,
            self_member_id: Some("m-1".to_string()),
            on_action: move |_| {},
        }
    }
}

#[test]
fn group_session_panel_renders_member_list() {
    let html = render_root(group_panel_two_members);
    assert!(html.contains("ali"), "leader name missing: {html}");
    assert!(html.contains("ben"), "second member missing: {html}");
}

// ---- AdminView --------------------------------------------------

fn admin_with_users() -> Element {
    let users = vec![
        AdminUser {
            id: "1".into(),
            name: "boss".into(),
            is_admin: true,
        },
        AdminUser {
            id: "2".into(),
            name: "alice".into(),
            is_admin: false,
        },
    ];
    rsx! {
        AdminView {
            users: users,
            current_user_id: "1".to_string(),
            status: None,
            on_action: move |_| {},
        }
    }
}

fn admin_with_status() -> Element {
    rsx! {
        AdminView {
            users: Vec::<AdminUser>::new(),
            current_user_id: "1".to_string(),
            status: Some("Created alice".to_string()),
            on_action: move |_| {},
        }
    }
}

#[test]
fn admin_view_renders_user_rows_with_self_delete_disabled() {
    let html = render_root(admin_with_users);
    assert!(html.contains("boss"), "{html}");
    assert!(html.contains("alice"), "{html}");
    assert!(html.contains("(admin)"), "{html}");
    // The current user (id=1) renders a `you` button (disabled),
    // the other renders a `Delete` button (enabled).
    assert!(html.contains(">you<"), "self-delete not rendered as 'you': {html}");
    assert!(html.contains(">Delete<"), "{html}");
}

#[test]
fn admin_view_status_banner_renders_when_present() {
    let html = render_root(admin_with_status);
    assert!(html.contains("pharos-admin-status"), "{html}");
    assert!(html.contains("Created alice"), "{html}");
}

// ---- SearchView -------------------------------------------------

fn search_idle_with_hits() -> Element {
    let hits = vec![
        SearchHint {
            id: "1".into(),
            name: "Blade Runner".into(),
            kind: ItemKind::Movie,
            matched_term: "blade".into(),
        },
        SearchHint {
            id: "2".into(),
            name: "Vangelis - Tales".into(),
            kind: ItemKind::Audio,
            matched_term: "vang".into(),
        },
    ];
    rsx! {
        SearchView {
            query: "bl".to_string(),
            hits: hits,
            status: SearchStatus::Idle,
            on_query: move |_| {},
            on_play: move |_| {},
        }
    }
}

fn search_loading() -> Element {
    rsx! {
        SearchView {
            query: "bl".to_string(),
            hits: Vec::<SearchHint>::new(),
            status: SearchStatus::Loading,
            on_query: move |_| {},
            on_play: move |_| {},
        }
    }
}

fn search_empty() -> Element {
    rsx! {
        SearchView {
            query: "nope".to_string(),
            hits: Vec::<SearchHint>::new(),
            status: SearchStatus::Empty,
            on_query: move |_| {},
            on_play: move |_| {},
        }
    }
}

fn search_error() -> Element {
    rsx! {
        SearchView {
            query: "x".to_string(),
            hits: Vec::<SearchHint>::new(),
            status: SearchStatus::Error("network down".into()),
            on_query: move |_| {},
            on_play: move |_| {},
        }
    }
}

#[test]
fn search_view_renders_input_and_grouped_hits() {
    let html = render_root(search_idle_with_hits);
    assert!(html.contains(r#"type="search""#), "{html}");
    assert!(html.contains("Blade Runner"), "{html}");
    assert!(html.contains("Vangelis - Tales"), "{html}");
    // Group headings present for both kinds.
    assert!(html.contains(">Video<"), "{html}");
    assert!(html.contains(">Audio<"), "{html}");
}

#[test]
fn search_view_loading_branch_renders_indicator() {
    let html = render_root(search_loading);
    assert!(html.contains("pharos-search-loading"), "{html}");
    assert!(html.contains("Searching"), "{html}");
}

#[test]
fn search_view_empty_branch_renders_empty_text() {
    let html = render_root(search_empty);
    assert!(html.contains("pharos-empty"), "{html}");
    assert!(html.contains("No matches"), "{html}");
}

#[test]
fn search_view_error_branch_renders_error_class() {
    let html = render_root(search_error);
    assert!(html.contains("pharos-error"), "{html}");
    assert!(html.contains("network down"), "{html}");
}

// ---- ItemDetailView ---------------------------------------------

fn detail_unplayed_no_position() -> Element {
    rsx! {
        ItemDetailView {
            detail: ItemDetail {
                id: "1".into(),
                name: "Blade Runner".into(),
                kind: ItemKind::Movie,
                run_time_ticks: 117 * 60 * 10_000_000,
                played: false,
                play_count: 0,
                is_favorite: false,
                playback_position_ticks: 0,
            },
            error: None,
            on_action: move |_| {},
        }
    }
}

fn detail_resumable_played_favorite() -> Element {
    rsx! {
        ItemDetailView {
            detail: ItemDetail {
                id: "2".into(),
                name: "The Expanse - S01E01".into(),
                kind: ItemKind::Episode,
                run_time_ticks: 45 * 60 * 10_000_000,
                played: false,
                play_count: 3,
                is_favorite: true,
                playback_position_ticks: 30 * 60 * 10_000_000,
            },
            error: None,
            on_action: move |_| {},
        }
    }
}

#[test]
fn detail_view_renders_title_runtime_and_play_button() {
    let html = render_root(detail_unplayed_no_position);
    assert!(html.contains("Blade Runner"), "{html}");
    assert!(html.contains("1h 57m"), "runtime missing: {html}");
    assert!(html.contains("pharos-detail-play"), "{html}");
    // No resume — Play (not Resume).
    assert!(html.contains(">Play<"), "{html}");
    assert!(!html.contains("Resume from"), "{html}");
    // Unplayed + non-favourite states.
    assert!(html.contains("Mark played"), "{html}");
    assert!(html.contains("☆ Favourite"), "{html}");
}

#[test]
fn detail_view_resume_button_renders_when_positionticks_set() {
    let html = render_root(detail_resumable_played_favorite);
    assert!(html.contains(">Resume<"), "{html}");
    assert!(html.contains("Resume from 30m"), "{html}");
    // Favourite-on state.
    assert!(html.contains("★ Favourite"), "{html}");
    // play_count display.
    assert!(html.contains("pharos-detail-playcount"), "{html}");
    assert!(html.contains(">3<"), "{html}");
}
