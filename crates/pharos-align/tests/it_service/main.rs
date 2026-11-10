//! The song pipeline with a scripted decoder and model, mirroring
//! `aligner/tests/test_server.py`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod engine;
mod http;
mod spans;
