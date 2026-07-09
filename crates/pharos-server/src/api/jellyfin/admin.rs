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
use pharos_core::{AuthError, SecretString, TokenStore, UserId, UserPolicy, UserRecord, UserStore};
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
        .route(
            "/users/{user_id}/password",
            web::post().to(set_user_password),
        )
        // Library admin.
        .route("/library/refresh", web::post().to(library_refresh))
        // Dashboard empty-stub surfaces.
        .route("/scheduledtasks", web::get().to(scheduled_tasks))
        .route("/plugins", web::get().to(empty_array))
        .route("/system/logs", web::get().to(system_logs))
        .route("/system/logs/log", web::get().to(system_logs_file))
        .route(
            "/system/activitylog/entries",
            web::get().to(activity_log_entries),
        )
        // POST writes to /System/Configuration are accepted + no-op'd;
        // pharos's runtime config is the toml file (re-read on restart).
        .route(
            "/system/configuration",
            web::post().to(post_system_configuration),
        )
        .route(
            "/system/configuration/{key}",
            web::post().to(post_system_configuration_key),
        )
        // T58 phase 3 — API keys. `device_id` doubles as the key id; the
        // raw token string is returned ONCE on creation and never
        // surfaced via list afterwards.
        .route("/auth/keys", web::get().to(list_api_keys))
        .route("/auth/keys", web::post().to(create_api_key))
        .route("/auth/keys/{device_id}", web::delete().to(revoke_api_key));
}

const API_KEY_PREFIX: &str = "apikey:";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct CreateApiKeyQuery {
    /// `App` matches jellyfin-web's `/Auth/Keys` form param. Tucks into
    /// the token's `device_id` as `apikey:{app}` so the key shows up
    /// in /Sessions with the app name and is revokable via DELETE
    /// `/Auth/Keys/{device_id}`.
    #[serde(default)]
    app: String,
}

async fn list_api_keys(
    state: web::Data<AppState>,
    user: AuthUser,
) -> Result<impl Responder, actix_web::Error> {
    require_admin(&user)?;
    let tokens = state
        .stores
        .tokens_for(user.0.id)
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    let items: Vec<serde_json::Value> = tokens
        .into_iter()
        .filter(|t| t.device_id.starts_with(API_KEY_PREFIX))
        .map(|t| {
            let app_name = t
                .device_id
                .strip_prefix(API_KEY_PREFIX)
                .unwrap_or(&t.device_id)
                .to_string();
            serde_json::json!({
                "AppName": app_name,
                // Jellyfin clients display DateCreated only — they
                // never see the raw token after issuance.
                "DateCreated": iso8601_from_unix(t.issued_at_unix_secs),
                // `device_id` doubles as the stable id for DELETE.
                "AccessToken": "",
                "Id": t.device_id,
            })
        })
        .collect();
    let total = items.len() as u32;
    Ok(HttpResponse::Ok().json(serde_json::json!({
        "Items": items,
        "TotalRecordCount": total,
        "StartIndex": 0,
    })))
}

async fn create_api_key(
    state: web::Data<AppState>,
    user: AuthUser,
    q: web::Query<CreateApiKeyQuery>,
) -> Result<impl Responder, actix_web::Error> {
    require_admin(&user)?;
    let app_name = q.app.trim();
    if app_name.is_empty() {
        return Err(error::ErrorBadRequest("App query param required"));
    }
    let device_id = format!("{API_KEY_PREFIX}{app_name}");
    let token = state
        .stores
        .issue(user.0.id, &device_id)
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    Ok(HttpResponse::Ok().json(serde_json::json!({
        "AppName": app_name,
        "AccessToken": token.0.expose(),
        "Id": device_id,
        "DateCreated": iso8601_from_unix(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
        ),
    })))
}

async fn revoke_api_key(
    state: web::Data<AppState>,
    user: AuthUser,
    path: web::Path<String>,
) -> Result<impl Responder, actix_web::Error> {
    require_admin(&user)?;
    let device_id = path.into_inner();
    if !device_id.starts_with(API_KEY_PREFIX) {
        return Err(error::ErrorBadRequest("not an API key id"));
    }
    let dropped = state
        .stores
        .revoke_tokens_by_device(user.0.id, &device_id)
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    if dropped == 0 {
        return Err(error::ErrorNotFound("api key not found"));
    }
    Ok(HttpResponse::NoContent().finish())
}

fn iso8601_from_unix(secs: i64) -> String {
    // Reuse the same simple formatter the user_data layer uses — keep
    // chrono out of the binary. Pull in from dto if it's already
    // exposed, else implement inline.
    use crate::api::jellyfin::dto::format_iso8601;
    format_iso8601(secs)
}

