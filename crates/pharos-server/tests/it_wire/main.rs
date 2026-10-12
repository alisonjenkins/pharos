//! Consolidated integration-test binary: one link instead of 8; each `mod` is a former tests/*.rs.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod dlna;
mod edge_inputs;
mod jellyfin_api;
mod jellyfin_socket;
mod jellyfin_socket_push;
mod route_smoke;
mod wire_baseline;
mod wire_invariants;
