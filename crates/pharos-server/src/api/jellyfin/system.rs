use crate::api::jellyfin::ci_query::CiQuery;
use crate::{
    api::jellyfin::{auth_extractor::AuthUser, dto::SystemInfoDto},
    state::AppState,
};
use actix_web::{error, web, HttpResponse, Responder};
use pharos_core::{PreferenceStore, TokenStore};
use pharos_store_sqlx::ServerConfigStore;
use serde::Deserialize;

pub fn register(cfg: &mut web::ServiceConfig) {
    // T31: lowercase-only routes; `LowercasePath` middleware folds the
    // PascalCase requests jellyfin-web sends onto these.
    cfg.route("/system/info", web::get().to(system_info))
        .route("/system/info/public", web::get().to(system_info_public))
        // Dashboard landing page's storage panel. pharos doesn't track disk
        // usage; an empty `Folders` list renders the panel cleanly (a 404 left
        // it blank + logged an error).
        .route("/system/info/storage", web::get().to(system_storage))
        .route("/system/configuration", web::get().to(system_configuration))
        // Named config sub-sections the dashboard fetches (Networking →
        // `network`, Live TV → `livetv`, …). Without a GET these 404'd and the
        // section threw instead of rendering its (default) form.
        .route(
            "/system/configuration/{key}",
            web::get().to(system_configuration_named),
        )
        // 10.11 Backups dashboard page lists backups; pharos has no backup
        // feature, so an empty list renders the "no backups" state cleanly.
        .route("/backup", web::get().to(empty_backup_list))
        .route("/system/endpoint", web::get().to(system_endpoint))
        .route(
            "/displaypreferences/{id}",
            web::get().to(display_preferences),
        )
        .route(
            "/displaypreferences/{id}",
            web::post().to(display_preferences_update),
        )
        .route(
            "/users/{user_id}/configuration",
            web::post().to(user_configuration_update),
        )
        .route("/playback/bitratetest", web::get().to(bitrate_test))
        // Localization endpoints — clients pull these once at
        // startup to populate language / country dropdowns. Empty
        // arrays keep the dropdown rendering ("no choices") without
        // 404 cascade. Real localization data lands when pharos has
        // a settings UI.
        .route(
            "/localization/cultures",
            web::get().to(localization_cultures),
        )
        .route(
            "/localization/countries",
            web::get().to(localization_countries),
        )
        .route(
            "/localization/parentalratings",
            web::get().to(localization_parental_ratings),
        )
        .route("/localization/options", web::get().to(localization_options))
        // Per-client device listing — admin dashboard reads this.
        .route("/devices", web::get().to(devices_list))
        .route("/devices", web::delete().to(delete_device))
        .route("/devices/info", web::get().to(devices_list))
        .route("/devices/options", web::post().to(device_options))
        // MediaSegments (intro/outro skip) — empty stub keeps the
        // client's pre-playback fetch from cascading 404s.
        .route(
            "/mediasegments/{item_id}",
            web::get().to(media_segments_stub),
        );
}

#[derive(serde::Deserialize)]
struct BitrateTestQuery {
    #[serde(default = "default_bitrate_size")]
    size: usize,
}

fn default_bitrate_size() -> usize {
    500_000
}

async fn bitrate_test(q: CiQuery<BitrateTestQuery>) -> impl Responder {
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
        "QuickConnectAvailable": true,
        "StartupWizardCompleted": true,
    }))
}

