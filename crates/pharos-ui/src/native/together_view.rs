//! Watch together's screens as pure components (014 US1, US2): the groups to
//! join, an invite link's landing, and the member strip over the player. Data
//! in through props, actions out through handlers; no IO, so they render
//! under `dioxus-ssr`.

use dioxus::prelude::*;

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
pub fn MemberStrip(members: Vec<Member>, on_leave: EventHandler<()>) -> Element {
    rsx! {
        div { class: "pharos-members", role: "group", aria_label: "Watching together",
            ul { class: "pharos-member-list",
                for member in members {
                    li {
                        key: "{member.id}",
                        class: if member.buffering { "pharos-member is-buffering" } else { "pharos-member" },
                        "{member.name}"
                        if member.buffering {
                            span { class: "pharos-member-buffering", role: "status", " buffering" }
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
    use super::*;

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
        }
    }

    fn group() -> GroupSummary {
        GroupSummary {
            id: "g1".into(),
            name: "Friday film".into(),
            item: None,
            members: vec![member("a", "Ada", false), member("b", "Bo", true)],
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
    }
}
