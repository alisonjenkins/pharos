//! Consolidated integration-test binary: one link instead of 4; each `mod` is a former tests/*.rs.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod distributed_sync;
mod group_conformance;
mod group_drift;
mod hub_integration;
