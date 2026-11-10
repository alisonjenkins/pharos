//! Domain services shared by the Jellyfin and pharos-native wire layers
//! (V172, research R5). Each takes typed inputs and returns domain types; a
//! wire layer only parses, calls one of these, and encodes.

pub mod access;
pub mod auth;
pub mod devices;
pub mod events;
pub mod font_subset;
pub mod group_seen;
pub mod library;
pub mod lyrics;
pub mod metadata_edit;
pub mod metadata_text;
pub mod mix;
pub mod person;
pub mod play_state;
pub mod playback;
pub mod search;
pub mod segments;
pub mod user_data;
pub mod watch;
