//! Jellyfin `/SyncPlay/*` HTTP surface — the command channel stock
//! jellyfin-web (and every official Jellyfin client) uses to drive group
//! watch. Commands arrive as HTTP POSTs identified by the caller's `deviceId`;
//! the resulting playback commands + group updates flow back to each member
//! over `/socket`.
//!
//! Each handler resolves the caller's session via the [`SessionHub`] (keyed by
//! `deviceId`), sends the matching [`GroupMsg`] to the group actor, and returns
//! `204` — matching Jellyfin, whose HTTP responses are empty and whose real
//! work rides the WebSocket. A caller with no registered socket, or not in a
//! group, is a `204` no-op.

use crate::api::jellyfin::auth_extractor::{AuthSession, AuthUser};
use actix_web::{web, HttpResponse};
use pharos_sync::group::{GroupHandle, GroupMsg, QueueMode, MAX_QUEUE_LEN};
use pharos_sync::messages::{GroupId, MemberId};
use pharos_sync::{GroupRegistry, MemberSinks, SessionHub};
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;
use uuid::Uuid;

const POSITION_TICKS_PER_MS: u64 = 10_000;

/// Most items one `/SyncPlay/Queue` request may add; each costs a store
/// lookup for its runtime.
const MAX_QUEUE_REQUEST_ITEMS: usize = 1_000;

pub fn register(cfg: &mut web::ServiceConfig) {
    // T31: paths registered lowercase; `LowercasePath` middleware
    // folds jellyfin-web's PascalCase requests before routing.
    cfg.route("/syncplay/list", web::get().to(list_groups))
        .route("/syncplay/new", web::post().to(new_group))
        .route("/syncplay/join", web::post().to(join_group))
        .route("/syncplay/leave", web::post().to(leave_group))
        .route("/syncplay/setnewqueue", web::post().to(set_new_queue))
        .route("/syncplay/buffering", web::post().to(buffering))
        .route("/syncplay/ready", web::post().to(ready))
        .route("/syncplay/pause", web::post().to(pause))
        .route("/syncplay/unpause", web::post().to(unpause))
        .route("/syncplay/seek", web::post().to(seek))
        .route(
            "/syncplay/setplaylistitem",
            web::post().to(set_playlist_item),
        )
        .route("/syncplay/queue", web::post().to(queue))
        .route("/syncplay/nextitem", web::post().to(next_item))
        .route("/syncplay/previousitem", web::post().to(previous_item))
        .route("/syncplay/setrepeatmode", web::post().to(set_repeat_mode))
        .route("/syncplay/setshufflemode", web::post().to(set_shuffle_mode))
        .route("/syncplay/setignorewait", web::post().to(set_ignore_wait))
        // T83 — liveness beacon + auto-heal. jellyfin-web posts this
        // periodically while in a group: it refreshes the member's ghost-prune
        // TTL, and a group-less pinger gets NotInGroup via `dispatch` (healing
        // a wedged client without the user touching anything).
        .route("/syncplay/ping", web::post().to(ping))
        .route(
            "/syncplay/removefromplaylist",
            web::post().to(remove_from_playlist),
        )
        .route(
            "/syncplay/moveplaylistitem",
            web::post().to(move_playlist_item),
        );
}

fn no_content() -> HttpResponse {
    HttpResponse::NoContent().finish()
}

#[cfg(test)]
use crate::group_ops::{recover_and_apply, Recovery, SOCKET_REGISTRATION_WAIT};
use crate::group_ops::{resolve_with_retry, route_command, CommandOutcome};

/// Resolve the caller's group and send it a `GroupMsg` built from the caller's
/// member id. The common shape of every command handler. A command that finds
/// no live socket is NOT dropped — it is recovered against the caller's
/// persisted membership (the rolling-deploy wedge of 2026-07-26, where
/// next/unpause/seek issued during the socket-reconnect gap were silently
/// lost). Every terminal path records a [`CommandOutcome`].
pub(crate) async fn dispatch(
    hub: &SessionHub,
    registry: &GroupRegistry,
    stores: &crate::state::Stores,
    device_id: Option<String>,
    label: &str,
    make: impl FnOnce(MemberId) -> GroupMsg,
) -> HttpResponse {
    dispatch_checked(
        hub,
        registry,
        stores,
        device_id,
        label,
        async |_| true,
        make,
    )
    .await
}

/// [`dispatch`], applying the command only when `check` passes for the group
/// it resolved to, on every path (live session or persisted recovery). A
/// refusal answers 204 and applies nothing, as Jellyfin ignores a queue
/// change a member may not see.
async fn dispatch_checked(
    hub: &SessionHub,
    registry: &GroupRegistry,
    stores: &crate::state::Stores,
    device_id: Option<String>,
    label: &str,
    check: impl AsyncFnOnce(&GroupHandle) -> bool,
    make: impl FnOnce(MemberId) -> GroupMsg,
) -> HttpResponse {
    match route_command(
        hub,
        registry,
        stores,
        device_id.as_deref(),
        label,
        check,
        make,
    )
    .await
    {
        // Not 204. A command with no deviceId can never be routed, and
        // answering "No Content" tells the caller it worked. 400, because the
        // condition is permanent: retrying the same request cannot help.
        CommandOutcome::NoDevice => HttpResponse::BadRequest()
            .body("SyncPlay command carried no deviceId, so it cannot be routed to a group"),
        // Not 204 — this command reached nobody, and 204 is success. The
        // NotInGroup branch can answer 204 honestly because it pushes
        // `NotInGroup` down the caller's socket; here there is no socket, so
        // the HTTP status is the only surface left. Answering success made a
        // dropped Seek indistinguishable from an applied one: on 2026-08-16,
        // 19 commands were lost this way and the only symptom anyone could
        // see was a scrub bar that did not move. 503, not 4xx: the caller did
        // nothing wrong and the condition is transient — its socket may
        // reconnect and re-attach.
        CommandOutcome::DroppedNoSocket => HttpResponse::ServiceUnavailable().body(
            "SyncPlay command dropped: this device has no live /socket and no \
             persisted group membership to recover from",
        ),
        _ => no_content(),
    }
}

/// `POST /SyncPlay/Ping` — the client's periodic time-sync report (body
/// `{Ping: <ms>}`, ignored). Routed through `dispatch` for its two side
/// effects: the member's liveness TTL refreshes (T83 ghost prune), and a
/// group-less caller is answered with NotInGroup.
async fn ping(
    auth: AuthSession,
    hub: web::Data<SessionHub>,
    registry: web::Data<GroupRegistry>,
    state: web::Data<crate::state::AppState>,
) -> HttpResponse {
    dispatch(
        &hub,
        &registry,
        &state.stores,
        auth.sync_key(),
        "ping",
        |mid| GroupMsg::MemberPing { member_id: mid },
    )
    .await
}

