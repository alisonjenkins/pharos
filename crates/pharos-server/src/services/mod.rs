//! Domain services shared by the Jellyfin and pharos-native wire layers
//! (V172, research R5). Each takes typed inputs and returns domain types; a
//! wire layer only parses, calls one of these, and encodes.

pub mod access;
pub mod auth;
pub mod library;
