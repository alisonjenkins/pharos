use crate::{api::jellyfin::dto::SystemInfoDto, state::AppState};
use actix_web::{web, HttpResponse, Responder};

pub fn register(cfg: &mut web::ServiceConfig) {
    cfg.route("/System/Info", web::get().to(system_info))
        .route("/System/Info/Public", web::get().to(system_info));
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
