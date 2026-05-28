use crate::{
    api::jellyfin::{
        auth_extractor::{auth_header_from_request, AuthUser},
        dto::{AuthenticateByNameRequest, AuthenticationResultDto, SessionInfoDto, UserDto},
    },
    state::AppState,
};
use actix_web::{error::ErrorUnauthorized, web, HttpRequest, HttpResponse, Responder};
use pharos_core::{AuthBackend, AuthError, PreferenceStore, SecretString, TokenStore};
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
    .route("/users/me", web::get().to(me))
    .route("/users/public", web::get().to(public_users))
    .route("/users/{user_id}", web::get().to(user_by_id))
    .route(
        "/quickconnect/enabled",
        web::get().to(quick_connect_enabled),
    )
    .route("/quickconnect/initiate", web::post().to(quick_connect_initiate))
    .route("/quickconnect/initiate", web::get().to(quick_connect_initiate))
    .route("/quickconnect/authorize", web::post().to(quick_connect_authorize))
    .route("/quickconnect/connect", web::get().to(quick_connect_connect))
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
    Ok(HttpResponse::Ok().json(serde_json::json!({
        "Code": entry.code,
        "Secret": entry.secret,
        "DeviceId": entry.device_id,
        "Authenticated": false,
        "DateAdded": "",
    })))
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct AuthorizeQuery {
    #[serde(default)]
    code: String,
}

/// `/QuickConnect/Authorize?Code=…` — signed-in user vouches for the
/// pending request. Pharos gates on any authenticated bearer (the
/// user effectively authorizes themselves); admin-only flow is a
/// future tightening if it bites.
async fn quick_connect_authorize(
    state: web::Data<AppState>,
    user: AuthUser,
    q: web::Query<AuthorizeQuery>,
) -> Result<impl Responder, actix_web::Error> {
    if q.code.trim().is_empty() {
        return Err(actix_web::error::ErrorBadRequest("Code query param required"));
    }
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    state
        .quick_connect
        .tx
        .send(crate::quick_connect::QcMsg::Authorize {
            code: q.code.clone(),
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

#[derive(serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ConnectQuery {
    #[serde(default)]
    secret: String,
}

/// `/QuickConnect/Connect?Secret=…` — poll endpoint. Returns the
/// pending request's current state. Once `Authenticated:true`, the
/// response carries an `AccessToken` and the pending record is
/// consumed (one-shot).
async fn quick_connect_connect(
    state: web::Data<AppState>,
    q: web::Query<ConnectQuery>,
) -> Result<impl Responder, actix_web::Error> {
    if q.secret.trim().is_empty() {
        return Err(actix_web::error::ErrorBadRequest("Secret query param required"));
    }
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    state
        .quick_connect
        .tx
        .send(crate::quick_connect::QcMsg::Connect {
            secret: q.secret.clone(),
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
    let Some(by) = entry.authorized_by else {
        // Not yet authorized — client keeps polling.
        return Ok(HttpResponse::Ok().json(serde_json::json!({
            "Code": entry.code,
            "DeviceId": entry.device_id,
            "Authenticated": false,
        })));
    };
    // Authorized — mint an AccessToken against the authorizing user.
    let token = crate::quick_connect::issue_token(&state.stores, by, &entry.device_id)
        .await
        .map_err(actix_web::error::ErrorInternalServerError)?;
    Ok(HttpResponse::Ok().json(serde_json::json!({
        "Code": entry.code,
        "DeviceId": entry.device_id,
        "Authenticated": true,
        "AccessToken": token,
    })))
}

async fn branding_configuration() -> impl Responder {
    HttpResponse::Ok().json(serde_json::json!({
        "LoginDisclaimer": "",
        "CustomCss": "",
        "SplashscreenEnabled": false,
    }))
}

async fn branding_css() -> impl Responder {
    HttpResponse::Ok().content_type("text/css").body("")
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
