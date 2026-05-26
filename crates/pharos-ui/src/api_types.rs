//! Plain-data DTOs the UI consumes from the Jellyfin-compat API.
//! Shapes match what the server emits (V16 — UI only talks public API).
//! No serde here yet — these are pure component-prop types; T24 phase 2
//! adds the fetch layer that deserialises wire JSON into these.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoggedInUser {
    pub id: String,
    pub name: String,
    pub server_id: String,
    pub access_token: String,
    pub is_admin: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryItem {
    pub id: String,
    pub name: String,
    pub kind: ItemKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    Movie,
    Episode,
    Audio,
}

impl ItemKind {
    pub fn from_jellyfin_type(s: &str) -> Self {
        match s {
            "Movie" => Self::Movie,
            "Episode" => Self::Episode,
            "Audio" => Self::Audio,
            _ => Self::Movie,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Movie => "Movie",
            Self::Episode => "Episode",
            Self::Audio => "Audio",
        }
    }
}
