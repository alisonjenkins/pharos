//! Jellyfin `/LiveTV` API surface (T47).
//!
//! Phase 1 wires read-only endpoints — Channels + Programs from the
//! configured `TunerBackend`. Recordings / Timers / SeriesTimers
//! return empty so jellyfin-web's Live TV view renders without
//! throwing. /LiveTv/Channels/{id}/Stream redirects to the upstream
//! `stream_url`; transcode-on-tune is its own follow-up.

use crate::{api::jellyfin::auth_extractor::AuthUser, state::AppState};
use actix_web::{http::header, web, HttpResponse, Responder};
use pharos_core::TunerBackend;
use serde::Deserialize;

pub fn register(cfg: &mut web::ServiceConfig) {
    // T31 lowercase canonical routes; middleware folds PascalCase.
    cfg.route("/livetv/info", web::get().to(info))
        .route("/livetv/channels", web::get().to(channels))
        .route("/livetv/channels/{id}", web::get().to(channel))
        .route("/livetv/channels/{id}/stream", web::get().to(stream))
        .route(
            "/livetv/channels/{id}/images/primary",
            web::get().to(channel_image_primary),
        )
        .route("/livetv/programs", web::get().to(programs))
        // jellyfin-web's home "On Now" section fetches this UNGUARDED (no
        // .catch): a 404 rejection propagates into the home page's
        // Promise.all and kills EVERY home section (Next Up, Resume,
        // Latest — B17). Empty result → the section hides itself cleanly.
        .route(
            "/livetv/programs/recommended",
            web::get().to(empty_items_result),
        )
        .route("/livetv/recordings", web::get().to(empty_items_result))
        .route("/livetv/timers", web::get().to(empty_items_result))
        .route("/livetv/seriestimers", web::get().to(empty_items_result))
        .route("/livetv/tunerhosts", web::get().to(empty_items_result));
}

/// 302 to the channel's M3U logo URL. Jellyfin clients render the
/// channel grid by fetching `/Items/{ImageTag}/Images/Primary` (or the
/// equivalent live-tv route) — without this redirect the grid shows a
/// broken-image placeholder even though every parsed channel has a
/// `tvg-logo` URL.
/// Public on purpose — `<img src=…>` cannot inject auth headers and
/// jellyfin-web does not append `api_key` to channel logo URLs. Matches
/// the also-public `/items/{id}/images/{type}` route (Jellyfin parity).
async fn channel_image_primary(
    state: web::Data<AppState>,
    path: web::Path<String>,
) -> impl Responder {
    let Some(backend) = state.live_tv.as_ref() else {
        return HttpResponse::NotFound().body("");
    };
    let id = path.into_inner();
    let chs = match backend.channels().await {
        Ok(v) => v,
        Err(e) => return HttpResponse::InternalServerError().body(e.to_string()),
    };
    let Some(ch) = chs.into_iter().find(|c| c.id == id) else {
        return HttpResponse::NotFound().body("");
    };
    let Some(logo) = ch.logo_url else {
        return HttpResponse::NotFound().body("");
    };
    HttpResponse::Found()
        .insert_header((header::LOCATION, logo))
        .finish()
}

async fn info(state: web::Data<AppState>, _user: AuthUser) -> impl Responder {
    let enabled = state.live_tv.is_some();
    HttpResponse::Ok().json(serde_json::json!({
        "IsEnabled": enabled,
        "Services": if enabled { vec!["M3U/XMLTV"] } else { vec![] },
        "EnabledUsers": [],
    }))
}

async fn channels(state: web::Data<AppState>, _user: AuthUser) -> impl Responder {
    let Some(backend) = state.live_tv.as_ref() else {
        return HttpResponse::Ok().json(empty_items_result_value());
    };
    let chs = match backend.channels().await {
        Ok(v) => v,
        Err(e) => return HttpResponse::InternalServerError().body(e.to_string()),
    };
    let server_id = state.server_id.clone();
    let items: Vec<serde_json::Value> = chs
        .into_iter()
        .map(|c| {
            serde_json::json!({
                "Id": c.id,
                "Name": c.name,
                "ChannelNumber": c.number,
                "Type": "Channel",
                "MediaType": "Video",
                "ServerId": server_id,
                "IsFolder": false,
                "ImageTags": if c.logo_url.is_some() {
                    serde_json::json!({ "Primary": c.id })
                } else {
                    serde_json::json!({})
                },
                "ChannelGroupName": c.group_title,
            })
        })
        .collect();
    let total = items.len() as u32;
    HttpResponse::Ok().json(serde_json::json!({
        "Items": items,
        "TotalRecordCount": total,
        "StartIndex": 0,
    }))
}

async fn channel(
    state: web::Data<AppState>,
    _user: AuthUser,
    path: web::Path<String>,
) -> impl Responder {
    let Some(backend) = state.live_tv.as_ref() else {
        return HttpResponse::NotFound().body("");
    };
    let id = path.into_inner();
    let chs = match backend.channels().await {
        Ok(v) => v,
        Err(e) => return HttpResponse::InternalServerError().body(e.to_string()),
    };
    let Some(ch) = chs.into_iter().find(|c| c.id == id) else {
        return HttpResponse::NotFound().body("");
    };
    HttpResponse::Ok().json(serde_json::json!({
        "Id": ch.id,
        "Name": ch.name,
        "ChannelNumber": ch.number,
        "Type": "Channel",
        "MediaType": "Video",
        "ServerId": state.server_id,
        "ChannelGroupName": ch.group_title,
    }))
}

