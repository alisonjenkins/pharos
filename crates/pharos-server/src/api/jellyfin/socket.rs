//! Jellyfin `/socket` WebSocket — multipurpose multiplex of `MessageType`
//! payloads. Phase 1 covers the SyncPlay subset so existing Jellyfin phone
//! and TV clients participate in pharos's improved group sync (V20).
//!
//! Non-SyncPlay messages (KeepAlive, Sessions, etc.) are accepted and
//! ignored — phase 2 will fan them out to the relevant subsystems.

use super::auth_extractor::AuthUser;
use super::socket_messages::{
    CommandData, GroupInfoData, GroupStateUpdate, GroupUpdateData, Inbound, Outbound,
    PlayQueueUpdate, QueuePlaylistItem, SyncPlayJoinData, SyncPlayPlayData, SyncPlaySeekData,
};
use crate::state::{AppState, SocketBroadcast};
use actix_web::{web, HttpRequest, HttpResponse};
use actix_ws::{AggregatedMessage, Session};
use futures_util::StreamExt;
use pharos_jellyfin_api::dto::format_iso8601_ms;
use pharos_sync::{
    group::{GroupHandle, GroupMsg},
    messages::{GroupId, GroupPlayState, MemberId, QueueItemInfo, ServerMsg},
    registry::GroupRegistry,
    MemberSinks, SessionHub,
};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{broadcast, mpsc, oneshot};

const POSITION_TICKS_PER_MS: u64 = 10_000;

fn unix_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Extract the `deviceId` query parameter from the `/socket` URL
/// (`/socket?api_key=…&deviceId=…`). Jellyfin clients put it in the query
/// string on the WS handshake; it is the stable session key the HTTP SyncPlay
/// handlers use to reach this socket via the [`SessionHub`].
fn device_id_from_query(qs: &str) -> Option<String> {
    qs.split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(k, _)| k.eq_ignore_ascii_case("deviceid"))
        .map(|(_, v)| percent_decode(v))
}

/// Minimal percent-decode for a query value (deviceIds are usually plain, but
/// may contain `%XX` escapes). Avoids pulling in a urlencoding crate.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                if let (Some(hi), Some(lo)) = (hi, lo) {
                    out.push((hi * 16 + lo) as u8);
                    i += 3;
                    continue;
                }
                out.push(bytes[i]);
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The `Idle|Waiting|Playing|Paused` wire string for a `GroupPlayState`.
fn play_state_str(s: GroupPlayState) -> &'static str {
    match s {
        GroupPlayState::Idle => "Idle",
        GroupPlayState::Waiting => "Waiting",
        GroupPlayState::Playing => "Playing",
        GroupPlayState::Paused => "Paused",
    }
}

/// Context the socket threads into [`translate_outbound`] so a `ServerMsg`
/// (which carries only relative timing) can be rendered as the absolute-time,
/// queue-aware Jellyfin wire shapes.
struct TranslateCtx<'a> {
    group_id: Option<GroupId>,
    /// Group wall-clock epoch (unix ms); `at_server_ms + epoch = When`.
    epoch_unix_ms: u64,
    /// The current queue item's `PlaylistItemId` — commands whose id doesn't
    /// match the client's loaded item are dropped, so every command carries it.
    current_pli: Option<&'a str>,
}

pub fn register(cfg: &mut web::ServiceConfig) {
    cfg.route("/socket", web::get().to(ws_entry));
}

async fn ws_entry(
    req: HttpRequest,
    body: web::Payload,
    state: web::Data<AppState>,
    registry: web::Data<GroupRegistry>,
    hub: web::Data<SessionHub>,
    sinks: web::Data<MemberSinks>,
    user: AuthUser,
) -> Result<HttpResponse, actix_web::Error> {
    let (response, session, stream) = actix_ws::handle(&req, body)?;
    let stream = stream
        .aggregate_continuations()
        .max_continuation_size(64 * 1024);
    let bus_rx = state.bus.subscribe();
    let user_id_str = user.0.id.0.simple().to_string();
    // `deviceId` (WS query string) is the key the HTTP SyncPlay handlers use to
    // reach this socket. Fall back to the member id when absent so a client
    // without one still gets a stable-per-connection key.
    let device_id = device_id_from_query(req.query_string());
    actix_web::rt::spawn(handle_connection(
        session,
        stream,
        state.clone(),
        registry.get_ref().clone(),
        hub.get_ref().clone(),
        sinks.get_ref().clone(),
        device_id,
        bus_rx,
        user.0.name,
        user_id_str,
    ));
    Ok(response)
}