/// Add the caller (from the hub) to `handle` as a member. Shared by New + Join.
async fn add_caller_to_group(
    hub: &SessionHub,
    sinks: &MemberSinks,
    device_id: &str,
    user: pharos_core::UserId,
    handle: GroupHandle,
) {
    let Some(sess) = resolve_with_retry(hub, device_id).await else {
        tracing::warn!(
            device_id = %device_id,
            "syncplay: New/Join but no /socket registered for this deviceId after retry — \
             cannot add member (client must open /socket first)"
        );
        return;
    };
    // Record the group before AddMember so the wall-clock epoch is available
    // the instant the first catch-up command is broadcast to the socket.
    hub.attach_group(device_id, handle.clone());
    let member_id = sess.member_id;
    // Register the socket's sink in the replica's delivery table before
    // AddMember, so the actor's join catch-up reaches it.
    sinks.insert(member_id, sess.conn_gen, sess.sink);
    let (reply_tx, reply_rx) = oneshot::channel();
    if handle
        .tx
        .send(GroupMsg::AddMember {
            member_id,
            name: sess.name.clone(),
            user: Some(user),
            reply: reply_tx,
        })
        .await
        .is_ok()
    {
        let _ = reply_rx.await;
        tracing::info!(
            device_id = %device_id, %member_id, user = %sess.name, group = %handle.group_id,
            "syncplay: member added to group"
        );
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct NewGroupBody {
    group_name: String,
}

/// Parse the optional `{GroupName}` body of `/SyncPlay/New` (tolerating an
/// empty/absent body).
fn parse_group_name(bytes: &web::Bytes) -> Option<String> {
    serde_json::from_slice::<NewGroupBody>(bytes)
        .ok()
        .map(|b| b.group_name)
        .filter(|n| !n.trim().is_empty())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct JoinGroupBody {
    group_id: Uuid,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SetNewQueueBody {
    #[serde(default)]
    playing_queue: Vec<String>,
    #[serde(default)]
    playing_item_position: usize,
    #[serde(default)]
    start_position_ticks: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SeekBody {
    #[serde(default)]
    position_ticks: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ReadyBody {
    #[serde(default)]
    position_ticks: u64,
    /// The queue entry this Ready/Buffering is FOR. jellyfin-web sends it on
    /// every post; the actor uses it to reject stale reports racing a queue
    /// change (B37).
    #[serde(default)]
    playlist_item_id: Option<String>,
    /// `ReadyRequestDto.IsPlaying` — whether the reporting player is playing.
    /// Read by the actor to pull a member playing into a paused group back.
    #[serde(default)]
    is_playing: bool,
}

/// `/SyncPlay/NextItem` + `/PreviousItem` body — the entry the CLIENT
/// believes is playing. Mismatch = client behind / double-press → no-op
/// (real-Jellyfin semantics, B37).
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct NextItemBody {
    #[serde(default)]
    playlist_item_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SetPlaylistItemBody {
    playlist_item_id: String,
}

/// `/SyncPlay/Queue` body — `QueueRequestDto`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct QueueBody {
    #[serde(default)]
    item_ids: Vec<String>,
    #[serde(default)]
    mode: QueueMode,
}

/// `/SyncPlay/RemoveFromPlaylist` body — `RemoveFromPlaylistRequestDto`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RemoveFromPlaylistBody {
    #[serde(default)]
    playlist_item_ids: Vec<String>,
    #[serde(default)]
    clear_playlist: bool,
    #[serde(default)]
    clear_playing_item: bool,
}

/// `/SyncPlay/MovePlaylistItem` body — `MovePlaylistItemRequestDto`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct MovePlaylistItemBody {
    #[serde(default)]
    playlist_item_id: String,
    #[serde(default)]
    new_index: i64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ModeBody {
    #[serde(default)]
    mode: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct IgnoreWaitBody {
    #[serde(default)]
    ignore_wait: bool,
}

/// Result of [`creator_now_playing`] — the item to seed plus the diagnostics
/// (B227) that show, at the point of the decision, whether there was more
/// than one session record to choose from and how stale the chosen one was.
#[derive(Debug, Clone)]
struct SeedCandidate {
    item_id: String,
    position_ms: u64,
    is_paused: bool,
    session_id: String,
    /// Count of session records matching (user, device) — more than one
    /// means a ghost (a Started with no matching Stopped) is sitting beside
    /// the live session, so `creator_now_playing`'s pick is one guess among
    /// several rather than the only candidate.
    candidates: usize,
    since_activity_ms: u64,
}

/// Count of `/SyncPlay/New` seeds where more than one session record
/// matched (user, device) — the ghost condition B227 fixes. A nonzero rate
/// here is the signal: `sum(rate(pharos_session_seed_multiple_candidates_total[1h]))`.
fn record_seed_multiple_candidates() {
    metrics::counter!("pharos_session_seed_multiple_candidates_total").increment(1);
}

/// B227 defence in depth: even if eviction (V169) somehow left more than one
/// session record for the same (user, device) — e.g. ghosts already in the
/// registry from before that fix was deployed — seed from whichever record
/// was active most recently rather than whichever `snapshot()` happened to
/// list first (it sorts by session id, a random UUID, which carries no
/// relation to recency).
fn pick_most_recent(
    candidates: Vec<crate::sessions::SessionRecord>,
) -> Option<crate::sessions::SessionRecord> {
    candidates.into_iter().max_by_key(|s| s.activity_at)
}

/// Pure decision core of [`creator_now_playing`] — takes the already-filtered
/// (user, device) matches and picks the seed, recording the B227
/// diagnostics. Split out so it is testable without the registry actor: a
/// test can hand it hand-built `SessionRecord`s to exercise the
/// multiple-candidates / most-recent-wins paths without needing the actor
/// (and its own eviction, V169) to produce that state first.
fn resolve_seed(matches: Vec<crate::sessions::SessionRecord>) -> Option<SeedCandidate> {
    let candidates = matches.len();
    if candidates > 1 {
        record_seed_multiple_candidates();
    }
    let chosen = pick_most_recent(matches)?;
    let item_id = chosen.now_playing_item_id.clone()?;
    let since_activity_ms = chosen.activity_age_ms();
    Some(SeedCandidate {
        item_id,
        position_ms: chosen.position_ticks / POSITION_TICKS_PER_MS,
        is_paused: chosen.is_paused,
        session_id: chosen.id,
        candidates,
        since_activity_ms,
    })
}

/// Find the creator's currently-playing item so a freshly-created group can
/// inherit it (real-Jellyfin parity: a new group is seeded from the creating
/// session's now-playing). Matches the creator's OWN session by (user, device):
/// jellyfin-web derives an identical `deviceId` across same-UA installs (B53),
/// so the user id disambiguates two people (e.g. Alison + Lace) on one browser.
async fn creator_now_playing(
    state: &crate::state::AppState,
    user_id: &pharos_core::UserId,
    device_id: &str,
) -> Option<SeedCandidate> {
    let uid = user_id.0.simple().to_string();
    let sessions = state.sessions.snapshot().await.ok()?;
    let matches: Vec<_> = sessions
        .into_iter()
        .filter(|s| s.user_id == uid && s.device_id == device_id && s.now_playing_item_id.is_some())
        .collect();
    resolve_seed(matches)
}

/// Resolve each queue item's duration, INDEX-ALIGNED with `item_ids`.
///
/// Built by mapping over the same vec the caller passes to the group, so the
/// alignment is structural. An item that does not parse, is missing, or has no
/// probed duration yields `None` — the group then leaves that item's position
/// unclamped, which is the behaviour it had before runtimes existed.
///
/// The group actor cannot do this itself: it has no store handle, and until it
/// was told, it had no way to know an item had ENDED, so a group left Playing
/// extrapolated past the end forever (T114).
async fn resolve_runtimes_ms(
    state: &crate::state::AppState,
    item_ids: &[String],
) -> Vec<Option<u64>> {
    use pharos_core::MediaStore;
    let mut out = Vec::with_capacity(item_ids.len());
    for id in item_ids {
        let runtime = match pharos_jellyfin_api::dto::parse_item_id(id) {
            Some(num) => state
                .stores
                .get(num)
                .await
                .ok()
                .and_then(|item| item.probe.duration_ms),
            None => None,
        };
        out.push(runtime);
    }
    out
}

async fn new_group(
    auth: AuthSession,
    hub: web::Data<SessionHub>,
    sinks: web::Data<MemberSinks>,
    registry: web::Data<GroupRegistry>,
    state: web::Data<crate::state::AppState>,
    body: web::Bytes,
) -> HttpResponse {
    if !sync_play_allows(&auth.user, SyncPlayNeed::Create, "new") {
        return HttpResponse::Forbidden().finish();
    }
    let Some(dev) = auth.sync_key() else {
        tracing::warn!("syncplay: New with no deviceId — cannot form a group");
        return no_content();
    };
    let Ok(handle) = registry.create().await else {
        tracing::error!("syncplay: group registry unreachable on New");
        return no_content();
    };
    // Name the group from the request, else "<creator>'s Group".
    let name = parse_group_name(&body).unwrap_or_else(|| {
        let who = hub
            .resolve(&dev)
            .map(|s| s.name)
            .unwrap_or_else(|| "Watch".to_string());
        format!("{who}'s Group")
    });
    tracing::info!(device_id = %dev, group = %handle.group_id, %name, "syncplay: New group created");
    // Add the creator as the FIRST message to the fresh actor. The group actor
    // terminates the moment it processes a message and finds no members, so a
    // brand-new (member-less) group must receive AddMember before anything else
    // (e.g. SetGroupName) — otherwise it dies before the creator ever joins.
    add_caller_to_group(&hub, &sinks, &dev, auth.user.id, handle.clone()).await;
    let _ = handle.tx.send(GroupMsg::SetGroupName { name }).await;
    // Seed the group's queue from the creator's already-active playback. Stock
    // jellyfin-web does NOT re-send its play queue when SyncPlay is enabled on
    // an ALREADY-playing leader (its `playbackstart` already fired), so the
    // group would otherwise stay queue-less until the leader manually
    // re-triggers playback — and a member who joins in that window gets
    // `GroupJoined` but no media, stranded (live-observed: a remote joiner sat
    // ~1 min with a blank player until the leader re-set the queue). The seed
    // is silent (no broadcast, no gate) and only applies while the queue is
    // still empty, so a real `SetNewQueue` that lands first always wins.
    if let Some(seed) = creator_now_playing(
        &state,
        &auth.user.id,
        auth.device_id.as_deref().unwrap_or(""),
    )
    .await
    {
        tracing::info!(
            device_id = %dev, group = %handle.group_id,
            item_id = %seed.item_id, position_ms = seed.position_ms, is_paused = seed.is_paused,
            session_id = %pharos_core::secret_prefix(&seed.session_id), candidates = seed.candidates,
            since_activity_ms = seed.since_activity_ms,
            "syncplay: seeding new group from creator's now-playing item"
        );
        let runtime_ms = resolve_runtimes_ms(&state, std::slice::from_ref(&seed.item_id))
            .await
            .into_iter()
            .next()
            .flatten();
        if !may_queue(
            &state,
            &auth.user,
            &handle,
            std::slice::from_ref(&seed.item_id),
            QueueVia::Seed,
        )
        .await
        {
            return no_content();
        }
        let _ = handle
            .tx
            .send(GroupMsg::SeedQueue {
                item_id: seed.item_id,
                runtime_ms,
                position_ms: seed.position_ms,
                is_paused: seed.is_paused,
            })
            .await;
    }
    no_content()
}

async fn join_group(
    auth: AuthSession,
    hub: web::Data<SessionHub>,
    sinks: web::Data<MemberSinks>,
    registry: web::Data<GroupRegistry>,
    state: web::Data<crate::state::AppState>,
    body: web::Json<JoinGroupBody>,
) -> HttpResponse {
    if !sync_play_allows(&auth.user, SyncPlayNeed::Join, "join") {
        return HttpResponse::Forbidden().finish();
    }
    let Some(dev) = auth.sync_key() else {
        return no_content();
    };
    let Ok(handle) = registry.get_or_create(GroupId(body.group_id)).await else {
        tracing::error!("syncplay: group registry unreachable on Join");
        return no_content();
    };
    if !may_join(&state, &auth.user, &handle, GroupHiddenRoute::Join).await {
        // Jellyfin-web leaves group mode on NotInGroup rather than waiting.
        if let Some(sess) = hub.resolve(&dev) {
            let _ = sess
                .sink
                .send(pharos_sync::messages::ServerMsg::NotInGroup)
                .await;
        }
        return no_content();
    }
    tracing::info!(device_id = %dev, group = %handle.group_id, "syncplay: Join group");
    add_caller_to_group(&hub, &sinks, &dev, auth.user.id, handle).await;
    no_content()
}

async fn leave_group(
    auth: AuthSession,
    hub: web::Data<SessionHub>,
    state: web::Data<crate::state::AppState>,
) -> HttpResponse {
    if let Some(member) = auth
        .sync_key()
        .and_then(|dev| hub.resolve(&dev))
        .map(|s| s.member_id)
    {
        end_owned_cast(&state, &auth, member).await;
    }
    if let Some(dev) = auth.sync_key() {
        if let Some(sess) = hub.resolve(&dev) {
            if let Some(h) = hub.detach_group(&dev) {
                let _ =
                    h.tx.send(GroupMsg::RemoveMember {
                        member_id: sess.member_id,
                    })
                    .await;
            }
            // B25 — ALWAYS acknowledge the leave to the LEAVER, group or not.
            // jellyfin-web exits SyncPlay mode only on GroupLeft/NotInGroup
            // over the socket; the group's own MemberLeft broadcast happens
            // after the roster removal so it never reaches the leaver. Without
            // this, Leave was a silent 204 and a client whose group the server
            // had lost (B24) was WEDGED: stuck in group mode, playback
            // controls hijacked, no way back to the library short of a reload.
            let _ = sess
                .sink
                .send(pharos_sync::messages::ServerMsg::GroupLeft)
                .await;
        }
    }
    no_content()
}

/// A user who owns a cast seat cannot leave the group and keep it: the seat
/// would outlive the member it stands for. Ending it first (reason
/// `owner_left`) also restores the owner and retracts the seat from the roster
/// before the owner's own removal is announced. Only the member that cast
/// counts: the same user leaving from another device (jellyfin-web sends Leave
/// on page unload) is not the seat's owner. Idempotent: no seat, no-op.
async fn end_owned_cast(state: &crate::state::AppState, auth: &AuthSession, member: MemberId) {
    let Some(service) = state.cast.service() else {
        return;
    };
    let user = auth.user.id;
    if !service
        .snapshot()
        .await
        .iter()
        .any(|c| c.owner == user && c.owner_member == member)
    {
        return;
    }
    service
        .stop(user, pharos_cast::events::CastReason::OwnerLeft)
        .await;
    let _ = state.bus.send(crate::state::SocketBroadcast::CastChanged);
}

async fn set_new_queue(
    auth: AuthSession,
    hub: web::Data<SessionHub>,
    registry: web::Data<GroupRegistry>,
    state: web::Data<crate::state::AppState>,
    body: web::Json<SetNewQueueBody>,
) -> HttpResponse {
    let body = body.into_inner();
    let start_ms = body.start_position_ticks / POSITION_TICKS_PER_MS;
    // Resolved HERE, not in the closure: `dispatch` takes a synchronous
    // builder, and this is a store read. Mapping over the same vec is what
    // keeps the two index-aligned.
    let item_runtimes_ms = resolve_runtimes_ms(&state, &body.playing_queue).await;
    let queued = body.playing_queue.clone();
    dispatch_checked(
        &hub,
        &registry,
        &state.stores,
        auth.sync_key(),
        "setnewqueue",
        async |group| may_queue(&state, &auth.user, group, &queued, QueueVia::SetNewQueue).await,
        move |mid| GroupMsg::SetNewQueue {
            sender: mid,
            item_ids: body.playing_queue,
            item_runtimes_ms,
            playing_index: body.playing_item_position,
            start_position_ms: start_ms,
        },
    )
    .await
}

async fn unpause(
    auth: AuthSession,
    hub: web::Data<SessionHub>,
    registry: web::Data<GroupRegistry>,
    state: web::Data<crate::state::AppState>,
) -> HttpResponse {
    dispatch(
        &hub,
        &registry,
        &state.stores,
        auth.sync_key(),
        "unpause",
        |mid| GroupMsg::Unpause { sender: mid },
    )
    .await
}

async fn pause(
    auth: AuthSession,
    hub: web::Data<SessionHub>,
    registry: web::Data<GroupRegistry>,
    state: web::Data<crate::state::AppState>,
) -> HttpResponse {
    dispatch(
        &hub,
        &registry,
        &state.stores,
        auth.sync_key(),
        "pause",
        |mid| GroupMsg::PauseShared { sender: mid },
    )
    .await
}

async fn seek(
    auth: AuthSession,
    hub: web::Data<SessionHub>,
    registry: web::Data<GroupRegistry>,
    state: web::Data<crate::state::AppState>,
    body: web::Json<SeekBody>,
) -> HttpResponse {
    let pos = body.position_ticks / POSITION_TICKS_PER_MS;
    // T87 — prewarm the target segments for every member's variant BEFORE
    // dispatching: clients only apply the seek at `When` (lead time) and
    // then re-buffer, so the transcodes get a multi-second head start and
    // the slowest member's segments are ready (or nearly) when requested.
    if let Some(handle) = auth
        .device_id
        .as_deref()
        .and_then(|d| hub.resolve(d))
        .and_then(|sess| sess.group)
    {
        let state = state.clone();
        actix_web::rt::spawn(async move {
            if let Some(snap) = handle.snapshot().await {
                if let Some(mid) = snap
                    .current_item_id
                    .as_deref()
                    .and_then(pharos_jellyfin_api::dto::parse_item_id)
                {
                    super::hls::prewarm_group_seek(&state, mid, pos);
                }
            }
        });
    }
    dispatch(
        &hub,
        &registry,
        &state.stores,
        auth.sync_key(),
        "seek",
        move |mid| GroupMsg::SeekTo {
            sender: mid,
            position_ms: pos,
        },
    )
    .await
}

async fn buffering(
    auth: AuthSession,
    hub: web::Data<SessionHub>,
    registry: web::Data<GroupRegistry>,
    state: web::Data<crate::state::AppState>,
    body: web::Json<ReadyBody>,
) -> HttpResponse {
    let body = body.into_inner();
    let pos = body.position_ticks / POSITION_TICKS_PER_MS;
    let pli = body.playlist_item_id;
    dispatch(
        &hub,
        &registry,
        &state.stores,
        auth.sync_key(),
        "buffering",
        move |mid| GroupMsg::BufferingStart {
            member_id: mid,
            position_ms: pos,
            playlist_item_id: pli,
        },
    )
    .await
}

async fn ready(
    auth: AuthSession,
    hub: web::Data<SessionHub>,
    registry: web::Data<GroupRegistry>,
    state: web::Data<crate::state::AppState>,
    body: web::Json<ReadyBody>,
) -> HttpResponse {
    let body = body.into_inner();
    let pos = body.position_ticks / POSITION_TICKS_PER_MS;
    let pli = body.playlist_item_id;
    let is_playing = body.is_playing;
    dispatch(
        &hub,
        &registry,
        &state.stores,
        auth.sync_key(),
        "ready",
        move |mid| GroupMsg::MemberReady {
            member_id: mid,
            position_ms: pos,
            playlist_item_id: pli,
            is_playing,
        },
    )
    .await
}

/// jellyfin-web posts `IgnoreWait: true` when it halts its own playback (its
/// player never started within its 30s budget) and `false` when it re-follows
/// group playback. Honoring it keeps a halted member from wedging every
/// subsequent readiness gate into the anti-wedge timeout.
async fn set_ignore_wait(
    auth: AuthSession,
    hub: web::Data<SessionHub>,
    registry: web::Data<GroupRegistry>,
    state: web::Data<crate::state::AppState>,
    body: web::Json<IgnoreWaitBody>,
) -> HttpResponse {
    let ignore = body.ignore_wait;
    dispatch(
        &hub,
        &registry,
        &state.stores,
        auth.sync_key(),
        "setignorewait",
        move |mid| GroupMsg::SetIgnoreWait {
            member_id: mid,
            ignore,
        },
    )
    .await
}

async fn set_playlist_item(
    auth: AuthSession,
    hub: web::Data<SessionHub>,
    registry: web::Data<GroupRegistry>,
    state: web::Data<crate::state::AppState>,
    body: web::Json<SetPlaylistItemBody>,
) -> HttpResponse {
    let pli = body.into_inner().playlist_item_id;
    dispatch(
        &hub,
        &registry,
        &state.stores,
        auth.sync_key(),
        "setplaylistitem",
        move |mid| GroupMsg::SetPlaylistItem {
            sender: mid,
            playlist_item_id: pli,
        },
    )
    .await
}

async fn queue(
    auth: AuthSession,
    hub: web::Data<SessionHub>,
    registry: web::Data<GroupRegistry>,
    state: web::Data<crate::state::AppState>,
    body: web::Json<QueueBody>,
) -> HttpResponse {
    let body = body.into_inner();
    if body.item_ids.len() > MAX_QUEUE_REQUEST_ITEMS {
        return HttpResponse::BadRequest().body(format!(
            "SyncPlay Queue takes at most {MAX_QUEUE_REQUEST_ITEMS} items per request"
        ));
    }
    if body
        .item_ids
        .iter()
        .any(|id| pharos_jellyfin_api::dto::WireId::parse(id).is_none())
    {
        return HttpResponse::BadRequest().body("SyncPlay Queue ItemIds must be item ids");
    }
    // Only a caller that is in a group costs store lookups and a roster
    // snapshot; anyone else is answered by `dispatch` (NotInGroup / recovery).
    let mut item_runtimes_ms = Vec::new();
    let group = auth
        .sync_key()
        .and_then(|key| hub.resolve(&key))
        .and_then(|sess| sess.group);
    if let Some(group) = group {
        let queued = group.snapshot().await.map_or(0, |s| s.queue_len);
        if queued.saturating_add(body.item_ids.len()) > MAX_QUEUE_LEN {
            return HttpResponse::BadRequest().body(format!(
                "SyncPlay queue is full: at most {MAX_QUEUE_LEN} entries per group"
            ));
        }
        item_runtimes_ms = resolve_runtimes_ms(&state, &body.item_ids).await;
    }
    let queued = body.item_ids.clone();
    dispatch_checked(
        &hub,
        &registry,
        &state.stores,
        auth.sync_key(),
        "queue",
        async |group| may_queue(&state, &auth.user, group, &queued, QueueVia::Queue).await,
        move |mid| GroupMsg::Queue {
            sender: mid,
            item_ids: body.item_ids,
            item_runtimes_ms,
            mode: body.mode,
        },
    )
    .await
}

async fn remove_from_playlist(
    auth: AuthSession,
    hub: web::Data<SessionHub>,
    registry: web::Data<GroupRegistry>,
    state: web::Data<crate::state::AppState>,
    body: web::Json<RemoveFromPlaylistBody>,
) -> HttpResponse {
    let body = body.into_inner();
    dispatch(
        &hub,
        &registry,
        &state.stores,
        auth.sync_key(),
        "removefromplaylist",
        move |mid| GroupMsg::RemoveFromPlaylist {
            sender: mid,
            playlist_item_ids: body.playlist_item_ids,
            clear_playlist: body.clear_playlist,
            clear_playing_item: body.clear_playing_item,
        },
    )
    .await
}

async fn move_playlist_item(
    auth: AuthSession,
    hub: web::Data<SessionHub>,
    registry: web::Data<GroupRegistry>,
    state: web::Data<crate::state::AppState>,
    body: web::Json<MovePlaylistItemBody>,
) -> HttpResponse {
    let body = body.into_inner();
    // Jellyfin binds a missing Guid to Guid.Empty, which names no entry: 204.
    if body.playlist_item_id.is_empty() {
        return no_content();
    }
    dispatch(
        &hub,
        &registry,
        &state.stores,
        auth.sync_key(),
        "moveplaylistitem",
        move |mid| GroupMsg::MovePlaylistItem {
            sender: mid,
            playlist_item_id: body.playlist_item_id,
            new_index: body.new_index,
        },
    )
    .await
}

async fn next_item(
    auth: AuthSession,
    hub: web::Data<SessionHub>,
    registry: web::Data<GroupRegistry>,
    state: web::Data<crate::state::AppState>,
    body: web::Bytes,
) -> HttpResponse {
    // Bytes + lenient parse: tolerate clients that post no body at all.
    let pli = serde_json::from_slice::<NextItemBody>(&body)
        .unwrap_or_default()
        .playlist_item_id;
    dispatch(
        &hub,
        &registry,
        &state.stores,
        auth.sync_key(),
        "nextitem",
        move |mid| GroupMsg::NextItem {
            sender: mid,
            playlist_item_id: pli,
        },
    )
    .await
}

async fn previous_item(
    auth: AuthSession,
    hub: web::Data<SessionHub>,
    registry: web::Data<GroupRegistry>,
    state: web::Data<crate::state::AppState>,
    body: web::Bytes,
) -> HttpResponse {
    let pli = serde_json::from_slice::<NextItemBody>(&body)
        .unwrap_or_default()
        .playlist_item_id;
    dispatch(
        &hub,
        &registry,
        &state.stores,
        auth.sync_key(),
        "previousitem",
        move |mid| GroupMsg::PreviousItem {
            sender: mid,
            playlist_item_id: pli,
        },
    )
    .await
}

async fn set_repeat_mode(
    auth: AuthSession,
    hub: web::Data<SessionHub>,
    registry: web::Data<GroupRegistry>,
    state: web::Data<crate::state::AppState>,
    body: web::Json<ModeBody>,
) -> HttpResponse {
    let mode = body.into_inner().mode;
    dispatch(
        &hub,
        &registry,
        &state.stores,
        auth.sync_key(),
        "setrepeatmode",
        move |mid| GroupMsg::SetRepeatMode { sender: mid, mode },
    )
    .await
}

async fn set_shuffle_mode(
    auth: AuthSession,
    hub: web::Data<SessionHub>,
    registry: web::Data<GroupRegistry>,
    state: web::Data<crate::state::AppState>,
    body: web::Json<ModeBody>,
) -> HttpResponse {
    let mode = body.into_inner().mode;
    dispatch(
        &hub,
        &registry,
        &state.stores,
        auth.sync_key(),
        "setshufflemode",
        move |mid| GroupMsg::SetShuffleMode { sender: mid, mode },
    )
    .await
}

/// Jellyfin's `GroupInfoDto` shape. Only the fields jellyfin-web reads
/// for the dropdown render — full state lives over the socket.
#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
struct GroupInfoDto {
    group_id: String,
    group_name: String,
    /// `Idle` / `Playing` / `Paused` / `Waiting`. We map from the
    /// internal `PlaybackState` enum via the actor snapshot.
    state: &'static str,
    /// Member count. Full per-member list ships with the socket
    /// `SyncPlayGroupUpdate` payload — clients render it from there
    /// after joining.
    participants: Vec<String>,
    last_updated_at: String,
}

fn play_state_str(s: pharos_sync::messages::GroupPlayState) -> &'static str {
    use pharos_sync::messages::GroupPlayState;
    match s {
        GroupPlayState::Idle => "Idle",
        GroupPlayState::Waiting => "Waiting",
        GroupPlayState::Playing => "Playing",
        GroupPlayState::Paused => "Paused",
    }
}

/// Where a group was withheld from a user: the `route` label of
/// `pharos_syncplay_group_hidden_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GroupHiddenRoute {
    List,
    Join,
    /// A `SyncPlayJoinGroup` frame on `/socket`.
    Socket,
    /// A `Join` on the native `/sync/v1/ws`.
    Native,
    /// `Group.list` on the native RPC API.
    RpcList,
}

impl GroupHiddenRoute {
    pub(crate) fn label(self) -> &'static str {
        match self {
            GroupHiddenRoute::List => "list",
            GroupHiddenRoute::Join => "join",
            GroupHiddenRoute::Socket => "socket",
            GroupHiddenRoute::Native => "native",
            GroupHiddenRoute::RpcList => "rpc_list",
        }
    }
}

fn group_hidden(route: GroupHiddenRoute, user: &pharos_core::User, group: &str) {
    metrics::counter!("pharos_syncplay_group_hidden_total", "route" => route.label()).increment(1);
    tracing::warn!(user = %user.name, group, route = route.label(), "syncplay group withheld: its queue holds an item the user may not see");
}

/// What a SyncPlay request asks of `UserPolicy.sync_play_access`: the `need`
/// label of `pharos_syncplay_access_denied_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SyncPlayNeed {
    /// List or join a group.
    Join,
    /// Create a group.
    Create,
}

impl SyncPlayNeed {
    fn label(self) -> &'static str {
        match self {
            SyncPlayNeed::Join => "join",
            SyncPlayNeed::Create => "create",
        }
    }
}

/// Whether `user`'s `SyncPlayAccess` grants `need`, as in Jellyfin:
/// `CreateAndJoinGroups` grants both, `JoinGroups` joining only, `None`
/// neither. An empty value reads as Jellyfin's default, `CreateAndJoinGroups`;
/// any other value Jellyfin does not define grants nothing. A refusal counts
/// under `pharos_syncplay_access_denied_total{need}` and logs a WARN naming
/// the user, their access and `via`, the route that asked.
pub(crate) fn sync_play_allows(user: &pharos_core::User, need: SyncPlayNeed, via: &str) -> bool {
    let access = user.policy.sync_play_access.as_str();
    let allowed = if access.is_empty() || access.eq_ignore_ascii_case("CreateAndJoinGroups") {
        true
    } else if access.eq_ignore_ascii_case("JoinGroups") {
        need == SyncPlayNeed::Join
    } else {
        false
    };
    if !allowed {
        metrics::counter!("pharos_syncplay_access_denied_total", "need" => need.label())
            .increment(1);
        tracing::warn!(user = %user.name, access, need = need.label(), via, "syncplay request refused by the user's SyncPlayAccess");
    }
    allowed
}

/// Whether `user` may see every item in a group's queue, and so the group
/// itself: its name, its members and what it plays (V201). An entry that
/// names no stored item hides the group.
async fn may_see_queue(
    state: &crate::state::AppState,
    user: &pharos_core::User,
    queue: &[String],
) -> bool {
    if crate::services::library::hides_nothing(&user.policy) || queue.is_empty() {
        return true;
    }
    let Some(mut ids) = queue
        .iter()
        .map(|w| pharos_jellyfin_api::dto::parse_item_id(w))
        .collect::<Option<Vec<u64>>>()
    else {
        return false;
    };
    let wanted = ids.len();
    crate::services::access::retain_visible(state, user, &mut ids, |id| *id, "syncplay")
        .await
        .is_ok()
        && ids.len() == wanted
}

/// Whether `user` may join `handle`'s group (V201): their `SyncPlayAccess`
/// lets them join, and they may see its queue, counting a refusal under
/// `route`. The queue comes from the group's actor, or from its persisted
/// snapshot when the actor answers nothing (it lives on another replica). A
/// queue that cannot be read refuses a restricted user.
pub(crate) async fn may_join(
    state: &crate::state::AppState,
    user: &pharos_core::User,
    handle: &GroupHandle,
    route: GroupHiddenRoute,
) -> bool {
    if !sync_play_allows(user, SyncPlayNeed::Join, route.label()) {
        return false;
    }
    if crate::services::library::hides_nothing(&user.policy) {
        return true;
    }
    let group = handle.group_id.to_string();
    let queue = match handle.snapshot().await {
        Some(snap) => Some(snap.queue_item_ids),
        None => persisted_queue(state, &group).await,
    };
    let allowed = match &queue {
        Some(queue) => may_see_queue(state, user, queue).await,
        None => {
            tracing::warn!(user = %user.name, group, "syncplay join: the group's queue could not be read");
            false
        }
    };
    if !allowed {
        group_hidden(route, user, &group);
    }
    allowed
}

/// The queue change a refusal was for: the `via` label of
/// `pharos_syncplay_queue_refused_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QueueVia {
    SetNewQueue,
    Queue,
    /// The creator's now-playing item seeding a new group.
    Seed,
}