async fn stream(
    state: web::Data<AppState>,
    _user: AuthUser,
    path: web::Path<String>,
) -> impl Responder {
    let Some(backend) = state.live_tv.as_ref() else {
        return HttpResponse::NotFound().body("");
    };
    let id = path.into_inner();
    let chs = match backend.channels().await {
        Ok(v) => v,
        Err(e) => return HttpResponse::InternalServerError().body(e.to_string()),
    };
    let Some(ch) = chs.into_iter().find(|c| c.id == id) else {
        return HttpResponse::NotFound().body("");
    };
    // 302 to the upstream URL. Jellyfin clients follow redirects on
    // playback URLs. Transcode-on-tune lands with T47 phase 2.
    HttpResponse::Found()
        .insert_header((header::LOCATION, ch.stream_url))
        .finish()
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProgramsQuery {
    /// ISO-8601 or unix-ms — we parse both via a permissive helper.
    #[serde(default)]
    min_start_date: Option<String>,
    #[serde(default)]
    max_end_date: Option<String>,
    #[serde(default = "default_window_hours")]
    window_hours: u64,
}

fn default_window_hours() -> u64 {
    6
}

async fn programs(
    state: web::Data<AppState>,
    _user: AuthUser,
    q: web::Query<ProgramsQuery>,
) -> impl Responder {
    let Some(backend) = state.live_tv.as_ref() else {
        return HttpResponse::Ok().json(empty_items_result_value());
    };
    let now_ms = unix_ms_now();
    let start_ms = q
        .min_start_date
        .as_deref()
        .and_then(parse_time_ms)
        .unwrap_or(now_ms);
    let end_ms = q
        .max_end_date
        .as_deref()
        .and_then(parse_time_ms)
        .unwrap_or_else(|| start_ms + q.window_hours * 3_600_000);
    let programs = match backend.programs(start_ms, end_ms).await {
        Ok(v) => v,
        Err(e) => return HttpResponse::InternalServerError().body(e.to_string()),
    };
    let server_id = state.server_id.clone();
    let items: Vec<serde_json::Value> = programs
        .into_iter()
        .map(|p| {
            serde_json::json!({
                "Id": format!("{}-{}", p.channel_id, p.start_unix_ms),
                "Name": p.title,
                "Overview": p.description,
                "ChannelId": p.channel_id,
                "Type": "Program",
                "ServerId": server_id,
                "StartDate": unix_ms_to_iso8601(p.start_unix_ms),
                "EndDate": unix_ms_to_iso8601(p.end_unix_ms),
                "IsLive": false,
                "IsKids": false,
                "IsMovie": false,
                "IsNews": false,
                "IsSeries": false,
                "IsSports": false,
            })
        })
        .collect();
    let total = items.len() as u32;
    HttpResponse::Ok().json(serde_json::json!({
        "Items": items,
        "TotalRecordCount": total,
        "StartIndex": 0,
    }))
}

async fn empty_items_result(_user: AuthUser) -> impl Responder {
    HttpResponse::Ok().json(empty_items_result_value())
}

fn empty_items_result_value() -> serde_json::Value {
    serde_json::json!({
        "Items": [],
        "TotalRecordCount": 0,
        "StartIndex": 0,
    })
}

fn unix_ms_now() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Parse Jellyfin's loose `time` query — accepts unix-ms ints and
/// ISO-8601 strings (no full RFC 3339 — UTC only, 4-digit year).
fn parse_time_ms(s: &str) -> Option<u64> {
    if let Ok(ms) = s.parse::<u64>() {
        return Some(ms);
    }
    // YYYY-MM-DDTHH:MM:SS(.fff)?Z
    let len = s.len();
    if len < 19 {
        return None;
    }
    let y: i64 = s[0..4].parse().ok()?;
    let m: u32 = s[5..7].parse().ok()?;
    let d: u32 = s[8..10].parse().ok()?;
    let h: u32 = s[11..13].parse().ok()?;
    let min: u32 = s[14..16].parse().ok()?;
    let sec: u32 = s[17..19].parse().ok()?;
    let unix = ymd_hms_to_unix(y, m, d, h, min, sec)?;
    Some((unix as u64).saturating_mul(1000))
}

fn unix_ms_to_iso8601(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    let (y, m, d, h, mi, s) = unix_to_ymd_hms(secs);
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}.000Z")
}

fn unix_to_ymd_hms(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let day = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let h = (tod / 3600) as u32;
    let mi = ((tod % 3600) / 60) as u32;
    let s = (tod % 60) as u32;
    // Inverse of Hinnant's civil-from-days.
    let z = day + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m_raw = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m_raw <= 2 { y + 1 } else { y };
    (y, m_raw as u32, d, h, mi, s)
}

fn ymd_hms_to_unix(y: i64, m: u32, d: u32, h: u32, mi: u32, s: u32) -> Option<i64> {
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m as i64 - 3 } else { m as i64 + 9 };
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs_of_day = (h as i64) * 3600 + (mi as i64) * 60 + (s as i64);
    days.checked_mul(86_400)?.checked_add(secs_of_day)
}