#[allow(clippy::too_many_arguments)]
async fn handle_connection<S>(
    mut session: Session,
    mut stream: S,
    state: web::Data<AppState>,
    registry: GroupRegistry,
    hub: SessionHub,
    sinks: MemberSinks,
    device_id: Option<String>,
    mut bus_rx: broadcast::Receiver<SocketBroadcast>,
    member_name: String,
    bound_user_id: String,
) where
    S: futures_util::Stream<Item = Result<AggregatedMessage, actix_ws::ProtocolError>> + Unpin,
{
    let started = Instant::now();
    let (out_tx, mut out_rx) = mpsc::channel::<ServerMsg>(64);
    let mut current_group: Option<GroupHandle> = None;
    // Register this socket so HTTP SyncPlay handlers can reach it by deviceId.
    // Fall back to a random key when the client sent no deviceId (such a client
    // can't be reached by HTTP anyway). The hub owns a STABLE member id per
    // device across reconnects.
    let device_key = device_id.unwrap_or_else(|| format!("anon-{}", MemberId::new()));
    let reg = hub.register(device_key.clone(), member_name.clone(), out_tx.clone());
    let member_id = reg.member_id;
    let conn_gen = reg.gen;
    // Register this socket's sink in the per-replica delivery table. Safe to do
    // before any group join: the actor only ever delivers to members in its
    // roster, so an unrostered sink receives nothing until AddMember admits it.
    // On a reconnect this replaces the stale sink with the fresh one.
    sinks.insert(member_id, out_tx.clone());
    // Jellyfin-wire context derived from the ServerMsg stream: the current
    // group id (from `Joined`) and the current queue item's PlaylistItemId
    // (from `PlayQueue`), both needed to shape outbound commands.
    let mut current_group_id: Option<GroupId> = None;
    let mut current_pli: Option<String> = None;
    // Reconnect into an existing group: refresh the group's sink to THIS socket
    // and re-sync. Membership survived the disconnect, so no re-Join is needed.
    //
    // B24 — when the HUB has no group (this process restarted since the device
    // last joined), fall back to the PERSISTED snapshots: member ids are
    // deterministic per device, so a snapshot naming this member proves the
    // membership. Re-attach + hydrate (get_or_create runs the takeover path)
    // and resync — the watch party survives the deploy without the client
    // even knowing.
    let mut initial_group = reg.group;
    if initial_group.is_none() {
        if let Some(gid) =
            crate::sync_recovery::find_persisted_group(&state.stores, member_id).await
        {
            if let Ok(h) = registry.get_or_create(gid).await {
                hub.attach_group(&device_key, h.clone());
                tracing::info!(
                    device_id = %device_key, %member_id, group = %gid,
                    "syncplay: recovered persisted membership after restart"
                );
                initial_group = Some(h);
            }
        }
    }
    if let Some(group) = initial_group {
        current_group_id = Some(group.group_id);
        // Sink already refreshed in MemberSinks above; ask the actor to re-send
        // the catch-up so the reconnected client re-syncs to current state.
        let _ = group.tx.send(GroupMsg::ResyncMember { member_id }).await;
        tracing::info!(device_id = %device_key, %member_id, group = %group.group_id, "syncplay: /socket reconnected into existing group");
    } else {
        tracing::info!(
            device_id = %device_key,
            %member_id,
            user = %member_name,
            "syncplay: /socket connected + registered in hub"
        );
    }
    // Real Jellyfin sends ForceKeepAlive on open; jellyfin-web only starts its
    // client-side KeepAlive timer after receiving it, so without this the socket
    // is dropped/churned. Data = the server's idle-timeout seconds.
    {
        let out = Outbound::new("ForceKeepAlive", serde_json::json!(60));
        let _ = send_outbound(&mut session, &out).await;
    }
    // P23 — server-initiated keep-alive. Tick every 30 s; track the
    // last time we observed client traffic so a peer that stopped
    // responding (TCP black-holed) gets dropped instead of leaking
    // file descriptors.
    let mut keepalive_tick = tokio::time::interval(std::time::Duration::from_secs(30));
    keepalive_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_client_seen = Instant::now();
    const IDLE_DROP: std::time::Duration = std::time::Duration::from_secs(120);

    'pump: loop {
        tokio::select! {
            biased;
            _ = keepalive_tick.tick() => {
                if last_client_seen.elapsed() > IDLE_DROP {
                    break 'pump;
                }
                let out = Outbound::new("KeepAlive", serde_json::Value::Null);
                if send_outbound(&mut session, &out).await.is_err() {
                    break 'pump;
                }
            }
            Some(server_msg) = out_rx.recv() => {
                // Cache the wire context this ServerMsg carries.
                match &server_msg {
                    ServerMsg::Joined { group_id, .. } => current_group_id = Some(*group_id),
                    ServerMsg::PlayQueue { items, playing_index, .. } => {
                        current_pli = items.get(*playing_index).map(|i| i.playlist_item_id.clone());
                    }
                    _ => {}
                }
                let epoch = current_group
                    .as_ref()
                    .map(|h| h.epoch_unix_ms)
                    .or_else(|| hub.epoch_of(&device_key))
                    .unwrap_or(0);
                let ctx = TranslateCtx {
                    group_id: current_group_id.or_else(|| current_group.as_ref().map(|h| h.group_id)),
                    epoch_unix_ms: epoch,
                    current_pli: current_pli.as_deref(),
                };
                if let Some(out) = translate_outbound(server_msg, &ctx) {
                    // Trace every SyncPlay message the socket pushes to a
                    // client (group updates + commands). Debug-level: silent at
                    // the default filter, but invaluable when diagnosing a
                    // "group won't sync" report — it shows exactly what reached
                    // each device. `kind` is the GroupUpdate `Type` or the
                    // command name, whichever the payload carries.
                    let kind = out
                        .data
                        .get("Type")
                        .and_then(|v| v.as_str())
                        .or_else(|| out.data.get("Command").and_then(|v| v.as_str()))
                        .unwrap_or("");
                    tracing::debug!(device_id = %device_key, msg = %out.message_type, kind, "syncplay: → client");
                    if send_outbound(&mut session, &out).await.is_err() {
                        break 'pump;
                    }
                }
            }
            broadcast_msg = bus_rx.recv() => {
                // Lagged means the broadcast buffer overran this
                // subscriber. Stay connected; the next library refresh
                // will sync the client anyway.
                let Ok(b) = broadcast_msg else { continue };
                // V9: UserDataChanged + PlaybackProgress are scoped
                // to one user. Drop the broadcast on this socket
                // unless the bound bearer matches — otherwise user A
                // learns user B watched item 42 (info leak across
                // tenants).
                if let SocketBroadcast::UserDataChanged { user_id, .. } = &b {
                    if user_id != &bound_user_id {
                        continue;
                    }
                }
                if let SocketBroadcast::PlaybackProgress { user_id, .. } = &b {
                    if user_id != &bound_user_id {
                        continue;
                    }
                }
                if let Some(out) = translate_broadcast(b) {
                    if send_outbound(&mut session, &out).await.is_err() {
                        break 'pump;
                    }
                }
            }
            frame = stream.next() => {
                let Some(frame) = frame else { break 'pump };
                // Any incoming frame counts as client liveness.
                last_client_seen = Instant::now();
                match frame {
                    Ok(AggregatedMessage::Text(txt)) => {
                        let inbound: Inbound = match serde_json::from_str(&txt) {
                            Ok(v) => v,
                            Err(_) => continue 'pump,
                        };
                        // KeepAlive: reply in-line. The Jellyfin clients
                        // close the socket after ~10 s of silence; this
                        // pong keeps it open.
                        if inbound.message_type == "KeepAlive" {
                            let out = Outbound::new(
                                "KeepAlive",
                                serde_json::Value::Null,
                            );
                            if send_outbound(&mut session, &out).await.is_err() {
                                break 'pump;
                            }
                            // T83 — forward as a liveness beacon so the group's
                            // ghost prune (MEMBER_TTL_MS) never reaps a member
                            // whose socket is demonstrably alive. jellyfin-web
                            // KeepAlives every ~30s — cheap, non-persisting.
                            if let Some(g) = hub
                                .resolve(&device_key)
                                .and_then(|s| s.group)
                            {
                                let _ = g.tx.send(GroupMsg::MemberPing { member_id }).await;
                            }
                            continue 'pump;
                        }
                        handle_inbound(
                            inbound,
                            &mut current_group,
                            member_id,
                            &member_name,
                            &registry,
                            started,
                        )
                        .await;
                    }
                    Ok(AggregatedMessage::Ping(p)) => {
                        let _ = session.pong(&p).await;
                    }
                    Ok(AggregatedMessage::Close(_)) | Err(_) => break 'pump,
                    Ok(_) => {}
                }
            }
        }
    }

    tracing::info!(device_id = %device_key, %member_id, "syncplay: /socket disconnected");
    // B26 — a socket that broke because WE are draining (SIGTERM: rolling
    // deploy) must NOT dismantle the membership: the whole point of the
    // persisted snapshot is that the next process recovers it. Removing the
    // member here emptied the group during the drain, and the emptying actor
    // deleted its own snapshot — recovery found nothing.
    if crate::state::is_shutting_down() {
        return;
    }
    // WS-native path (vestigial phone/TV clients): its group lives only in
    // `current_group`, so remove immediately (sink + roster).
    if let Some(h) = current_group.take() {
        let _ = h.tx.send(GroupMsg::RemoveMember { member_id }).await;
        sinks.remove(member_id);
    }
    // HTTP path: membership lives in the hub and must SURVIVE this disconnect so
    // a reconnect (jellyfin-web reconnects its socket constantly) re-attaches
    // instead of orphaning the member. Schedule a generation-guarded teardown:
    // if a newer socket connects within the grace window it bumps the
    // generation and this no-ops; otherwise the member is removed. The sink is
    // only dropped when the teardown actually fires — a reconnect re-inserted a
    // fresh sink under the same member id, which must not be wiped.
    const RECONNECT_GRACE: std::time::Duration = std::time::Duration::from_secs(20);
    let hub2 = hub.clone();
    let dev2 = device_key.clone();
    let sinks2 = sinks.clone();
    actix_web::rt::spawn(async move {
        tokio::time::sleep(RECONNECT_GRACE).await;
        // B26 — re-check at fire time: a drain that began during the grace
        // window must also leave the membership for the next process.
        if crate::state::is_shutting_down() {
            return;
        }
        if let Some(group) = hub2.remove_if_current_gen(&dev2, conn_gen) {
            let _ = group.tx.send(GroupMsg::RemoveMember { member_id }).await;
            sinks2.remove(member_id);
        }
    });
    let _ = session.clone().close(None).await;
}

