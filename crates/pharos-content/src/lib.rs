//! The Pharos library as a media browse tree, for the Android media service
//! and the phone app (spec 044). Reads the library over the native API
//! (`pharos-client`, capnp-rpc over a websocket). Plain Rust in and out: the
//! JNI layer calls [`Library`] and gets [`Node`]s.
//!
//! ```text
//! root
//! +- music ........... music/recent  music/albums  music/artists -> album/<id>
//! |                    music/mixes -> mix/<n>        album/<id>   -> track/<id>
//! +- audiobooks ...... empty: the server has no audiobook kind yet
//! signin ............. shown instead of any node until a QuickConnect code
//!                      is approved in the Pharos web UI
//! ```
mod cache;
mod error;
mod fault;
mod fuzzy;
mod http;
mod id;
mod library;
mod net;
mod node;
mod prefetch;
mod resume;
#[cfg(test)]
mod testserver;
mod token;
mod tree;

pub use cache::{Download, DownloadTrack, OfflineSummary, DEFAULT_CAP_BYTES, MIN_FREE_BYTES};
pub use error::ContentError;
pub use fault::unreachable as server_unreachable;
pub use id::NodeId;
pub use library::{AuthState, Library, Timeouts};
pub use node::{Node, ART_WIDTH};
pub use resume::{
    Resume, Verdict as ResumeVerdict, MIN_TRACK_MS as RESUME_MIN_TRACK_MS,
    SAVE_EVERY as RESUME_SAVE_EVERY,
};
pub use tree::{autoplay, SearchReport};
