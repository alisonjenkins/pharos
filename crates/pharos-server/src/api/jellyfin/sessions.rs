//! Jellyfin /Sessions + /PlayState reporting.
//!
//! Routes accept the standard Jellyfin POST bodies, forward to the
//! `SessionRegistry` actor (V18) for in-memory tracking, and return
//! 204 No Content. GET /Sessions returns the actor's snapshot.

use crate::progress_writes::{self, Decision, ProgressKey};
use crate::{
    api::jellyfin::auth_extractor::{auth_header_from_request, AuthSession, AuthUser},
    api::jellyfin::syncplay::dispatch,
    cast_service::{CastReject, CastRejectReason, StartCast},
    cast_sessions::tv_session_id,
    sessions::{SessionEvent, SessionRecord},
    state::{AppState, SocketBroadcast},
};
use actix_web::{error, http::StatusCode, web, HttpRequest, HttpResponse, Responder};
use pharos_cast::events::CastReason;
use pharos_core::{MediaStore, PlaybackProgress, UserDataStore};
use pharos_sync::{
    group::GroupMsg,
    messages::{GroupPlayState, MemberId},
    GroupRegistry, SessionHub,
};
use serde::Deserialize;
use std::time::Instant;
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
    _user: AuthUser,
    req: HttpRequest,
) -> Result<impl Responder, actix_web::Error> {
    use pharos_core::TokenStore;
    let token = crate::api::jellyfin::auth_extractor::extract_token(&req)
        .ok_or_else(|| error::ErrorUnauthorized("missing token"))?;
    state
        .stores
        .revoke(&token)
        .await
        .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    Ok(HttpResponse::NoContent().finish())
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
    let position_ticks = body.position_ticks.unwrap_or(0);
    let item_id_str = body.item_id.clone();
    let parsed_item_id = pharos_jellyfin_api::dto::parse_item_id(&item_id_str);
    // One read serves both the broadcast's kind and the DTO's runtime.
    let item = match parsed_item_id {
        Some(id) => state.stores.get(id).await.ok(),
        None => None,
    };

    if let Some(session_id) = body.play_session_id.clone() {
        state
            .sessions
            .apply(SessionEvent::Progress {
                session_id: session_id.clone(),
                item_id: body.item_id,
                position_ticks,
                is_paused: body.is_paused,
            })
            .await
            .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
        // P10 — fan out to /socket subscribers so the Currently
        // Watching sidebar updates live without polling.
        // Resolve the item's kind so the broadcast can stamp the
        // kotlin-REQUIRED `NowPlayingItem.Type` (a Type-less BaseItemDto
        // throws MissingFieldException and crashes native Android TV — B78).
        let item_kind = item.as_ref().map(|m| m.kind);
        state.notify_playback_progress(
            &session_id,
            &user.0.id.0.simple().to_string(),
            &item_id_str,
            item_kind,
            position_ticks,
            body.is_paused,
        );
    }

    // T33: persist the resume position so the Resume row picks the
    // item up across sessions. The session-snapshot path above lives
    // and dies with the in-process actor (V18) and won't survive
    // restarts.
    // parse_item_id: clients echo the canonical 32-hex wire id (B15) — a
    // plain decimal parse silently failed here, so resume positions
    // stopped persisting for hex-era clients.
    if let Some(item_id) = parsed_item_id {
        // Keep nudging the pre-generator from progress too: covers a server
        // restart mid-episode (session started before this process) and
        // clients that skip /Sessions/Playing. Deduped by the worker, so the
        // every-~10s cadence costs one HashSet probe.
        if let Some(tx) = &state.trickplay_priority {
            let _ = tx.send(item_id);
        }
        // A report that is silent about tracks is not a statement that the
        // user chose none; the store keeps the stored choice for a `None`.
        let progress = PlaybackProgress {
            position_ticks,
            at: now_unix(),
            audio_stream_index: body.audio_stream_index,
            subtitle_stream_index: body.subtitle_stream_index,
            only_if_newer: false,
        };
        let key = ProgressKey::new(user.0.id, item_id, body.play_session_id.as_deref());
        // Only the database write is deferred: the session registry and the
        // socket fan-out above already ran for this report.
        let decision = state
            .progress_writes
            .admit(&key, Instant::now(), body.is_paused, progress);
        if let Decision::Persist(reason) = decision {
            match progress_writes::persist(&state.stores, &key, progress, reason.label(), false)
                .await
            {
                Ok(data) => {
                    state
                        .progress_writes
                        .persisted(&key, Instant::now(), &progress);
                    // B36 — broadcast the full DTO (with PlayedPercentage
                    // from the item's runtime) so other windows' resume
                    // bars track the live position.
                    let runtime = item
                        .as_ref()
                        .and_then(|it| it.probe.run_time_ticks())
                        .unwrap_or(0);
                    let dto = pharos_jellyfin_api::dto::UserItemDataDto::from_domain_with_runtime(
                        item_id, data, runtime,
                    );
                    if let Ok(entry) = serde_json::to_value(&dto) {
                        state.notify_user_data_changed(
                            &user.0.id.0.simple().to_string(),
                            vec![entry],
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, item = item_id, "playback progress not persisted");
                    // Keep it for the background tick to retry.
                    state.progress_writes.hold(&key, progress);
                }
            }
        } else {
            progress_writes::count_held();
        }
    }
    Ok(HttpResponse::NoContent().finish())
}

fn now_unix() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

async fn playing_stopped(
    state: web::Data<AppState>,
    user: AuthUser,
    body: web::Json<StoppedBody>,
) -> Result<impl Responder, actix_web::Error> {
    let body = body.into_inner();
    let progress_session = body.play_session_id.clone();
    if let Some(session_id) = body.play_session_id {
        state
            .sessions
            .apply(SessionEvent::Stopped { session_id })
            .await
            .map_err(|e| error::ErrorInternalServerError(e.to_string()))?;
    }

    // Persist final UserData. If the client stopped within the last
    // ~10% of the runtime, treat as "finished" — mark played, reset
    // resume position. Otherwise save the position so the Resume row
    // picks the item up later. Without this the Resume row holds
    // every finished item forever (jellyfin-web only sends an
    // explicit /PlayedItems POST on manual mark-played).
    if let Some(item_id_str) = body.item_id {
        // parse_item_id: clients echo the canonical 32-hex wire id (B15) — a
        // plain decimal parse silently failed here, so resume positions
        // stopped persisting for hex-era clients.
        if let Some(item_id) = pharos_jellyfin_api::dto::parse_item_id(&item_id_str) {
            let position = body.position_ticks.unwrap_or(0);
            // The stop below is this session's final write; a report still
            // held for it is older and must not follow it into the database.
            state.progress_writes.forget(&ProgressKey::new(
                user.0.id,
                item_id,
                progress_session.as_deref(),
            ));
            let runtime = state
                .stores
                .get(item_id)
                .await
                .ok()
                .and_then(|it| it.probe.run_time_ticks())
                .unwrap_or(0);
            // P36 — threshold configurable via [server].played_threshold_pct
            // (default 90, clamped to [50, 100] at boot).
            let played_complement = 100u64.saturating_sub(state.played_threshold_pct as u64);
            let finished = runtime > 0
                && position >= runtime.saturating_sub(runtime * played_complement / 100);
            if let Ok(mut data) = state.stores.get_user_data(user.0.id, item_id).await {
                if finished {
                    data.played = true;
                    data.play_count = data.play_count.saturating_add(1);
                    data.last_played_position_ticks = 0;
                } else {
                    data.last_played_position_ticks = position;
                }
                data.last_played_at = now_unix();
                if body.audio_stream_index.is_some() {
                    data.audio_stream_index = body.audio_stream_index;
                }
                if body.subtitle_stream_index.is_some() {
                    data.subtitle_stream_index = body.subtitle_stream_index;
                }
                if state
                    .stores
                    .set_user_data(user.0.id, item_id, data)
                    .await
                    .is_ok()
                {
                    // B36 — full DTO so the just-finished (or resumable)
                    // item's card flips its watched tick / resume bar
                    // everywhere without a refresh. `runtime` was
                    // already fetched above for the finished check.
                    let dto = pharos_jellyfin_api::dto::UserItemDataDto::from_domain_with_runtime(
                        item_id, data, runtime,
                    );
                    if let Ok(entry) = serde_json::to_value(&dto) {
                        state.notify_user_data_changed(
                            &user.0.id.0.simple().to_string(),
                            vec![entry],
                        );
                    }
                }
            }
        }
    }
    Ok(HttpResponse::NoContent().finish())
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
///
/// A configured TV's id is applied to the caller's SyncPlay group instead
/// ([`tv_transport`]); nothing is relayed to a player (V169).
async fn session_playstate_command(
    state: web::Data<AppState>,
    auth: AuthSession,
    req: HttpRequest,
    path: web::Path<(String, String)>,
    body: Option<web::Json<serde_json::Value>>,
    hub: Option<web::Data<SessionHub>>,
    registry: Option<web::Data<GroupRegistry>>,
) -> HttpResponse {
    let (session_id, command) = path.into_inner();
    let arg = body
        .map(|b| b.into_inner())
        .unwrap_or(serde_json::json!({}));
    if let Some(tv_name) = virtual_tv_name(&state, &session_id) {
        let ticks = seek_ticks(req.query_string(), &arg);
        let group = hub.zip(registry);
        return tv_transport(
            &state,
            &auth,
            group.as_ref().map(|(h, r)| (h.get_ref(), r.get_ref())),
            &tv_name,
            TvTransport::parse(&canonical_command(&command), ticks),
        )
        .await;
    }
    let controlling = auth.user.id.0.simple().to_string();
    state.notify_session_command(&session_id, &controlling, &canonical_command(&command), arg);
    HttpResponse::NoContent().finish()
}

/// What a PlayState command aimed at a TV session means for the group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TvTransport {
    Pause,
    Unpause,
    PlayPause,
    /// `None` when the request named no position.
    Seek(Option<u64>),
    Next,
    Previous,
    Stop,
    /// A PlayState command with no group equivalent (volume, rewind, ...).
    Unsupported,
}

impl TvTransport {
    fn parse(canonical: &str, seek_ticks: Option<u64>) -> Self {
        match canonical {
            "Pause" => Self::Pause,
            "Unpause" => Self::Unpause,
            "PlayPause" => Self::PlayPause,
            "Seek" => Self::Seek(seek_ticks),
            "NextTrack" => Self::Next,
            "PreviousTrack" => Self::Previous,
            "Stop" => Self::Stop,
            _ => Self::Unsupported,
        }
    }
}

/// `SeekPositionTicks` from the query (where Jellyfin clients put it) or the
/// body, whichever is present.
fn seek_ticks(query: &str, body: &serde_json::Value) -> Option<u64> {
    query
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| k.eq_ignore_ascii_case("seekPositionTicks"))
        .and_then(|(_, v)| v.parse().ok())
        .or_else(|| {
            body.get("SeekPositionTicks")
                .and_then(serde_json::Value::as_u64)
        })
}