async fn handle_inbound(
    inbound: Inbound,
    current_group: &mut Option<GroupHandle>,
    member_id: MemberId,
    member_name: &str,
    registry: &GroupRegistry,
    started: Instant,
) {
    let _ = started;
    match inbound.message_type.as_str() {
        "SyncPlayCreateGroup" => {
            if current_group.is_some() {
                return;
            }
            let handle = match registry.create().await {
                Ok(h) => h,
                Err(_) => return,
            };
            join_via_handle(handle, current_group, member_id, member_name).await;
        }
        "SyncPlayJoinGroup" => {
            let Ok(data) = serde_json::from_value::<SyncPlayJoinData>(inbound.data) else {
                return;
            };
            let group_id = GroupId(data.group_id);
            let handle = match registry.get_or_create(group_id).await {
                Ok(h) => h,
                Err(_) => return,
            };
            join_via_handle(handle, current_group, member_id, member_name).await;
        }
        "SyncPlayLeaveGroup" => {
            if let Some(h) = current_group.take() {
                let _ = h.tx.send(GroupMsg::RemoveMember { member_id }).await;
            }
        }
        "SyncPlayPlay" => {
            let Some(h) = current_group else { return };
            let data: SyncPlayPlayData =
                serde_json::from_value(inbound.data).unwrap_or(SyncPlayPlayData {
                    playback_position_ticks: 0,
                });
            let _ =
                h.tx.send(GroupMsg::LeaderPlay {
                    sender: member_id,
                    position_ms: data.playback_position_ticks / POSITION_TICKS_PER_MS,
                })
                .await;
        }
        "SyncPlayPause" | "SyncPlayUnpause" => {
            let Some(h) = current_group else { return };
            let _ = h.tx.send(GroupMsg::LeaderPause { sender: member_id }).await;
        }
        "SyncPlaySeek" => {
            let Some(h) = current_group else { return };
            let Ok(data) = serde_json::from_value::<SyncPlaySeekData>(inbound.data) else {
                return;
            };
            let _ =
                h.tx.send(GroupMsg::LeaderSeek {
                    sender: member_id,
                    position_ms: data.position_ticks / POSITION_TICKS_PER_MS,
                })
                .await;
        }
        "SyncPlayBuffering" => {
            let Some(h) = current_group else { return };
            let _ =
                h.tx.send(GroupMsg::BufferingStart {
                    member_id,
                    position_ms: 0,
                    // Socket-frame variant carries no PlaylistItemId (stock
                    // jellyfin-web posts Buffering over HTTP, not here).
                    playlist_item_id: None,
                })
                .await;
        }
        "SyncPlayReady" => {
            let Some(h) = current_group else { return };
            let _ = h.tx.send(GroupMsg::BufferingEnd { member_id }).await;
        }
        // KeepAlive, Sessions, etc. — phase 2.
        _ => {}
    }
}

