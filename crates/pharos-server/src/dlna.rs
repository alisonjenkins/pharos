//! DLNA / UPnP MediaServer phase 1 (T48).
//!
//! HTTP-level XML + SOAP handlers for the `MediaServer:1` device.
//! Pure helpers (device description, Browse response, SCPD schemas)
//! live in `pharos-discovery::dlna_xml`; this module owns the actix
//! glue + the `AppState`-bound side of the handler (server id check,
//! media store lookup).

use crate::state::AppState;
use actix_web::{web, HttpRequest, HttpResponse};
use pharos_core::MediaStore;
use pharos_discovery::dlna_xml::{
    browse_response_xml, device_description_xml, empty_soap_response, extract_xml_tag,
    parse_browse_u32, CONNECTION_MANAGER_SCPD, CONTENT_DIRECTORY_SCPD,
};

pub fn register(cfg: &mut web::ServiceConfig) {
    // T31 lowercase routes — LowercasePath middleware handles
    // PascalCase variants. UPnP clients send mixed case for both
    // /Dlna/{id}/... and the per-service paths.
    cfg.route(
        "/dlna/{server_id}/description.xml",
        web::get().to(description),
    )
    .route(
        "/dlna/{server_id}/contentdirectory/scpd.xml",
        web::get().to(content_directory_scpd),
    )
    .route(
        "/dlna/{server_id}/contentdirectory/control",
        web::post().to(content_directory_control),
    )
    .route(
        "/dlna/{server_id}/contentdirectory/events",
        web::route().to(events_stub),
    )
    .route(
        "/dlna/{server_id}/connectionmanager/scpd.xml",
        web::get().to(connection_manager_scpd),
    )
    .route(
        "/dlna/{server_id}/connectionmanager/control",
        web::post().to(connection_manager_control),
    );
}

async fn description(
    req: HttpRequest,
    state: web::Data<AppState>,
    path: web::Path<String>,
) -> HttpResponse {
    let server_id = path.into_inner();
    if server_id != state.server_id {
        return HttpResponse::NotFound().body("");
    }
    let base = base_url_from_request(&req);
    let body = device_description_xml(&state.server_id, &state.server_name, &base);
    HttpResponse::Ok()
        .content_type("text/xml; charset=\"utf-8\"")
        .body(body)
}

async fn content_directory_scpd(
    state: web::Data<AppState>,
    path: web::Path<String>,
) -> HttpResponse {
    if path.into_inner() != state.server_id {
        return HttpResponse::NotFound().body("");
    }
    HttpResponse::Ok()
        .content_type("text/xml; charset=\"utf-8\"")
        .body(CONTENT_DIRECTORY_SCPD)
}

async fn connection_manager_scpd(
    state: web::Data<AppState>,
    path: web::Path<String>,
) -> HttpResponse {
    if path.into_inner() != state.server_id {
        return HttpResponse::NotFound().body("");
    }
    HttpResponse::Ok()
        .content_type("text/xml; charset=\"utf-8\"")
        .body(CONNECTION_MANAGER_SCPD)
}

async fn content_directory_control(
    req: HttpRequest,
    state: web::Data<AppState>,
    path: web::Path<String>,
    body: web::Bytes,
) -> HttpResponse {
    if path.into_inner() != state.server_id {
        return HttpResponse::NotFound().body("");
    }
    let body_str = std::str::from_utf8(&body).unwrap_or_default();
    let action = soap_action_name(&req);
    match action.as_deref() {
        Some("Browse") => {
            // Pull ObjectID + StartingIndex + RequestedCount. Pharos
            // doesn't model folders yet so any ObjectID resolves to
            // the flat library — but pagination is honoured so control
            // points that scroll a 50k library don't re-fetch the
            // whole DIDL response on every page.
            let object_id = extract_xml_tag(body_str, "ObjectID").unwrap_or_default();
            let starting_index = parse_browse_u32(body_str, "StartingIndex").unwrap_or(0);
            let requested_count = parse_browse_u32(body_str, "RequestedCount").unwrap_or(0);
            let items = match state.stores.list().await {
                Ok(v) => v,
                Err(e) => {
                    return HttpResponse::InternalServerError().body(e.to_string());
                }
            };
            let base = base_url_from_request(&req);
            let xml = browse_response_xml(
                &items,
                &object_id,
                &base,
                &state.server_id,
                starting_index,
                requested_count,
            );
            HttpResponse::Ok()
                .content_type("text/xml; charset=\"utf-8\"")
                .body(xml)
        }
        Some("GetSortCapabilities") | Some("GetSearchCapabilities") | Some("GetSystemUpdateID") => {
            HttpResponse::Ok()
                .content_type("text/xml; charset=\"utf-8\"")
                .body(empty_soap_response(&action.unwrap_or_default()))
        }
        _ => HttpResponse::BadRequest().body("unknown SOAPAction"),
    }
}

async fn connection_manager_control(
    req: HttpRequest,
    state: web::Data<AppState>,
    path: web::Path<String>,
    _body: web::Bytes,
) -> HttpResponse {
    if path.into_inner() != state.server_id {
        return HttpResponse::NotFound().body("");
    }
    let action = soap_action_name(&req).unwrap_or_default();
    HttpResponse::Ok()
        .content_type("text/xml; charset=\"utf-8\"")
        .body(empty_soap_response(&action))
}

async fn events_stub() -> HttpResponse {
    // UPnP GENA events — SUBSCRIBE/UNSUBSCRIBE. Phase 1 acknowledges
    // without actually delivering events.
    HttpResponse::Ok()
        .insert_header(("SID", "uuid:pharos-events-0"))
        .insert_header(("TIMEOUT", "Second-1800"))
        .finish()
}

/// Parse `SOAPACTION: "urn:schemas-upnp-org:service:ContentDirectory:1#Browse"`
/// → `Some("Browse")`. Lives in the server module because it depends
/// on `actix_web::HttpRequest`.
fn soap_action_name(req: &HttpRequest) -> Option<String> {
    let raw = req
        .headers()
        .get("SOAPACTION")
        .or_else(|| req.headers().get("SoapAction"))
        .or_else(|| req.headers().get("soapaction"))?;
    let s = raw.to_str().ok()?.trim().trim_matches('"');
    s.rsplit_once('#').map(|(_, name)| name.to_string())
}

fn base_url_from_request(req: &HttpRequest) -> String {
    let scheme = req.connection_info().scheme().to_string();
    let host = req.connection_info().host().to_string();
    format!("{scheme}://{host}")
}
