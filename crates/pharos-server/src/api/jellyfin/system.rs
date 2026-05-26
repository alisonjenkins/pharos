use crate::{api::jellyfin::dto::SystemInfoDto, state::AppState};
use actix_web::{web, HttpResponse, Responder};

pub fn register(cfg: &mut web::ServiceConfig) {
    for path in ["/System/Info", "/system/info"] {
        cfg.route(path, web::get().to(system_info));
    }
    for path in ["/System/Info/Public", "/system/info/public"] {
        cfg.route(path, web::get().to(system_info));
    }
    for path in ["/System/Configuration", "/system/configuration"] {
        cfg.route(path, web::get().to(system_configuration));
    }
    for path in ["/System/Endpoint", "/system/endpoint"] {
        cfg.route(path, web::get().to(system_endpoint));
    }
    for path in ["/DisplayPreferences/{id}", "/displaypreferences/{id}"] {
        cfg.route(path, web::get().to(display_preferences))
            .route(path, web::post().to(display_preferences_update));
    }
    for path in ["/Playback/BitrateTest", "/playback/bitratetest"] {
        cfg.route(path, web::get().to(bitrate_test));
    }
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
    })
}
