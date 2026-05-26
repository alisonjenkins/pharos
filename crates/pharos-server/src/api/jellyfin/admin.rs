//! Jellyfin admin / dashboard endpoints (T46).
//!
//! Drives the jellyfin-web `/#/dashboard` tree. Every mutation here is
//! gated on `user.policy.admin` (V8/V9). Read-only stubs (ScheduledTasks,
//! Plugins, Logs) return empty arrays so the dashboard renders the empty
//! state rather than throwing on a 404.

use crate::{
    api::jellyfin::{
        auth_extractor::AuthUser,
        dto::{UserDto, UserPolicyDto},
    },
    auth::BuiltinAuth,
    state::AppState,
};
use actix_web::{error, web, HttpResponse, Responder};
use pharos_core::{AuthError, SecretString, UserId, UserPolicy, UserRecord, UserStore};
use serde::Deserialize;
use uuid::Uuid;

pub fn register(cfg: &mut web::ServiceConfig) {
    // T31 lowercase routes; LowercasePath middleware folds PascalCase.
    cfg
        // /Users admin variant — lists everyone, not just the bearer.
        .route("/users", web::get().to(list_users))
        .route("/users/new", web::post().to(create_user))
        .route("/users/{user_id}", web::delete().to(delete_user))
        .route("/users/{user_id}/policy", web::post().to(set_user_policy))
        .route("/users/{user_id}/password", web::post().to(set_user_password))
        // Library admin.
        .route("/library/refresh", web::post().to(library_refresh))
        // Dashboard empty-stub surfaces.
        .route("/scheduledtasks", web::get().to(empty_array))
        .route("/plugins", web::get().to(empty_array))
        .route("/system/logs", web::get().to(empty_array))
        .route("/system/activitylog/entries", web::get().to(empty_items_result))
        // POST writes to /System/Configuration are accepted + no-op'd;
        // pharos's runtime config is the toml file (re-read on restart).
        .route("/system/configuration", web::post().to(post_system_configuration))
        .route(
            "/system/configuration/{key}",
            web::post().to(post_system_configuration_key),
        );
}

async fn empty_array(_user: AuthUser) -> impl Responder {
    let empty: Vec<serde_json::Value> = Vec::new();
    HttpResponse::Ok().json(empty)
}

async fn empty_items_result(_user: AuthUser) -> impl Responder {
    HttpResponse::Ok().json(serde_json::json!({
        "Items": [],
        "TotalRecordCount": 0,
        "StartIndex": 0,
    }))
}

