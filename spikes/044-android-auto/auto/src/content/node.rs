//! One entry in the car's browse tree, and the URLs it carries.
//!
//! Image and stream URLs hold the Media-scoped token as `api_key` (ADR-0058):
//! the car host and `MediaPlayer` cannot send a header. The token reaches
//! only item images and streams and expires (6 h by default), so URLs are
//! built per request and a stale one means asking the node again.

use pharos_client::api::{Item, ItemKind, ItemRef};

use super::id::NodeId;

/// Widest Primary image the server makes; it rounds a smaller `fillWidth`
/// up to one of its encode widths (ADR-0063), so 480 is the car-sized pick.
pub const ART_WIDTH: u32 = 480;

/// Codecs Android's `MediaPlayer` decodes. `universal` streams the file as it
/// is when its codec is listed and remuxes to the first listed codec if not.
const AUDIO_CODECS: &str = "mp3,aac,flac,opus,vorbis";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    /// The stable string id (see [`NodeId`]).
    pub id: String,
    pub title: String,
    /// The artist for a track, album or artist line; empty when none.
    pub subtitle: String,
    pub browsable: bool,
    pub playable: bool,
    /// Artwork the car can fetch with no header, [`ART_WIDTH`] wide.
    pub art_url: Option<String>,
    /// For a playable track: an audio URL that needs no header.
    pub stream_url: Option<String>,
    /// 0 when unknown or not a track.
    pub duration_ms: u64,
}

impl Node {
    pub(crate) fn folder(id: NodeId, title: &str) -> Self {
        Self {
            id: id.to_string(),
            title: title.to_owned(),
            subtitle: String::new(),
            browsable: true,
            playable: false,
            art_url: None,
            stream_url: None,
            duration_ms: 0,
        }
    }

    /// The sign-in row: not playable, not browsable; the title is the
    /// instruction the user reads off the car screen.
    pub fn sign_in(code: &str) -> Self {
        Self {
            id: NodeId::SignIn.to_string(),
            title: format!("Sign in: approve code {code}"),
            subtitle: "Approve this code in Pharos on another device".to_owned(),
            browsable: false,
            playable: false,
            art_url: None,
            stream_url: None,
            duration_ms: 0,
        }
    }
}

/// Where this server is and the token to put in URLs.
#[derive(Debug, Clone)]
pub struct Urls {
    base: String,
    token: String,
}

impl Urls {
    pub fn new(base: &str, token: &str) -> Self {
        Self {
            base: base.trim_end_matches('/').to_owned(),
            token: token.to_owned(),
        }
    }

    /// The server's `image` path made absolute, sized for the car.
    pub fn art(&self, image: &str) -> Option<String> {
        if !image.starts_with('/') {
            return None;
        }
        let join = if image.contains('?') { '&' } else { '?' };
        Some(format!(
            "{}{image}{join}fillWidth={ART_WIDTH}&api_key={}",
            self.base,
            pct(&self.token)
        ))
    }

    /// The audio stream of a stored item; synthesised groups have none.
    pub fn stream(&self, id: ItemRef) -> Option<String> {
        let ItemRef::Real(n) = id else { return None };
        Some(format!(
            "{}/audio/{n:032x}/universal?AudioCodec={AUDIO_CODECS}&api_key={}",
            self.base,
            pct(&self.token)
        ))
    }

    /// An API item as a browse node; `None` for kinds the car does not show.
    pub fn node(&self, item: &Item) -> Option<Node> {
        let (id, browsable, playable) = match item.kind {
            ItemKind::Audio => (NodeId::Track(item.id), false, true),
            ItemKind::Album => (NodeId::Album(item.id), true, false),
            ItemKind::Artist => (NodeId::Artist(item.id), true, false),
            _ => return None,
        };
        let subtitle = match item.kind {
            ItemKind::Audio => item.artist.clone(),
            _ => item.series_name.clone(),
        }
        .unwrap_or_default();
        Some(Node {
            id: id.to_string(),
            title: item.name.clone(),
            subtitle,
            browsable,
            playable,
            art_url: item.image.as_deref().and_then(|i| self.art(i)),
            stream_url: playable.then(|| self.stream(item.id)).flatten(),
            duration_ms: item.runtime_ms,
        })
    }
}