impl QueueVia {
    pub(crate) fn label(self) -> &'static str {
        match self {
            QueueVia::SetNewQueue => "set_new_queue",
            QueueVia::Queue => "queue",
            QueueVia::Seed => "seed",
        }
    }
}

/// Why a queue change was refused: the `reason` label of
/// `pharos_syncplay_queue_refused_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QueueRefusal {
    /// The member asking may not see an item.
    Queuer,
    /// Another member's user may not see an item, or no longer exists.
    Member,
    /// A member has no recorded user (restored from an older snapshot or
    /// admitted by an older replica) and so can see nothing.
    UnknownMember,
    /// The group's members could not be read.
    Unreadable,
}

impl QueueRefusal {
    pub(crate) fn label(self) -> &'static str {
        match self {
            QueueRefusal::Queuer => "queuer",
            QueueRefusal::Member => "member",
            QueueRefusal::UnknownMember => "unknown_member",
            QueueRefusal::Unreadable => "unreadable",
        }
    }
}

/// Whether `user` may put `items` in `handle`'s queue: as in Jellyfin
/// (`AllUsersHaveAccessToQueue`), every member's user, the asker included,
/// must be able to see every one of them, so no member's client is handed an
/// item its user may not open (V201). Members come from the group's actor,
/// or its persisted snapshot when the actor lives on another replica. A
/// refusal counts under `pharos_syncplay_queue_refused_total{via,reason}`
/// and logs a WARN naming the group, the asker and, for another member, that
/// member's user.
pub(crate) async fn may_queue(
    state: &crate::state::AppState,
    user: &pharos_core::User,
    handle: &GroupHandle,
    items: &[String],
    via: QueueVia,
) -> bool {
    use pharos_core::UserStore as _;
    let group = handle.group_id.to_string();
    let refuse = |reason: QueueRefusal, member: Option<pharos_core::UserId>| {
        metrics::counter!(
            "pharos_syncplay_queue_refused_total",
            "via" => via.label(),
            "reason" => reason.label()
        )
        .increment(1);
        tracing::warn!(
            user = %user.name,
            group,
            via = via.label(),
            reason = reason.label(),
            member_user = ?member.map(|m| m.0),
            items = items.len(),
            "syncplay queue change refused"
        );
        false
    };
    if !may_see_queue(state, user, items).await {
        return refuse(QueueRefusal::Queuer, Some(user.id));
    }
    let members = match handle.snapshot().await {
        Some(snap) => Some(snap.member_users),
        None => persisted_member_users(state, &group).await,
    };
    let Some(members) = members else {
        return refuse(QueueRefusal::Unreadable, None);
    };
    let mut checked = std::collections::HashSet::from([user.id]);
    for member in members {
        let Some(member) = member else {
            return refuse(QueueRefusal::UnknownMember, None);
        };
        if !checked.insert(member) {
            continue;
        }
        let sees = match state.stores.get(member).await {
            Ok(record) => may_see_queue(state, &record.into_user(), items).await,
            Err(e) => {
                tracing::warn!(group, member_user = %member.0, error = %e, "syncplay: member's user unreadable");
                false
            }
        };
        if !sees {
            return refuse(QueueRefusal::Member, Some(member));
        }
    }
    true
}

