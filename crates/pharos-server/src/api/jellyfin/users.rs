use crate::{
    api::jellyfin::{
        auth_extractor::AuthUser,
        dto::{AuthenticateByNameRequest, AuthenticationResultDto, SessionInfoDto, UserDto},
    },
    state::AppState,
};
use actix_web::{error::ErrorUnauthorized, web, HttpRequest, HttpResponse, Responder};
use pharos_core::{AuthBackend, AuthError, SecretString, TokenStore};
use uuid::Uuid;

pub fn register(cfg: &mut web::ServiceConfig) {
    // T31: paths registered in lowercase only — `LowercasePath`
    // middleware normalises incoming requests, so jellyfin-web's
    // mixed-case URIs (/Users/AuthenticateByName, /Branding/Css)
    // resolve here too.
    cfg.route("/users/authenticatebyname", web::post().to(authenticate_by_name))
        .route("/users/me", web::get().to(me))
        .route("/users/public", web::get().to(public_users))
        .route("/users/{user_id}", web::get().to(user_by_id))
        .route("/quickconnect/enabled", web::get().to(quick_connect_enabled))
        .route("/branding/configuration", web::get().to(branding_configuration))
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

/// We do not implement Quick Connect (would need a separate flow);
/// reporting false ensures jellyfin-web hides the Quick Connect UI.
async fn quick_connect_enabled() -> impl Responder {
    HttpResponse::Ok().json(false)
}

async fn branding_configuration() -> impl Responder {
    HttpResponse::Ok().json(serde_json::json!({
        "LoginDisclaimer": "",
        "CustomCss": "",
        "SplashscreenEnabled": false,
    }))
}

async fn branding_css() -> impl Responder {
    HttpResponse::Ok()
        .content_type("text/css")
        .body("")
}

async fn authenticate_by_name(
    state: web::Data<AppState>,
    req: HttpRequest,
    body: web::Json<AuthenticateByNameRequest>,
) -> Result<impl Responder, actix_web::Error> {
    let body = body.into_inner();
    let password = SecretString::new(body.pw);

    let user = match state
        .auth
        .authenticate(&body.username, &password)
        .await
    {
        Ok(u) => u,
        Err(AuthError::InvalidCredentials) | Err(AuthError::UserNotFound) => {
            return Err(ErrorUnauthorized("invalid credentials"));
        }
        Err(e) => return Err(actix_web::error::ErrorInternalServerError(e.to_string())),
    };

    let device_id = req
        .headers()
        .get("X-Emby-Authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(parse_device_id)
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
            device_name: "Unknown".into(),
            client: "Unknown".into(),
            application_version: "0".into(),
            server_id: state.server_id.clone(),
        },
        user: UserDto::from_domain(&user, &state.server_id),
        access_token: token.0.expose().to_string(),
        server_id: state.server_id.clone(),
    };
    Ok(HttpResponse::Ok().json(result))
}

async fn me(state: web::Data<AppState>, user: AuthUser) -> impl Responder {
    HttpResponse::Ok().json(UserDto::from_domain(&user.0, &state.server_id))
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
    Ok(HttpResponse::Ok().json(UserDto::from_domain(&user.0, &state.server_id)))
}

/// Parse `DeviceId=` (preferred) or `Device=` (fallback) from a
/// MediaBrowser/Emby authorization header.
fn parse_device_id(value: &str) -> Option<String> {
    let after = value.strip_prefix("MediaBrowser").or_else(|| value.strip_prefix("Emby"))?;
    let mut device_id: Option<String> = None;
    let mut device: Option<String> = None;
    for part in after.split(',') {
        let part = part.trim();
        let Some((k, raw)) = part.split_once('=') else {
            continue;
        };
        let v = raw.trim().trim_matches('"').trim();
        if v.is_empty() {
            continue;
        }
        match k.trim().to_ascii_lowercase().as_str() {
            "deviceid" => device_id = Some(v.to_string()),
            "device" => device = Some(v.to_string()),
            _ => {}
        }
    }
    device_id.or(device)
}
