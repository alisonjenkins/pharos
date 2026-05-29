//! pharos-jellyfin-api — leaf-crate-extracted DTOs and serde types
//! for the Jellyfin compat surface.
//!
//! Phase A.1: pure-data modules that previously lived in
//! `pharos-server::api::jellyfin::*`. The server crate re-exports
//! these so every existing caller (`use crate::api::jellyfin::device_profile::*`)
//! keeps compiling without edit churn.
//!
//! - [`device_profile`] — `DeviceProfileDto` + `Decision` negotiator
//!   for direct-play / remux / transcode classification. Wire-shape
//!   driven by jellyfin-web's `DeviceProfile` POST body.
//! - [`socket_messages`] — `Inbound` / `Outbound` envelopes for the
//!   Jellyfin `/socket` WebSocket. SyncPlay subset wraps
//!   `pharos-sync::{ClientMsg, ServerMsg}` at translation time.
//!
//! Tests for these modules are integration tests on this crate —
//! `tests/negotiator_proptest.rs` + `tests/socket_msg_fuzz.rs` no
//! longer boot the full server; they exercise the serde shapes
//! directly.

pub mod device_profile;
pub mod dto;
pub mod socket_messages;
