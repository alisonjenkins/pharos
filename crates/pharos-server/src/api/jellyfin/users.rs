use crate::{
    api::jellyfin::{
        auth_extractor::{auth_header_from_request, AuthUser},
        dto::{AuthenticateByNameRequest, AuthenticationResultDto, SessionInfoDto, UserDto},
    },
    state::AppState,
};
use actix_web::{error::ErrorUnauthorized, web, HttpRequest, HttpResponse, Responder};
use pharos_core::{AuthBackend, AuthError, PreferenceStore, SecretString, TokenStore, UserStore};
use uuid::Uuid;

pub fn register(cfg: &mut web::ServiceConfig) {
    // T31: paths registered in lowercase only — `LowercasePath`
    // middleware normalises incoming requests, so jellyfin-web's
    // mixed-case URIs (/Users/AuthenticateByName, /Branding/Css)
    // resolve here too.
    cfg.route(
        "/users/authenticatebyname",
        web::post().to(authenticate_by_name),
    )
    .route(
        "/users/authenticatewithquickconnect",
        web::post().to(authenticate_with_quick_connect),
    )
    .route("/users/me", web::get().to(me))
    .route("/users/public", web::get().to(public_users))
    .route("/users/{user_id}", web::get().to(user_by_id))
    .route(
        "/quickconnect/enabled",
        web::get().to(quick_connect_enabled),
    )
    .route(
        "/quickconnect/initiate",
        web::post().to(quick_connect_initiate),
    )
    .route(
        "/quickconnect/initiate",
        web::get().to(quick_connect_initiate),
    )
    .route(
        "/quickconnect/authorize",
        web::post().to(quick_connect_authorize),
    )
    .route(
        "/quickconnect/connect",
        web::get().to(quick_connect_connect),
    )
    .route(
        "/branding/configuration",
        web::get().to(branding_configuration),
    )
    .route("/branding/css", web::get().to(branding_css))
    // Some Jellyfin clients (jellyfin-web included) request the
    // branding CSS with a `.css` suffix; same handler.
    .route("/branding/css.css", web::get().to(branding_css));
}

/// Jellyfin's "tile picker" on the login page calls this. Return an
/// empty array — clients drop to the manual login form.
async fn public_users() -> impl Responder {
    let empty: Vec<serde_json::Value> = Vec::new();
    HttpResponse::Ok().json(empty)
}

async fn quick_connect_enabled() -> impl Responder {
    HttpResponse::Ok().json(true)
}

/// `/QuickConnect/Initiate` — unauthenticated. Returns a fresh
/// `(Code, Secret, DeviceId)` triple. Client polls Connect with the
/// Secret while showing the Code on screen for the user to read out.
async fn quick_connect_initiate(
    state: web::Data<AppState>,
    req: HttpRequest,
) -> Result<impl Responder, actix_web::Error> {
    let auth = auth_header_from_request(&req);
    let device_id = auth
        .device_id
        .clone()
        .or_else(|| auth.device.clone())
        .unwrap_or_else(|| Uuid::new_v4().simple().to_string());
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    state
        .quick_connect
        .tx
        .send(crate::quick_connect::QcMsg::Initiate {
            device_id: device_id.clone(),
            reply: reply_tx,
        })
        .await
        .map_err(|e| actix_web::error::ErrorInternalServerError(e.to_string()))?;
    let entry = reply_rx
        .await
        .map_err(|e| actix_web::error::ErrorInternalServerError(e.to_string()))?;
    // DateAdded MUST be a real ISO8601 timestamp: the Jellyfin Android TV app
    // (jellyfin-sdk-kotlin) deserializes it as a DateTime, and an empty string
    // fails to parse — which makes the app treat Quick Connect as unavailable
    // and grey out the button. An empty/omitted value silently breaks it.
    Ok(HttpResponse::Ok().json(serde_json::json!({
        "Code": entry.code,
        "Secret": entry.secret,
        "DeviceId": entry.device_id,
        // DeviceName / AppName / AppVersion are non-null strings in Jellyfin's
        // QuickConnectResult; the Android/Google TV app's kotlin SDK rejects the
        // whole response if they're missing (→ greys out the button). Echo them
        // from the caller's `X-Emby-Authorization` header.
        "DeviceName": auth.device.clone().unwrap_or_default(),
        "AppName": auth.client.clone().unwrap_or_default(),
        "AppVersion": auth.version.clone().unwrap_or_default(),
        "Authenticated": false,
        "DateAdded": pharos_jellyfin_api::dto::format_iso8601(entry.created_unix_secs),
    })))
}

