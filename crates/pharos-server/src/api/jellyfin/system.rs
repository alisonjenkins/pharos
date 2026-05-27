use crate::{
    api::jellyfin::{auth_extractor::AuthUser, dto::SystemInfoDto},
    state::AppState,
};
use actix_web::{error, web, HttpResponse, Responder};
use pharos_core::PreferenceStore;
use serde::Deserialize;

pub fn register(cfg: &mut web::ServiceConfig) {
    // T31: lowercase-only routes; `LowercasePath` middleware folds the
    // PascalCase requests jellyfin-web sends onto these.
    cfg.route("/system/info", web::get().to(system_info))
        .route("/system/info/public", web::get().to(system_info))
        .route("/system/configuration", web::get().to(system_configuration))
        .route("/system/endpoint", web::get().to(system_endpoint))
        .route("/displaypreferences/{id}", web::get().to(display_preferences))
        .route(
            "/displaypreferences/{id}",
            web::post().to(display_preferences_update),
        )
        .route(
            "/users/{user_id}/configuration",
            web::post().to(user_configuration_update),
        )
        .route("/playback/bitratetest", web::get().to(bitrate_test));
}

#[derive(serde::Deserialize)]
struct BitrateTestQuery {
    #[serde(rename = "Size")]
    #[serde(default = "default_bitrate_size")]
    size: usize,
}

fn default_bitrate_size() -> usize {
    500_000
}

async fn bitrate_test(q: web::Query<BitrateTestQuery>) -> impl Responder {
    // Real Jellyfin streams `Size` bytes for the client to measure
    // throughput. Phase-1 stub: return the exact byte count of zeros.
    let n = q.size.min(50 * 1024 * 1024); // cap at 50 MB so abuse can't DoS
    HttpResponse::Ok()
        .content_type("application/octet-stream")
        .body(vec![0u8; n])
}

async fn system_configuration() -> impl Responder {
    HttpResponse::Ok().json(serde_json::json!({
        "EnableMetrics": true,
        "EnableNormalizedItemByNameIds": true,
        "EnableCaseSensitiveItemIds": true,
        "EnableExternalContentInSuggestions": false,
        "DisableLiveTvChannelUserDataName": true,
        "ServerName": "pharos",
        "UICulture": "en-US",
        "PreferredMetadataLanguage": "en",
        "MetadataCountryCode": "US",
        "QuickConnectAvailable": false,
        "StartupWizardCompleted": true,
    }))
}

async fn system_endpoint() -> impl Responder {
    HttpResponse::Ok().json(serde_json::json!({
        "IsLocal": true,
        "IsInNetwork": true,
    }))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DisplayPrefsQuery {
    #[serde(default = "default_client")]
    client: String,
    #[serde(default)]
    #[allow(dead_code)]
    user_id: Option<String>,
}

fn default_client() -> String {
    "emby".into()
}

async fn display_preferences(
    state: web::Data<AppState>,
    user: AuthUser,
    path: web::Path<String>,
    q: web::Query<DisplayPrefsQuery>,
) -> Result<impl Responder, actix_web::Error> {
    let dp_id = path.into_inner();
    let stored = state
        .stores
        .get_display_preferences(user.0.id, &dp_id, &q.client)
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    let body = match stored {
        Some(json) => serde_json::from_str(&json).unwrap_or_else(|_| default_prefs(&dp_id, &q.client)),
        None => default_prefs(&dp_id, &q.client),
    };
    Ok(HttpResponse::Ok().json(body))
}

fn default_prefs(id: &str, client: &str) -> serde_json::Value {
    serde_json::json!({
        "Id": id,
        "ViewType": "",
        "SortBy": "SortName",
        "RememberIndexing": false,
        "PrimaryImageHeight": 0,
        "PrimaryImageWidth": 0,
        "CustomPrefs": {},
        "ScrollDirection": "Vertical",
        "ShowBackdrop": true,
        "RememberSorting": false,
        "SortOrder": "Ascending",
        "ShowSidebar": false,
        "Client": client,
    })
}

async fn display_preferences_update(
    state: web::Data<AppState>,
    user: AuthUser,
    path: web::Path<String>,
    q: web::Query<DisplayPrefsQuery>,
    body: web::Json<serde_json::Value>,
) -> Result<impl Responder, actix_web::Error> {
    let dp_id = path.into_inner();
    let json = serde_json::to_string(&body.into_inner())
        .map_err(|e| error::ErrorBadRequest(e.to_string()))?;
    state
        .stores
        .set_display_preferences(user.0.id, &dp_id, &q.client, &json)
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    Ok(HttpResponse::NoContent().finish())
}

/// Reconstruct the URL clients should advertise when telling other
/// clients to connect here. Derived from the request's Host header
/// + scheme — the canonical Jellyfin "use whatever URL the client
/// just hit you on" pattern. Used by casting / SyncPlay clients
/// to publish a reachable URL to peer sessions.
fn derive_local_address(req: &actix_web::HttpRequest) -> String {
    let conn = req.connection_info();
    let scheme = conn.scheme();
    let host = conn.host();
    format!("{scheme}://{host}")
}

async fn user_configuration_update(
    state: web::Data<AppState>,
    user: AuthUser,
    path: web::Path<String>,
    body: web::Json<serde_json::Value>,
) -> Result<impl Responder, actix_web::Error> {
    // V9 spirit: bearer must match path.
    let bearer = user.0.id.0.simple().to_string();
    if path.into_inner() != bearer {
        return Err(error::ErrorForbidden("user mismatch"));
    }
    let json = serde_json::to_string(&body.into_inner())
        .map_err(|e| error::ErrorBadRequest(e.to_string()))?;
    state
        .stores
        .set_user_configuration(user.0.id, &json)
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    Ok(HttpResponse::NoContent().finish())
}

/// We advertise a Jellyfin schema version >= jellyfin-web's
/// `minServerVersion` so unmodified clients accept us. The pharos
/// internal version (`state.version`) is unaffected — see `/info` for
/// the real one. Bump this when targeting a newer jellyfin-web build.
const ADVERTISED_JELLYFIN_VERSION: &str = "10.11.0";

async fn system_info(
    state: web::Data<AppState>,
    req: actix_web::HttpRequest,
) -> impl Responder {
    let _ = state.version;
    HttpResponse::Ok().json(SystemInfoDto {
        id: state.server_id.clone(),
        server_name: state.server_name.clone(),
        version: ADVERTISED_JELLYFIN_VERSION.to_string(),
        product_name: "Jellyfin Server",
        operating_system: std::env::consts::OS,
        local_address: derive_local_address(&req),
        startup_wizard_completed: true,
        cast_receiver_id: "F007D354",
        operating_system_display_name: "pharos",
        has_pending_restart: false,
        is_shutting_down: false,
        supports_library_monitor: false,
        web_socket_port_number: 8096,
        completed_installations: vec![],
        can_self_restart: false,
        can_launch_web_browser: false,
        program_data_path: "/var/lib/pharos",
        web_path: "/usr/share/jellyfin-web",
        items_by_name_path: "/var/lib/pharos/itemsbyname",
        cache_path: "/var/lib/pharos/cache",
        log_path: "/var/log/pharos",
        internal_metadata_path: "/var/lib/pharos/metadata",
        transcoding_temp_path: "/var/lib/pharos/transcodes",
        has_update_available: false,
        encoder_location: "System",
        system_architecture: std::env::consts::ARCH,
    })
}
