//! Group-sync subsystem. See `docs/group-sync-protocol.md` in the
//! `pharos` repo for the wire protocol.
//!
//! Layout:
//! - [`messages`] — `ClientMsg` / `ServerMsg` wire types
//! - [`clock`] — per-member rolling offset estimator (median over N=9)
//! - [`group`] — `Group` actor; one tokio task per active group (V18)
//! - [`registry`] — `GroupRegistry` actor; routes Join requests
//! - [`ws`] — `/sync/v1/ws` extended path handler (Jellyfin /socket
//!   bridge lives in `pharos-server::api::jellyfin::socket`)
//! - [`host`] — `TokenResolver` trait the server impls so the WS
//!   handler can authenticate without depending on the server crate

pub mod clock;
pub mod group;
pub mod host;
pub mod messages;
pub mod registry;
pub mod ws;

pub use host::TokenResolver;
pub use messages::{ClientMsg, ErrorCode, GroupId, MemberId, MemberSummary, ServerMsg};
pub use registry::GroupRegistry;