async fn join_via_handle(
    handle: GroupHandle,
    current_group: &mut Option<GroupHandle>,
    member_id: MemberId,
    member_name: &str,
) {
    // The member's sink is already in the replica's MemberSinks (registered on
    // socket connect), so AddMember carries no sink — it just admits the member.
    let (reply_tx, reply_rx) = oneshot::channel();
    if handle
        .tx
        .send(GroupMsg::AddMember {
            member_id,
            name: member_name.to_string(),
            reply: reply_tx,
        })
        .await
        .is_err()
    {
        return;
    }
    if reply_rx.await.is_err() {
        return;
    }
    // The group actor sends `ServerMsg::Joined` to our sink (see AddMember), so
    // the client's GroupJoined notification flows through `translate_outbound`
    // uniformly with the HTTP-driven path — no manual self-emit needed.
    *current_group = Some(handle);
}

/// Map the engine's snake_case queue reason to Jellyfin's `PlayQueueUpdateReason`
/// (the value jellyfin-web's QueueCore switches on).
fn play_queue_reason(r: &str) -> String {
    match r {
        // A late joiner loads the playlist fresh at the group's current
        // position — Jellyfin's "NewPlaylist" does exactly that.
        "new_playlist" | "user_joined" => "NewPlaylist",
        "set_current_item" => "SetCurrentItem",
        "next_item" => "NextItem",
        "previous_item" => "PreviousItem",
        "repeat_mode" => "RepeatMode",
        "shuffle_mode" => "ShuffleMode",
        _ => "NewPlaylist",
    }
    .to_string()
}