/// Percent-encode everything outside RFC 3986's unreserved set.
fn pct(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    pub(crate) fn item(id: ItemRef, kind: ItemKind, name: &str) -> Item {
        Item {
            id,
            kind,
            name: name.to_owned(),
            year: None,
            runtime_ms: 0,
            image: None,
            played: false,
            favorite: false,
            position_ms: 0,
            series_name: None,
            season: None,
            episode: None,
            parent: None,
            library_kind: None,
            imdb_rating: None,
            critic_rating: None,
            artist: None,
            stereo: None,
            damaged: false,
            new_children: 0,
        }
    }

    fn urls() -> Urls {
        Urls::new("https://pharos.example/", "tok+en/1")
    }

    #[test]
    fn art_is_absolute_sized_and_carries_the_encoded_token() {
        assert_eq!(
            urls().art("/Items/ab/Images/Primary?tag=t").unwrap(),
            "https://pharos.example/Items/ab/Images/Primary?tag=t&fillWidth=480&api_key=tok%2Ben%2F1"
        );
        assert_eq!(
            urls().art("/x").unwrap(),
            "https://pharos.example/x?fillWidth=480&api_key=tok%2Ben%2F1"
        );
    }

    #[test]
    fn art_that_is_not_a_server_path_is_dropped() {
        assert_eq!(urls().art("https://elsewhere/x.jpg"), None);
    }

    #[test]
    fn a_real_item_streams_from_its_32_hex_wire_id() {
        assert_eq!(
            urls().stream(ItemRef::Real(255)).unwrap(),
            "https://pharos.example/audio/000000000000000000000000000000ff/universal?AudioCodec=mp3,aac,flac,opus,vorbis&api_key=tok%2Ben%2F1"
        );
        assert_eq!(urls().stream(ItemRef::Synth(255)), None);
    }

    #[test]
    fn a_track_is_playable_with_artist_art_and_stream() {
        let mut t = item(ItemRef::Real(9), ItemKind::Audio, "A Song");
        t.artist = Some("An Artist".into());
        t.image = Some("/Items/09/Images/Primary?tag=z".into());
        t.runtime_ms = 61_000;
        let n = urls().node(&t).unwrap();
        assert_eq!(n.id, "track/9");
        assert_eq!(n.subtitle, "An Artist");
        assert!(n.playable && !n.browsable);
        assert!(n.art_url.unwrap().contains("fillWidth=480"));
        assert!(n.stream_url.unwrap().contains("/audio/"));
        assert_eq!(n.duration_ms, 61_000);
    }

    #[test]
    fn albums_and_artists_browse_and_have_no_stream() {
        let mut a = item(ItemRef::Synth(4), ItemKind::Album, "A Record");
        a.series_name = Some("An Artist".into());
        let n = urls().node(&a).unwrap();
        assert_eq!(n.id, "album/s4");
        assert_eq!(n.subtitle, "An Artist");
        assert!(n.browsable && !n.playable && n.stream_url.is_none());
        let n = urls()
            .node(&item(ItemRef::Synth(5), ItemKind::Artist, "B"))
            .unwrap();
        assert_eq!(n.id, "artist/s5");
    }

    #[test]
    fn kinds_the_car_does_not_show_are_dropped() {
        for kind in [
            ItemKind::Movie,
            ItemKind::Book,
            ItemKind::Library,
            ItemKind::Other,
        ] {
            assert_eq!(urls().node(&item(ItemRef::Real(1), kind, "x")), None);
        }
    }

    #[test]
    fn the_sign_in_row_carries_the_code_and_is_inert() {
        let n = Node::sign_in("123456");
        assert_eq!(n.title, "Sign in: approve code 123456");
        assert!(!n.browsable && !n.playable);
        assert_eq!(NodeId::parse(&n.id).unwrap(), NodeId::SignIn);
    }
}
