//! Watch together's screens as pure components (014 US1, US2): the groups to
//! join, an invite link's landing, and the member strip over the player. Data
//! in through props, actions out through handlers; no IO, so they render
//! under `dioxus-ssr`.

use dioxus::prelude::*;

use super::activity::{WaitPill, LOG_LINES};
use super::seen::{initial, SeenRow};
use super::together::{GroupSummary, Member};

/// The Together page: the groups running now, each with a Join button.
#[component]
pub fn TogetherView(groups: Vec<GroupSummary>, on_join: EventHandler<String>) -> Element {
    rsx! {
        section { class: "pharos-together",
            h2 { "Watch together" }
            if groups.is_empty() {
                p { class: "pharos-empty", "No one is watching together right now" }
            }
            ul { class: "pharos-groups",
                for group in groups {
                    li { key: "{group.id}", class: "pharos-group",
                        div { class: "pharos-group-text",
                            span { class: "pharos-group-name", "{group.name}" }
                            span { class: "pharos-muted", {member_names(&group.members)} }
                        }
                        button {
                            class: "pharos-button",
                            r#type: "button",
                            onclick: {
                                let id = group.id.clone();
                                move |_| on_join.call(id.clone())
                            },
                            "Join"
                        }
                    }
                }
            }
        }
    }
}

/// Where an invite link lands: the group if it still runs, otherwise a note
/// that it has ended. An ended group is never silently replaced by a new one.
#[component]
pub fn InviteView(
    group: Option<GroupSummary>,
    on_join: EventHandler<String>,
    on_start_new: EventHandler<()>,
) -> Element {
    rsx! {
        section { class: "pharos-together",
            match group {
                Some(group) => rsx! {
                    h2 { "{group.name}" }
                    p { class: "pharos-muted", {member_names(&group.members)} }
                    div { class: "pharos-actions",
                        button {
                            class: "pharos-button",
                            r#type: "button",
                            onclick: {
                                let id = group.id.clone();
                                move |_| on_join.call(id.clone())
                            },
                            "Join"
                        }
                    }
                },
                None => rsx! {
                    h2 { "This group has ended" }
                    div { class: "pharos-actions",
                        button {
                            class: "pharos-button",
                            r#type: "button",
                            onclick: move |_| on_start_new.call(()),
                            "Start a new group"
                        }
                    }
                },
            }
        }
    }
}

/// The members over the player, and Leave. Sits in the watch page's top bar,
/// so it fades with the rest of the chrome.
#[component]
pub fn MemberStrip(
    members: Vec<Member>,
    on_leave: EventHandler<()>,
    /// Each member's mark on the item playing. Hover or focus on a member
    /// shows it.
    #[props(default)]
    seen: Vec<SeenRow>,
    /// Ids of the members not following the group's tracks.
    #[props(default)]
    own_tracks: Vec<String>,
    /// The activity log's lines, oldest first; the last three show.
    #[props(default)]
    activity: Vec<String>,
) -> Element {
    let shown = activity.len().saturating_sub(LOG_LINES);
    rsx! {
        div { class: "pharos-members", role: "group", aria_label: "Watching together",
            ul { class: "pharos-member-list",
                for member in members {
                    li {
                        key: "{member.id}",
                        class: if member.buffering { "pharos-member is-buffering" } else { "pharos-member" },
                        tabindex: if seen.is_empty() { None } else { Some("0") },
                        "{member.name}"
                        if let Some(row) = seen.iter().find(|r| r.member_id == member.id) {
                            span {
                                class: "pharos-member-seen {row.mark.state()}",
                                title: "{row.name}: {row.mark.label()}",
                                " {row.mark.label()}"
                            }
                        }
                        if member.buffering {
                            span { class: "pharos-member-buffering", role: "status", " buffering" }
                        }
                        if own_tracks.contains(&member.id) {
                            span { class: "pharos-member-own-tracks", " own tracks" }
                        }
                    }
                }
            }
            button {
                class: "pharos-control pharos-member-leave",
                r#type: "button",
                onclick: move |_| on_leave.call(()),
                "Leave"
            }
            if !activity.is_empty() {
                ul { class: "pharos-activity-log", aria_label: "Recent activity",
                    for line in activity.into_iter().skip(shown) {
                        li { class: "pharos-activity-line", "{line}" }
                    }
                }
            }
        }
    }
}

