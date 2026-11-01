//! Spec 017: the pharos-native Cap'n Proto API. One binary for the group
//! (CLAUDE.md). Most tests drive the transport-independent dispatch
//! (`rpc::bootstrap_client`) in-thread; `connect.rs` covers the WebSocket
//! upgrade itself.
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "../common/mod.rs"]
mod common;

mod bootstrap;
mod connect;
mod gate;
