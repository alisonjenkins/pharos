//! Group-sync subsystem. See `docs/group-sync-protocol.md` in the
//! `pharos` repo for the wire protocol.
//!
//! Layout:
//! - [`messages`] — `ClientMsg` / `ServerMsg` wire types
//! - [`clock`] — per-member rolling offset estimator (median over N=9)
//! - [`group`] — `Group` actor; one tokio task per active group (V18)
//! - [`registry`] — `GroupRegistry` actor; routes Join requests
//! - [`hub`] — `SessionHub`; bridges HTTP SyncPlay commands (keyed by
//!   `deviceId`) to the per-socket member sinks
//! - [`ws`] — `/sync/v1/ws` extended path handler (Jellyfin /socket
//!   bridge lives in `pharos-server::api::jellyfin::socket`)
//! - [`host`] — `TokenResolver` trait the server impls so the WS
//!   handler can authenticate without depending on the server crate

pub mod bus;
pub mod bus_delivery;
pub mod clock;
pub mod delivery;
pub mod distributed;
pub mod group;
pub mod host;
pub mod hub;
pub mod messages;
pub mod persistence;
pub mod registry;
pub mod ws;

pub use bus::{BusError, LocalSyncBus, SyncBus};
pub use bus_delivery::{spawn_ingress, BusDelivery, BusMsg};
pub use delivery::{Delivery, LocalDelivery, MemberSinks};
pub use distributed::{CommandSink, Distributed, HydrationSource, OwnershipSource};
pub use host::TokenResolver;
pub use hub::{ResolvedSession, SessionHub};
pub use messages::{ClientMsg, ErrorCode, GroupId, MemberId, MemberSummary, ServerMsg};
pub use persistence::GroupPersistence;
pub use registry::GroupRegistry;
