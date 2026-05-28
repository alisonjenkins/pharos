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
use pharos_ui::client::{
    ActivityEntry, AdminUser, ApiKey, BrandingConfig, DeviceEntry, ItemChapter, ItemDetail,
    LibraryFolder, LiveChannel, LiveProgram, LocalizationCulture, LogEntry, PluginEntry,
    QuickConnectInitiate, RemoteSession, ScheduledTask, SearchHint, UserConfiguration,
};
use pharos_ui::views::{
    AdminTab, AdminView, GroupMember, GroupSessionPanel, GroupSnapshot, ItemDetailView,
    LibraryView, LiveTvStatus, LiveTvView, LoginForm, PlayerView, PrefsTab, PrefsView,
    QuickConnectAuthorizeView, QuickConnectGuestStatus, QuickConnectGuestView, RemoteControlView,
    SavedServer, SearchStatus, SearchView, ServerPickerView,
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
    assert!(
        html.contains(r#"type="text""#),
        "missing username input: {html}"
    );
    assert!(
        html.contains(r#"type="password""#),
        "missing password input: {html}"
    );
    assert!(html.contains(r#"autocomplete="username""#), "{html}");
    assert!(
        html.contains(r#"autocomplete="current-password""#),
        "{html}"
    );
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

fn player_movie_with_quality() -> Element {
    use pharos_ui::views::QualityOption;
    rsx! {
        PlayerView {
            item_id: "10".to_string(),
            kind: ItemKind::Movie,
            access_token: "tok".to_string(),
            server_base: "http://x".to_string(),
            quality_options: vec![
                QualityOption { label: "Auto".into(), max_bitrate: 0 },
                QualityOption { label: "1080p · 8 Mbps".into(), max_bitrate: 8_000_000 },
                QualityOption { label: "720p · 4 Mbps".into(), max_bitrate: 4_000_000 },
            ],
            current_max_bitrate: Some(4_000_000),
            on_event: move |_| {},
        }
    }
}

fn player_movie_with_chapters() -> Element {
    // 100-minute movie (60_000_000_000 ticks). Three chapter markers
    // — at start, halfway, and 75%.
    let run_time_ticks: u64 = 60_000_000_000;
    rsx! {
        PlayerView {
            item_id: "42".to_string(),
            kind: ItemKind::Movie,
            access_token: "tok".to_string(),
            server_base: "http://x".to_string(),
            run_time_ticks: run_time_ticks,
            chapters: vec![
                ItemChapter { name: "Opening".into(), start_position_ticks: 0 },
                ItemChapter { name: "Twist".into(), start_position_ticks: run_time_ticks / 2 },
                ItemChapter { name: "Climax".into(), start_position_ticks: run_time_ticks * 3 / 4 },
            ],
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

fn player_movie_with_tracks() -> Element {
    use pharos_ui::api_types::{MediaTrack, PlaybackTracks};
    rsx! {
        PlayerView {
            item_id: "3".to_string(),
            kind: ItemKind::Movie,
            access_token: "tok".to_string(),
            server_base: "http://x".to_string(),
            tracks: PlaybackTracks {
                audio: vec![
                    MediaTrack {
                        index: 1,
                        language: Some("eng".into()),
                        title: Some("English".into()),
                        ..Default::default()
                    },
                    MediaTrack {
                        index: 2,
                        language: Some("jpn".into()),
                        title: Some("Japanese".into()),
                        ..Default::default()
                    },
                ],
                subtitle: vec![MediaTrack {
                    index: 2,
                    language: Some("eng".into()),
                    title: Some("English".into()),
                    delivery_url: Some("/Videos/3/3/Subtitles/2/Stream.vtt".into()),
                    is_default: true,
                    ..Default::default()
                }],
            },
            on_event: move |_| {},
        }
    }
}

#[test]
fn player_view_renders_subtitle_track_when_tracks_supplied() {
    let html = render_root(player_movie_with_tracks);
    // Native <track> for the browser CC picker.
    assert!(html.contains("<track"), "{html}");
    assert!(html.contains("Subtitles/2/Stream.vtt"), "{html}");
    // Pharos-side OSD picker also renders.
    assert!(html.contains("pharos-player-osd"), "{html}");
    assert!(html.contains("pharos-player-subtitles"), "{html}");
    // T57 phase 3 — subtitle picker is interactive (Off + per-track).
    assert!(html.contains("pharos-player-subtitle-pick"), "{html}");
    assert!(html.contains("pharos-player-subtitle-off"), "{html}");
    assert!(html.contains(">Off<"), "{html}");
    // Audio picker now renders one button per track.
    assert!(html.contains("pharos-player-audio-pick"), "{html}");
    assert!(html.contains("English (eng)"), "{html}");
    assert!(html.contains("Japanese (jpn)"), "{html}");
}

#[test]
fn player_view_renders_audio_for_audio_kind() {
    let html = render_root(player_audio);
    assert!(html.contains("<audio"), "audio element missing: {html}");
    assert!(html.contains("/Audio/2/universal"), "{html}");
    // Audio kind exposes the minimise toggle.
    assert!(html.contains("pharos-player-minimise"), "{html}");
}

#[test]
fn player_view_renders_chapter_strip_with_pct_positions() {
    let html = render_root(player_movie_with_chapters);
    assert!(html.contains("pharos-player-chapters"), "{html}");
    assert!(html.contains("pharos-player-chapter"), "{html}");
    assert!(html.contains("Opening"), "{html}");
    assert!(html.contains("Twist"), "{html}");
    assert!(html.contains("Climax"), "{html}");
    // Marker positions: 0%, 50%, 75%.
    assert!(html.contains("left: 0.00%"), "{html}");
    assert!(html.contains("left: 50.00%"), "{html}");
    assert!(html.contains("left: 75.00%"), "{html}");
}

#[test]
fn player_view_renders_quality_picker_and_fullscreen() {
    let html = render_root(player_movie_with_quality);
    assert!(html.contains("pharos-player-quality"), "{html}");
    assert!(html.contains("<select"), "{html}");
    assert!(html.contains(r#"value="4000000""#), "{html}");
    // All three options rendered.
    assert!(html.contains("Auto"), "{html}");
    assert!(html.contains("1080p · 8 Mbps"), "{html}");
    assert!(html.contains("720p · 4 Mbps"), "{html}");
    // Fullscreen button.
    assert!(html.contains("pharos-player-fullscreen"), "{html}");
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
    assert!(
        html.contains(">you<"),
        "self-delete not rendered as 'you': {html}"
    );
    assert!(html.contains(">Delete<"), "{html}");
    // T50 phase 2: admin-toggle checkbox + password reset form per row.
    assert!(html.contains("pharos-admin-user-policy"), "{html}");
    assert!(html.contains("pharos-admin-user-reset"), "{html}");
    // Self-row's reset input is disabled (points user at prefs view).
    // Non-self row carries the standard placeholder.
    assert!(html.contains("Reset password"), "{html}");
    assert!(html.contains("Reset (current pw needed)"), "{html}");
}

#[test]
fn admin_view_status_banner_renders_when_present() {
    let html = render_root(admin_with_status);
    assert!(html.contains("pharos-admin-status"), "{html}");
    assert!(html.contains("Created alice"), "{html}");
}

fn admin_libraries_tab() -> Element {
    rsx! {
        AdminView {
            users: Vec::<AdminUser>::new(),
            current_user_id: "1".to_string(),
            status: None,
            active_tab: AdminTab::Libraries,
            libraries: vec![
                LibraryFolder {
                    item_id: "lib-1".into(),
                    name: "Movies".into(),
                    collection_type: "movies".into(),
                    locations: vec!["/data/movies".into()],
                },
                LibraryFolder {
                    item_id: "lib-2".into(),
                    name: "Shows".into(),
                    collection_type: "tvshows".into(),
                    locations: vec!["/data/tv".into(), "/mnt/tv".into()],
                },
            ],
            on_action: move |_| {},
        }
    }
}

fn admin_devices_tab() -> Element {
    rsx! {
        AdminView {
            users: Vec::<AdminUser>::new(),
            current_user_id: "1".to_string(),
            status: None,
            active_tab: AdminTab::Devices,
            devices: vec![DeviceEntry {
                id: "d1".into(),
                name: "Pixel 9".into(),
                app_name: "Finamp".into(),
                last_user_name: "ali".into(),
            }],
            on_action: move |_| {},
        }
    }
}

fn admin_activity_tab_empty() -> Element {
    rsx! {
        AdminView {
            users: Vec::<AdminUser>::new(),
            current_user_id: "1".to_string(),
            status: None,
            active_tab: AdminTab::Activity,
            activity: Vec::<ActivityEntry>::new(),
            on_action: move |_| {},
        }
    }
}

#[test]
fn admin_view_libraries_tab_renders_library_list() {
    let html = render_root(admin_libraries_tab);
    assert!(html.contains(r#"data-tab="libraries""#), "{html}");
    assert!(html.contains("pharos-admin-section-libraries"), "{html}");
    assert!(html.contains("Movies"), "{html}");
    assert!(html.contains("Shows"), "{html}");
    assert!(html.contains("/data/tv"), "{html}");
    assert!(html.contains("tvshows"), "{html}");
}

#[test]
fn admin_view_devices_tab_renders_table_rows() {
    let html = render_root(admin_devices_tab);
    assert!(html.contains(r#"data-tab="devices""#), "{html}");
    assert!(html.contains("pharos-admin-section-devices"), "{html}");
    assert!(html.contains("<table"), "{html}");
    assert!(html.contains("Pixel 9"), "{html}");
    assert!(html.contains("Finamp"), "{html}");
}

#[test]
fn admin_view_activity_tab_renders_empty_state() {
    let html = render_root(admin_activity_tab_empty);
    assert!(html.contains(r#"data-tab="activity""#), "{html}");
    assert!(html.contains("pharos-admin-section-activity"), "{html}");
    assert!(html.contains("No activity recorded"), "{html}");
}

fn admin_scheduled_tasks_tab() -> Element {
    rsx! {
        AdminView {
            users: Vec::<AdminUser>::new(),
            current_user_id: "1".to_string(),
            status: None,
            active_tab: AdminTab::ScheduledTasks,
            scheduled_tasks: vec![ScheduledTask {
                id: "t1".into(),
                name: "Library scan".into(),
                category: "Library".into(),
                state: "Idle".into(),
                last_execution_iso: "2026-05-28T05:00:00Z".into(),
            }],
            on_action: move |_| {},
        }
    }
}

fn admin_plugins_tab_empty() -> Element {
    rsx! {
        AdminView {
            users: Vec::<AdminUser>::new(),
            current_user_id: "1".to_string(),
            status: None,
            active_tab: AdminTab::Plugins,
            plugins: Vec::<PluginEntry>::new(),
            on_action: move |_| {},
        }
    }
}

fn admin_logs_tab() -> Element {
    rsx! {
        AdminView {
            users: Vec::<AdminUser>::new(),
            current_user_id: "1".to_string(),
            status: None,
            active_tab: AdminTab::Logs,
            logs: vec![LogEntry {
                name: "pharos.log".into(),
                size_bytes: 12345,
                date_modified_iso: "2026-05-28T07:00:00Z".into(),
            }],
            on_action: move |_| {},
        }
    }
}

#[test]
fn admin_view_scheduled_tasks_tab_renders_table() {
    let html = render_root(admin_scheduled_tasks_tab);
    assert!(html.contains(r#"data-tab="scheduledtasks""#), "{html}");
    assert!(html.contains("pharos-admin-section-scheduledtasks"), "{html}");
    assert!(html.contains("Library scan"), "{html}");
    assert!(html.contains("pharos-admin-task-state"), "{html}");
    assert!(html.contains(">Idle<"), "{html}");
    assert!(html.contains("2026-05-28T05:00:00Z"), "{html}");
}

#[test]
fn admin_view_plugins_tab_renders_empty_state() {
    let html = render_root(admin_plugins_tab_empty);
    assert!(html.contains(r#"data-tab="plugins""#), "{html}");
    assert!(html.contains("pharos-admin-section-plugins"), "{html}");
    assert!(html.contains("No plugins installed"), "{html}");
}

#[test]
fn admin_view_logs_tab_renders_log_list() {
    let html = render_root(admin_logs_tab);
    assert!(html.contains(r#"data-tab="logs""#), "{html}");
    assert!(html.contains("pharos-admin-section-logs"), "{html}");
    assert!(html.contains("pharos.log"), "{html}");
    assert!(html.contains("12345"), "{html}");
}

fn admin_apikeys_with_new_secret() -> Element {
    rsx! {
        AdminView {
            users: Vec::<AdminUser>::new(),
            current_user_id: "1".to_string(),
            status: None,
            active_tab: AdminTab::ApiKeys,
            api_keys: vec![ApiKey {
                id: "apikey:cli".into(),
                app_name: "cli".into(),
                date_created_iso: "2026-05-28T08:00:00Z".into(),
            }],
            new_api_key_secret: Some("secret-token-xyz".to_string()),
            on_action: move |_| {},
        }
    }
}

fn admin_apikeys_empty() -> Element {
    rsx! {
        AdminView {
            users: Vec::<AdminUser>::new(),
            current_user_id: "1".to_string(),
            status: None,
            active_tab: AdminTab::ApiKeys,
            api_keys: Vec::<ApiKey>::new(),
            on_action: move |_| {},
        }
    }
}

#[test]
fn admin_view_apikeys_tab_surfaces_new_secret_and_revoke_button() {
    let html = render_root(admin_apikeys_with_new_secret);
    assert!(html.contains(r#"data-tab="apikeys""#), "{html}");
    assert!(html.contains("pharos-admin-section-apikeys"), "{html}");
    assert!(html.contains("pharos-admin-apikey-new"), "{html}");
    assert!(html.contains("secret-token-xyz"), "{html}");
    assert!(html.contains("pharos-admin-apikey-revoke"), "{html}");
    assert!(html.contains("cli"), "{html}");
    assert!(html.contains("2026-05-28T08:00:00Z"), "{html}");
}

fn admin_branding_tab() -> Element {
    rsx! {
        AdminView {
            users: Vec::<AdminUser>::new(),
            current_user_id: "1".to_string(),
            status: None,
            active_tab: AdminTab::Branding,
            branding: BrandingConfig {
                server_name: "My Pharos".into(),
                login_disclaimer: "Welcome aboard".into(),
                custom_css: "body{}".into(),
            },
            on_action: move |_| {},
        }
    }
}

#[test]
fn admin_view_branding_tab_renders_populated_form() {
    let html = render_root(admin_branding_tab);
    assert!(html.contains(r#"data-tab="branding""#), "{html}");
    assert!(html.contains("pharos-admin-section-branding"), "{html}");
    assert!(html.contains(r#"value="My Pharos""#), "{html}");
    assert!(html.contains("Welcome aboard"), "{html}");
    assert!(html.contains("pharos-admin-branding-save"), "{html}");
}

#[test]
fn admin_view_apikeys_tab_renders_empty_state_and_create_form() {
    let html = render_root(admin_apikeys_empty);
    assert!(html.contains("No API keys issued"), "{html}");
    assert!(html.contains("pharos-admin-apikey-create"), "{html}");
    // No new-secret banner when none queued.
    assert!(!html.contains("pharos-admin-apikey-new"), "{html}");
}

// ---- ServerPickerView ------------------------------------------

fn server_picker_with_two_servers() -> Element {
    rsx! {
        ServerPickerView {
            saved: vec![
                SavedServer {
                    server_id: "srv-1".into(),
                    base_url: "https://home.example.com".into(),
                    name: "Home".into(),
                    last_user_name: "ali".into(),
                },
                SavedServer {
                    server_id: "srv-2".into(),
                    base_url: "https://work.example.com".into(),
                    name: "Work".into(),
                    last_user_name: "".into(),
                },
            ],
            default_url: "https://home.example.com".to_string(),
            status: None,
            on_action: move |_| {},
        }
    }
}

fn server_picker_empty() -> Element {
    rsx! {
        ServerPickerView {
            saved: Vec::<SavedServer>::new(),
            default_url: "https://pharos.local".to_string(),
            status: Some("Couldn't reach https://broken.example".to_string()),
            on_action: move |_| {},
        }
    }
}

#[test]
fn server_picker_renders_saved_servers_and_default_url() {
    let html = render_root(server_picker_with_two_servers);
    assert!(html.contains("Select server"), "{html}");
    assert!(html.contains("Home"), "{html}");
    assert!(html.contains("Work"), "{html}");
    assert!(html.contains("home.example.com"), "{html}");
    assert!(html.contains("Last: ali"), "{html}");
    // "Forget" buttons render per row.
    assert!(html.contains(">Forget<"), "{html}");
    // Default URL pre-fills the add input.
    assert!(
        html.contains(r#"value="https://home.example.com""#),
        "{html}"
    );
}

fn remote_with_sessions() -> Element {
    let sessions = vec![
        RemoteSession {
            id: "s1".into(),
            user_id: "u1".into(),
            user_name: "ali".into(),
            device_name: "Pixel 9".into(),
            client: "Finamp".into(),
            now_playing_item_id: Some("item-9".into()),
            position_ticks: 0,
            is_paused: false,
        },
        RemoteSession {
            id: "s2".into(),
            user_id: "u1".into(),
            user_name: "ali".into(),
            device_name: "this browser".into(),
            client: "pharos-ui".into(),
            now_playing_item_id: None,
            position_ticks: 0,
            is_paused: false,
        },
    ];
    rsx! {
        RemoteControlView {
            sessions: sessions,
            self_session_id: Some("s2".to_string()),
            status: None,
            on_action: move |_| {},
        }
    }
}

fn remote_empty() -> Element {
    rsx! {
        RemoteControlView {
            sessions: Vec::<RemoteSession>::new(),
            self_session_id: None,
            status: None,
            on_action: move |_| {},
        }
    }
}

#[test]
fn remote_view_renders_sessions_and_hides_actions_on_self() {
    let html = render_root(remote_with_sessions);
    assert!(html.contains("pharos-remote-sessions"), "{html}");
    assert!(html.contains("Pixel 9"), "{html}");
    assert!(html.contains("Finamp"), "{html}");
    assert!(html.contains("playing item-9"), "{html}");
    // s1 (remote) has Pause/Play/Stop buttons.
    assert!(html.contains("pharos-remote-action-pause"), "{html}");
    assert!(html.contains("pharos-remote-action-stop"), "{html}");
    // s2 (self) renders with the self marker + no actions section.
    assert!(html.contains("pharos-remote-session-self"), "{html}");
    assert!(html.contains("this device"), "{html}");
    // Volume + seek controls render for remote sessions only.
    assert!(html.contains("pharos-remote-volume"), "{html}");
    assert!(html.contains("pharos-remote-seek"), "{html}");
    assert!(
        html.contains(r#"type="range""#),
        "volume slider missing: {html}"
    );
}

#[test]
fn remote_view_renders_empty_state() {
    let html = render_root(remote_empty);
    assert!(html.contains("pharos-remote-empty"), "{html}");
    assert!(html.contains("No active sessions"), "{html}");
}

#[test]
fn server_picker_renders_empty_state_with_status() {
    let html = render_root(server_picker_empty);
    assert!(html.contains("pharos-server-picker-empty"), "{html}");
    assert!(html.contains("No saved servers"), "{html}");
    assert!(html.contains("pharos-server-picker-status"), "{html}");
    assert!(
        html.contains("Couldn&#x27;t reach") || html.contains("Couldn't reach"),
        "{html}"
    );
    assert!(html.contains(r#"value="https://pharos.local""#), "{html}");
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
                ..Default::default()
            },
            error: None,
            primary_image_url: None,
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
                series_name: Some("The Expanse".into()),
                season_index: Some(1),
                episode_index: Some(1),
                ..Default::default()
            },
            error: None,
            primary_image_url: None,
            on_action: move |_| {},
        }
    }
}

fn detail_audio() -> Element {
    rsx! {
        ItemDetailView {
            detail: ItemDetail {
                id: "5".into(),
                name: "End Titles".into(),
                kind: ItemKind::Audio,
                run_time_ticks: 3 * 60 * 10_000_000,
                artists: vec!["Vangelis".into()],
                album: Some("Blade Runner OST".into()),
                album_artists: vec!["Vangelis".into()],
                ..Default::default()
            },
            error: None,
            primary_image_url: Some("/Items/5/Images/Primary?api_key=tok".into()),
            on_action: move |_| {},
        }
    }
}

fn detail_episode_breadcrumb_series_only() -> Element {
    rsx! {
        ItemDetailView {
            detail: ItemDetail {
                id: "9".into(),
                name: "Pilot".into(),
                kind: ItemKind::Episode,
                run_time_ticks: 60 * 60 * 10_000_000,
                series_name: Some("Andor".into()),
                season_index: None,
                episode_index: None,
                ..Default::default()
            },
            error: None,
            primary_image_url: None,
            on_action: move |_| {},
        }
    }
}

fn detail_with_cast_overview_genres_backdrop() -> Element {
    use pharos_ui::client::ItemPerson;
    rsx! {
        ItemDetailView {
            detail: ItemDetail {
                id: "33".into(),
                name: "Blade Runner".into(),
                kind: ItemKind::Movie,
                run_time_ticks: 117 * 60 * 10_000_000,
                overview: Some("A blade runner hunts replicants.".into()),
                genres: vec!["Sci-Fi".into(), "Drama".into()],
                has_backdrop_image: true,
                people: vec![
                    ItemPerson {
                        id: "p1".into(),
                        name: "Harrison Ford".into(),
                        kind: "Actor".into(),
                        role: "Rick Deckard".into(),
                        has_image: true,
                    },
                    ItemPerson {
                        id: "p2".into(),
                        name: "Ridley Scott".into(),
                        kind: "Director".into(),
                        role: String::new(),
                        has_image: false,
                    },
                ],
                ..Default::default()
            },
            error: None,
            primary_image_url: None,
            backdrop_image_url: Some("/Items/33/Images/Backdrop?api_key=tok".to_string()),
            person_image_url_template: Some("/Items/{person_id}/Images/Primary".to_string()),
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
    // T54 phase 2: series breadcrumb + S/E.
    assert!(html.contains("pharos-detail-series"), "{html}");
    assert!(html.contains("The Expanse"), "{html}");
    assert!(html.contains("S01E01"), "{html}");
}

#[test]
fn detail_view_audio_renders_artist_album_and_primary_image() {
    let html = render_root(detail_audio);
    assert!(html.contains("pharos-detail-audio-meta"), "{html}");
    assert!(html.contains("Vangelis"), "{html}");
    assert!(html.contains("Blade Runner OST"), "{html}");
    // album_artists same as artists → no separate line.
    assert!(
        !html.contains("Album artist:"),
        "duplicate album artist line: {html}"
    );
    // Image figure rendered when primary_image_url set.
    assert!(html.contains("pharos-detail-primary"), "{html}");
    assert!(
        html.contains("/Items/5/Images/Primary?api_key=tok"),
        "{html}"
    );
}

#[test]
fn detail_view_episode_with_only_series_name_omits_se_label() {
    let html = render_root(detail_episode_breadcrumb_series_only);
    assert!(html.contains("pharos-detail-series"), "{html}");
    assert!(html.contains("Andor"), "{html}");
    // No S/E label when both indices missing.
    assert!(!html.contains("S00"), "{html}");
    assert!(!html.contains("pharos-detail-episode-index"), "{html}");
}

#[test]
fn detail_view_phase3_renders_backdrop_overview_genres_cast() {
    let html = render_root(detail_with_cast_overview_genres_backdrop);
    // Backdrop image
    assert!(html.contains("pharos-detail-backdrop"), "{html}");
    assert!(
        html.contains("/Items/33/Images/Backdrop?api_key=tok"),
        "{html}"
    );
    // Overview text
    assert!(html.contains("pharos-detail-overview"), "{html}");
    assert!(
        html.contains("A blade runner hunts replicants."),
        "{html}"
    );
    // Genres
    assert!(html.contains("pharos-detail-genres"), "{html}");
    assert!(html.contains("Sci-Fi, Drama"), "{html}");
    // Cast section
    assert!(html.contains("pharos-detail-cast"), "{html}");
    assert!(html.contains("Harrison Ford"), "{html}");
    assert!(html.contains("Rick Deckard"), "{html}");
    // Actor with image renders an img with substituted person id.
    assert!(
        html.contains("/Items/p1/Images/Primary"),
        "{html}"
    );
    // Director without image renders kind label `(Director)` instead.
    assert!(html.contains("Ridley Scott"), "{html}");
    assert!(html.contains("(Director)"), "{html}");
    // Director's portrait is suppressed (no has_image).
    assert!(
        !html.contains("/Items/p2/Images/Primary"),
        "{html}"
    );
}

// ---- LiveTvView -------------------------------------------------

fn live_tv_with_two_channels() -> Element {
    let channels = vec![
        LiveChannel {
            id: "c1".into(),
            name: "BBC One".into(),
            number: "1".into(),
            group: Some("UK".into()),
            has_logo: true,
        },
        LiveChannel {
            id: "c2".into(),
            name: "BBC Two".into(),
            number: "2".into(),
            group: None,
            has_logo: false,
        },
    ];
    let programs = vec![LiveProgram {
        id: "c1-1".into(),
        channel_id: "c1".into(),
        title: "Six O'Clock News".into(),
        overview: None,
        start_iso: "2026-05-28T18:00:00.000Z".into(),
        end_iso: "2026-05-28T18:30:00.000Z".into(),
    }];
    rsx! {
        LiveTvView {
            channels: channels,
            programs: programs,
            status: LiveTvStatus::Idle,
            logo_url_template: Some("/LiveTv/Channels/{id}/Images/Primary".to_string()),
            on_action: move |_| {},
        }
    }
}

fn live_tv_empty() -> Element {
    rsx! {
        LiveTvView {
            channels: Vec::<LiveChannel>::new(),
            programs: Vec::<LiveProgram>::new(),
            status: LiveTvStatus::Empty,
            logo_url_template: None,
            on_action: move |_| {},
        }
    }
}

fn live_tv_loading() -> Element {
    rsx! {
        LiveTvView {
            channels: Vec::<LiveChannel>::new(),
            programs: Vec::<LiveProgram>::new(),
            status: LiveTvStatus::Loading,
            logo_url_template: None,
            on_action: move |_| {},
        }
    }
}

#[test]
fn live_tv_view_renders_channel_grid_with_epg_and_logo() {
    let html = render_root(live_tv_with_two_channels);
    assert!(html.contains("BBC One"), "{html}");
    assert!(html.contains("BBC Two"), "{html}");
    // EPG entry rendered + HH:MM time extracted.
    assert!(html.contains("18:00"), "{html}");
    assert!(
        html.contains("Six O&#x27;Clock News") || html.contains("Six O'Clock News"),
        "{html}"
    );
    // Logo substitution: c1 has logo → has rendered <img> with /Primary path,
    // c2 has no logo → no <img>.
    assert!(
        html.contains("/LiveTv/Channels/c1/Images/Primary"),
        "{html}"
    );
    assert!(
        !html.contains("/LiveTv/Channels/c2/Images/Primary"),
        "{html}"
    );
    // No-listing fallback for the second channel.
    assert!(html.contains("no listings"), "{html}");
}

#[test]
fn live_tv_view_renders_empty_state() {
    let html = render_root(live_tv_empty);
    assert!(html.contains("pharos-livetv-empty"), "{html}");
    assert!(html.contains("No channels configured"), "{html}");
}

#[test]
fn live_tv_view_renders_loading_state() {
    let html = render_root(live_tv_loading);
    assert!(html.contains("pharos-livetv-loading"), "{html}");
    assert!(html.contains("Loading channels"), "{html}");
}

// ---- PrefsView --------------------------------------------------

fn prefs_display() -> Element {
    rsx! {
        PrefsView {
            config: UserConfiguration {
                hide_played_in_latest: true,
                display_missing_episodes: false,
                ..Default::default()
            },
            active_tab: PrefsTab::Display,
            status: None,
            on_action: move |_| {},
        }
    }
}

fn prefs_playback_with_status() -> Element {
    rsx! {
        PrefsView {
            config: UserConfiguration {
                audio_language_preference: "jpn".into(),
                subtitle_language_preference: "eng".into(),
                subtitle_mode: "Smart".into(),
                ..Default::default()
            },
            active_tab: PrefsTab::Playback,
            status: Some("Saved.".to_string()),
            on_action: move |_| {},
        }
    }
}

fn prefs_home() -> Element {
    rsx! {
        PrefsView {
            config: UserConfiguration {
                enable_next_episode_auto_play: true,
                ..Default::default()
            },
            active_tab: PrefsTab::Home,
            status: None,
            on_action: move |_| {},
        }
    }
}

#[test]
fn prefs_view_display_tab_renders_display_pane_and_tabs() {
    let html = render_root(prefs_display);
    assert!(html.contains(r#"data-tab="display""#), "{html}");
    assert!(html.contains("pharos-prefs-pane-display"), "{html}");
    assert!(html.contains("Show missing episodes"), "{html}");
    assert!(html.contains("Hide played items"), "{html}");
    // Tab nav contains all three tabs.
    assert!(html.contains(">Display<"), "{html}");
    assert!(html.contains(">Playback<"), "{html}");
    assert!(html.contains(">Home<"), "{html}");
    // Save button always renders.
    assert!(html.contains("pharos-prefs-save"), "{html}");
}

#[test]
fn prefs_view_playback_tab_renders_inputs_and_status() {
    let html = render_root(prefs_playback_with_status);
    assert!(html.contains("pharos-prefs-pane-playback"), "{html}");
    assert!(html.contains(r#"value="jpn""#), "{html}");
    assert!(html.contains(r#"value="eng""#), "{html}");
    // Subtitle-mode select rendered with `Smart` selected.
    assert!(html.contains("<select"), "{html}");
    assert!(html.contains("pharos-prefs-status"), "{html}");
    assert!(html.contains("Saved."), "{html}");
}

#[test]
fn prefs_view_home_tab_renders_auto_play_toggle() {
    let html = render_root(prefs_home);
    assert!(html.contains("pharos-prefs-pane-home"), "{html}");
    assert!(html.contains("Auto-play next episode"), "{html}");
}

fn prefs_languages() -> Element {
    rsx! {
        PrefsView {
            config: UserConfiguration {
                audio_language_preference: "jpn".into(),
                subtitle_language_preference: "eng".into(),
                ..Default::default()
            },
            active_tab: PrefsTab::Languages,
            status: None,
            cultures: vec![
                LocalizationCulture {
                    name: "English".into(),
                    two_letter_iso: "en".into(),
                    three_letter_iso: "eng".into(),
                },
                LocalizationCulture {
                    name: "Japanese".into(),
                    two_letter_iso: "ja".into(),
                    three_letter_iso: "jpn".into(),
                },
            ],
            on_action: move |_| {},
        }
    }
}

#[test]
fn prefs_view_languages_tab_renders_dropdowns() {
    let html = render_root(prefs_languages);
    assert!(html.contains("pharos-prefs-pane-languages"), "{html}");
    assert!(html.contains("Preferred audio language"), "{html}");
    assert!(html.contains("Preferred subtitle language"), "{html}");
    assert!(html.contains("English"), "{html}");
    assert!(html.contains("Japanese"), "{html}");
    // Languages tab is wired into the tab strip.
    assert!(html.contains(">Languages<"), "{html}");
}

// ---- QuickConnect ------------------------------------------------

fn qc_guest_pending() -> Element {
    rsx! {
        QuickConnectGuestView {
            pending: Some(QuickConnectInitiate {
                code: "654321".into(),
                secret: "sec".into(),
                device_id: "dev".into(),
            }),
            status: QuickConnectGuestStatus::Pending,
            on_action: move |_| {},
        }
    }
}

fn qc_guest_idle() -> Element {
    rsx! {
        QuickConnectGuestView {
            pending: None,
            status: QuickConnectGuestStatus::Idle,
            on_action: move |_| {},
        }
    }
}

fn qc_authorize_form() -> Element {
    rsx! {
        QuickConnectAuthorizeView {
            status: None,
            on_action: move |_| {},
        }
    }
}

#[test]
fn quick_connect_guest_pending_renders_code_and_status() {
    let html = render_root(qc_guest_pending);
    assert!(html.contains("pharos-qc-guest"), "{html}");
    assert!(html.contains("654321"), "{html}");
    assert!(html.contains("Waiting for approval"), "{html}");
    assert!(html.contains("pharos-qc-guest-start"), "{html}");
    assert!(html.contains(">New code<"), "{html}");
}

#[test]
fn quick_connect_guest_idle_shows_start_prompt() {
    let html = render_root(qc_guest_idle);
    assert!(html.contains("pharos-qc-guest-empty"), "{html}");
    assert!(html.contains(">Start<"), "{html}");
    // No code rendered until Initiate.
    assert!(!html.contains("pharos-qc-guest-code"), "{html}");
}

#[test]
fn quick_connect_authorize_renders_input_and_submit() {
    let html = render_root(qc_authorize_form);
    assert!(html.contains("pharos-qc-authorize"), "{html}");
    assert!(html.contains(r#"inputmode="numeric""#), "{html}");
    assert!(html.contains(r#"maxlength="6""#), "{html}");
    assert!(html.contains("pharos-qc-authorize-submit"), "{html}");
    assert!(html.contains(">Authorize<"), "{html}");
}