/// How others join the group: its invite link to send, a Copy button and
/// what the copy did. Opens under the player's top bar.
#[component]
pub fn InvitePanel(
    url: String,
    /// What the last copy did ("Link copied"), if one was tried.
    note: Option<String>,
    on_copy: EventHandler<()>,
    on_close: EventHandler<()>,
) -> Element {
    rsx! {
        div {
            class: "pharos-invite",
            role: "dialog",
            aria_label: "Invite to watch together",
            p { class: "pharos-invite-text",
                "Send this link to anyone who can sign in here. They join at the group's place."
            }
            input {
                id: INVITE_LINK_ID,
                class: "pharos-invite-link",
                r#type: "text",
                readonly: true,
                aria_label: "Invite link",
                value: "{url}",
            }
            div { class: "pharos-invite-actions",
                button {
                    class: "pharos-button",
                    r#type: "button",
                    onclick: move |_| on_copy.call(()),
                    "Copy link"
                }
                button {
                    class: "pharos-button pharos-button-secondary",
                    r#type: "button",
                    onclick: move |_| on_close.call(()),
                    "Done"
                }
                if let Some(note) = note {
                    span { class: "pharos-invite-note", role: "status", "{note}" }
                }
            }
        }
    }
}

/// The invite link's field, selected for a manual copy when the clipboard
/// refuses.
pub const INVITE_LINK_ID: &str = "pharos-invite-link";

/// The newest activity line over the video. It sits beside the chrome, not in
/// it, so it stays visible while the controls are hidden; the caller passes
/// `None` once the line is older than the toast time.
#[component]
pub fn ActivityToast(line: Option<String>) -> Element {
    let Some(line) = line else {
        return rsx! {};
    };
    rsx! { div { class: "pharos-activity-toast", role: "status", aria_live: "polite", "{line}" } }
}

/// The group notes' content, apart from the toast, as the watch page passes
/// it through the player.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NotesView {
    pub connection: Option<String>,
    pub wait: Option<WaitPill>,
    pub status: Option<String>,
    pub undo: bool,
}

/// What the viewer needs to know about the group over the video, stacked
/// under the top bar and outside the chrome, so each shows whether or not
/// the controls are up (spec 038 G5): a lost connection, who the group waits
/// on, a group that ended or could not be rejoined, Leave's undo, and the
/// newest activity line.
#[component]
pub fn GroupNotes(
    #[props(default)] connection: Option<String>,
    #[props(default)] wait: Option<WaitPill>,
    #[props(default)] status: Option<String>,
    /// Leave's undo, while it can still be taken.
    #[props(default)]
    undo: bool,
    #[props(default)] on_undo: EventHandler<()>,
    #[props(default)] toast: Option<String>,
) -> Element {
    // The toast saying who it waits on repeats the pill.
    let toast = toast.filter(|t| wait.as_ref().map_or(true, |w| w.text() != *t));
    if connection.is_none() && wait.is_none() && status.is_none() && !undo && toast.is_none() {
        return rsx! {};
    }
    rsx! {
        div { class: "pharos-watch-notes",
            if let Some(line) = connection {
                p { class: "pharos-watch-note pharos-connection-lost", role: "status", "{line}" }
            }
            if let Some(pill) = wait {
                p {
                    class: if matches!(pill, WaitPill::Ready) { "pharos-wait-pill is-ready" } else { "pharos-wait-pill" },
                    role: "status",
                    aria_live: "polite",
                    {pill.text()}
                }
            }
            if let Some(message) = status {
                p { class: "pharos-watch-note", role: "status", "{message}. You can keep watching." }
            }
            if undo {
                p { class: "pharos-watch-note pharos-leave-undo", role: "status",
                    "You left the group. "
                    button {
                        class: "pharos-button pharos-button-secondary",
                        r#type: "button",
                        onclick: move |_| on_undo.call(()),
                        "Undo"
                    }
                }
            }
            ActivityToast { line: toast }
        }
    }
}

