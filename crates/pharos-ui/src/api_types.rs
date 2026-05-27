//! Plain-data DTOs the UI consumes from the Jellyfin-compat API.
//! Shapes match what the server emits (V16 — UI only talks public API).
//! Derives serde so the WASM-side `client` module can parse responses
//! directly; tests cover the parsing.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoggedInUser {
    pub id: String,
    pub name: String,
    pub server_id: String,
    pub access_token: String,
    pub is_admin: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryItem {
    pub id: String,
    pub name: String,
    pub kind: ItemKind,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ItemKind {
    #[default]
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

/// Subtitle / audio track entry the PlayerView's picker iterates
/// over. Mirrors the subset of jellyfin-web's MediaStream shape pharos
/// emits — pure data so the picker is host-renderable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct MediaTrack {
    pub index: u32,
    pub codec: Option<String>,
    pub language: Option<String>,
    pub title: Option<String>,
    pub is_default: bool,
    pub is_external: bool,
    pub delivery_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PlaybackTracks {
    pub audio: Vec<MediaTrack>,
    pub subtitle: Vec<MediaTrack>,
}
