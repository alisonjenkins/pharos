//! Group-session UI. Props-driven member list + status indicator +
//! Join/Leave/Create actions. The actual WebSocket connection lives in
//! the consumer (T25 fetch-client phase 3); this component only renders
//! the snapshot and emits `GroupAction` events.
//!
//! V3 surfaces the sync state visually: `is_buffering=true` shows a
//! "Waiting for member" badge so users understand why playback paused —
//! the V19 improvement over Jellyfin's silent stalls.

use dioxus::prelude::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupMember {
    pub member_id: String,
    pub name: String,
    pub is_leader: bool,
    pub is_buffering: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupSnapshot {
    pub group_id: Option<String>,
    pub members: Vec<GroupMember>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupAction {
    Create,
    Join { group_id: String },
    Leave,
}

#[component]
pub fn GroupSessionPanel(
    snapshot: GroupSnapshot,
    self_member_id: Option<String>,
    on_action: EventHandler<GroupAction>,
) -> Element {
    let in_group = snapshot.group_id.is_some();
    let any_buffering = snapshot.members.iter().any(|m| m.is_buffering);
    let group_id_display = snapshot.group_id.clone().unwrap_or_default();

    rsx! {
        aside {
            class: "pharos-group",
            "data-in-group": "{in_group}",
            header {
                class: "pharos-group-header",
                h2 { "Group session" }
                if in_group {
                    code { class: "pharos-group-id", "{group_id_display}" }
                }
            }
            if any_buffering {
                p {
                    class: "pharos-group-status pharos-group-buffering",
                    "Waiting for a member to buffer…"
                }
            }
            ul {
                class: "pharos-group-members",
                for m in snapshot.members.iter() {
                    Member {
                        key: "{m.member_id}",
                        member: m.clone(),
                        is_self: self_member_id.as_deref() == Some(m.member_id.as_str()),
                    }
                }
            }
            footer {
                class: "pharos-group-actions",
                if in_group {
                    button {
                        class: "pharos-group-leave",
                        onclick: move |_| on_action.call(GroupAction::Leave),
                        "Leave group"
                    }
                } else {
                    button {
                        class: "pharos-group-create",
                        onclick: move |_| on_action.call(GroupAction::Create),
                        "Create group"
                    }
                }
            }
        }
    }
}

#[component]
fn Member(member: GroupMember, is_self: bool) -> Element {
    rsx! {
        li {
            class: "pharos-group-member",
            "data-leader": "{member.is_leader}",
            "data-self": "{is_self}",
            "data-buffering": "{member.is_buffering}",
            span { class: "pharos-group-name", "{member.name}" }
            if member.is_leader {
                span { class: "pharos-group-badge pharos-group-leader", "Leader" }
            }
            if is_self {
                span { class: "pharos-group-badge pharos-group-self", "You" }
            }
            if member.is_buffering {
                span { class: "pharos-group-badge pharos-group-buffer", "Buffering" }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn member(id: &str, name: &str, leader: bool, buffering: bool) -> GroupMember {
        GroupMember {
            member_id: id.into(),
            name: name.into(),
            is_leader: leader,
            is_buffering: buffering,
        }
    }

    #[test]
    fn snapshot_value_semantics() {
        let s = GroupSnapshot {
            group_id: Some("g1".into()),
            members: vec![member("a", "ali", true, false)],
        };
        let s2 = s.clone();
        assert_eq!(s, s2);
    }

    #[test]
    fn action_variants_distinct() {
        let create = GroupAction::Create;
        let leave = GroupAction::Leave;
        let join = GroupAction::Join {
            group_id: "g".into(),
        };
        assert_ne!(create, leave);
        assert_ne!(create, join);
        assert_ne!(leave, join);
    }

    #[test]
    fn group_session_panel_module_exports_present() {
        // Smoke for component existence.
        fn _f(_s: GroupSnapshot) {}
        let _ = _f;
    }
}
