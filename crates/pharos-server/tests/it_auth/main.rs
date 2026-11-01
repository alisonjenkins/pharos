//! Consolidated integration-test binary: one link instead of 17; each `mod` is a former tests/*.rs.
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "../common/store_calls.rs"]
mod store_calls;

mod auth_header_fuzz;
mod auth_rejected_metrics;
mod concurrent_auth_audit;
mod cross_user_isolation;
mod jellyfin_admin;
mod jellyfin_quick_connect;
mod jellyfin_session_logout;
mod jellyfin_sessions;
mod jellyfin_user_password;
mod last_admin_guard_audit;
mod login_throttle;
mod no_path_leak;
mod no_token_echo;
mod password_change_audit;
mod password_debug_audit;
mod route_auth_audit;
mod sessions_tenant_audit;
