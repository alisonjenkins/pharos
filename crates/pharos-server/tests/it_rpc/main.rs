//! Spec 017: the pharos-native Cap'n Proto API. One binary for the group
//! (CLAUDE.md). Most tests drive the transport-independent dispatch
//! (`rpc::bootstrap_client`) in-thread; `connect.rs` covers the WebSocket
//! upgrade itself.
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "../common/mod.rs"]
mod common;

mod avatar;
mod bootstrap;
mod client;
mod client_api;
mod client_group;
mod connect;
mod gate;
mod group;
mod library;
mod limits;
mod login;
mod lyrics;
mod media_token;
mod mix;
mod password;
mod playback;
mod prefs;
mod quick_connect;
mod revocation;
mod revocation_sites;
mod session;
mod telemetry;
mod traffic;
mod user_data;