/// A `SyncPlayCommand` (`SendCommand`) with the absolute-time + queue fields the
/// Jellyfin client requires.
/// `Guid.Empty` in Jellyfin's simple (dashless) form — the value real
/// Jellyfin sends for "no playlist item" and what we fall back to for a
/// group id before the join settles. Strict SDK clients parse it fine.
const EMPTY_GUID: &str = "00000000000000000000000000000000";

fn command(
    ctx: &TranslateCtx,
    kind: &'static str,
    at_server_ms: u64,
    position_ms: Option<u64>,
) -> Option<Outbound> {
    let when = format_iso8601_ms(ctx.epoch_unix_ms as i64 + at_server_ms as i64);
    Some(Outbound::new(
        "SyncPlayCommand",
        serde_json::to_value(CommandData {
            // GroupId/When/EmittedAt/PlaylistItemId are non-nullable in the
            // C# SendCommand — always emitted (kotlin apps fail otherwise).
            group_id: ctx
                .group_id
                .map(|g| g.to_string())
                .unwrap_or_else(|| EMPTY_GUID.to_string()),
            command: kind,
            position_ticks: position_ms.map(|p| p * POSITION_TICKS_PER_MS),
            when,
            emitted_at: format_iso8601_ms(unix_now_ms()),
            playlist_item_id: ctx
                .current_pli
                .map(str::to_string)
                .unwrap_or_else(|| EMPTY_GUID.to_string()),
        })
        .ok()?,
    ))
}

/// A `SyncPlayGroupUpdate` envelope: `{ GroupId, Type, Data }`.
fn group_update(
    ctx: &TranslateCtx,
    kind: &'static str,
    data: serde_json::Value,
) -> Option<Outbound> {
    Some(Outbound::new(
        "SyncPlayGroupUpdate",
        serde_json::to_value(GroupUpdateData {
            kind,
            group_id: ctx.group_id.map(|g| g.to_string()).unwrap_or_default(),
            data,
        })
        .ok()?,
    ))
}