/// Case-INSENSITIVE lookup of a single query-string parameter. Jellyfin's
/// API params are camelCase (`secret`, `code`) and ASP.NET binds them
/// case-insensitively, so the official clients disagree on casing: the
/// Android TV / mobile SDKs send `?secret=`/`?code=` while jellyfin-web sends
/// `?Secret=`/`?Code=`. A case-sensitive (serde PascalCase) extractor 400s
/// every non-web client — so match on any casing, mirroring the real server.
/// Uses actix's own query parser, so values are percent-decoded correctly.
fn query_param_ci(query: &str, key: &str) -> Option<String> {
    web::Query::<std::collections::HashMap<String, String>>::from_query(query)
        .ok()?
        .into_inner()
        .into_iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v)
}

/// `/QuickConnect/Authorize?code=…` — signed-in user vouches for the
/// pending request. Pharos gates on any authenticated bearer (the
/// user effectively authorizes themselves); admin-only flow is a
/// future tightening if it bites.
async fn quick_connect_authorize(
    state: web::Data<AppState>,
    user: AuthUser,
    req: HttpRequest,
) -> Result<impl Responder, actix_web::Error> {
    let code = query_param_ci(req.query_string(), "code").unwrap_or_default();
    if code.trim().is_empty() {
        return Err(actix_web::error::ErrorBadRequest(
            "Code query param required",
        ));
    }
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    state
        .quick_connect
        .tx
        .send(crate::quick_connect::QcMsg::Authorize {
            code,
            by: user.0.id,
            reply: reply_tx,
        })
        .await
        .map_err(|e| actix_web::error::ErrorInternalServerError(e.to_string()))?;
    let ok = reply_rx
        .await
        .map_err(|e| actix_web::error::ErrorInternalServerError(e.to_string()))?;
    if !ok {
        return Err(actix_web::error::ErrorNotFound("unknown code"));
    }
    Ok(HttpResponse::Ok().json(true))
}

/// `/QuickConnect/Connect?secret=…` — poll endpoint (Jellyfin's
/// `QuickConnectResult`). READ-ONLY: returns the pending request's current
/// state, echoing back `Secret` so the client can hand it to the finalize
/// call. No token is minted here — the client polls this until
/// `Authenticated:true`, then exchanges the secret at
/// `/Users/AuthenticateWithQuickConnect` (which is where the record is
/// consumed and the AccessToken issued). `secret` is matched case-
/// insensitively (Android/mobile SDKs send `secret`, jellyfin-web `Secret`).
async fn quick_connect_connect(
    state: web::Data<AppState>,
    req: HttpRequest,
) -> Result<impl Responder, actix_web::Error> {
    let secret = query_param_ci(req.query_string(), "secret").unwrap_or_default();
    if secret.trim().is_empty() {
        return Err(actix_web::error::ErrorBadRequest(
            "Secret query param required",
        ));
    }
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    state
        .quick_connect
        .tx
        .send(crate::quick_connect::QcMsg::Connect {
            secret,
            reply: reply_tx,
        })
        .await
        .map_err(|e| actix_web::error::ErrorInternalServerError(e.to_string()))?;
    let entry = reply_rx
        .await
        .map_err(|e| actix_web::error::ErrorInternalServerError(e.to_string()))?;
    let Some(entry) = entry else {
        return Err(actix_web::error::ErrorNotFound("unknown or expired secret"));
    };
    // `Secret` MUST be echoed: jellyfin-web's login loop passes `data.Secret`
    // (this response's field, not the one it kept) to the finalize call.
    Ok(HttpResponse::Ok().json(serde_json::json!({
        "Code": entry.code,
        "Secret": entry.secret,
        "DeviceId": entry.device_id,
        "Authenticated": entry.authorized_by.is_some(),
        // Non-nullable DateTime in the C# QuickConnectResult — the kotlin
        // SDK (Android TV) rejects the poll response without it.
        "DateAdded": pharos_jellyfin_api::dto::format_iso8601(entry.created_unix_secs),
    })))
}