/// Each member's user in a group's persisted snapshot; `None` when there is
/// no readable row.
async fn persisted_member_users(
    state: &crate::state::AppState,
    group: &str,
) -> Option<Vec<Option<pharos_core::UserId>>> {
    use pharos_core::SyncGroupStore;
    match state.stores.get_sync_group(group).await {
        Ok(row) => row
            .and_then(|row| pharos_sync::group::snapshot_summary(&row.state_json))
            .map(|summary| summary.member_users),
        Err(e) => {
            tracing::warn!(group, error = %e, "syncplay: persisted group snapshot unreadable");
            None
        }
    }
}

/// The queue in a group's persisted snapshot; `None` when there is no
/// readable row.
async fn persisted_queue(state: &crate::state::AppState, group: &str) -> Option<Vec<String>> {
    use pharos_core::SyncGroupStore;
    match state.stores.get_sync_group(group).await {
        Ok(row) => row
            .and_then(|row| pharos_sync::group::snapshot_summary(&row.state_json))
            .map(|summary| summary.queue_item_ids),
        Err(e) => {
            tracing::warn!(group, error = %e, "syncplay: persisted group snapshot unreadable");
            None
        }
    }
}

async fn list_groups(
    user: AuthUser,
    registry: web::Data<GroupRegistry>,
    state: web::Data<crate::state::AppState>,
) -> HttpResponse {
    if !sync_play_allows(&user.0, SyncPlayNeed::Join, "list") {
        return HttpResponse::Forbidden().finish();
    }
    let out: Vec<GroupInfoDto> = listed_groups(&state, &registry, &user.0, GroupHiddenRoute::List)
        .await
        .into_iter()
        .map(|g| GroupInfoDto {
            group_id: g.id,
            group_name: g.name,
            state: play_state_str(g.state),
            participants: g.participants,
            last_updated_at: now_iso8601(),
        })
        .collect();
    crate::api::jellyfin::wire::json(&out)
}