fn translate_outbound(msg: ServerMsg, ctx: &TranslateCtx) -> Option<Outbound> {
    match msg {
        ServerMsg::Play {
            at_server_ms,
            position_ms,
        } => command(ctx, "Unpause", at_server_ms, Some(position_ms)),
        // Pause MUST carry PositionTicks: jellyfin-web's schedulePause seeks
        // to the command's position after pausing, so a missing value seeks
        // the client to 0:00.
        ServerMsg::Pause {
            at_server_ms,
            position_ms,
        } => command(ctx, "Pause", at_server_ms, Some(position_ms)),
        ServerMsg::Seek {
            at_server_ms,
            position_ms,
        } => command(ctx, "Seek", at_server_ms, Some(position_ms)),
        ServerMsg::Joined {
            group_id: gid,
            members,
            ..
        } => {
            let info = GroupInfoData {
                group_id: gid.to_string(),
                group_name: "SyncPlay".to_string(),
                state: "Idle",
                participants: members.into_iter().map(|m| m.name).collect(),
                last_updated_at: format_iso8601_ms(unix_now_ms()),
            };
            // Use the joined group's id even if ctx hasn't cached it yet.
            Some(Outbound::new(
                "SyncPlayGroupUpdate",
                serde_json::to_value(GroupUpdateData {
                    kind: "GroupJoined",
                    group_id: gid.to_string(),
                    data: serde_json::to_value(info).ok()?,
                })
                .ok()?,
            ))
        }
        ServerMsg::MemberJoined { member } => {
            group_update(ctx, "UserJoined", serde_json::Value::String(member.name))
        }
        ServerMsg::MemberLeft { member_id, name } => group_update(
            ctx,
            "UserLeft",
            // B37 — jellyfin-web renders this string verbatim in the "left
            // the group" toast; real Jellyfin sends the USERNAME. The uuid is
            // only a last-resort fallback for a nameless (hydrated-legacy)
            // roster entry.
            serde_json::Value::String(if name.is_empty() {
                member_id.to_string()
            } else {
                name
            }),
        ),
        ServerMsg::StateUpdate { state, reason } => group_update(
            ctx,
            "StateUpdate",
            serde_json::to_value(GroupStateUpdate {
                state: play_state_str(state),
                reason,
            })
            .ok()?,
        ),
        ServerMsg::PlayQueue {
            reason,
            items,
            playing_index,
            start_position_ms,
            is_playing,
            repeat_mode,
            shuffle_mode,
            last_update_unix_ms,
        } => {
            let update = PlayQueueUpdate {
                reason: play_queue_reason(&reason),
                // Stable per-queue-version timestamp from the engine — NOT
                // `unix_now_ms()`. A catch-up re-send carries the same value so
                // jellyfin-web's `LastUpdate <=` guard drops the duplicate
                // instead of restarting playback (→ "no active player").
                last_update: format_iso8601_ms(last_update_unix_ms as i64),
                playlist: items
                    .into_iter()
                    .map(|i: QueueItemInfo| QueuePlaylistItem {
                        item_id: i.item_id,
                        playlist_item_id: i.playlist_item_id,
                    })
                    .collect(),
                playing_item_index: playing_index,
                start_position_ticks: start_position_ms * POSITION_TICKS_PER_MS,
                is_playing,
                shuffle_mode,
                repeat_mode,
            };
            group_update(ctx, "PlayQueue", serde_json::to_value(update).ok()?)
        }
        // Leadership is purely a pharos-engine concept. Jellyfin's SyncPlay is
        // server-authoritative and has NO leader — no client (jellyfin-web,
        // phone, or TV) implements a `LeaderChanged` GroupUpdateType, so every
        // one of them logs `processGroupUpdate: command LeaderChanged not
        // recognised` (a console.error) on receipt. It drives nothing client
        // side, so don't emit it: keep the election internal and the client
        // console clean.
        ServerMsg::LeaderChange { .. } => None,
        // B24 — the session sent a group command but the server has no
        // membership for it (unrecoverable: no persisted snapshot names it).
        // jellyfin-web handles NotInGroup by disabling SyncPlay locally — a
        // visible clean exit instead of a silent one-sided desync.
        ServerMsg::NotInGroup => group_update(ctx, "NotInGroup", serde_json::Value::Null),
        // B25 — leave acknowledgement to the leaver; jellyfin-web exits
        // SyncPlay mode (disableSyncPlay) on this.
        ServerMsg::GroupLeft => group_update(ctx, "GroupLeft", serde_json::Value::Null),
        ServerMsg::Welcome { .. } | ServerMsg::Pong { .. } | ServerMsg::Error { .. } => None,
    }
}

async fn send_outbound(session: &mut Session, msg: &Outbound) -> Result<(), actix_ws::Closed> {
    let s = serde_json::to_string(msg).map_err(|_| actix_ws::Closed)?;
    session.text(s).await
}

