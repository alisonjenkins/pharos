//! Jellyfin /Sessions + /PlayState reporting.
//!
//! Routes accept the standard Jellyfin POST bodies, forward to the
//! `SessionRegistry` actor (V18) for in-memory tracking, and return
//! 204 No Content. GET /Sessions returns the actor's snapshot.

use crate::services::play_state;
use crate::{
    api::jellyfin::auth_extractor::{auth_header_from_request, AuthUser},
    sessions::{SessionEvent, SessionRecord},
    state::AppState,
};
use actix_web::{error, web, HttpRequest, HttpResponse, Responder};
use serde::Deserialize;
use uuid::Uuid;

pub fn register(cfg: &mut web::ServiceConfig) {
    // T31: lowercase canonical paths; `LowercasePath` middleware
    // rewrites jellyfin-web's PascalCase before the router matches.
    cfg.route("/sessions", web::get().to(list_sessions))
        .route("/sessions/logout", web::post().to(logout))
        .route("/sessions/playing", web::post().to(playing_started))
        .route(
            "/sessions/playing/progress",
            web::post().to(playing_progress),
        )
        .route("/sessions/playing/stopped", web::post().to(playing_stopped))
        .route("/sessions/capabilities", web::post().to(capabilities))
        .route("/sessions/capabilities/full", web::post().to(capabilities))
        // Remote-control commands targeted at a specific session
        // (T40 phase 2 / T-fix-17). Jellyfin's casting / "play here"
        // UI POSTs to these paths after picking a session.
        .route(
            "/sessions/{id}/playing/{command}",
            web::post().to(session_playstate_command),
        )
        .route(
            "/sessions/{id}/command/{command}",
            web::post().to(session_general_command),
        )
        .route(
            "/sessions/{id}/playing",
            web::post().to(session_play_request),
        );
}