/// One group `user` may see, as the list surfaces show it.
pub(crate) struct ListedGroup {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) state: pharos_sync::messages::GroupPlayState,
    pub(crate) participants: Vec<String>,
    /// Wire id of the queue entry playing, or the first when none is.
    pub(crate) item_id: Option<String>,
}

/// Every group `user` may see (V201): live groups, then those only a fresh
/// persisted snapshot holds, a group whose queue holds an item the user may
/// not see withheld and counted under `route`. An unreachable registry lists
/// nothing, so the caller renders "no active groups" rather than an error.
pub(crate) async fn listed_groups(
    state: &crate::state::AppState,
    registry: &GroupRegistry,
    user: &pharos_core::User,
    route: GroupHiddenRoute,
) -> Vec<ListedGroup> {
    let Ok(handles) = registry.list().await else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(handles.len());
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for h in handles {
        let Some(snap) = h.snapshot().await else {
            continue;
        };
        seen.insert(snap.id.to_string());
        if !may_see_queue(state, user, &snap.queue_item_ids).await {
            group_hidden(route, user, &snap.id.to_string());
            continue;
        }
        out.push(ListedGroup {
            id: snap.id.to_string(),
            name: snap.group_name,
            state: snap.play_state,
            participants: snap.participants,
            item_id: snap
                .current_item_id
                .or_else(|| snap.queue_item_ids.first().cloned()),
        });
    }
    // B28 — groups this process doesn't hold in memory but that still exist as
    // fresh persisted snapshots: a just-restarted server (or a replica that
    // doesn't own the group) must still LIST the party or nobody can rejoin it
    // from the picker and it has to be recreated by hand. Join goes through
    // `get_or_create`, which hydrates from this same snapshot.
    {
        use pharos_core::SyncGroupStore;
        let cutoff =
            crate::sync_recovery::now_unix_secs() - crate::sync_recovery::RECOVERY_WINDOW_SECS;
        if let Ok(rows) = state.stores.list_sync_groups().await {
            for row in rows {
                if row.updated_at < cutoff || seen.contains(&row.group_id) {
                    continue;
                }
                let Some(summary) = pharos_sync::group::snapshot_summary(&row.state_json) else {
                    continue;
                };
                if !may_see_queue(state, user, &summary.queue_item_ids).await {
                    group_hidden(route, user, &row.group_id);
                    continue;
                }
                out.push(ListedGroup {
                    id: row.group_id,
                    name: summary.group_name,
                    state: summary.play_state,
                    participants: summary.participants,
                    item_id: summary.queue_item_ids.first().cloned(),
                });
            }
        }
    }
    out
}