/// Translate a server-side `SocketBroadcast` into a Jellyfin-shaped
/// `Outbound`. T40 phase 2 — keeps the wire format identical to what
/// jellyfin-web expects when it subscribes via Sessions/LibraryChanged.
pub(crate) fn translate_broadcast(b: SocketBroadcast) -> Option<Outbound> {
    match b {
        SocketBroadcast::LibraryChanged { added, removed } => {
            // LIB-A4 — surface the scan deltas in the arrays jellyfin-web
            // reads to refresh surgically. `IsEmpty` mirrors Jellyfin: true
            // only when nothing at all changed (a bare cache-bust hint).
            let is_empty = added.is_empty() && removed.is_empty();
            Some(Outbound::new(
                "LibraryChanged",
                serde_json::json!({
                    "FoldersAddedTo": [],
                    "FoldersRemovedFrom": [],
                    "ItemsAdded": added,
                    "ItemsRemoved": removed,
                    "ItemsUpdated": [],
                    "CollectionFolders": [],
                    "IsEmpty": is_empty,
                }),
            ))
        }
        SocketBroadcast::UserDataChanged { user_id, entries } => Some(Outbound::new(
            "UserDataChanged",
            // B36 — each entry is a full serialized UserItemDataDto.
            // jellyfin-web matches cards by `ItemId` (32-hex wire id) /
            // the detail page by `Key`, then applies `Played`,
            // `IsFavorite`, `PlayedPercentage` … in place. A bare
            // `{ItemId}` stub matched nothing and carried no state, so
            // the UI never updated without a manual refresh.
            serde_json::json!({
                "UserId": user_id,
                "UserDataList": entries,
            }),
        )),
        SocketBroadcast::SessionCommand {
            session_id,
            command,
            arg,
        } => Some(if is_playstate_command(&command) {
            // PlayState family — playback transport
            // (Play/Pause/Unpause/Stop/Seek/NextTrack/PreviousTrack/
            // Rewind/FastForward/PlayPause).
            // jellyfin-web's playback engine listens for this MessageType.
            Outbound::new(
                "PlayState",
                serde_json::json!({
                    "ControllingUserId": "",
                    "SessionId": session_id,
                    "Command": command,
                    "SeekPositionTicks": arg.get("SeekPositionTicks").cloned(),
                }),
            )
        } else {
            // GeneralCommand family — display, volume, mute, fullscreen.
            // Jellyfin's wire shape nests `Arguments` for string args
            // (DisplayMessage, DisplayContent) and surfaces well-known
            // numeric args (`Volume`) at the top of the Arguments map.
            // We pass the full `arg` value through so clients that
            // expect specific keys can still find them.
            Outbound::new(
                "GeneralCommand",
                serde_json::json!({
                    "ControllingUserId": "",
                    "SessionId": session_id,
                    "Name": command,
                    "Arguments": arg,
                }),
            )
        }),
        // P10 — minimal Sessions payload carrying just the session
        // that changed. jellyfin-web's Currently Watching sidebar +
        // remote-control screens listen for this MessageType and
        // patch the matched session in-place by Id.
        SocketBroadcast::PlaybackProgress {
            session_id,
            user_id,
            item_id,
            position_ticks,
            is_paused,
        } => Some(Outbound::new(
            "Sessions",
            serde_json::json!([{
                "Id": session_id,
                "UserId": user_id,
                "NowPlayingItem": { "Id": item_id },
                "PlayState": {
                    "PositionTicks": position_ticks,
                    "IsPaused": is_paused,
                },
            }]),
        )),
    }
}