fn refuse(status: StatusCode, reason: &str, message: String) -> HttpResponse {
    HttpResponse::build(status).body(format!("{reason}: {message}"))
}

/// Apply a transport command aimed at a TV session to the group the TV's cast
/// belongs to, as the caller, through the same `dispatch` the SyncPlay routes
/// use so every command is counted (V160). `Stop` from the cast's owner ends
/// the cast; from anyone else it is refused. A caller outside the cast's group
/// is refused: they hold no seat on that TV.
async fn tv_transport(
    state: &AppState,
    auth: &AuthSession,
    group: Option<(&SessionHub, &GroupRegistry)>,
    tv_name: &str,
    command: TvTransport,
) -> HttpResponse {
    let (Some(service), Some((hub, registry))) = (state.cast.service(), group) else {
        return refuse(
            StatusCode::SERVICE_UNAVAILABLE,
            CastRejectReason::Unavailable.label(),
            format!("controlling {tv_name:?} is not running on this server"),
        );
    };
    if command == TvTransport::Unsupported {
        tracing::debug!(tv = %tv_name, "cast: PlayState command with no group equivalent ignored");
        return HttpResponse::NoContent().finish();
    }
    let Some(cast) = service
        .snapshot()
        .await
        .into_iter()
        .find(|c| c.tv.eq_ignore_ascii_case(tv_name))
    else {
        return refuse(
            StatusCode::CONFLICT,
            "no_cast",
            format!("nothing is casting to {tv_name:?}"),
        );
    };
    let Some(sync_key) = auth.sync_key() else {
        return refuse(
            StatusCode::BAD_REQUEST,
            CastRejectReason::NotInGroup.label(),
            "the request carried no deviceId, so it cannot be tied to a SyncPlay group".into(),
        );
    };
    let group = hub.resolve(&sync_key).and_then(|s| s.group);
    let Some(group) = group.filter(|g| g.group_id == cast.group) else {
        return refuse(
            StatusCode::FORBIDDEN,
            CastRejectReason::NotInGroup.label(),
            format!(
                "user {:?} is not in the SyncPlay group casting to {tv_name:?}",
                auth.user.name
            ),
        );
    };

    let send = |label: &'static str, make: GroupMsgMaker| {
        dispatch(
            hub,
            registry,
            &state.stores,
            Some(sync_key.clone()),
            label,
            make,
        )
    };
    match command {
        TvTransport::Pause => send("pause", |mid| GroupMsg::PauseShared { sender: mid }).await,
        TvTransport::Unpause => send("unpause", |mid| GroupMsg::Unpause { sender: mid }).await,
        TvTransport::PlayPause => {
            // Waiting is a gate on the way to playing: the group intends to play.
            let playing = group.snapshot().await.is_some_and(|s| {
                matches!(
                    s.play_state,
                    GroupPlayState::Playing | GroupPlayState::Waiting
                )
            });
            if playing {
                send("pause", |mid| GroupMsg::PauseShared { sender: mid }).await
            } else {
                send("unpause", |mid| GroupMsg::Unpause { sender: mid }).await
            }
        }
        TvTransport::Seek(None) => refuse(
            StatusCode::BAD_REQUEST,
            "missing_position",
            "Seek needs a SeekPositionTicks value".into(),
        ),
        TvTransport::Seek(Some(ticks)) => {
            let position_ms = ticks / TICKS_PER_MS;
            dispatch(
                hub,
                registry,
                &state.stores,
                Some(sync_key.clone()),
                "seek",
                move |mid| GroupMsg::SeekTo {
                    sender: mid,
                    position_ms,
                },
            )
            .await
        }
        TvTransport::Next => {
            send("nextitem", |mid| GroupMsg::NextItem {
                sender: mid,
                playlist_item_id: None,
            })
            .await
        }
        TvTransport::Previous => {
            send("previousitem", |mid| GroupMsg::PreviousItem {
                sender: mid,
                playlist_item_id: None,
            })
            .await
        }
        TvTransport::Stop => {
            if cast.owner != auth.user.id {
                return refuse(
                    StatusCode::FORBIDDEN,
                    "not_owner",
                    format!(
                        "only the user who cast to {tv_name:?} can stop it; {:?} can pause or seek the group",
                        auth.user.name
                    ),
                );
            }
            service.stop(auth.user.id, CastReason::UserStop).await;
            let _ = state.bus.send(SocketBroadcast::CastChanged);
            HttpResponse::NoContent().finish()
        }
        TvTransport::Unsupported => HttpResponse::NoContent().finish(),
    }
}

