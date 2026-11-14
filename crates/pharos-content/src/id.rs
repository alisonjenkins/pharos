//! The media ids the car sees. They are strings, stable across restarts and
//! reconnects: the server's own item ids (`42`, `s7`) behind a short prefix,
//! so an id the car remembers (a "recent" entry, a voice follow-up) still
//! resolves later.

use std::fmt;

use pharos_client::api::ItemRef;

use super::error::ContentError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NodeId {
    Root,
    Music,
    Audiobooks,
    Recent,
    Albums,
    Artists,
    Mixes,
    Album(ItemRef),
    Artist(ItemRef),
    Track(ItemRef),
    /// The Nth built-in mix (`pharos_client::mix::presets`).
    Mix(u8),
    SignIn,
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NodeId::Root => f.write_str("root"),
            NodeId::Music => f.write_str("music"),
            NodeId::Audiobooks => f.write_str("audiobooks"),
            NodeId::Recent => f.write_str("music/recent"),
            NodeId::Albums => f.write_str("music/albums"),
            NodeId::Artists => f.write_str("music/artists"),
            NodeId::Mixes => f.write_str("music/mixes"),
            NodeId::Album(i) => write!(f, "album/{i}"),
            NodeId::Artist(i) => write!(f, "artist/{i}"),
            NodeId::Track(i) => write!(f, "track/{i}"),
            NodeId::Mix(n) => write!(f, "mix/{n}"),
            NodeId::SignIn => f.write_str("signin"),
        }
    }
}

impl NodeId {
    pub fn parse(s: &str) -> Result<Self, ContentError> {
        let bad = || ContentError::BadId(s.to_owned());
        let item = |n: &str| n.parse::<ItemRef>().map_err(|_| bad());
        Ok(match s {
            "root" => NodeId::Root,
            "music" => NodeId::Music,
            "audiobooks" => NodeId::Audiobooks,
            "music/recent" => NodeId::Recent,
            "music/albums" => NodeId::Albums,
            "music/artists" => NodeId::Artists,
            "music/mixes" => NodeId::Mixes,
            "signin" => NodeId::SignIn,
            _ => match s.split_once('/') {
                Some(("album", n)) => NodeId::Album(item(n)?),
                Some(("artist", n)) => NodeId::Artist(item(n)?),
                Some(("track", n)) => NodeId::Track(item(n)?),
                Some(("mix", n)) => NodeId::Mix(n.parse().map_err(|_| bad())?),
                _ => return Err(bad()),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn all() -> Vec<NodeId> {
        vec![
            NodeId::Root,
            NodeId::Music,
            NodeId::Audiobooks,
            NodeId::Recent,
            NodeId::Albums,
            NodeId::Artists,
            NodeId::Mixes,
            NodeId::Album(ItemRef::Synth(7)),
            NodeId::Artist(ItemRef::Synth(0)),
            NodeId::Track(ItemRef::Real(42)),
            NodeId::Mix(3),
            NodeId::SignIn,
        ]
    }

    #[test]
    fn every_id_round_trips_through_its_string() {
        for id in all() {
            assert_eq!(NodeId::parse(&id.to_string()).unwrap(), id, "{id}");
        }
    }

    #[test]
    fn ids_are_distinct_strings() {
        let mut strings: Vec<String> = all().iter().map(NodeId::to_string).collect();
        strings.sort();
        strings.dedup();
        assert_eq!(strings.len(), all().len());
    }

    /// The spike's fixed ids stay valid so a car that remembered them works.
    #[test]
    fn the_spike_ids_keep_their_strings() {
        assert_eq!(NodeId::Music.to_string(), "music");
        assert_eq!(NodeId::Audiobooks.to_string(), "audiobooks");
        assert_eq!(NodeId::Root.to_string(), "root");
    }

    #[test]
    fn a_bad_id_is_named_in_the_error() {
        for bad in [
            "",
            "track",
            "track/",
            "track/-1",
            "track/1x",
            "album/s",
            "mix/300",
            "nope/1",
            "music/other",
        ] {
            let e = NodeId::parse(bad).expect_err(bad);
            assert!(
                matches!(&e, ContentError::BadId(s) if s == bad),
                "{bad}: {e}"
            );
        }
    }
}