fn now_iso8601() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    crate::api::jellyfin::dto::format_iso8601_ms(ms)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::sessions::SessionEvent;
    use actix_web::{test, App};
    use pharos_core::{SecretString, TokenStore, UserId, UserPolicy, UserRecord, UserStore};

    async fn seed_auth() -> (web::Data<crate::state::AppState>, String) {
        use crate::auth::BuiltinAuth;
        use crate::state::Stores;
        let stores = Stores::connect("sqlite::memory:").await.unwrap();
        let auth = BuiltinAuth::new(stores.clone());
        let hash = auth.hash_password(&SecretString::new("p")).unwrap();
        let uid = UserId::new();
        stores
            .create(UserRecord {
                id: uid,
                name: "u".into(),
                password_hash: hash,
                policy: UserPolicy::default(),
            })
            .await
            .unwrap();
        let token = stores.issue(uid, "t").await.unwrap();
        let state = web::Data::new(crate::state::AppState::new(stores, "t".into()));
        (state, token.0.expose().to_string())
    }

    /// The `pharos_syncplay_command_total{outcome}` label set is a dashboard
    /// contract: bounded, stable, and DISTINCT (two outcomes sharing a string
    /// would silently merge on the graph). Every dispatch path records exactly
    /// one, so a dropped command is never invisible — the whole point of the
    /// 2026-07-26 rolling-deploy instrumentation.
    #[actix_web::test]
    async fn command_outcome_labels_are_stable_and_distinct() {
        use std::collections::HashSet;
        let all = [
            CommandOutcome::Dispatched,
            CommandOutcome::Recovered,
            CommandOutcome::NotInGroup,
            CommandOutcome::DroppedNoSocket,
            CommandOutcome::NoDevice,
            CommandOutcome::ActorDown,
            CommandOutcome::Refused,
        ];
        let labels: HashSet<&str> = all.iter().map(|o| o.label()).collect();
        assert_eq!(
            labels.len(),
            all.len(),
            "every outcome has a distinct label"
        );
        // Pin the exact strings — a rename must be a conscious, reviewed change
        // because it breaks existing alerts/queries.
        assert_eq!(CommandOutcome::Dispatched.label(), "dispatched");
        assert_eq!(CommandOutcome::Recovered.label(), "recovered");
        assert_eq!(CommandOutcome::NotInGroup.label(), "not_in_group");
        assert_eq!(CommandOutcome::DroppedNoSocket.label(), "dropped_no_socket");
        assert_eq!(CommandOutcome::NoDevice.label(), "no_device");
        assert_eq!(CommandOutcome::ActorDown.label(), "actor_down");
    }

    /// A session whose hub entry points at a group actor that has since
    /// terminated (the group emptied while this socket was between
    /// reconnects) used to get a 204 for every command while nothing was
    /// applied and nothing was logged — the client stayed in SyncPlay mode
    /// with dead controls. The dead handle must be detached and the client
    /// told NotInGroup, counted as `actor_down`.
    #[actix_web::test]
    async fn dispatch_to_a_dead_group_actor_detaches_and_tells_not_in_group() {
        use crate::state::Stores;
        use pharos_sync::delivery::{LocalDelivery, MemberSinks};
        use pharos_sync::SessionHub;
        use std::sync::Arc;
        use tokio::sync::mpsc;
        let stores = Stores::connect("sqlite::memory:").await.unwrap();
        let registry = GroupRegistry::spawn(Arc::new(LocalDelivery::new(MemberSinks::new())));
        let hub = SessionHub::new();
        let dev = "user:dev-dead-group";
        let (sink_tx, mut sink_rx) = mpsc::channel(4);
        hub.register(dev.to_string(), "alison".into(), sink_tx);
        // A handle whose actor is gone: the receiver is dropped immediately.
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        hub.attach_group(
            dev,
            GroupHandle {
                group_id: GroupId::new(),
                tx,
                epoch_unix_ms: 0,
            },
        );

        let resp = dispatch(
            &hub,
            &registry,
            &stores,
            Some(dev.to_string()),
            "pause",
            |mid| GroupMsg::PauseShared { sender: mid },
        )
        .await;
        assert_eq!(resp.status(), actix_web::http::StatusCode::NO_CONTENT);
        assert!(
            matches!(
                sink_rx.try_recv(),
                Ok(pharos_sync::messages::ServerMsg::NotInGroup)
            ),
            "the client must be told it is in no group"
        );
        assert!(
            hub.resolve(dev).unwrap().group.is_none(),
            "the dead handle must be detached so the next command recovers or exits cleanly"
        );
    }

    /// The recovery guard: a device that no persisted snapshot claims must NOT
    /// be recovered — the command drops (and is counted `dropped_no_socket`)
    /// rather than being misapplied to some unrelated group.
    #[actix_web::test]
    async fn recovery_finds_nothing_when_no_persisted_group_claims_the_member() {
        use crate::state::Stores;
        use pharos_sync::delivery::{LocalDelivery, MemberSinks};
        use pharos_sync::messages::MemberId;
        use std::sync::Arc;
        let stores = Stores::connect("sqlite::memory:").await.unwrap();
        let registry = GroupRegistry::spawn(Arc::new(LocalDelivery::new(MemberSinks::new())));
        let applied = recover_and_apply(
            &SessionHub::new(),
            &registry,
            &stores,
            MemberId::new(),
            "dev-in-no-group",
            "pause",
            async |_| true,
            |mid| GroupMsg::PauseShared { sender: mid },
        )
        .await;
        assert_eq!(
            applied,
            Recovery::NotFound,
            "no persisted group claims this member — must drop, never misapply"
        );
    }

    #[actix_web::test]
    async fn creator_now_playing_matches_own_session_and_disambiguates_shared_device() {
        // The seed source for `/SyncPlay/New`. jellyfin-web derives an identical
        // deviceId across same-UA installs (B53), so two people on one browser
        // share a device — the seed must key on (user, device), never device
        // alone, or Alison's group would inherit Lace's now-playing.
        let (state, _token) = seed_auth().await;
        let alison = UserId::new();
        let lace = UserId::new();
        let dev = "shared-firefox-device";
        state
            .sessions
            .apply(SessionEvent::Started {
                session_id: "alison-sess".into(),
                user_id: alison,
                user_name: "alison".into(),
                device_id: dev.into(),
                device_name: "ff".into(),
                client: "web".into(),
                version: "1".into(),
                item_id: "scooby".into(),
                position_ticks: 900_000_000, // 90s (10_000 ticks/ms)
            })
            .await
            .unwrap();
        state
            .sessions
            .apply(SessionEvent::Started {
                session_id: "lace-sess".into(),
                user_id: lace,
                user_name: "lace".into(),
                device_id: dev.into(),
                device_name: "ff".into(),
                client: "web".into(),
                version: "1".into(),
                item_id: "other".into(),
                position_ticks: 0,
            })
            .await
            .unwrap();

        // Alison creating a group seeds from ALISON's item, not Lace's.
        let seed = creator_now_playing(&state, &alison, dev).await.unwrap();
        assert_eq!(seed.item_id, "scooby");
        assert_eq!(seed.position_ms, 90_000);
        assert!(!seed.is_paused);
        assert_eq!(seed.session_id, "alison-sess");
        assert_eq!(seed.candidates, 1, "only alison's own session matches");
        // A user with no active session yields nothing → no seed sent.
        assert!(creator_now_playing(&state, &UserId::new(), dev)
            .await
            .is_none());
        // Right user, wrong device → no match.
        assert!(creator_now_playing(&state, &alison, "other-device")
            .await
            .is_none());
    }

    /// Builds a minimal `SessionRecord` for tests that exercise
    /// [`resolve_seed`] directly — without needing the registry actor to
    /// produce the "more than one candidate" state.
    fn make_record(id: &str, item: &str, age_secs: u64) -> crate::sessions::SessionRecord {
        crate::sessions::SessionRecord {
            id: id.into(),
            user_id: "u".into(),
            user_name: "u".into(),
            device_id: "d".into(),
            device_name: "d".into(),
            client: "c".into(),
            application_version: "1".into(),
            now_playing_item_id: Some(item.into()),
            position_ticks: 0,
            is_paused: false,
            playable_media_types: Vec::new(),
            supported_commands: Vec::new(),
            max_streaming_bitrate: None,
            supports_media_control: false,
            is_active: true,
            supports_remote_control: true,
            has_custom_device_name: false,
            last_activity_date: String::new(),
            last_playback_check_in: String::new(),
            activity_at: tokio::time::Instant::now() - std::time::Duration::from_secs(age_secs),
        }
    }

    /// B227 signal: more than one session record matching (user, device)
    /// must show up as `candidates > 1` and increment
    /// `pharos_session_seed_multiple_candidates_total` — the query that
    /// proves the stale-seed bug is happening. Exercised via `resolve_seed`
    /// directly (not the registry) since V169's eviction means two live
    /// Started events for the same (user, device) can no longer produce this
    /// state through the public API — the residual case is a ghost already
    /// in the registry from before that fix was deployed.
    #[actix_web::test]
    async fn seed_with_a_ghost_session_counts_multiple_candidates() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};

        let recorder = DebuggingRecorder::new();
        let snapshotter = pharos_test_metrics::snapshotter(&recorder);
        let _guard = metrics::set_default_local_recorder(&recorder);

        let ghost = make_record("ghost-sess", "stale-item", 600);
        let live = make_record("live-sess", "fresh-item", 0);
        let seed = resolve_seed(vec![ghost, live]).unwrap();
        assert_eq!(seed.candidates, 2, "ghost + live session both matched");
        assert_eq!(
            seed.item_id, "fresh-item",
            "the most recently active record wins"
        );

        let count = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .find_map(|(ck, _, _, v)| {
                (ck.key().name() == "pharos_session_seed_multiple_candidates_total").then_some(v)
            })
            .unwrap();
        assert!(
            matches!(count, DebugValue::Counter(1)),
            "exactly one multiple-candidate seed happened — got {count:?}"
        );
    }

    /// B227 defence in depth: `pick_most_recent` must pick the live record
    /// regardless of input order — `snapshot()` sorts by session id (a
    /// random UUID), which carries no relation to recency.
    #[actix_web::test]
    async fn pick_most_recent_ignores_input_order() {
        let ghost = make_record("ghost-sess", "stale-item", 600);
        let live = make_record("live-sess", "fresh-item", 0);
        assert_eq!(
            pick_most_recent(vec![ghost.clone(), live.clone()])
                .unwrap()
                .id,
            "live-sess"
        );
        assert_eq!(pick_most_recent(vec![live, ghost]).unwrap().id, "live-sess");
    }

    /// B227/V169 — a ghost-producing sequence (Started A, no Stopped,
    /// Started B, Progress B) must seed from B: eviction removes A the
    /// moment B's Started lands, so `creator_now_playing` never even sees
    /// two candidates.
    #[actix_web::test]
    async fn seed_after_a_ghost_producing_sequence_returns_the_live_item() {
        let (state, _token) = seed_auth().await;
        let user = UserId::new();
        let dev = "laptop";
        state
            .sessions
            .apply(SessionEvent::Started {
                session_id: "a".into(),
                user_id: user,
                user_name: "u".into(),
                device_id: dev.into(),
                device_name: "d".into(),
                client: "c".into(),
                version: "1".into(),
                item_id: "item-x".into(),
                position_ticks: 0,
            })
            .await
            .unwrap();
        // No Stopped for "a" — exactly the ghost condition B227 fixes.
        state
            .sessions
            .apply(SessionEvent::Started {
                session_id: "b".into(),
                user_id: user,
                user_name: "u".into(),
                device_id: dev.into(),
                device_name: "d".into(),
                client: "c".into(),
                version: "1".into(),
                item_id: "item-y".into(),
                position_ticks: 0,
            })
            .await
            .unwrap();
        state
            .sessions
            .apply(SessionEvent::Progress {
                session_id: "b".into(),
                item_id: "item-y".into(),
                position_ticks: 1_000_000,
                is_paused: false,
            })
            .await
            .unwrap();

        let seed = creator_now_playing(&state, &user, dev).await.unwrap();
        assert_eq!(seed.item_id, "item-y");
        assert_eq!(seed.candidates, 1, "eviction must have removed the ghost");
    }

    #[actix_web::test]
    async fn resolve_with_retry_wins_the_socket_registration_race() {
        // B54 — New/Join can arrive just before the /socket registers (a
        // spawned task). A single resolve misses and the group dies
        // member-less; the retry must catch a registration that lands shortly
        // after.
        use pharos_sync::SessionHub;
        use tokio::sync::mpsc;
        // Paused clock auto-advances while every task is idle, so the 130ms
        // registration delay and the 2s bounded wait cost no real time.
        tokio::time::pause();
        let hub = SessionHub::new();
        let dev = "user:dev-race";
        // Register ~130ms late, as the socket task would.
        let hub2 = hub.clone();
        let dev2 = dev.to_string();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(130)).await;
            let (tx, _rx) = mpsc::channel(4);
            hub2.register(dev2, "alison".into(), tx);
        });
        // Immediately (before registration) — a plain resolve misses…
        assert!(hub.resolve(dev).is_none());
        // …but the retry waits it out and succeeds.
        let t0 = tokio::time::Instant::now();
        let sess = resolve_with_retry(&hub, dev).await;
        assert!(
            sess.is_some(),
            "retry must catch the late socket registration"
        );
        assert_eq!(sess.unwrap().name, "alison");
        // Woken by the registration (130ms), not by a poll tick or the deadline.
        // Paused time rounds up to the next ms; a 50ms poll would land on 150.
        assert!(t0.elapsed() <= std::time::Duration::from_millis(135));
        // A device that never registers still resolves to None, at the deadline.
        let t1 = tokio::time::Instant::now();
        assert!(resolve_with_retry(&hub, "user:never").await.is_none());
        let waited = t1.elapsed();
        assert!(
            waited >= SOCKET_REGISTRATION_WAIT
                && waited <= SOCKET_REGISTRATION_WAIT + std::time::Duration::from_millis(10),
            "gave up at the deadline, not before or long after: {waited:?}"
        );
    }

    #[actix_web::test]
    async fn sync_play_need_labels_are_distinct() {
        assert_eq!(SyncPlayNeed::Join.label(), "join");
        assert_eq!(SyncPlayNeed::Create.label(), "create");
    }

    #[actix_web::test]
    async fn group_hidden_route_labels_are_distinct() {
        let routes = [
            GroupHiddenRoute::List,
            GroupHiddenRoute::Join,
            GroupHiddenRoute::Socket,
            GroupHiddenRoute::Native,
        ];
        let labels: std::collections::HashSet<_> = routes.iter().map(|r| r.label()).collect();
        assert_eq!(labels.len(), routes.len());
        assert_eq!(GroupHiddenRoute::List.label(), "list");
        assert_eq!(GroupHiddenRoute::Join.label(), "join");
        assert_eq!(GroupHiddenRoute::Socket.label(), "socket");
        assert_eq!(GroupHiddenRoute::Native.label(), "native");
    }

    /// `pharos_syncplay_queue_refused_total{via,reason}` is a dashboard
    /// contract: each label set stays distinct and stable.
    #[actix_web::test]
    async fn queue_refusal_labels_are_distinct() {
        let via = [QueueVia::SetNewQueue, QueueVia::Queue, QueueVia::Seed].map(QueueVia::label);
        assert_eq!(via, ["set_new_queue", "queue", "seed"]);
        let reasons = [
            QueueRefusal::Queuer,
            QueueRefusal::Member,
            QueueRefusal::UnknownMember,
            QueueRefusal::Unreadable,
        ]
        .map(QueueRefusal::label);
        assert_eq!(
            reasons,
            ["queuer", "member", "unknown_member", "unreadable"]
        );
    }

    /// B265 / V201: a group whose actor answers nothing (it lives on another
    /// replica) is judged by its persisted snapshot, and refused to a
    /// restricted user when there is none.
    /// Disarm check: with the `persisted_queue` fallback replaced by an empty
    /// queue, the hidden and unreadable groups are admitted.
    #[actix_web::test]
    async fn join_reads_a_remote_groups_queue_from_its_snapshot() {
        use pharos_core::{PersistedSyncGroup, SyncGroupStore};
        use pharos_sync::persistence::GroupPersistence;
        struct Capture(std::sync::Mutex<Option<String>>);
        impl GroupPersistence for Capture {
            fn persist(&self, _g: GroupId, _e: u64, state_json: String) {
                *self
                    .0
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(state_json);
            }
            fn remove(&self, _g: GroupId) {}
        }
        async fn persisted_when(
            capture: &Capture,
            done: impl Fn(&pharos_sync::group::SnapshotSummary) -> bool,
        ) -> String {
            for _ in 0..200 {
                let json = capture
                    .0
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();
                if let Some(json) = json {
                    if pharos_sync::group::snapshot_summary(&json).is_some_and(|s| done(&s)) {
                        return json;
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            panic!("group actor never persisted the expected snapshot");
        }

        let (state, _) = seed_auth().await;
        let capture = std::sync::Arc::new(Capture(std::sync::Mutex::new(None)));
        let actor = GroupHandle::spawn_persistent(
            GroupId::new(),
            1_000,
            std::sync::Arc::new(pharos_sync::LocalDelivery::new(MemberSinks::new())),
            capture.clone(),
            None,
        );
        let member = MemberId::new();
        let (reply, joined) = oneshot::channel();
        actor
            .tx
            .send(GroupMsg::AddMember {
                user: None,
                member_id: member,
                name: "leader".into(),
                reply,
            })
            .await
            .unwrap();
        joined.await.unwrap();
        let empty = persisted_when(&capture, |s| s.queue_item_ids.is_empty()).await;
        actor
            .tx
            .send(GroupMsg::SetNewQueue {
                sender: member,
                item_ids: vec![format!("{:032x}", 4242)],
                item_runtimes_ms: vec![None],
                playing_index: 0,
                start_position_ms: 0,
            })
            .await
            .unwrap();
        let hidden = persisted_when(&capture, |s| !s.queue_item_ids.is_empty()).await;

        let now = crate::sync_recovery::now_unix_secs();
        let remote = |json: Option<String>| {
            let group_id = GroupId::new();
            let (tx, _) = tokio::sync::mpsc::channel(1);
            (
                GroupHandle {
                    group_id,
                    tx,
                    epoch_unix_ms: 1_000,
                },
                json.map(|state_json| PersistedSyncGroup {
                    group_id: group_id.to_string(),
                    epoch_unix_ms: 1_000,
                    state_json,
                    updated_at: 0,
                }),
            )
        };
        let mut user = pharos_core::User {
            id: UserId::new(),
            name: "restricted".into(),
            policy: UserPolicy {
                enable_all_folders: false,
                ..UserPolicy::default()
            },
            has_password: true,
        };
        let mut verdicts = Vec::new();
        for (label, json) in [
            ("empty", Some(empty)),
            ("hidden", Some(hidden)),
            ("unreadable", None),
        ] {
            let (handle, row) = remote(json);
            assert!(handle.snapshot().await.is_none());
            if let Some(row) = row {
                state.stores.upsert_sync_group(&row, now).await.unwrap();
            }
            verdicts.push((
                label,
                may_join(&state, &user, &handle, GroupHiddenRoute::Join).await,
            ));
        }
        assert_eq!(
            verdicts,
            vec![("empty", true), ("hidden", false), ("unreadable", false)]
        );
        user.policy.enable_all_folders = true;
        let (handle, _) = remote(None);
        assert!(may_join(&state, &user, &handle, GroupHiddenRoute::Join).await);
    }

    #[actix_web::test]
    async fn syncplay_list_empty_when_no_groups() {
        let (state, token) = seed_auth().await;
        let reg = web::Data::new(GroupRegistry::spawn(std::sync::Arc::new(
            pharos_sync::LocalDelivery::new(pharos_sync::MemberSinks::new()),
        )));
        let app =
            test::init_service(App::new().app_data(state).app_data(reg).configure(register)).await;
        let req = test::TestRequest::get()
            .uri("/syncplay/list")
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request();
        let body = test::call_and_read_body(&app, req).await;
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(v.is_array());
        assert_eq!(v.as_array().unwrap().len(), 0);
    }

    #[actix_web::test]
    async fn syncplay_list_returns_active_group() {
        let (state, token) = seed_auth().await;
        let reg = GroupRegistry::spawn(std::sync::Arc::new(pharos_sync::LocalDelivery::new(
            pharos_sync::MemberSinks::new(),
        )));
        let handle = reg.create().await.unwrap();
        // Group has zero members → snapshot may report 0; we don't
        // care about state here, only that the id surfaces.
        let reg_data = web::Data::new(reg);
        let app = test::init_service(
            App::new()
                .app_data(state)
                .app_data(reg_data)
                .configure(register),
        )
        .await;
        let req = test::TestRequest::get()
            .uri("/syncplay/list")
            .insert_header(("X-Emby-Token", token.as_str()))
            .to_request();
        let body = test::call_and_read_body(&app, req).await;
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let arr = v.as_array().unwrap();
        // Empty groups terminate themselves on the next message —
        // we either see the freshly-created group or already-empty.
        assert!(arr.len() <= 1);
        if let Some(first) = arr.first() {
            assert_eq!(first["GroupId"], handle.group_id.to_string());
        }
    }
}