/// "In this group": what each member has seen of the item shown. `rows` has
/// one entry per member, each with the marks to show (a series page shows the
/// series' and the season's). Nothing renders outside a group.
#[component]
pub fn InThisGroup(rows: Vec<(String, Vec<SeenRow>)>) -> Element {
    if rows.is_empty() {
        return rsx! {};
    }
    rsx! {
        section { class: "pharos-group-seen", aria_label: "In this group",
            h3 { class: "pharos-section-title", "In this group" }
            for (scope, marks) in rows {
                if !scope.is_empty() {
                    p { class: "pharos-muted", "{scope}" }
                }
                ul { class: "pharos-seen-list",
                    for row in marks {
                        li { key: "{row.member_id}", class: "pharos-seen {row.mark.state()}",
                            span { class: "pharos-seen-name", "{row.name}" }
                            span { class: "pharos-seen-mark", " {row.mark.label()}" }
                        }
                    }
                }
            }
        }
    }
}

/// Small initials for who has seen an episode, on its row. Members who have
/// not seen it show dimmed, so the row says who is still to catch up.
#[component]
pub fn SeenDots(rows: Vec<SeenRow>) -> Element {
    if rows.is_empty() {
        return rsx! {};
    }
    rsx! {
        span { class: "pharos-seen-dots", role: "group", aria_label: "Seen by",
            for row in rows {
                span {
                    key: "{row.member_id}",
                    class: "pharos-seen-dot {row.mark.state()}",
                    title: "{row.name}: {row.mark.label()}",
                    role: "img",
                    aria_label: "{row.name}: {row.mark.label()}",
                    "{initial(&row.name)}"
                }
            }
        }
    }
}