async fn list_users(
    state: web::Data<AppState>,
    user: AuthUser,
) -> Result<impl Responder, actix_web::Error> {
    require_admin(&user)?;
    let users = state
        .stores
        .list()
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    let dtos: Vec<UserDto> = users
        .iter()
        .map(|u| {
            UserDto::from_domain(
                &u.clone().into_user(),
                &state.server_id,
            )
        })
        .collect();
    Ok(HttpResponse::Ok().json(dtos))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct CreateUserBody {
    name: String,
    #[serde(default)]
    password: Option<String>,
}

async fn create_user(
    state: web::Data<AppState>,
    user: AuthUser,
    body: web::Json<CreateUserBody>,
) -> Result<impl Responder, actix_web::Error> {
    require_admin(&user)?;
    let body = body.into_inner();
    if body.name.trim().is_empty() {
        return Err(error::ErrorBadRequest("name required"));
    }
    let password = SecretString::new(body.password.unwrap_or_default());
    let auth = BuiltinAuth::new(state.stores.clone());
    let hash = auth
        .hash_password(&password)
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    let record = UserRecord {
        id: UserId::new(),
        name: body.name,
        password_hash: hash,
        policy: UserPolicy::default(),
    };
    match state.stores.create(record.clone()).await {
        Ok(()) => {}
        Err(AuthError::Conflict) => return Err(error::ErrorConflict("name taken")),
        Err(e) => return Err(error::ErrorInternalServerError(e.to_string())),
    }
    let dto = UserDto::from_domain(&record.into_user(), &state.server_id);
    state.notify_library_changed();
    Ok(HttpResponse::Ok().json(dto))
}

async fn delete_user(
    state: web::Data<AppState>,
    user: AuthUser,
    path: web::Path<String>,
) -> Result<impl Responder, actix_web::Error> {
    require_admin(&user)?;
    let target = parse_user_id(&path.into_inner())?;
    // Refuse to nuke the admin who's logged in — losing the only
    // admin would brick the dashboard.
    if target == user.0.id {
        return Err(error::ErrorBadRequest("cannot delete self"));
    }
    match state.stores.delete(target).await {
        Ok(()) => Ok(HttpResponse::NoContent().finish()),
        Err(AuthError::UserNotFound) => Err(error::ErrorNotFound("user not found")),
        Err(e) => Err(error::ErrorInternalServerError(e.to_string())),
    }
}

async fn set_user_policy(
    state: web::Data<AppState>,
    user: AuthUser,
    path: web::Path<String>,
    body: web::Json<UserPolicyDto>,
) -> Result<impl Responder, actix_web::Error> {
    require_admin(&user)?;
    let target = parse_user_id(&path.into_inner())?;
    let policy = UserPolicy {
        admin: body.is_administrator,
    };
    match state.stores.set_policy(target, policy).await {
        Ok(()) => Ok(HttpResponse::NoContent().finish()),
        Err(AuthError::UserNotFound) => Err(error::ErrorNotFound("user not found")),
        Err(e) => Err(error::ErrorInternalServerError(e.to_string())),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SetPasswordBody {
    #[serde(default)]
    new_pw: String,
    /// Jellyfin's UI sends `CurrentPw`; admin reset can leave it empty.
    #[serde(default)]
    #[allow(dead_code)]
    current_pw: String,
    #[serde(default)]
    reset_password: bool,
}

async fn set_user_password(
    state: web::Data<AppState>,
    user: AuthUser,
    path: web::Path<String>,
    body: web::Json<SetPasswordBody>,
) -> Result<impl Responder, actix_web::Error> {
    // Either the bearer matches the path id, or the bearer is admin.
    let target = parse_user_id(&path.into_inner())?;
    if target != user.0.id && !user.0.policy.admin {
        return Err(error::ErrorForbidden("admin required"));
    }
    let body = body.into_inner();
    let new_password = if body.reset_password {
        SecretString::new(String::new())
    } else {
        SecretString::new(body.new_pw)
    };
    let auth = BuiltinAuth::new(state.stores.clone());
    let hash = auth
        .hash_password(&new_password)
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    // No `set_password` on the store yet — refetch + recreate.
    let mut record = state.stores.get(target).await.map_err(|e| match e {
        AuthError::UserNotFound => error::ErrorNotFound("user not found"),
        other => error::ErrorInternalServerError(other.to_string()),
    })?;
    // Delete + re-create round-trips through a single transaction-like
    // sequence; the sqlite cascade drops tokens, forcing a re-login on
    // the user, which matches Jellyfin's behaviour.
    state
        .stores
        .delete(record.id)
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    record.password_hash = hash;
    state
        .stores
        .create(record)
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    Ok(HttpResponse::NoContent().finish())
}

async fn library_refresh(
    state: web::Data<AppState>,
    user: AuthUser,
) -> Result<impl Responder, actix_web::Error> {
    require_admin(&user)?;
    // Spawn the scan on the runtime and return immediately — Jellyfin's
    // admin UI expects 204 quickly, then polls /ScheduledTasks for
    // progress (not implemented yet; the LibraryChanged broadcast on
    // completion is enough for connected clients to invalidate caches).
    let state = state.into_inner();
    actix_web::rt::spawn(async move {
        let scanner = pharos_scanner::FsScanner::new(
            pharos_scanner::FfmpegProber::new(),
        );
        for root in &state.media_roots {
            match scanner.scan_into(root, &state.stores).await {
                Ok(n) => tracing::info!(
                    root = %root.display(),
                    imported = n,
                    "library refresh: root scanned"
                ),
                Err(e) => tracing::warn!(
                    root = %root.display(),
                    error = %e,
                    "library refresh: scan failed"
                ),
            }
        }
        state.notify_library_changed();
    });
    Ok(HttpResponse::NoContent().finish())
}

async fn post_system_configuration(
    user: AuthUser,
    _body: web::Bytes,
) -> Result<impl Responder, actix_web::Error> {
    require_admin(&user)?;
    // Pharos's runtime config lives in `config.toml`; the dashboard's
    // System / Configuration form posts JSON we accept but don't yet
    // mutate. Track follow-up under T46 phase 2.
    Ok(HttpResponse::NoContent().finish())
}

async fn post_system_configuration_key(
    user: AuthUser,
    _path: web::Path<String>,
    _body: web::Bytes,
) -> Result<impl Responder, actix_web::Error> {
    require_admin(&user)?;
    Ok(HttpResponse::NoContent().finish())
}

fn require_admin(user: &AuthUser) -> Result<(), actix_web::Error> {
    if !user.0.policy.admin {
        return Err(error::ErrorForbidden("admin required"));
    }
    Ok(())
}

fn parse_user_id(s: &str) -> Result<UserId, actix_web::Error> {
    Uuid::parse_str(s)
        .map(UserId)
        .map_err(|_| error::ErrorBadRequest("invalid user id"))
}