async fn branding_configuration(state: web::Data<AppState>) -> impl Responder {
    let cfg = state.effective_branding().await;
    HttpResponse::Ok().json(serde_json::json!({
        "LoginDisclaimer": cfg.login_disclaimer.unwrap_or_default(),
        "CustomCss": cfg.custom_css.clone().unwrap_or_default(),
        "SplashscreenEnabled": false,
    }))
}

async fn branding_css(state: web::Data<AppState>) -> impl Responder {
    let cfg = state.effective_branding().await;
    HttpResponse::Ok()
        .content_type("text/css")
        .body(cfg.custom_css.unwrap_or_default())
}

/// Current wall-clock time as the ISO8601 string the session DTO carries.
fn now_iso() -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    pharos_jellyfin_api::dto::format_iso8601_ms(ms)
}

async fn authenticate_by_name(
    state: web::Data<AppState>,
    req: HttpRequest,
    body: web::Json<AuthenticateByNameRequest>,
) -> Result<impl Responder, actix_web::Error> {
    let body = body.into_inner();
    let password = SecretString::new(body.pw);

    let user = match state.auth.authenticate(&body.username, &password).await {
        Ok(u) => u,
        Err(AuthError::InvalidCredentials) | Err(AuthError::UserNotFound) => {
            return Err(ErrorUnauthorized("invalid credentials"));
        }
        Err(e) => return Err(actix_web::error::ErrorInternalServerError(e.to_string())),
    };

    let auth = auth_header_from_request(&req);
    let device_id = auth
        .device_id
        .clone()
        .or_else(|| auth.device.clone())
        .unwrap_or_else(|| Uuid::new_v4().simple().to_string());

    let token = state
        .stores
        .issue(user.id, &device_id)
        .await
        .map_err(|e| actix_web::error::ErrorInternalServerError(e.to_string()))?;

    let user_id_str = user.id.0.simple().to_string();
    // T73 — record the login for the dashboard Activity panel.
    state.record_activity(
        &format!("{} logged in", user.name),
        "SessionStarted",
        Some(&user_id_str),
        auth.client.as_deref(),
    );
    let result = AuthenticationResultDto {
        session_info: SessionInfoDto {
            id: Uuid::new_v4().simple().to_string(),
            user_id: user_id_str.clone(),
            user_name: user.name.clone(),
            device_id,
            device_name: auth.device_label(),
            client: auth.client_label(),
            application_version: auth.version_label(),
            server_id: state.server_id.clone(),
            // Always-on-wire session flags (non-nullable in the C# DTO).
            last_activity_date: now_iso(),
            last_playback_check_in: now_iso(),
            is_active: true,
            supports_media_control: false,
            supports_remote_control: false,
            has_custom_device_name: false,
        },
        user: UserDto::from_domain(&user, &state.server_id),
        access_token: token.0.expose().to_string(),
        server_id: state.server_id.clone(),
    };
    Ok(HttpResponse::Ok().json(result))
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct QuickConnectDto {
    // Accept both `Secret` (jellyfin-web) and `secret` (mobile/TV SDKs) in the
    // JSON body — Jellyfin's server binds case-insensitively.
    #[serde(default, alias = "secret")]
    secret: String,
}

/// `POST /Users/AuthenticateWithQuickConnect` — the finalize step of the
/// Quick Connect flow. UNAUTHENTICATED: the `Secret` in the body IS the
/// credential (the user already vouched for it via `/QuickConnect/Authorize`
/// on a signed-in device). Consumes the authorized pending request, issues an
/// AccessToken against the authorizing user, and returns the same
/// `AuthenticationResult` shape as `/Users/AuthenticateByName` — which is what
/// jellyfin-web reads for `result.User.Id` + `result.AccessToken`.
async fn authenticate_with_quick_connect(
    state: web::Data<AppState>,
    req: HttpRequest,
    body: web::Json<QuickConnectDto>,
) -> Result<impl Responder, actix_web::Error> {
    let secret = body.into_inner().secret;
    if secret.trim().is_empty() {
        return Err(actix_web::error::ErrorBadRequest("Secret required"));
    }
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    state
        .quick_connect
        .tx
        .send(crate::quick_connect::QcMsg::Consume {
            secret,
            reply: reply_tx,
        })
        .await
        .map_err(|e| actix_web::error::ErrorInternalServerError(e.to_string()))?;
    let entry = reply_rx
        .await
        .map_err(|e| actix_web::error::ErrorInternalServerError(e.to_string()))?;
    // None = unknown/expired secret OR not yet authorized. Either way the
    // caller isn't cleared to log in.
    let Some(entry) = entry else {
        return Err(ErrorUnauthorized("quick connect not authorized"));
    };
    let by = entry.authorized_by.ok_or_else(|| {
        actix_web::error::ErrorInternalServerError("consumed entry lacks authorizer")
    })?;

    // Resolve the authorizing user (same path the auth extractor uses).
    let user = state
        .stores
        .get(by)
        .await
        .map_err(|e| ErrorUnauthorized(format!("authorizing user unavailable: {e}")))?
        .into_user();

    let token = state
        .stores
        .issue(by, &entry.device_id)
        .await
        .map_err(|e| actix_web::error::ErrorInternalServerError(e.to_string()))?;

    let auth = auth_header_from_request(&req);
    let user_id_str = user.id.0.simple().to_string();
    state.record_activity(
        &format!("{} logged in via Quick Connect", user.name),
        "SessionStarted",
        Some(&user_id_str),
        auth.client.as_deref(),
    );
    let result = AuthenticationResultDto {
        session_info: SessionInfoDto {
            id: Uuid::new_v4().simple().to_string(),
            user_id: user_id_str.clone(),
            user_name: user.name.clone(),
            device_id: entry.device_id.clone(),
            device_name: auth.device_label(),
            client: auth.client_label(),
            application_version: auth.version_label(),
            server_id: state.server_id.clone(),
            // Always-on-wire session flags (non-nullable in the C# DTO).
            last_activity_date: now_iso(),
            last_playback_check_in: now_iso(),
            is_active: true,
            supports_media_control: false,
            supports_remote_control: false,
            has_custom_device_name: false,
        },
        user: UserDto::from_domain(&user, &state.server_id),
        access_token: token.0.expose().to_string(),
        server_id: state.server_id.clone(),
    };
    Ok(HttpResponse::Ok().json(result))
}

async fn me(state: web::Data<AppState>, user: AuthUser) -> impl Responder {
    HttpResponse::Ok().json(user_dto_with_config(&state, &user.0).await)
}

async fn user_by_id(
    state: web::Data<AppState>,
    user: AuthUser,
    path: web::Path<String>,
) -> Result<impl Responder, actix_web::Error> {
    let requested = path.into_inner();
    let bearer_id = user.0.id.0.simple().to_string();
    if requested != bearer_id {
        return Err(actix_web::error::ErrorForbidden("user mismatch"));
    }
    Ok(HttpResponse::Ok().json(user_dto_with_config(&state, &user.0).await))
}

/// Build a `UserDto` overriding the default `Configuration` block
/// with whatever the user last POSTed to `/Users/{u}/Configuration`.
async fn user_dto_with_config(state: &AppState, user: &pharos_core::User) -> UserDto {
    let mut dto = UserDto::from_domain(user, &state.server_id);
    if let Ok(Some(json)) = state.stores.get_user_configuration(user.id).await {
        if let Ok(cfg) = serde_json::from_str(&json) {
            dto.configuration = cfg;
        }
    }
    dto
}