/// `POST /Sessions/Logout` — end the calling session. Revokes only the
/// token on this request, so the user's other devices stay signed in.
async fn logout(
    state: web::Data<AppState>,
    user: AuthUser,
    req: HttpRequest,
) -> Result<impl Responder, actix_web::Error> {
    use crate::services::auth::{revoke_token, RevokeCause};
    let token = crate::api::jellyfin::auth_extractor::extract_token(&req)
        .ok_or_else(|| error::ErrorUnauthorized("missing token"))?;
    revoke_token(&state, user.0.id, &token, RevokeCause::Logout)
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    Ok(HttpResponse::NoContent()
        .cookie(crate::api::jellyfin::auth_extractor::cleared_auth_cookie(
            &req,
        ))
        .finish())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct PlayingBody {
    #[serde(default)]
    item_id: String,
    #[serde(default)]
    play_session_id: Option<String>,
    #[serde(default)]
    position_ticks: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ProgressBody {
    #[serde(default)]
    item_id: String,
    #[serde(default)]
    play_session_id: Option<String>,
    #[serde(default)]
    position_ticks: Option<u64>,
    #[serde(default)]
    is_paused: bool,
    /// The tracks the client is actually playing. Recorded so a resume
    /// returns to them (`RememberAudioSelections`); absent in reports that
    /// know nothing about tracks, which must not erase a stored choice.
    #[serde(default)]
    audio_stream_index: Option<i32>,
    #[serde(default)]
    subtitle_stream_index: Option<i32>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct StoppedBody {
    #[serde(default)]
    play_session_id: Option<String>,
    #[serde(default)]
    item_id: Option<String>,
    #[serde(default)]
    position_ticks: Option<u64>,
    #[serde(default)]
    audio_stream_index: Option<i32>,
    #[serde(default)]
    subtitle_stream_index: Option<i32>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct CapabilitiesBody {
    #[serde(default)]
    playable_media_types: Vec<String>,
    #[serde(default)]
    supported_commands: Vec<String>,
    #[serde(default)]
    max_streaming_bitrate: Option<u64>,
    #[serde(default)]
    supports_media_control: bool,
    /// Optional explicit session id binding. Most clients send the
    /// device id instead; we derive the session via the auth header
    /// when this is missing.
    #[serde(default)]
    id: Option<String>,
}

/// The `/Sessions` view for one caller: admins see every active session,
/// non-admins only their own (V9: user A must not read user B's now-playing
/// item). Shared by `GET /Sessions` and the `/socket` `SessionsStart` push.
pub(crate) async fn visible_sessions(
    state: &AppState,
    user_id_hex: &str,
    admin: bool,
) -> Result<Vec<crate::sessions::SessionRecord>, String> {
    let snap = state.sessions.snapshot().await.map_err(|e| e.to_string())?;
    Ok(if admin {
        snap
    } else {
        snap.into_iter()
            .filter(|s| s.user_id == user_id_hex)
            .collect()
    })
}

async fn list_sessions(
    state: web::Data<AppState>,
    user: AuthUser,
) -> Result<impl Responder, actix_web::Error> {
    let mut visible = visible_sessions(
        &state,
        &user.0.id.0.simple().to_string(),
        user.0.policy.admin,
    )
    .await
    .map_err(error::ErrorInternalServerError)?;
    if let Some(directory) = &state.cast_directory {
        let viewer = crate::cast_sessions::ViewerInfo {
            user_id: user.0.id.0.simple().to_string(),
            is_admin: user.0.policy.admin,
        };
        visible.extend(
            crate::cast_sessions::visible_sessions(directory.as_ref(), &viewer)
                .iter()
                .map(SessionRecord::from),
        );
    }
    Ok(crate::api::jellyfin::wire::json(&visible))
}

async fn playing_started(
    state: web::Data<AppState>,
    user: AuthUser,
    req: HttpRequest,
    body: web::Json<PlayingBody>,
) -> Result<impl Responder, actix_web::Error> {
    let body = body.into_inner();
    if let Some(id) = pharos_jellyfin_api::dto::parse_item_id(&body.item_id) {
        crate::api::jellyfin::require_item_access(&state, &user.0, id, "playback").await?;
    }
    // Bump the item being played to the front of the trickplay/subtitle
    // pre-generator. PlaybackInfo already nudges, but not every client calls
    // it (and a pod restarted mid-episode would otherwise never re-learn what's
    // playing) — the playback-report path is the ground truth for "watched
    // RIGHT NOW". Best-effort; duplicates are deduped by the worker.
    if let Some(tx) = &state.trickplay_priority {
        if let Some(id) = pharos_jellyfin_api::dto::parse_item_id(&body.item_id) {
            let _ = tx.send(id);
        }
    }
    let session_id = body
        .play_session_id
        .unwrap_or_else(|| Uuid::new_v4().simple().to_string());
    let auth = auth_header_from_request(&req);
    state
        .sessions
        .apply(SessionEvent::Started {
            session_id,
            user_id: user.0.id,
            user_name: user.0.name.clone(),
            device_id: auth
                .device_id
                .clone()
                .or_else(|| auth.device.clone())
                .unwrap_or_else(|| "unknown".into()),
            device_name: auth.device_label(),
            client: auth.client_label(),
            version: auth.version_label(),
            item_id: body.item_id,
            position_ticks: body.position_ticks.unwrap_or(0),
        })
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    Ok(HttpResponse::NoContent().finish())
}

async fn playing_progress(
    state: web::Data<AppState>,
    user: AuthUser,
    body: web::Json<ProgressBody>,
) -> Result<impl Responder, actix_web::Error> {
    let body = body.into_inner();
    let report = play_state::Report {
        play_session_id: body.play_session_id.as_deref(),
        item_wire_id: Some(&body.item_id),
        position_ticks: body.position_ticks.unwrap_or(0),
        is_paused: body.is_paused,
        audio_stream_index: body.audio_stream_index,
        subtitle_stream_index: body.subtitle_stream_index,
    };
    play_state::report(&state, &user.0, report)
        .await
        .map_err(report_error)?;
    Ok(HttpResponse::NoContent().finish())
}

async fn playing_stopped(
    state: web::Data<AppState>,
    user: AuthUser,
    body: web::Json<StoppedBody>,
) -> Result<impl Responder, actix_web::Error> {
    let body = body.into_inner();
    let report = play_state::Report {
        play_session_id: body.play_session_id.as_deref(),
        item_wire_id: body.item_id.as_deref(),
        position_ticks: body.position_ticks.unwrap_or(0),
        is_paused: false,
        audio_stream_index: body.audio_stream_index,
        subtitle_stream_index: body.subtitle_stream_index,
    };
    play_state::stop(&state, &user.0, report)
        .await
        .map_err(report_error)?;
    Ok(HttpResponse::NoContent().finish())
}

/// A withheld item answers as a missing one (V187); anything else is a 500.
fn report_error(e: play_state::ReportError) -> actix_web::Error {
    match e {
        play_state::ReportError::Withheld(hidden) => crate::api::jellyfin::access_error(hidden),
        other => error::ErrorInternalServerError(other),
    }
}

/// Minimal, infallible percent-decode of a query value (`+`→space, `%XX`→byte).
/// Dependency-free; capability values are plain ASCII but decode defensively.
fn qs_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < b.len() => {
                let hex = |c: u8| (c as char).to_digit(16);
                match (hex(b[i + 1]), hex(b[i + 2])) {
                    (Some(h), Some(l)) => {
                        out.push((h * 16 + l) as u8);
                        i += 3;
                    }
                    _ => {
                        out.push(b[i]);
                        i += 1;
                    }
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parse `CapabilitiesBody` from the query string of the body-less
/// `POST /Sessions/Capabilities`. Case-insensitive keys; repeated keys
/// (`playableMediaTypes=Video&playableMediaTypes=Audio`) collect into a Vec.
/// Never fails — an unparseable field is simply skipped (B65).
fn caps_from_query(qs: &str) -> CapabilitiesBody {
    let mut caps = CapabilitiesBody::default();
    for pair in qs.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let val = qs_decode(v);
        match k.to_ascii_lowercase().as_str() {
            "playablemediatypes" => caps.playable_media_types.push(val),
            "supportedcommands" => caps.supported_commands.push(val),
            "maxstreamingbitrate" => caps.max_streaming_bitrate = val.parse().ok(),
            "supportsmediacontrol" => {
                caps.supports_media_control = val.eq_ignore_ascii_case("true")
            }
            "id" => caps.id = Some(val),
            _ => {}
        }
    }
    caps
}

async fn capabilities(
    state: web::Data<AppState>,
    user: AuthUser,
    req: HttpRequest,
    // B65 — `POST /Sessions/Capabilities` (non-Full) sends its fields as QUERY
    // params with NO body (the Android/Google-TV app does this immediately
    // after login: `?playableMediaTypes=Video&playableMediaTypes=Audio&...`).
    // A required `web::Json<_>` 400'd it (no body to parse) → the TV crashed
    // ("Something went wrong"). Make the body OPTIONAL and fall back to the
    // query. `/Sessions/Capabilities/Full` still sends a JSON body → Some.
    body: Option<web::Json<CapabilitiesBody>>,
) -> impl Responder {
    // P28 — persist client capabilities so the /Sessions snapshot
    // reflects them. jellyfin-web's remote-control screen reads the
    // resulting `SupportedCommands` to grey out unsupported buttons.
    let body = match body {
        Some(b) => b.into_inner(),
        None => caps_from_query(req.query_string()),
    };
    // Session-id binding priority: explicit Id field → derive from
    // device-id in the Emby Authorization header (matches our
    // Started event session-id pattern: user.0.id + device-id hash).
    let auth = crate::api::jellyfin::auth_extractor::auth_header_from_request(&req);
    let session_id = body.id.unwrap_or_else(|| {
        let device_id = auth.device_id.clone().unwrap_or_else(|| "unknown".into());
        format!("{}-{}", user.0.id.0.simple(), device_id)
    });
    let _ = state
        .sessions
        .apply(SessionEvent::SetCapabilities {
            session_id,
            playable_media_types: body.playable_media_types,
            supported_commands: body.supported_commands,
            max_streaming_bitrate: body.max_streaming_bitrate,
            supports_media_control: body.supports_media_control,
        })
        .await;
    HttpResponse::NoContent().finish()
}

/// `POST /Sessions/{id}/Playing/{command}` — PlayState transport
/// commands (Pause / Play / Stop / Seek / NextTrack / PreviousTrack
/// / Rewind / FastForward / SetVolume / etc).
///
/// Body may carry a `SeekPositionTicks` (Seek) or `Volume` (SetVolume)
/// payload; we forward everything via the SessionCommand broadcast so
/// the receiving WS handler decides how to interpret it.
async fn session_playstate_command(
    state: web::Data<AppState>,
    user: AuthUser,
    path: web::Path<(String, String)>,
    body: Option<web::Json<serde_json::Value>>,
) -> impl Responder {
    let (session_id, command) = path.into_inner();
    let arg = body
        .map(|b| b.into_inner())
        .unwrap_or(serde_json::json!({}));
    let controlling = user.0.id.0.simple().to_string();
    state.notify_session_command(&session_id, &controlling, &canonical_command(&command), arg);
    HttpResponse::NoContent().finish()
}

/// `POST /Sessions/{id}/Command/{command}` — general commands
/// (DisplayContent, ToggleMute, ToggleFullscreen, ...). Same wire
/// shape as PlayState; the target client filters on the command name.
async fn session_general_command(
    state: web::Data<AppState>,
    user: AuthUser,
    path: web::Path<(String, String)>,
    body: Option<web::Json<serde_json::Value>>,
) -> impl Responder {
    let (session_id, command) = path.into_inner();
    let arg = body
        .map(|b| b.into_inner())
        .unwrap_or(serde_json::json!({}));
    let controlling = user.0.id.0.simple().to_string();
    state.notify_session_command(&session_id, &controlling, &canonical_command(&command), arg);
    HttpResponse::NoContent().finish()
}

/// Map the (already-lowercased by `LowercasePath` middleware) command
/// path component back to Jellyfin's canonical PascalCase form so the
/// broadcast `Command` field reads as the receiving client expects.
fn canonical_command(s: &str) -> String {
    match s.to_ascii_lowercase().as_str() {
        "play" => "Play",
        "pause" => "Pause",
        "unpause" => "Unpause",
        "stop" => "Stop",
        "seek" => "Seek",
        "nexttrack" => "NextTrack",
        "previoustrack" => "PreviousTrack",
        "rewind" => "Rewind",
        "fastforward" => "FastForward",
        "playpause" => "PlayPause",
        "setvolume" => "SetVolume",
        "setrepeatmode" => "SetRepeatMode",
        "setshufflequeue" => "SetShuffleQueue",
        "setaudiostreamindex" => "SetAudioStreamIndex",
        "setsubtitlestreamindex" => "SetSubtitleStreamIndex",
        "togglemute" => "ToggleMute",
        "togglefullscreen" => "ToggleFullscreen",
        "displaycontent" => "DisplayContent",
        "displaymessage" => "DisplayMessage",
        "channelup" => "ChannelUp",
        "channeldown" => "ChannelDown",
        // Fallback: title-case the first letter so unknown commands
        // still resemble Jellyfin's shape without losing the rest.
        other => return title_case(other),
    }
    .to_string()
}

fn title_case(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_ascii_uppercase().to_string() + chars.as_str(),
        None => String::new(),
    }
}

/// `POST /Sessions/{id}/Playing` — "play these items here". Body
/// carries `ItemIds`, `StartPositionTicks`, `PlayCommand` (PlayNow /
/// PlayNext / PlayLast). Forwards verbatim as a `Play` command so
/// the receiving client kicks off playback.
async fn session_play_request(
    state: web::Data<AppState>,
    user: AuthUser,
    path: web::Path<String>,
    body: Option<web::Json<serde_json::Value>>,
) -> impl Responder {
    let session_id = path.into_inner();
    let arg = body
        .map(|b| b.into_inner())
        .unwrap_or(serde_json::json!({}));
    let controlling = user.0.id.0.simple().to_string();
    // "Play" is a distinct MessageType (PlayRequest), not a PlaystateCommand —
    // the socket translator routes it accordingly (B79).
    state.notify_session_command(&session_id, &controlling, "Play", arg);
    HttpResponse::NoContent().finish()
}
