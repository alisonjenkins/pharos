//! Consolidated integration-test binary for the cast seat (009): one link
//! instead of 12; each `mod` is a former tests/*.rs.
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "../common/cast_rig.rs"]
mod cast_rig;
#[path = "../common/mod.rs"]
mod common;

mod cast_commands;
mod cast_end;
mod cast_item_type;
mod cast_mock_tv;
mod cast_order;
mod cast_restart_recovery;
mod cast_service;
mod cast_start;
mod cast_token;
mod cast_token_renewal;
mod cast_token_scope;
mod cast_virtual_session;