const TICKS_PER_MS: u64 = 10_000;

type GroupMsgMaker = fn(MemberId) -> GroupMsg;

/// `POST /Sessions/{id}/Command/{command}` — general commands
/// (DisplayContent, ToggleMute, ToggleFullscreen, ...). Same wire
/// shape as PlayState; the target client filters on the command name.
///
/// A TV's id is not a player to relay to, and a TV's volume and display belong
/// to the TV: the command is ignored (spec 009).
async fn session_general_command(
    state: web::Data<AppState>,
    user: AuthUser,
    path: web::Path<(String, String)>,
    body: Option<web::Json<serde_json::Value>>,
) -> impl Responder {
    let (session_id, command) = path.into_inner();
    if let Some(tv_name) = virtual_tv_name(&state, &session_id) {
        tracing::debug!(tv = %tv_name, %command, "cast: general command for a TV session ignored");
        return HttpResponse::NoContent().finish();
    }
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

/// The configured TV a session id names, if any. Only these ids are virtual;
/// every other id is a real player and keeps the plain relay.
fn virtual_tv_name(state: &AppState, session_id: &str) -> Option<String> {
    let directory = state.cast_directory.as_ref()?;
    let wanted = session_id.to_ascii_lowercase();
    directory
        .config()
        .tvs
        .iter()
        .find(|tv| tv_session_id(&tv.name) == wanted)
        .map(|tv| tv.name.clone())
}

/// The HTTP status for a refused cast start. A refusal that is the caller's to
/// fix is 4xx; one that is a dependency failing (the TV, the token store, the
/// service) is 503, so the caller's next move follows from the status (V165).
const fn reject_status(reason: CastRejectReason) -> StatusCode {
    match reason {
        CastRejectReason::NotInGroup => StatusCode::BAD_REQUEST,
        CastRejectReason::NotPermitted => StatusCode::FORBIDDEN,
        CastRejectReason::TvBusy | CastRejectReason::UserHasSeat => StatusCode::CONFLICT,
        CastRejectReason::TvUnreachable
        | CastRejectReason::TokenFailed
        | CastRejectReason::NotGroupOwner
        | CastRejectReason::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
    }
}

/// `<reason>: <message>` as the plain-text body Jellyfin clients show, so the
/// stable reason is both machine-readable and part of the sentence.
fn reject_response(reject: &CastReject) -> HttpResponse {
    HttpResponse::build(reject_status(reject.reason)).body(format!(
        "{}: {}",
        reject.reason.label(),
        reject.message
    ))
}

/// Hand the caller's seat in their SyncPlay group over to `tv_name`.
async fn start_cast(state: &AppState, auth: &AuthSession, tv_name: String) -> HttpResponse {
    let Some(service) = state.cast.service() else {
        return reject_response(&CastReject {
            reason: CastRejectReason::Unavailable,
            message: format!("casting to {tv_name:?} is not running on this server"),
        });
    };
    let Some(owner_sync_key) = auth.sync_key() else {
        return reject_response(&CastReject {
            reason: CastRejectReason::NotInGroup,
            message: "the request carried no deviceId, so it cannot be tied to a SyncPlay group"
                .into(),
        });
    };
    let started = service
        .start(StartCast {
            user_id: auth.user.id,
            user_name: auth.user.name.clone(),
            owner_sync_key: owner_sync_key.clone(),
            tv_name,
        })
        .await;
    match started {
        Ok(()) => {
            // The TV now plays in this player's place; stop the local player or
            // the owner hears the item twice. Addressed to this device alone.
            state.notify_device_command(
                &owner_sync_key,
                &auth.user.id.0.simple().to_string(),
                "Stop",
                serde_json::json!({}),
            );
            let _ = state.bus.send(SocketBroadcast::CastChanged);
            HttpResponse::NoContent().finish()
        }
        Err(reject) => reject_response(&reject),
    }
}

/// `POST /Sessions/{id}/Playing` — "play these items here". Body
/// carries `ItemIds`, `StartPositionTicks`, `PlayCommand` (PlayNow /
/// PlayNext / PlayLast). Forwards verbatim as a `Play` command so
/// the receiving client kicks off playback.
///
/// A configured TV's id is not a player: picking it hands the caller's seat in
/// their SyncPlay group to the TV instead (spec 009).
async fn session_play_request(
    state: web::Data<AppState>,
    auth: AuthSession,
    path: web::Path<String>,
    body: Option<web::Json<serde_json::Value>>,
) -> HttpResponse {
    let session_id = path.into_inner();
    if let Some(tv_name) = virtual_tv_name(&state, &session_id) {
        return start_cast(&state, &auth, tv_name).await;
    }
    let arg = body
        .map(|b| b.into_inner())
        .unwrap_or(serde_json::json!({}));
    let controlling = auth.user.id.0.simple().to_string();
    // "Play" is a distinct MessageType (PlayRequest), not a PlaystateCommand —
    // the socket translator routes it accordingly (B79).
    state.notify_session_command(&session_id, &controlling, "Play", arg);
    HttpResponse::NoContent().finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seek_ticks_come_from_the_query_or_the_body_and_commands_map_to_the_group() {
        let none = serde_json::json!({});
        assert_eq!(seek_ticks("seekPositionTicks=90", &none), Some(90));
        assert_eq!(seek_ticks("SeekPositionTicks=90&x=1", &none), Some(90));
        assert_eq!(
            seek_ticks("", &serde_json::json!({"SeekPositionTicks": 7})),
            Some(7)
        );
        assert_eq!(seek_ticks("seekPositionTicks=abc", &none), None);
        assert_eq!(
            TvTransport::parse("Seek", Some(5)),
            TvTransport::Seek(Some(5))
        );
        assert_eq!(TvTransport::parse("NextTrack", None), TvTransport::Next);
        assert_eq!(
            TvTransport::parse("SetVolume", None),
            TvTransport::Unsupported
        );
        assert_eq!(TvTransport::parse("Rewind", None), TvTransport::Unsupported);
    }

    #[test]
    fn every_reject_reason_has_a_status_and_only_dependency_failures_are_5xx() {
        for reason in CastRejectReason::ALL {
            let status = reject_status(reason);
            let dependency = matches!(
                reason,
                CastRejectReason::TvUnreachable
                    | CastRejectReason::TokenFailed
                    | CastRejectReason::NotGroupOwner
                    | CastRejectReason::Unavailable
            );
            assert_eq!(
                status.is_server_error(),
                dependency,
                "{} -> {status}",
                reason.label()
            );
        }
        assert_eq!(
            reject_status(CastRejectReason::NotPermitted),
            StatusCode::FORBIDDEN
        );
    }
}