fn member_names(members: &[Member]) -> String {
    match members {
        [] => "Empty".to_owned(),
        _ => members
            .iter()
            .map(|m| m.name.as_str())
            .collect::<Vec<_>>()
            .join(", "),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;
    use crate::native::seen::Mark;

    fn render(body: impl Fn() -> Element + 'static) -> String {
        let body: std::rc::Rc<dyn Fn() -> Element> = std::rc::Rc::new(body);
        let mut dom =
            VirtualDom::new_with_props(|body: std::rc::Rc<dyn Fn() -> Element>| body(), body);
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    fn member(id: &str, name: &str, buffering: bool) -> Member {
        Member {
            id: id.into(),
            name: name.into(),
            buffering,
            own_tracks: false,
        }
    }

    fn group() -> GroupSummary {
        GroupSummary {
            id: "g1".into(),
            name: "Friday film".into(),
            item: None,
            members: vec![member("a", "Ada", false), member("b", "Bo", true)],
            tracks: crate::native::track_follow::GroupTracksView::default(),
            you: String::new(),
        }
    }

    #[test]
    fn no_groups_says_no_one_is_watching() {
        let html = render(|| rsx! { TogetherView { groups: vec![], on_join: |_| {} } });
        assert!(
            html.contains("No one is watching together right now"),
            "{html}"
        );
        assert!(!html.contains(">Join<"), "{html}");
    }

    #[test]
    fn each_group_lists_its_members_with_a_join_button() {
        let html = render(|| rsx! { TogetherView { groups: vec![group()], on_join: |_| {} } });
        assert!(
            html.contains("Friday film") && html.contains("Ada, Bo"),
            "{html}"
        );
        assert!(html.contains(">Join<"), "{html}");
        assert!(!html.contains("No one is watching"), "{html}");
    }

    #[test]
    fn a_live_invite_offers_to_join() {
        let html = render(|| {
            rsx! { InviteView { group: Some(group()), on_join: |_| {}, on_start_new: |_| {} } }
        });
        assert!(
            html.contains("Friday film") && html.contains(">Join<"),
            "{html}"
        );
        assert!(!html.contains("has ended"), "{html}");
    }

    #[test]
    fn an_ended_invite_says_so_and_offers_a_new_group() {
        let html = render(|| {
            rsx! { InviteView { group: None, on_join: |_| {}, on_start_new: |_| {} } }
        });
        assert!(html.contains("This group has ended"), "{html}");
        assert!(html.contains(">Start a new group<"), "{html}");
        assert!(!html.contains(">Join<"), "{html}");
    }

    #[test]
    fn the_strip_names_members_marks_buffering_and_offers_leave() {
        let html = render(|| {
            rsx! { MemberStrip {
                members: vec![member("a", "Ada", false), member("b", "Bo", true)],
                on_leave: |_| {},
            } }
        });
        assert!(html.contains("Ada") && html.contains("Bo"), "{html}");
        assert_eq!(html.matches("pharos-member-buffering").count(), 1, "{html}");
        assert!(html.contains(">Leave<"), "{html}");
        assert!(!html.contains("pharos-member-seen"), "{html}");
    }

    fn row(id: &str, name: &str, mark: Mark) -> SeenRow {
        SeenRow {
            member_id: id.into(),
            name: name.into(),
            mark,
        }
    }

    #[test]
    fn the_strip_shows_each_members_mark_on_the_item_playing() {
        let html = render(|| {
            rsx! { MemberStrip {
                members: vec![member("a", "Ada", false), member("b", "Bo", false)],
                on_leave: |_| {},
                seen: vec![row("a", "Ada", Mark::Seen), row("b", "Bo", Mark::Partway(40))],
            } }
        });
        assert!(
            html.contains("is-seen") && html.contains("> seen<"),
            "{html}"
        );
        assert!(
            html.contains("40%") && html.contains("is-partway"),
            "{html}"
        );
    }

    #[test]
    fn the_strip_marks_members_on_their_own_tracks() {
        let html = render(|| {
            rsx! { MemberStrip {
                members: vec![member("a", "Ada", false), member("b", "Bo", false)],
                on_leave: |_| {},
                own_tracks: vec!["b".to_owned()],
            } }
        });
        assert_eq!(
            html.matches("pharos-member-own-tracks").count(),
            1,
            "{html}"
        );
        let (ada, bo) = html.split_once("Bo").expect("Bo is listed");
        assert!(
            !ada.contains("own-tracks") && bo.contains("own tracks"),
            "{html}"
        );
    }

    #[test]
    fn the_strip_lists_the_last_three_activity_lines() {
        let lines = ["one", "two", "three", "four"].map(String::from).to_vec();
        let html = render(move || {
            rsx! { MemberStrip {
                members: vec![member("a", "Ada", false)],
                on_leave: |_| {},
                activity: lines.clone(),
            } }
        });
        assert_eq!(html.matches("pharos-activity-line").count(), 3, "{html}");
        assert!(!html.contains(">one<"), "{html}");
        assert!(html.contains(">four<"), "{html}");
    }

    #[test]
    fn no_activity_shows_no_log() {
        let html = render(|| {
            rsx! { MemberStrip { members: vec![member("a", "Ada", false)], on_leave: |_| {} } }
        });
        assert!(!html.contains("pharos-activity"), "{html}");
    }

    #[test]
    fn the_toast_shows_a_line_and_nothing_without_one() {
        let html =
            render(|| rsx! { ActivityToast { line: Some("Sam paused at 12:04".to_owned()) } });
        assert!(
            html.contains("pharos-activity-toast") && html.contains("Sam paused at 12:04"),
            "{html}"
        );
        let html = render(|| rsx! { ActivityToast { line: None } });
        assert!(!html.contains("pharos-activity-toast"), "{html}");
    }

    #[test]
    fn group_notes_stack_what_is_happening_and_nothing_when_quiet() {
        let html = render(|| rsx! { GroupNotes {} });
        assert!(!html.contains("pharos-watch-notes"), "{html}");
        let html = render(|| {
            rsx! { GroupNotes {
                connection: Some("Connection lost. Reconnecting…".to_owned()),
                wait: Some(WaitPill::Waiting("Waiting for Sam".to_owned())),
                status: Some("This group has ended".to_owned()),
                undo: true,
                toast: Some("Bo joined".to_owned()),
            } }
        });
        let at = |needle: &str| {
            html.find(needle)
                .unwrap_or_else(|| panic!("{needle}: {html}"))
        };
        assert!(at("Connection lost") < at("Waiting for Sam"), "{html}");
        assert!(at("Waiting for Sam") < at("This group has ended. You can keep watching."));
        assert!(at(">Undo<") < at("Bo joined"), "{html}");
        assert!(!html.contains("role=\"alert\""), "news, not alerts: {html}");
    }

    #[test]
    fn the_toast_does_not_repeat_the_wait_pill() {
        let html = render(|| {
            rsx! { GroupNotes {
                wait: Some(WaitPill::Waiting("Waiting for Sam".to_owned())),
                toast: Some("Waiting for Sam".to_owned()),
            } }
        });
        assert_eq!(html.matches("Waiting for Sam").count(), 1, "{html}");
        assert!(!html.contains("pharos-activity-toast"), "{html}");
        let ready = render(|| rsx! { GroupNotes { wait: Some(WaitPill::Ready) } });
        assert!(
            ready.contains("is-ready") && ready.contains("Everyone"),
            "{ready}"
        );
    }

    #[test]
    fn in_this_group_names_each_member_with_a_mark() {
        let html = render(|| {
            rsx! { InThisGroup { rows: vec![(String::new(), vec![
                row("a", "Ada", Mark::Episodes { seen: 6, of: 10 }),
                row("b", "Bo", Mark::NotSeen),
            ])] } }
        });
        assert!(html.contains("In this group"), "{html}");
        assert!(
            html.contains("Ada") && html.contains("6/10 episodes"),
            "{html}"
        );
        assert!(html.contains("Bo") && html.contains("not seen"), "{html}");
    }

    #[test]
    fn outside_a_group_nothing_is_shown() {
        let html = render(|| rsx! { InThisGroup { rows: vec![] } SeenDots { rows: vec![] } });
        assert!(
            !html.contains("In this group") && !html.contains("pharos-seen"),
            "{html}"
        );
    }

    #[test]
    fn an_episode_row_shows_an_initial_per_member_dimmed_when_unseen() {
        let html = render(|| {
            rsx! { SeenDots { rows: vec![
                row("a", "Ada", Mark::Seen),
                row("b", "Bo", Mark::NotSeen),
            ] } }
        });
        assert_eq!(html.matches("pharos-seen-dot ").count(), 2, "{html}");
        assert!(
            html.contains("is-seen") && html.contains("is-unseen"),
            "{html}"
        );
        assert!(html.contains(">A<") && html.contains(">B<"), "{html}");
    }

    #[test]
    fn the_invite_panel_shows_the_link_to_copy_and_what_the_copy_did() {
        let link = "https://pharos.example/ui/together/g1";
        let html = render(move || {
            rsx! { InvitePanel { url: link, note: None, on_copy: |_| {}, on_close: |_| {} } }
        });
        assert!(html.contains(&format!(r#"value="{link}""#)), "{html}");
        assert!(html.contains("readonly"), "{html}");
        assert!(
            html.contains(">Copy link<") && html.contains(">Done<"),
            "{html}"
        );
        assert!(!html.contains("pharos-invite-note"), "{html}");
        let copied = render(move || {
            rsx! { InvitePanel {
                url: link,
                note: Some("Link copied".to_owned()),
                on_copy: |_| {},
                on_close: |_| {},
            } }
        });
        assert!(copied.contains(">Link copied<"), "{copied}");
    }
}