fn is_playstate_command(cmd: &str) -> bool {
    matches!(
        cmd,
        "Play"
            | "Pause"
            | "Unpause"
            | "PlayPause"
            | "Stop"
            | "Seek"
            | "NextTrack"
            | "PreviousTrack"
            | "Rewind"
            | "FastForward"
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn translate_pause_carries_position_ticks() {
        // jellyfin-web's schedulePause seeks to the command's PositionTicks;
        // a Pause without one seeks the client to 0:00.
        let ctx = TranslateCtx {
            group_id: None,
            epoch_unix_ms: 0,
            current_pli: Some("pli-1"),
        };
        let out = translate_outbound(
            ServerMsg::Pause {
                at_server_ms: 1_000,
                position_ms: 654_321,
            },
            &ctx,
        )
        .unwrap();
        assert_eq!(out.message_type, "SyncPlayCommand");
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&out).unwrap()).unwrap();
        assert_eq!(v["Data"]["Command"], "Pause");
        assert_eq!(v["Data"]["PositionTicks"], 654_321u64 * 10_000);
    }

    #[test]
    fn translate_library_changed_emits_libchanged_outbound() {
        let out = translate_broadcast(SocketBroadcast::LibraryChanged {
            added: Vec::new(),
            removed: Vec::new(),
        })
        .unwrap();
        assert_eq!(out.message_type, "LibraryChanged");
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&out).unwrap()).unwrap();
        assert_eq!(v["MessageType"], "LibraryChanged");
        // Jellyfin's LibraryChanged payload exposes these arrays even when empty.
        assert!(v["Data"]["ItemsUpdated"].is_array());
        assert!(v["Data"]["ItemsAdded"].is_array());
        // A bare hint (no deltas) reports IsEmpty = true.
        assert_eq!(v["Data"]["IsEmpty"], true);
    }

    #[test]
    fn translate_library_changed_carries_scan_deltas() {
        // LIB-A4 — a scan's added/removed ids land in the wire arrays
        // jellyfin-web reads to refresh surgically.
        let out = translate_broadcast(SocketBroadcast::LibraryChanged {
            added: vec!["10".into(), "20".into()],
            removed: vec!["30".into()],
        })
        .unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&out).unwrap()).unwrap();
        assert_eq!(v["Data"]["ItemsAdded"], serde_json::json!(["10", "20"]));
        assert_eq!(v["Data"]["ItemsRemoved"], serde_json::json!(["30"]));
        assert_eq!(v["Data"]["IsEmpty"], false);
    }

    #[test]
    fn translate_member_left_carries_username_not_uuid() {
        // B37 — jellyfin-web renders the UserLeft payload verbatim in its
        // "left the group" toast; real Jellyfin sends the USERNAME.
        let member_id = pharos_sync::MemberId::new();
        let ctx = TranslateCtx {
            group_id: Some(pharos_sync::GroupId::new()),
            epoch_unix_ms: 0,
            current_pli: None,
        };
        let out = translate_outbound(
            ServerMsg::MemberLeft {
                member_id,
                name: "jana".into(),
            },
            &ctx,
        )
        .unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&out).unwrap()).unwrap();
        assert_eq!(v["Data"]["Type"], "UserLeft");
        assert_eq!(v["Data"]["Data"], "jana");
    }

    #[test]
    fn translate_userdata_changed_carries_full_dto_entries() {
        // B36 — the wire UserDataList must carry the serialized
        // UserItemDataDto verbatim (ItemId, Key, Played, …), not a bare
        // {ItemId} stub: jellyfin-web patches cards by ItemId/Key and
        // reads the state fields off each entry.
        let entry = serde_json::json!({
            "ItemId": "0000000000000000000000000000002a",
            "Key": "42",
            "Played": true,
            "IsFavorite": false,
            "PlayCount": 1,
            "PlaybackPositionTicks": 0,
            "PlayedPercentage": 0.0,
        });
        let out = translate_broadcast(SocketBroadcast::UserDataChanged {
            user_id: "u-1".into(),
            entries: vec![entry.clone()],
        })
        .unwrap();
        assert_eq!(out.message_type, "UserDataChanged");
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&out).unwrap()).unwrap();
        assert_eq!(v["Data"]["UserId"], "u-1");
        assert_eq!(v["Data"]["UserDataList"][0], entry);
    }

    #[test]
    fn translate_playback_progress_emits_sessions_outbound() {
        let out = translate_broadcast(SocketBroadcast::PlaybackProgress {
            session_id: "s-1".into(),
            user_id: "u-1".into(),
            item_id: "42".into(),
            position_ticks: 12_345_000,
            is_paused: true,
        })
        .unwrap();
        assert_eq!(out.message_type, "Sessions");
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&out).unwrap()).unwrap();
        assert_eq!(v["Data"][0]["Id"], "s-1");
        assert_eq!(v["Data"][0]["UserId"], "u-1");
        assert_eq!(v["Data"][0]["NowPlayingItem"]["Id"], "42");
        assert_eq!(v["Data"][0]["PlayState"]["PositionTicks"], 12_345_000);
        assert_eq!(v["Data"][0]["PlayState"]["IsPaused"], true);
    }

    #[test]
    fn translate_session_command_emits_playstate_outbound() {
        let out = translate_broadcast(SocketBroadcast::SessionCommand {
            session_id: "s-1".into(),
            command: "Pause".into(),
            arg: serde_json::json!({}),
        })
        .unwrap();
        assert_eq!(out.message_type, "PlayState");
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&out).unwrap()).unwrap();
        assert_eq!(v["Data"]["SessionId"], "s-1");
        assert_eq!(v["Data"]["Command"], "Pause");
        // Empty ControllingUserId for server-originated commands.
        assert_eq!(v["Data"]["ControllingUserId"], "");
    }

    #[test]
    fn translate_session_command_passes_seek_position_ticks_through() {
        let out = translate_broadcast(SocketBroadcast::SessionCommand {
            session_id: "s-1".into(),
            command: "Seek".into(),
            arg: serde_json::json!({ "SeekPositionTicks": 9876543 }),
        })
        .unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&out).unwrap()).unwrap();
        assert_eq!(v["Data"]["SeekPositionTicks"], 9876543);
    }

    #[test]
    fn translate_session_command_routes_general_commands_as_general_command() {
        let out = translate_broadcast(SocketBroadcast::SessionCommand {
            session_id: "s-1".into(),
            command: "SetVolume".into(),
            arg: serde_json::json!({ "Volume": 60 }),
        })
        .unwrap();
        assert_eq!(out.message_type, "GeneralCommand");
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&out).unwrap()).unwrap();
        assert_eq!(v["Data"]["Name"], "SetVolume");
        assert_eq!(v["Data"]["Arguments"]["Volume"], 60);
    }

    #[test]
    fn translate_session_command_togglemute_is_general_not_playstate() {
        let out = translate_broadcast(SocketBroadcast::SessionCommand {
            session_id: "s-2".into(),
            command: "ToggleMute".into(),
            arg: serde_json::json!({}),
        })
        .unwrap();
        assert_eq!(out.message_type, "GeneralCommand");
    }
}
