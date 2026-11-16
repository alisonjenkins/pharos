//! Narrowing and ordering the Books list, as the web Books sheet does (V1451):
//! an order (Name, Published, Added), a read state (Unread, Reading, Read), a
//! subject, and one the web has no need of, only the books downloaded to this
//! reader.
//!
//! Everything but "downloaded" is asked of pharos, so the total and the paging
//! follow it. "Downloaded" names the books by id instead, from the record of
//! what is on this reader.

use crate::api::query_escape;
use kobo_json::Value;

/// The order the list comes in.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Order {
    /// A to Z by title. The only order the letter jump makes sense in.
    #[default]
    Name,
    /// Newest published first.
    Published,
    /// Newest added to pharos first.
    Added,
}

impl Order {
    pub const ALL: [Self; 3] = [Self::Name, Self::Published, Self::Added];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Name => "Name",
            Self::Published => "Published",
            Self::Added => "Added",
        }
    }

    /// The action naming it on the filter screen.
    #[must_use]
    pub const fn action(self) -> &'static str {
        match self {
            Self::Name => "books-order-name",
            Self::Published => "books-order-published",
            Self::Added => "books-order-added",
        }
    }

    /// The `SortBy` and `SortOrder` it asks pharos for: those of the web.
    const fn query(self) -> &'static str {
        match self {
            Self::Name => "SortBy=SortName&SortOrder=Ascending",
            Self::Published => "SortBy=PremiereDate,ProductionYear,SortName&SortOrder=Descending",
            Self::Added => "SortBy=DateCreated,SortName&SortOrder=Descending",
        }
    }
}

/// Which books by how far they have been read, as pharos counts it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ReadState {
    #[default]
    All,
    Unread,
    Reading,
    Read,
}

impl ReadState {
    pub const ALL: [Self; 4] = [Self::All, Self::Unread, Self::Reading, Self::Read];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Unread => "Unread",
            Self::Reading => "Reading",
            Self::Read => "Read",
        }
    }

    #[must_use]
    pub const fn action(self) -> &'static str {
        match self {
            Self::All => "books-read-all",
            Self::Unread => "books-read-unread",
            Self::Reading => "books-read-reading",
            Self::Read => "books-read-read",
        }
    }

    /// What it adds to the query; empty for all.
    const fn query(self) -> &'static str {
        match self {
            Self::All => "",
            Self::Unread => "&IsPlayed=false",
            Self::Reading => "&Filters=IsResumable",
            Self::Read => "&IsPlayed=true",
        }
    }
}

/// How the Books list is narrowed and ordered. The default is the list as it
/// first opens: every book, by name.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Narrow {
    pub order: Order,
    pub read: ReadState,
    /// Only books on this subject.
    pub subject: Option<String>,
    /// Only the books downloaded to this reader.
    pub downloaded: bool,
}

impl Narrow {
    /// Whether anything but the order narrows the list.
    #[must_use]
    pub fn narrows(&self) -> bool {
        self.read != ReadState::All || self.subject.is_some() || self.downloaded
    }

    /// The query string pharos is asked with, after the item type, the
    /// fields and the paging. Locked books are always left out (V1449).
    #[must_use]
    pub fn query(&self) -> String {
        let mut query = format!(
            "{}&IsReadable=true{}",
            self.order.query(),
            self.read.query()
        );
        if let Some(subject) = &self.subject {
            query.push_str("&Genres=");
            query.push_str(&query_escape(subject));
        }
        query
    }

    /// What the list is showing, in a few words for over it; `None` when
    /// nothing narrows it and it is in name order.
    #[must_use]
    pub fn summary(&self) -> Option<String> {
        let mut parts: Vec<String> = Vec::new();
        if self.read != ReadState::All {
            parts.push(self.read.label().to_owned());
        }
        if let Some(subject) = &self.subject {
            parts.push(subject.clone());
        }
        if self.downloaded {
            parts.push("Downloaded".to_owned());
        }
        if self.order != Order::Name {
            parts.push(format!("by {}", self.order.label().to_lowercase()));
        }
        (!parts.is_empty()).then(|| parts.join(" · "))
    }
}

/// The subjects pharos's books carry, by name, from `/Genres`. A row with no
/// name is dropped.
#[must_use]
pub fn parse_subjects(bytes: &[u8]) -> Option<Vec<String>> {
    let reply = kobo_json::parse(std::str::from_utf8(bytes).ok()?).ok()?;
    let items = reply.get("Items").and_then(Value::as_array)?;
    let mut names: Vec<String> = items
        .iter()
        .filter_map(|item| item.get("Name").and_then(Value::as_str))
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect();
    names.sort_by_key(|name| name.to_lowercase());
    names.dedup();
    Some(names)
}

/// The subjects of the books the user can see.
#[must_use]
pub fn subjects_url(server: &str, user: &str) -> String {
    format!(
        "{server}/Genres?IncludeItemTypes=Book&Recursive=true&UserId={}",
        query_escape(user)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_list_is_by_name_and_leaves_out_locked_books() {
        let narrow = Narrow::default();
        assert_eq!(
            narrow.query(),
            "SortBy=SortName&SortOrder=Ascending&IsReadable=true"
        );
        assert!(!narrow.narrows());
        assert_eq!(narrow.summary(), None);
    }

    #[test]
    fn each_read_state_asks_what_the_web_asks() {
        let query = |read| {
            Narrow {
                read,
                ..Narrow::default()
            }
            .query()
        };
        assert!(query(ReadState::Unread).ends_with("&IsPlayed=false"));
        assert!(query(ReadState::Reading).ends_with("&Filters=IsResumable"));
        assert!(query(ReadState::Read).ends_with("&IsPlayed=true"));
        assert!(!query(ReadState::All).contains("IsPlayed"));
    }

    #[test]
    fn the_newer_orders_come_newest_first() {
        let query = |order| {
            Narrow {
                order,
                ..Narrow::default()
            }
            .query()
        };
        assert!(query(Order::Published)
            .starts_with("SortBy=PremiereDate,ProductionYear,SortName&SortOrder=Descending"));
        assert!(query(Order::Added).starts_with("SortBy=DateCreated,SortName&SortOrder=Descending"));
    }

    #[test]
    fn a_subject_is_escaped_and_named_in_the_summary() {
        let narrow = Narrow {
            subject: Some("Cloud & DevOps".to_owned()),
            read: ReadState::Unread,
            order: Order::Added,
            downloaded: true,
        };
        assert!(narrow.query().contains("&Genres=Cloud%20%26%20DevOps"));
        assert!(narrow.narrows());
        assert_eq!(
            narrow.summary().as_deref(),
            Some("Unread · Cloud & DevOps · Downloaded · by added")
        );
    }

    #[test]
    fn subjects_are_read_sorted_and_without_blanks() {
        let body = br#"{"Items":[{"Name":"history"},{"Name":" "},{"Name":"Art"},{"Id":"x"}]}"#;
        assert_eq!(
            parse_subjects(body),
            Some(vec!["Art".to_owned(), "history".to_owned()])
        );
        assert_eq!(parse_subjects(b"nope"), None);
    }

    #[test]
    fn the_subjects_url_asks_for_book_subjects_only() {
        assert_eq!(
            subjects_url("https://p", "u 1"),
            "https://p/Genres?IncludeItemTypes=Book&Recursive=true&UserId=u%201"
        );
    }
}