async fn empty_array(_user: AuthUser) -> impl Responder {
    let empty: Vec<serde_json::Value> = Vec::new();
    HttpResponse::Ok().json(empty)
}

/// T74 — `GET /ScheduledTasks`. Advertise pharos's real background jobs as
/// Jellyfin `TaskInfo` descriptors so the dashboard's Scheduled Tasks panel
/// renders them (the panel 404-guards on an empty list). These run on their own
/// schedules (library watch/refresh, the trickplay + subtitle pre-generators)
/// rather than the Jellyfin trigger model, so each is advertised `Idle` with an
/// interval trigger and no manual execution wired yet — enough for the panel to
/// list them; per-task Start/Stop is a later increment.
async fn scheduled_tasks(_user: AuthUser) -> impl Responder {
    fn task(id: &str, key: &str, name: &str, desc: &str, category: &str) -> serde_json::Value {
        serde_json::json!({
            "Name": name,
            "State": "Idle",
            "Id": id,
            "Key": key,
            "Description": desc,
            "Category": category,
            "IsHidden": false,
            "IsEnabled": true,
            "CurrentProgressPercentage": serde_json::Value::Null,
            "LastExecutionResult": serde_json::Value::Null,
            "Triggers": [],
        })
    }
    HttpResponse::Ok().json(serde_json::json!([
        task(
            "refresh-library",
            "RefreshLibrary",
            "Scan Media Library",
            "Walks the media roots and updates the catalogue (incremental by (mtime,size) + probe schema version).",
            "Library",
        ),
        task(
            "trickplay-images",
            "TrickplayImages",
            "Generate Trickplay Images",
            "Pre-generates scrub-preview sprite sheets for video items.",
            "Library",
        ),
        task(
            "extract-subtitles",
            "ExtractSubtitles",
            "Extract Subtitles",
            "Pre-extracts embedded text subtitle tracks to the subtitle cache for fast delivery.",
            "Library",
        ),
    ]))
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct ActivityLogQuery {
    start_index: Option<usize>,
    limit: Option<usize>,
}

/// T73 — `GET /System/ActivityLog/Entries`. Serves the in-memory activity ring
/// buffer (logins etc.) in Jellyfin's `ActivityLogEntryQueryResult` shape so the
/// dashboard Activity panel renders real events instead of a permanent blank.
async fn activity_log_entries(
    state: web::Data<AppState>,
    user: AuthUser,
    q: web::Query<ActivityLogQuery>,
) -> Result<impl Responder, actix_web::Error> {
    require_admin(&user)?;
    let start = q.start_index.unwrap_or(0);
    let limit = q.limit.unwrap_or(100).min(1000);
    let (total, items) = state.activity_entries(start, limit);
    Ok(HttpResponse::Ok().json(serde_json::json!({
        "Items": items,
        "TotalRecordCount": total,
        "StartIndex": start,
    })))
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
        .map(|u| UserDto::from_domain(&u.clone().into_user(), &state.server_id))
        .collect();
    Ok(HttpResponse::Ok().json(dtos))
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct CreateUserBody {
    name: String,
    #[serde(default)]
    password: Option<String>,
}

// V8: redact the password field in Debug output.
impl std::fmt::Debug for CreateUserBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CreateUserBody")
            .field("name", &self.name)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .finish()
    }
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
    // Symmetric to the self-delete guard: refuse a self-demotion if
    // it would leave zero admins. Otherwise the dashboard is bricked.
    if target == user.0.id && !policy.admin {
        let users = state
            .stores
            .list()
            .await
            .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
        let other_admins = users
            .iter()
            .filter(|u| u.id != user.0.id && u.policy.admin)
            .count();
        if other_admins == 0 {
            return Err(error::ErrorBadRequest(
                "cannot demote self — no other admins remain",
            ));
        }
    }
    match state.stores.set_policy(target, policy).await {
        Ok(()) => Ok(HttpResponse::NoContent().finish()),
        Err(AuthError::UserNotFound) => Err(error::ErrorNotFound("user not found")),
        Err(e) => Err(error::ErrorInternalServerError(e.to_string())),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SetPasswordBody {
    #[serde(default)]
    new_pw: String,
    /// Jellyfin's UI sends `CurrentPw`. Required on a self
    /// password change; admins changing someone else's password
    /// can omit it.
    #[serde(default)]
    current_pw: String,
    #[serde(default)]
    reset_password: bool,
}

// V8: redact both password fields. Reset flag is fine to show.
impl std::fmt::Debug for SetPasswordBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SetPasswordBody")
            .field("new_pw", &"<redacted>")
            .field("current_pw", &"<redacted>")
            .field("reset_password", &self.reset_password)
            .finish()
    }
}

