//! Jellyfin-web boot-time stubs (T-fix-31).
//!
//! jellyfin-web fetches a half-dozen endpoints on first paint to
//! populate dashboards + auth-provider pickers + notification
//! services. Pharos returns degenerate / static shapes here so the
//! console doesn't pile retry storms during normal use. None of these
//! exposes mutable state — they're shaped to make the client render
//! "nothing configured" without throwing.

use crate::api::jellyfin::auth_extractor::{auth_header_from_request, AuthUser};
use crate::state::AppState;
use actix_web::{web, HttpRequest, HttpResponse, Responder};

pub fn register(cfg: &mut web::ServiceConfig) {
    cfg
        // Plugin / package manager surfaces — none installed.
        .route("/packages", web::get().to(empty_array))
        .route("/repositories", web::get().to(empty_array))
        // Notification services — jellyfin-web's dashboard expects an
        // array for the picker; empty hides the section.
        .route("/notifications/services", web::get().to(empty_array))
        .route("/notifications/types", web::get().to(empty_array))
        .route("/notifications", web::get().to(notifications_summary))
        .route(
            "/notifications/{user_id}",
            web::get().to(notifications_user),
        )
        // Auth provider picker (admin dashboard). Pharos ships one
        // built-in provider; surface it so the dropdown isn't empty.
        .route("/auth/providers", web::get().to(auth_providers))
        .route("/auth/passwordresetproviders", web::get().to(empty_array))
        // Web configuration pages — plugin pages, empty.
        .route("/web/configurationpages", web::get().to(empty_array))
        // Items-tree empty endpoints jellyfin-web's player-pre-roll path
        // queries before playback. None of these have a Phase 1 source
        // of data; the empty shape stops the client from cascading 404s.
        .route("/items/{id}/intros", web::get().to(empty_items_result))
        .route("/items/{id}/localtrailers", web::get().to(empty_array))
        .route("/items/{id}/specialfeatures", web::get().to(empty_array))
        .route("/items/{id}/thememedia", web::get().to(theme_media))
        .route("/items/{id}/themesongs", web::get().to(empty_array))
        .route("/items/{id}/themevideos", web::get().to(empty_array))
        .route("/items/{id}/criticreviews", web::get().to(empty_array))
        // Wall-clock + uptime — jellyfin-web hits /GetUtcTime to skew
        // its session timer. Server uses this clock; we publish ours.
        .route("/getutctime", web::get().to(get_utc_time))
        // Client-side log / CRASH-report uploads — store + surface (B66).
        .route("/clientlog/document", web::post().to(client_log_document))
        // Stay-alive ping while playback is active.
        .route("/sessions/playing/ping", web::post().to(no_content));
}

/// `POST /ClientLog/Document` (B66) — a client (notably the native Android/TV
/// app) uploads a log or CRASH report as the raw request body. Previously
/// discarded (204 stub), which is why the Android TV's "crash report was sent"
/// had nothing on the server. Now surfaced in the SERVER log stream (kubectl /
/// Loki — where we actually read it) and, when a log dir is configured, written
/// to `<log_dir>/clientlog/`. Returns the filename (Jellyfin's
/// `ClientLogDocumentResponseDto`). Auth is best-effort (a crashing client may
/// not send a full header) — the report always lands.
async fn client_log_document(
    state: web::Data<AppState>,
    req: HttpRequest,
    body: web::Bytes,
) -> impl Responder {
    let auth = auth_header_from_request(&req);
    let client = auth.client.clone().unwrap_or_else(|| "client".into());
    let device = auth.device.clone().unwrap_or_default();
    let version = auth.version.clone().unwrap_or_default();
    let text = String::from_utf8_lossy(&body);
    // Cap the inline body so a huge upload can't flood the log; the full report
    // still lands on disk when a log dir is set.
    const INLINE_CAP: usize = 16 * 1024;
    let snippet: String = text.chars().take(INLINE_CAP).collect();
    let truncated = snippet.len() < text.len();
    tracing::warn!(
        client = %client, device = %device, version = %version,
        bytes = body.len(), truncated,
        "client log / crash report:\n{snippet}"
    );
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let safe_client: String = client
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let file_name = format!("{ts}_{safe_client}.log");
    if let Some(dir) = &state.log_dir {
        let cl = dir.join("clientlog");
        if tokio::fs::create_dir_all(&cl).await.is_ok() {
            let _ = tokio::fs::write(cl.join(&file_name), body.as_ref()).await;
        }
    }
    crate::api::jellyfin::wire::json(&pharos_jellyfin_api::dto::ClientLogDocumentResponseDto {
        file_name,
    })
}

