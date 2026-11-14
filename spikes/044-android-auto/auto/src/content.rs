//! Real library content for the browse tree (spike 044, lane 1).
//!
//! Replaces the hard-coded test tones with the user's Pharos library, read
//! over the native API (`pharos-client`, capnp-rpc over a websocket). Plain
//! Rust in and out: the JNI layer calls [`Library`] and gets [`Node`]s.
//!
//! ```text
//! root
//! +- music ........... music/recent  music/albums  music/artists -> album/<id>
//! |                    music/mixes -> mix/<n>        album/<id>   -> track/<id>
//! +- audiobooks ...... empty: the server has no audiobook kind yet
//! signin ............. shown instead of any node until a QuickConnect code
//!                      is approved in the Pharos web UI
//! ```
#![deny(clippy::unwrap_used, clippy::expect_used)]

mod error;
mod id;
mod library;
mod net;
mod node;
mod token;
mod tree;
// The Android TV app's sign-in state machine, shared rather than copied: it
// is std-only and covered by its own tests.
#[allow(dead_code)]
#[path = "../../../../crates/pharos-ui/src/native/tv_auth.rs"]
mod tv_auth;

pub use error::ContentError;
pub use id::NodeId;
pub use library::{AuthState, Library, Timeouts};
pub use node::{Node, ART_WIDTH};
