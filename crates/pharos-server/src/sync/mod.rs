//! Group-sync subsystem. See `docs/group-sync-protocol.md`.
//!
//! Layout:
//! - [`messages`] — `ClientMsg` / `ServerMsg` wire types
//! - [`clock`] — per-member rolling offset estimator (median over N=9)
//! - [`group`] — `Group` actor; one tokio task per active group (V18)
//! - [`registry`] — `GroupRegistry` actor; routes Join requests
//! - [`ws`] — `/sync/v1/ws` extended path handler (Jellyfin /socket bridge
//!   lands in T16 phase 2)

pub mod clock;
pub mod group;
pub mod messages;
pub mod registry;
pub mod ws;

pub use messages::{ClientMsg, ErrorCode, GroupId, MemberId, MemberSummary, ServerMsg};
pub use registry::GroupRegistry;
