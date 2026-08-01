//! pharos's own API surface — endpoints with no Jellyfin equivalent.
//!
//! Kept apart from `api::jellyfin` on purpose. Everything under that module is
//! answerable to a client's expectations of a real Jellyfin server; nothing
//! here is, so a pharos-only body cannot drift onto a path a stock client
//! already has assumptions about.

pub mod remote_items;
