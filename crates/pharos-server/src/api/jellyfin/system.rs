use crate::{api::jellyfin::dto::SystemInfoDto, state::AppState};
use actix_web::{web, HttpResponse, Responder};

pub fn register(cfg: &mut web::ServiceConfig) {
    cfg.route("/System/Info", web::get().to(system_info))
        .route("/System/Info/Public", web::get().to(system_info));
}

async fn system_info(state: web::Data<AppState>) -> impl Responder {
    HttpResponse::Ok().json(SystemInfoDto {
        id: state.server_id.clone(),
        server_name: state.server_name.clone(),
        version: state.version.to_string(),
        product_name: "Jellyfin Server",
        operating_system: std::env::consts::OS,
        local_address: String::new(),
        startup_wizard_completed: true,
    })
}
