use crate::{api::jellyfin::dto::SystemInfoDto, state::AppState};
use actix_web::{web, HttpResponse, Responder};

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

async fn display_preferences(path: actix_web::web::Path<String>) -> impl Responder {
    let id = path.into_inner();
    HttpResponse::Ok().json(serde_json::json!({
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
        "Client": "emby",
    }))
}

async fn display_preferences_update() -> impl Responder {
    HttpResponse::NoContent().finish()
}

/// We advertise a Jellyfin schema version >= jellyfin-web's
/// `minServerVersion` so unmodified clients accept us. The pharos
/// internal version (`state.version`) is unaffected — see `/info` for
/// the real one. Bump this when targeting a newer jellyfin-web build.
const ADVERTISED_JELLYFIN_VERSION: &str = "10.11.0";

async fn system_info(state: web::Data<AppState>) -> impl Responder {
    let _ = state.version;
    HttpResponse::Ok().json(SystemInfoDto {
        id: state.server_id.clone(),
        server_name: state.server_name.clone(),
        version: ADVERTISED_JELLYFIN_VERSION.to_string(),
        product_name: "Jellyfin Server",
        operating_system: std::env::consts::OS,
        local_address: String::new(),
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
