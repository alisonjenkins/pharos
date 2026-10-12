#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CastEvent {
    Started,
    Rejected,
    Ended,
    Lost,
    Reconnected,
    HandoffFailed,
    /// A persisted seat was re-created after a restart or a change of group owner.
    Resumed,
}

impl CastEvent {
    pub const ALL: [CastEvent; 7] = [
        CastEvent::Started,
        CastEvent::Rejected,
        CastEvent::Ended,
        CastEvent::Lost,
        CastEvent::Reconnected,
        CastEvent::HandoffFailed,
        CastEvent::Resumed,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            CastEvent::Started => "started",
            CastEvent::Rejected => "rejected",
            CastEvent::Ended => "ended",
            CastEvent::Lost => "lost",
            CastEvent::Reconnected => "reconnected",
            CastEvent::HandoffFailed => "handoff_failed",
            CastEvent::Resumed => "resumed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CastReason {
    NotInGroup,
    TvBusy,
    TvUnreachable,
    UserHasSeat,
    NotPermitted,
    OwnerLeft,
    GroupGone,
    GraceExpired,
    UserStop,
    TokenFailed,
    /// The TV a persisted seat points at is no longer configured.
    TvRemoved,
    /// A start reached a replica that does not run the group.
    NotGroupOwner,
}

impl CastReason {
    pub const ALL: [CastReason; 12] = [
        CastReason::NotInGroup,
        CastReason::TvBusy,
        CastReason::TvUnreachable,
        CastReason::UserHasSeat,
        CastReason::NotPermitted,
        CastReason::OwnerLeft,
        CastReason::GroupGone,
        CastReason::GraceExpired,
        CastReason::UserStop,
        CastReason::TokenFailed,
        CastReason::TvRemoved,
        CastReason::NotGroupOwner,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            CastReason::NotInGroup => "not_in_group",
            CastReason::TvBusy => "tv_busy",
            CastReason::TvUnreachable => "tv_unreachable",
            CastReason::UserHasSeat => "user_has_seat",
            CastReason::NotPermitted => "not_permitted",
            CastReason::OwnerLeft => "owner_left",
            CastReason::GroupGone => "group_gone",
            CastReason::GraceExpired => "grace_expired",
            CastReason::UserStop => "user_stop",
            CastReason::TokenFailed => "token_failed",
            CastReason::TvRemoved => "tv_removed",
            CastReason::NotGroupOwner => "not_group_owner",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn assert_labels_bounded(labels: &[&'static str]) {
        let distinct: HashSet<&str> = labels.iter().copied().collect();
        assert_eq!(
            distinct.len(),
            labels.len(),
            "labels must be distinct: {labels:?}"
        );
        for label in labels {
            assert!(!label.is_empty(), "label must not be empty");
            assert!(
                label.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "label must be lowercase snake_case: {label:?}"
            );
            assert!(
                !label.starts_with('_') && !label.ends_with('_'),
                "label must not start or end with underscore: {label:?}"
            );
        }
    }

    #[test]
    fn every_cast_event_label_is_distinct_and_snake_case() {
        let labels: Vec<&'static str> = CastEvent::ALL.iter().map(|e| e.label()).collect();
        assert_eq!(
            labels,
            [
                "started",
                "rejected",
                "ended",
                "lost",
                "reconnected",
                "handoff_failed",
                "resumed"
            ]
        );
        assert_labels_bounded(&labels);
    }

    #[test]
    fn every_cast_reason_label_is_distinct_and_snake_case() {
        let labels: Vec<&'static str> = CastReason::ALL.iter().map(|r| r.label()).collect();
        assert_eq!(
            labels,
            [
                "not_in_group",
                "tv_busy",
                "tv_unreachable",
                "user_has_seat",
                "not_permitted",
                "owner_left",
                "group_gone",
                "grace_expired",
                "user_stop",
                "token_failed",
                "tv_removed",
                "not_group_owner"
            ]
        );
        assert_labels_bounded(&labels);
    }
}
