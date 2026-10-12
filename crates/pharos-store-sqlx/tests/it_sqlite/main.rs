//! Consolidated integration-test binary: one link instead of 9; each `mod` is a former tests/*.rs.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod auth_sqlite;
mod bg_leadership;
mod book_roundtrip;
#[path = "../common/mod.rs"]
mod common;
mod language_index;
mod media_fts_facets;
mod media_query;
mod media_segments;
mod probe_roundtrip;
mod put_batch;
mod sqlite_store;