async fn set_user_password(
    state: web::Data<AppState>,
    user: AuthUser,
    path: web::Path<String>,
    body: web::Json<SetPasswordBody>,
) -> Result<impl Responder, actix_web::Error> {
    // Either the bearer matches the path id, or the bearer is admin.
    let target = parse_user_id(&path.into_inner())?;
    let is_self = target == user.0.id;
    if !is_self && !user.0.policy.admin {
        return Err(error::ErrorForbidden("admin required"));
    }
    let body = body.into_inner();
    // V8: a stolen session token must NOT be enough to change a
    // user's password. Self-change requires CurrentPw to match the
    // existing hash. Admin changing someone else's password (or
    // resetting their own with ResetPassword=true) skips this — it
    // matches Jellyfin's admin-reset flow.
    let auth = BuiltinAuth::new(state.stores.clone());
    let must_verify_current = is_self && !(user.0.policy.admin && body.reset_password);
    if must_verify_current {
        let current = SecretString::new(body.current_pw.clone());
        use pharos_core::AuthBackend;
        AuthBackend::authenticate(&auth, &user.0.name, &current)
            .await
            .map_err(|_| error::ErrorUnauthorized("current password mismatch"))?;
    }
    let new_password = if body.reset_password {
        SecretString::new(String::new())
    } else {
        SecretString::new(body.new_pw)
    };
    let hash = auth
        .hash_password(&new_password)
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    // Atomic single-UPDATE password swap. The previous delete+create
    // path could irreversibly destroy the account (+ cascaded tokens /
    // user_data) if the re-create failed for any reason — a routine
    // password change must never be able to lose a user.
    state
        .stores
        .set_password(target, hash)
        .await
        .map_err(|e| match e {
            AuthError::UserNotFound => error::ErrorNotFound("user not found"),
            other => error::ErrorInternalServerError(other.to_string()),
        })?;
    Ok(HttpResponse::NoContent().finish())
}

/// `POST /Library/Refresh` — Jellyfin's "Scan All Libraries" (the dashboard
/// button and the `refreshLibrary` flows). Spawns a background scan of every
/// configured media root and returns `204` immediately; the `LibraryChanged`
/// broadcast on completion lets connected clients invalidate caches. (Jellyfin's
/// UI polls `/ScheduledTasks` for progress — not implemented; the broadcast
/// suffices.)
///
/// pharos extension: `?force=true` re-probes every file, bypassing the
/// incremental `(mtime, size)` skip. The recovery path for a probe-schema
/// change (e.g. embedded-font `MediaAttachments`) whose new fields the
/// incremental scan would never backfill onto byte-identical existing files.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct LibraryRefreshQuery {
    force: bool,
}