/// Default objects for the dashboard's named config sub-sections. pharos
/// doesn't persist these (its config is the toml file), but returning a
/// well-shaped default lets each dashboard page render its form fields with
/// sensible values instead of throwing on a 404 / undefined access.
async fn system_configuration_named(
    state: web::Data<AppState>,
    path: web::Path<String>,
) -> Result<impl Responder, actix_web::Error> {
    let key = path.into_inner().to_ascii_lowercase();
    let mut body = match key.as_str() {
        "network" => serde_json::json!({
            "BaseUrl": "",
            "EnableHttps": false,
            "RequireHttps": false,
            "InternalHttpPort": 8096,
            "InternalHttpsPort": 8920,
            "PublicHttpPort": 8096,
            "PublicHttpsPort": 8920,
            "AutoDiscovery": false,
            "EnableUPnP": false,
            "EnableRemoteAccess": true,
            "EnableIPv4": true,
            "EnableIPv6": false,
            "IgnoreVirtualInterfaces": true,
            "EnablePublishedServerUriByRequest": false,
            "LocalNetworkSubnets": [],
            "LocalNetworkAddresses": [],
            "KnownProxies": [],
            "RemoteIPFilter": [],
            "IsRemoteIPFilterBlacklist": false,
            "PublishedServerUriBySubnet": [],
            "VirtualInterfaceNames": ["veth"],
        }),
        "livetv" => serde_json::json!({
            "GuideDays": null,
            "EnableMovieProviders": true,
            "RecordingPath": "",
            "MovieRecordingPath": "",
            "SeriesRecordingPath": "",
            "EnableRecordingSubfolders": false,
            "EnableOriginalAudioWithEncodedRecordings": false,
            "TunerHosts": [],
            "ListingProviders": [],
        }),
        "encoding" => serde_json::json!({
            "EncodingThreadCount": -1,
            "HardwareAccelerationType": "none",
            "EnableThrottling": false,
            "TranscodingTempPath": "",
            "AllowHevcEncoding": true,
        }),
        // Unknown key → an empty object (still valid JSON the form can read).
        _ => serde_json::json!({}),
    };
    // T72 — overlay any persisted section blob on the shaped defaults so a
    // dashboard change (POST to the same key) is reflected here and survives
    // restart. Stored keys win; keys pharos didn't persist keep their default.
    if let Some(raw) = state
        .stores
        .load_named_config(&key)
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?
    {
        if let (Some(base), Ok(serde_json::Value::Object(stored))) = (
            body.as_object_mut(),
            serde_json::from_str::<serde_json::Value>(&raw),
        ) {
            for (k, v) in stored {
                base.insert(k, v);
            }
        }
    }
    Ok(HttpResponse::Ok().json(body))
}

async fn empty_backup_list() -> impl Responder {
    HttpResponse::Ok().json(serde_json::json!([]))
}

async fn system_storage() -> impl Responder {
    // `SystemStorageInfo` shape — jellyfin-web maps over `.Folders`. Empty is
    // valid (renders "no data" rather than throwing). pharos doesn't surface
    // per-folder free/used space yet.
    HttpResponse::Ok().json(serde_json::json!({ "Folders": [] }))
}

