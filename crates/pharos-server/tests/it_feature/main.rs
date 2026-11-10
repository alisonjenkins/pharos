//! Consolidated integration-test binary: one link instead of 6; each `mod` is a former tests/*.rs.
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "../common/mod.rs"]
mod common;

mod admin_metadata_edit;
mod attachment_headers;
mod auth_cookie;
mod item_access;
mod jellyfin_feature_inventory;
mod jellyfin_feature_library_options;
mod jellyfin_feature_metadata;
mod jellyfin_feature_user_policy;
mod opensubtitles;
mod playback_access;
mod playlists;
mod styled_subtitle;
mod subtitle_fonts;
mod thin_endpoints;
mod user_policy_login;