async fn library_refresh(
    state: web::Data<AppState>,
    user: AuthUser,
    q: web::Query<LibraryRefreshQuery>,
) -> Result<impl Responder, actix_web::Error> {
    require_admin(&user)?;
    let state = state.into_inner();
    let roots = state.media_roots.clone();
    tracing::info!(
        force = q.force,
        roots = roots.len(),
        "library refresh requested"
    );
    // Shares the scan spawn with the add-library wizard: cfg-gated prober
    // (the distroless OCI image ships no `ffprobe`, so the libav prober is
    // mandatory there) + the post-scan library-id backfill + delta broadcast.
    crate::api::jellyfin::items::spawn_scan(state, roots, q.force);
    Ok(HttpResponse::NoContent().finish())
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct SystemConfigurationBody {
    server_name: Option<String>,
    login_disclaimer: Option<String>,
    custom_css: Option<String>,
}

async fn post_system_configuration(
    state: web::Data<AppState>,
    user: AuthUser,
    body: web::Bytes,
) -> Result<impl Responder, actix_web::Error> {
    require_admin(&user)?;
    // jellyfin-web's dashboard form POSTs a giant JSON blob — we only
    // mutate the three branding-style fields. Everything else is
    // accepted-and-ignored (pharos's other runtime config still lives
    // in config.toml, re-read on restart).
    let parsed: SystemConfigurationBody = if body.is_empty() {
        SystemConfigurationBody::default()
    } else {
        serde_json::from_slice(&body).unwrap_or_default()
    };
    let cfg = pharos_store_sqlx::RuntimeConfig {
        server_name: parsed
            .server_name
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        login_disclaimer: parsed.login_disclaimer,
        custom_css: parsed.custom_css,
    };
    state
        .stores
        .set_runtime_config(&cfg)
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    Ok(HttpResponse::NoContent().finish())
}

/// T72 — persist a named configuration section (`encoding`, `network`,
/// `metadata`, `livetv`, …). jellyfin-web's dashboard POSTs the section's
/// full form blob here; pharos stores it as an opaque JSON object keyed by
/// the section name and overlays it on the served defaults on the matching
/// GET, so the change survives restart. The incoming object is merged into
/// any previously-stored blob (a partial patch keeps prior keys), then
/// persisted. Non-object bodies are rejected — every Jellyfin section form
/// is a JSON object.
async fn post_system_configuration_key(
    state: web::Data<AppState>,
    user: AuthUser,
    path: web::Path<String>,
    body: web::Bytes,
) -> Result<impl Responder, actix_web::Error> {
    require_admin(&user)?;
    let key = path.into_inner().to_ascii_lowercase();
    let incoming: serde_json::Value = if body.is_empty() {
        serde_json::json!({})
    } else {
        serde_json::from_slice(&body)
            .map_err(|e| error::ErrorBadRequest(format!("invalid configuration JSON: {e}")))?
    };
    let patch = incoming
        .as_object()
        .ok_or_else(|| error::ErrorBadRequest("configuration body must be a JSON object"))?;
    // Merge onto any prior stored blob so a partial POST keeps earlier keys.
    let mut merged = match state
        .stores
        .load_named_config(&key)
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?
    {
        Some(raw) => serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&raw)
            .unwrap_or_default(),
        None => serde_json::Map::new(),
    };
    for (k, v) in patch {
        merged.insert(k.clone(), v.clone());
    }
    let serialized = serde_json::Value::Object(merged).to_string();
    state
        .stores
        .set_named_config(&key, &serialized)
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    Ok(HttpResponse::NoContent().finish())
}

pub(super) fn require_admin(user: &AuthUser) -> Result<(), actix_web::Error> {
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

/// `/System/Logs` — list regular files in `[obs].log_dir`. Returns
/// `[]` when log_dir is unset. Admin-only.
async fn system_logs(
    state: web::Data<AppState>,
    user: AuthUser,
) -> Result<impl Responder, actix_web::Error> {
    require_admin(&user)?;
    let Some(dir) = state.log_dir.as_ref() else {
        return Ok(HttpResponse::Ok().json(Vec::<serde_json::Value>::new()));
    };
    let entries = match std::fs::read_dir(dir) {
        Ok(it) => it,
        Err(_) => return Ok(HttpResponse::Ok().json(Vec::<serde_json::Value>::new())),
    };
    let mut out: Vec<serde_json::Value> = Vec::new();
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let size = meta.len();
        let mtime_secs = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let mtime_iso = crate::api::jellyfin::dto::format_iso8601(mtime_secs);
        out.push(serde_json::json!({
            "Name": name,
            "Size": size,
            "DateModified": mtime_iso,
        }));
    }
    out.sort_by(|a, b| {
        b["DateModified"]
            .as_str()
            .unwrap_or("")
            .cmp(a["DateModified"].as_str().unwrap_or(""))
    });
    Ok(HttpResponse::Ok().json(out))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct LogFileQuery {
    name: String,
}

/// `/System/Logs/Log?name=…` — stream a single log file's body.
/// Path traversal blocked: the resolved file's parent must equal
/// `state.log_dir` exactly.
async fn system_logs_file(
    state: web::Data<AppState>,
    user: AuthUser,
    q: web::Query<LogFileQuery>,
) -> Result<impl Responder, actix_web::Error> {
    require_admin(&user)?;
    let Some(dir) = state.log_dir.as_ref() else {
        return Err(error::ErrorNotFound("no log dir configured"));
    };
    let candidate = dir.join(&q.name);
    let canon_parent = candidate
        .parent()
        .and_then(|p| p.canonicalize().ok())
        .ok_or_else(|| error::ErrorBadRequest("invalid log path"))?;
    let canon_dir = dir
        .canonicalize()
        .map_err(|_| error::ErrorInternalServerError("log_dir missing"))?;
    if canon_parent != canon_dir {
        return Err(error::ErrorBadRequest("log path escapes log_dir"));
    }
    let body = std::fs::read(&candidate).map_err(|_| error::ErrorNotFound("log file not found"))?;
    Ok(HttpResponse::Ok()
        .content_type("text/plain; charset=utf-8")
        .body(body))
}