async fn system_endpoint() -> impl Responder {
    HttpResponse::Ok().json(serde_json::json!({
        "IsLocal": true,
        "IsInNetwork": true,
    }))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
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
    q: CiQuery<DisplayPrefsQuery>,
) -> Result<impl Responder, actix_web::Error> {
    let dp_id = path.into_inner();
    let stored = state
        .stores
        .get_display_preferences(user.0.id, &dp_id, &q.client)
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    let body = match stored {
        Some(json) => {
            serde_json::from_str(&json).unwrap_or_else(|_| default_prefs(&dp_id, &q.client))
        }
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
    q: CiQuery<DisplayPrefsQuery>,
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
/// clients to connect here.
///
/// Derived from the request's Host header + scheme — the canonical
/// Jellyfin "use whatever URL the client just hit you on" pattern.
/// Used by casting / SyncPlay clients to publish a reachable URL to
/// peer sessions.
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

/// `/System/Info/Public` — UNAUTHENTICATED. Serve only Jellyfin's
/// `PublicSystemInfo` subset, exactly like the real server. This previously
/// aliased the full (authenticated) `/System/Info` handler, exposing the
/// whole SystemInfo shape — the path values were hardcoded placeholders, not
/// real paths, but an anonymous caller has no business seeing server
/// internals at all (V9 hygiene).
async fn system_info_public(
    state: web::Data<AppState>,
    req: actix_web::HttpRequest,
) -> impl Responder {
    let branding = state.effective_branding().await;
    let server_name = branding
        .server_name
        .unwrap_or_else(|| state.server_name.clone());
    // Typed DTO + SIMD (sonic-rs) serialization — replaces the hand-built
    // json! so every wire field is compile-visible and auditable (the
    // json!→DTO sweep; kotlin-strictness class B13/B63/B64).
    crate::api::jellyfin::wire::json(&pharos_jellyfin_api::dto::PublicSystemInfoDto {
        id: state.server_id.clone(),
        server_name,
        version: ADVERTISED_JELLYFIN_VERSION.to_string(),
        product_name: "Jellyfin Server",
        operating_system: std::env::consts::OS,
        local_address: derive_local_address(&req),
        startup_wizard_completed: true,
    })
}

async fn system_info(
    state: web::Data<AppState>,
    req: actix_web::HttpRequest,
    _user: crate::api::jellyfin::auth_extractor::AuthUser,
) -> impl Responder {
    let _ = state.version;
    let branding = state.effective_branding().await;
    let server_name = branding
        .server_name
        .unwrap_or_else(|| state.server_name.clone());
    HttpResponse::Ok().json(SystemInfoDto {
        id: state.server_id.clone(),
        server_name,
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

/// A curated list of the languages pharos clients are most likely to
/// pick for audio + subtitle preferences. Pharos doesn't ship the
/// full ICU locale catalogue — that would bloat the binary for a
/// dropdown — but the slice covers ~95% of clients in practice. If a
/// user needs an exotic locale they can type it into the prefs view's
/// text field (which is the canonical source of truth).
async fn localization_cultures(_user: AuthUser) -> impl Responder {
    HttpResponse::Ok().json(LOCALIZATION_CULTURES)
}

pub(crate) const LOCALIZATION_CULTURES: &[Culture] = &[
    Culture::new("English", "en", "eng"),
    Culture::new("Spanish", "es", "spa"),
    Culture::new("French", "fr", "fre"),
    Culture::new("German", "de", "ger"),
    Culture::new("Italian", "it", "ita"),
    Culture::new("Portuguese", "pt", "por"),
    Culture::new("Dutch", "nl", "dut"),
    Culture::new("Russian", "ru", "rus"),
    Culture::new("Polish", "pl", "pol"),
    Culture::new("Turkish", "tr", "tur"),
    Culture::new("Czech", "cs", "cze"),
    Culture::new("Swedish", "sv", "swe"),
    Culture::new("Norwegian", "no", "nor"),
    Culture::new("Danish", "da", "dan"),
    Culture::new("Finnish", "fi", "fin"),
    Culture::new("Greek", "el", "gre"),
    Culture::new("Hungarian", "hu", "hun"),
    Culture::new("Romanian", "ro", "rum"),
    Culture::new("Arabic", "ar", "ara"),
    Culture::new("Hebrew", "he", "heb"),
    Culture::new("Hindi", "hi", "hin"),
    Culture::new("Japanese", "ja", "jpn"),
    Culture::new("Korean", "ko", "kor"),
    Culture::new("Chinese", "zh", "chi"),
    Culture::new("Vietnamese", "vi", "vie"),
    Culture::new("Thai", "th", "tha"),
    Culture::new("Ukrainian", "uk", "ukr"),
    Culture::new("Indonesian", "id", "ind"),
];

#[derive(Debug, Clone, Copy, serde::Serialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct Culture {
    name: &'static str,
    display_name: &'static str,
    // Jellyfin uses an all-caps `ISO` in these keys
    // (`TwoLetterISOLanguageName`), not the `Iso` that PascalCase would
    // produce from the snake_case field. Real clients key off the exact
    // casing, so rename explicitly.
    #[serde(rename = "TwoLetterISOLanguageName")]
    two_letter_iso_language_name: &'static str,
    #[serde(rename = "ThreeLetterISOLanguageName")]
    three_letter_iso_language_name: &'static str,
    #[serde(rename = "ThreeLetterISOLanguageNames")]
    three_letter_iso_language_names: &'static [&'static str],
}

impl Culture {
    const fn new(name: &'static str, two: &'static str, three: &'static str) -> Self {
        Self {
            name,
            display_name: name,
            two_letter_iso_language_name: two,
            three_letter_iso_language_name: three,
            three_letter_iso_language_names: &[],
        }
    }
}

async fn localization_countries(_user: AuthUser) -> impl Responder {
    HttpResponse::Ok().json(serde_json::json!([
        {
            "Name": "US",
            "DisplayName": "United States",
            "TwoLetterISORegionName": "US",
            "ThreeLetterISORegionName": "USA",
        }
    ]))
}

/// `GET /Localization/ParentalRatings` — the rating catalogue jellyfin-web's
/// user-policy editor loads for its "maximum allowed rating" picker. Each
/// `{ Name, Value }` pairs a rating label with Jellyfin's numeric score
/// (higher = more restrictive); the score is what a future `MaxParentalRating`
/// comparison uses. Curated to the common US (MPAA/TV) + UK (BBFC) labels —
/// pharos doesn't ship the full localization DB, but this covers the ratings
/// real libraries carry. Reference data only: serving it does not by itself
/// enforce any rating limit (T68 enforcement is separate).
async fn localization_parental_ratings(_user: AuthUser) -> impl Responder {
    HttpResponse::Ok().json(PARENTAL_RATINGS)
}

#[derive(Debug, Clone, Copy, serde::Serialize)]
#[serde(rename_all = "PascalCase")]
struct ParentalRating {
    name: &'static str,
    value: u32,
}

impl ParentalRating {
    const fn new(name: &'static str, value: u32) -> Self {
        Self { name, value }
    }
}

/// Ordered least→most restrictive. Values follow Jellyfin's scoring
/// convention (approve/family low, adult high) so a `MaxParentalRating`
/// comparison is a simple `<=`.
const PARENTAL_RATINGS: &[ParentalRating] = &[
    ParentalRating::new("Approved", 1),
    ParentalRating::new("G", 1),
    ParentalRating::new("TV-Y", 1),
    ParentalRating::new("TV-G", 1),
    ParentalRating::new("PG", 5),
    ParentalRating::new("TV-PG", 5),
    ParentalRating::new("PG-13", 7),
    ParentalRating::new("TV-14", 7),
    ParentalRating::new("R", 9),
    ParentalRating::new("TV-MA", 9),
    ParentalRating::new("NC-17", 10),
    ParentalRating::new("X", 10),
];

async fn localization_options(_user: AuthUser) -> impl Responder {
    HttpResponse::Ok().json(serde_json::json!([
        { "Name": "English (US)", "Value": "en-US" },
    ]))
}

/// `GET /Devices` + `/Devices/Info` — admin dashboard's device list.
/// Aggregated from the token store: each issued token is one device
/// record. Currently exposes (device_id, user_id, last_user_name).
async fn devices_list(
    state: web::Data<AppState>,
    user: AuthUser,
) -> Result<impl Responder, actix_web::Error> {
    use pharos_core::{TokenStore, UserStore};
    if !user.0.policy.admin {
        return Err(error::ErrorForbidden("admin required"));
    }
    // We don't have a `list_all_tokens` API. Walk users, list tokens
    // per user. Phase 1 small-N — admins live with the per-user scan.
    let users = state
        .stores
        .list()
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    let mut items: Vec<serde_json::Value> = Vec::new();
    for u in users {
        let tokens = state
            .stores
            .tokens_for(u.id)
            .await
            .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
        for t in tokens {
            // Operator-set custom name (POST /Devices/Options), else the id.
            let display_name = state
                .stores
                .load_named_config(&format!("devname:{}", t.device_id))
                .await
                .ok()
                .flatten()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| t.device_id.clone());
            items.push(serde_json::json!({
                "Id": t.device_id,
                "Name": display_name,
                "AppName": "Jellyfin",
                "AppVersion": "0",
                "LastUserId": u.id.0.simple().to_string(),
                "LastUserName": u.name,
                "DateLastActivity": "1970-01-01T00:00:00.0000000Z",
            }));
        }
    }
    let total = items.len() as u32;
    Ok(HttpResponse::Ok().json(serde_json::json!({
        "Items": items,
        "TotalRecordCount": total,
        "StartIndex": 0,
    })))
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case", default)]
struct DeviceIdQuery {
    id: Option<String>,
}

/// `DELETE /Devices?id={deviceId}` — the dashboard "delete device" button.
/// Revokes every token issued to that device id (across all users), kicking
/// the device: its next request 401s and it must re-authenticate.
async fn delete_device(
    state: web::Data<AppState>,
    user: AuthUser,
    q: CiQuery<DeviceIdQuery>,
) -> Result<impl Responder, actix_web::Error> {
    use pharos_core::UserStore;
    if !user.0.policy.admin {
        return Err(error::ErrorForbidden("admin required"));
    }
    let device_id =
        q.id.as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| error::ErrorBadRequest("missing id"))?;
    let users = state
        .stores
        .list()
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    let mut revoked = 0u64;
    for u in users {
        revoked += state
            .stores
            .revoke_tokens_by_device(u.id, device_id)
            .await
            .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    }
    tracing::info!(device_id, revoked, "device deleted (tokens revoked)");
    Ok(HttpResponse::NoContent().finish())
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct DeviceOptionsBody {
    custom_name: Option<String>,
}

/// `POST /Devices/Options?id={deviceId}` — persist an operator-set display
/// name for a device (shown in the devices list). Stored via named_config
/// (`devname:{id}`). Empty name clears the override.
async fn device_options(
    state: web::Data<AppState>,
    user: AuthUser,
    q: CiQuery<DeviceIdQuery>,
    body: Option<web::Json<DeviceOptionsBody>>,
) -> Result<impl Responder, actix_web::Error> {
    if !user.0.policy.admin {
        return Err(error::ErrorForbidden("admin required"));
    }
    let device_id =
        q.id.as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| error::ErrorBadRequest("missing id"))?;
    let name = body
        .and_then(|b| b.into_inner().custom_name)
        .unwrap_or_default();
    state
        .stores
        .set_named_config(&format!("devname:{device_id}"), name.trim())
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    Ok(HttpResponse::NoContent().finish())
}

async fn media_segments_stub(
    state: web::Data<AppState>,
    _user: AuthUser,
    path: web::Path<String>,
) -> impl Responder {
    let item_id = path.into_inner();
    let items = build_media_segments(&state, &item_id).await;
    let total = items.len() as u32;
    HttpResponse::Ok().json(serde_json::json!({
        "Items": items,
        "TotalRecordCount": total,
        "StartIndex": 0,
    }))
}

/// Walk the item's chapter list and project intro / outro / recap
/// chapters into Jellyfin's MediaSegment shape so jellyfin-web's
/// "Skip Intro" overlay fires.
///
/// Heuristic-only: titles matching common patterns are classified;
/// everything else is ignored.
async fn build_media_segments(
    state: &AppState,
    item_id: &str,
) -> Vec<pharos_jellyfin_api::dto::MediaSegmentDto> {
    use pharos_core::MediaStore;
    use pharos_jellyfin_api::dto::MediaSegmentDto;
    let Some(id) = pharos_jellyfin_api::dto::parse_item_id(item_id) else {
        return Vec::new();
    };
    let Ok(item) = state.stores.get(id).await else {
        return Vec::new();
    };
    use pharos_core::MediaSegmentStore;
    let mut out = Vec::new();
    // Track which segment Types a CHAPTER already supplied — chapters are exact
    // (author-labeled) so they win over a fingerprint/black-frame detection for
    // the same Type (T86/ADR-0018 #6, layered sources).
    let mut seen_types: std::collections::HashSet<&'static str> = std::collections::HashSet::new();
    let chapters = &item.probe.chapters;
    for (idx, c) in chapters.iter().enumerate() {
        let Some(seg_type) = classify_chapter_title(&c.title) else {
            continue;
        };
        seen_types.insert(seg_type);
        let start_ticks = c.start_ms.saturating_mul(10_000);
        // The chapter's `end_ms` is the *next* chapter's start —
        // ffprobe carries it explicitly so we trust it.
        let end_ticks = if c.end_ms > c.start_ms {
            c.end_ms.saturating_mul(10_000)
        } else {
            start_ticks.saturating_add(30_000_000_000) // 30s fallback
        };
        out.push(MediaSegmentDto::new(
            item_id,
            &idx.to_string(),
            start_ticks,
            end_ticks,
            seg_type,
        ));
    }
    // T86 — union the auto-DETECTED segments (audio-fingerprint intro/outro,
    // black-frame credits) the backfill persisted, skipping any Type a chapter
    // already covered. This is what makes Skip Intro / Skip Outro work for the
    // vast majority of TV rips, which carry no labeled intro chapters.
    if let Ok(detected) = state.stores.media_segments_for(id).await {
        for (di, seg) in detected.iter().enumerate() {
            let t = seg.kind.as_str();
            if seen_types.contains(t) {
                continue;
            }
            out.push(MediaSegmentDto::new(
                item_id,
                &format!("d{di}"),
                seg.start_ms.saturating_mul(10_000),
                seg.end_ms.saturating_mul(10_000),
                t,
            ));
        }
    }
    out
}

/// Map a free-text chapter title to a Jellyfin MediaSegmentType.
/// Returns None when the title doesn't look like an actionable
/// segment.
pub(crate) fn classify_chapter_title(title: &str) -> Option<&'static str> {
    let lc = title.to_ascii_lowercase();
    if lc.contains("intro") || lc.contains("opening") {
        return Some("Intro");
    }
    if lc.contains("outro")
        || lc.contains("end credits")
        || lc.contains("closing")
        || lc.starts_with("credits")
    {
        return Some("Outro");
    }
    if lc.contains("recap") || lc.contains("previously on") {
        return Some("Recap");
    }
    if lc.contains("preview") || lc.contains("next on") {
        return Some("Preview");
    }
    if lc.contains("commercial") {
        return Some("Commercial");
    }
    None
}

#[cfg(test)]
mod media_segments_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn classify_chapter_title_branches() {
        assert_eq!(classify_chapter_title("Opening"), Some("Intro"));
        assert_eq!(classify_chapter_title("Intro 1"), Some("Intro"));
        assert_eq!(classify_chapter_title("End credits"), Some("Outro"));
        assert_eq!(classify_chapter_title("Credits"), Some("Outro"));
        assert_eq!(classify_chapter_title("Previously on"), Some("Recap"));
        assert_eq!(classify_chapter_title("Recap"), Some("Recap"));
        assert_eq!(classify_chapter_title("Next on"), Some("Preview"));
        assert_eq!(
            classify_chapter_title("Commercial break"),
            Some("Commercial")
        );
        assert_eq!(classify_chapter_title("Chapter 4"), None);
        assert_eq!(classify_chapter_title("The Beach"), None);
    }
}