async fn empty_array(_user: AuthUser) -> impl Responder {
    let empty: Vec<serde_json::Value> = Vec::new();
    crate::api::jellyfin::wire::json(&empty)
}

async fn empty_items_result(_user: AuthUser) -> impl Responder {
    crate::api::jellyfin::wire::json(&serde_json::json!({
        "Items": [],
        "TotalRecordCount": 0,
        "StartIndex": 0,
    }))
}

async fn no_content() -> impl Responder {
    HttpResponse::NoContent().finish()
}

async fn notifications_summary(_user: AuthUser) -> impl Responder {
    crate::api::jellyfin::wire::json(&serde_json::json!({
        "UnreadCount": 0,
        "TotalRecordCount": 0,
        "Notifications": [],
    }))
}

async fn notifications_user(_user: AuthUser) -> impl Responder {
    crate::api::jellyfin::wire::json(&serde_json::json!({
        "Items": [],
        "TotalRecordCount": 0,
    }))
}

async fn auth_providers(user: AuthUser) -> Result<actix_web::HttpResponse, actix_web::Error> {
    // jellyfin contract — admin-only.
    crate::api::jellyfin::admin::require_admin(&user)?;
    // Single built-in provider so the dashboard's auth-provider
    // dropdown isn't empty. Matches jellyfin-web's expected shape
    // (`Name` + `Id`).
    Ok(crate::api::jellyfin::wire::json(&serde_json::json!([
        {
            "Name": "Default",
            "Id": "Jellyfin.Server.Implementations.Users.DefaultAuthenticationProvider"
        }
    ])))
}

async fn theme_media(_user: AuthUser) -> impl Responder {
    crate::api::jellyfin::wire::json(&serde_json::json!({
        "ThemeVideosResult": {
            "Items": [],
            "TotalRecordCount": 0,
            "StartIndex": 0,
            "OwnerId": "",
        },
        "ThemeSongsResult": {
            "Items": [],
            "TotalRecordCount": 0,
            "StartIndex": 0,
            "OwnerId": "",
        },
        "SoundtrackSongsResult": {
            "Items": [],
            "TotalRecordCount": 0,
            "StartIndex": 0,
            "OwnerId": "",
        },
    }))
}

async fn get_utc_time() -> impl Responder {
    use std::time::{SystemTime, UNIX_EPOCH};
    // Millisecond precision is load-bearing for SyncPlay: the client derives
    // its clock offset from this timestamp, and whole-second precision leaves
    // ±1 s of error — enough to desync a group. Sample once, between the two
    // reported instants (reception ≈ transmission at this resolution).
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let iso = crate::api::jellyfin::dto::format_iso8601_ms(ms);
    // `/GetUtcTime` is unauthenticated — jellyfin-web hits it before
    // the user has a token to skew its internal clock.
    crate::api::jellyfin::wire::json(&serde_json::json!({
        "RequestReceptionTime": iso,
        "ResponseTransmissionTime": iso,
    }))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use actix_web::test;

    #[actix_web::test]
    async fn get_utc_time_emits_iso_pair() {
        let app = test::init_service(
            actix_web::App::new().route("/getutctime", web::get().to(get_utc_time)),
        )
        .await;
        let req = test::TestRequest::get().uri("/getutctime").to_request();
        let body = test::call_and_read_body(&app, req).await;
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let r = v["RequestReceptionTime"].as_str().unwrap();
        // Format: YYYY-MM-DDTHH:MM:SS.0000000Z
        assert!(r.contains('T') && r.ends_with('Z'), "iso shape: {r}");
        assert_eq!(r, v["ResponseTransmissionTime"]);
    }
}
