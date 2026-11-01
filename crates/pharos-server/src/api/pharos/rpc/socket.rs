//! `GET /pharos/rpc`: the WebSocket upgrade and the byte pumps between the
//! socket and the connection's `RpcSystem` (research R3).
//!
//! capnp frames its own messages, so WebSocket message boundaries carry no
//! meaning: inbound binary messages are written into one end of a bounded
//! in-memory pipe, the `RpcSystem` reads and writes the other end, and its
//! output is sent back as binary messages. The bounded pipe is the
//! backpressure.

use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use actix_web::{web, HttpRequest, HttpResponse};
use actix_ws::{AggregatedMessage, CloseCode, CloseReason, Session};
use futures_util::StreamExt as _;
use pharos_api::labels::LIMIT_TOTAL;
use pharos_api::Connection;
use pharos_core::UserId;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::sync::broadcast;

use super::limits::{ConnectionLimits, AUTH_DEADLINE, MAX_UNAUTHENTICATED};
use super::{bootstrap_client, origin, ConnectionCtx};
use crate::config::NativeApiConfig;
use crate::services::auth::{user_for_token, TokenRefusal};
use crate::state::AppState;

/// The cookie `POST /pharos/session` sets (research R6).
pub const SESSION_COOKIE: &str = "pharos_session";
/// Largest inbound WebSocket message; capnp's own decode limit
/// ([`super::TRAVERSAL_LIMIT_WORDS`]) bounds a whole RPC message.
const MAX_FRAME: usize = 1 << 20;
/// In-memory pipe between the socket and the `RpcSystem`.
const PIPE_BYTES: usize = 64 * 1024;
const KEEPALIVE: Duration = Duration::from_secs(30);
const IDLE_DROP: Duration = Duration::from_secs(120);
/// Longest one token re-check may take before it counts as an outage.
const RECHECK_TIMEOUT: Duration = Duration::from_secs(5);
/// How soon an announced re-check that could not reach the store tries
/// again. An announcement is consumed once, so without this a stalled lookup
/// would leave a revoked session open until the next `revalidate_secs` tick.
const UNSURE_RETRY: Duration = Duration::from_secs(5);

/// Why an upgrade was refused before it became a connection. The label is
/// the `reason` of `pharos_rpc_refused_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    Origin(origin::OriginRefusal),
    /// [`super::limits::MAX_UNAUTHENTICATED`] connections are already open.
    TooManyUnauthenticated,
}

impl Refused {
    pub fn label(self) -> &'static str {
        match self {
            Refused::Origin(o) => o.label(),
            Refused::TooManyUnauthenticated => "too_many_unauthenticated",
        }
    }
}

/// Counts and logs a refused upgrade, naming the offending value. The
/// connection cap is also a limit (FR-010).
pub fn record_refused(refused: Refused, origin: Option<&str>, peer: &str) {
    metrics::counter!("pharos_rpc_refused_total", "reason" => refused.label()).increment(1);
    if refused == Refused::TooManyUnauthenticated {
        metrics::counter!(LIMIT_TOTAL, "limit" => "unauthenticated_connections").increment(1);
        tracing::warn!(
            limit = "unauthenticated_connections",
            value = MAX_UNAUTHENTICATED,
            max = MAX_UNAUTHENTICATED,
            peer,
            "native api limit reached"
        );
    }
    tracing::warn!(
        reason = refused.label(),
        origin = origin.unwrap_or(""),
        peer,
        "native api upgrade refused"
    );
}

/// How a connection ended. The label is the `cause` of
/// `pharos_rpc_closed_total`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloseCause {
    /// The client sent a close frame.
    PeerClose(Option<CloseCode>),
    /// The socket ended without a close frame.
    StreamEnd,
    ReadError(String),
    /// An inbound message over [`MAX_FRAME`] bytes (FR-010).
    MessageTooLarge,
    SendFailed,
    /// A text frame: this endpoint speaks binary capnp only.
    TextFrame,
    /// No successful `resume` within [`AUTH_DEADLINE`].
    AuthDeadline,
    /// Nothing heard from the client for [`IDLE_DROP`].
    IdleTimeout,
    /// A token this connection resumed with was revoked, or its user was
    /// disabled or deleted (T025a, T025).
    Revoked(Recheck),
    /// The `RpcSystem` stopped (it hit a protocol or decode error).
    RpcEnded(String),
}

impl CloseCause {
    pub fn label(&self) -> &'static str {
        match self {
            CloseCause::PeerClose(_) => "peer_close",
            CloseCause::StreamEnd => "stream_end",
            CloseCause::ReadError(_) => "read_error",
            CloseCause::MessageTooLarge => "message_too_large",
            CloseCause::SendFailed => "send_failed",
            CloseCause::TextFrame => "text_frame",
            CloseCause::AuthDeadline => "auth_deadline",
            CloseCause::IdleTimeout => "idle_timeout",
            CloseCause::Revoked(_) => "revoked",
            CloseCause::RpcEnded(_) => "rpc_ended",
        }
    }

    fn detail(&self) -> String {
        match self {
            CloseCause::PeerClose(code) => format!("{code:?}"),
            CloseCause::ReadError(e) | CloseCause::RpcEnded(e) => e.clone(),
            CloseCause::Revoked(trigger) => trigger.label().to_owned(),
            _ => String::new(),
        }
    }

    /// The close frame sent to the client, when the server is the one closing.
    fn reason(&self) -> Option<CloseReason> {
        let code = match self {
            CloseCause::TextFrame => CloseCode::Unsupported,
            CloseCause::MessageTooLarge => CloseCode::Size,
            CloseCause::AuthDeadline => CloseCode::Policy,
            CloseCause::IdleTimeout => CloseCode::Away,
            CloseCause::Revoked(_) => CloseCode::Policy,
            CloseCause::RpcEnded(_) => CloseCode::Protocol,
            _ => return None,
        };
        Some(CloseReason {
            code,
            description: Some(self.label().to_owned()),
        })
    }
}

/// What made a connection re-check its tokens; the `detail` of a `revoked`
/// close, so a query tells a revocation announced at once (T025) from one the
/// timer caught (T025a).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recheck {
    /// The `revalidate_secs` timer.
    Timer,
    /// A revocation announced for one of the connection's users.
    Announced,
    /// The announcement channel overflowed, so the connection re-checked in
    /// case it missed one.
    Lagged,
}

impl Recheck {
    pub fn label(self) -> &'static str {
        match self {
            Recheck::Timer => "timer",
            Recheck::Announced => "announced",
            Recheck::Lagged => "lagged",
        }
    }
}

/// Counts and logs a closed connection with its cause and lifetime.
pub fn record_closed(cause: &CloseCause, connection: u64, signed_in: bool, lived: Duration) {
    metrics::counter!("pharos_rpc_closed_total", "cause" => cause.label()).increment(1);
    if *cause == CloseCause::MessageTooLarge {
        metrics::counter!(LIMIT_TOTAL, "limit" => "message_size").increment(1);
        tracing::warn!(
            connection,
            limit = "message_size",
            max = MAX_FRAME,
            "native api limit reached"
        );
    }
    tracing::info!(
        connection,
        signed_in,
        cause = cause.label(),
        detail = %cause.detail(),
        lived_ms = lived.as_millis() as u64,
        "native api connection closed"
    );
}

/// A frame over `max_frame_size` is `Overflow`; continuations adding up past
/// `max_continuation_size` arrive as an `Io` error carrying only text.
fn is_oversized(e: &actix_ws::ProtocolError) -> bool {
    match e {
        actix_ws::ProtocolError::Overflow => true,
        actix_ws::ProtocolError::Io(io) => io.to_string().contains("maximum continuation size"),
        _ => false,
    }
}

/// Whether every token a connection resumed with still resolves (T025a). A
/// store outage keeps the connection (B216: ours and transient); only a
/// definite refusal ends it.
///
/// Each lookup is bounded by [`RECHECK_TIMEOUT`] and a timeout counts as an
/// outage: the re-check runs in the same select as the keepalive and idle
/// drop, so a stalled store must not hold those off.
async fn still_valid(state: &AppState, tokens: &[String]) -> Verdict {
    let mut verdict = Verdict::Valid;
    for token in tokens {
        let lookup = tokio::time::timeout(RECHECK_TIMEOUT, user_for_token(&state.stores, token));
        let Ok(result) = lookup.await else {
            tracing::warn!("native api: token re-check timed out; keeping the connection");
            verdict = Verdict::Unsure;
            continue;
        };
        match result {
            Ok(_) => {}
            Err(TokenRefusal::Unavailable(_)) => verdict = Verdict::Unsure,
            Err(refusal) => {
                tracing::info!(
                    reason = refusal.label(),
                    "native api: a resumed token no longer resolves"
                );
                return Verdict::Revoked;
            }
        }
    }
    verdict
}

/// What one re-check of a connection's tokens found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Valid,
    Revoked,
    /// A lookup timed out or the store was down; nothing was refused.
    Unsure,
}

/// Keeps `pharos_rpc_connections{transport}` true however the task ends.
struct OpenGauge;

impl OpenGauge {
    fn open() -> Self {
        metrics::gauge!("pharos_rpc_connections", "transport" => "websocket").increment(1.0);
        OpenGauge
    }
}

impl Drop for OpenGauge {
    fn drop(&mut self) {
        metrics::gauge!("pharos_rpc_connections", "transport" => "websocket").decrement(1.0);
    }
}

pub(super) async fn upgrade(
    req: HttpRequest,
    body: web::Payload,
    state: web::Data<AppState>,
    cfg: web::Data<NativeApiConfig>,
    limits: web::Data<ConnectionLimits>,
) -> Result<HttpResponse, actix_web::Error> {
    let origin_header = req
        .headers()
        .get(actix_web::http::header::ORIGIN)
        .map(|v| v.to_str().unwrap_or("<non-ascii>"));
    let peer = req
        .peer_addr()
        .map(|a| a.ip().to_string())
        .unwrap_or_default();
    if let Err(refusal) = origin::check(origin_header, &cfg.allowed_origins) {
        record_refused(Refused::Origin(refusal), origin_header, &peer);
        return Ok(HttpResponse::Forbidden().body(refusal.label()));
    }
    let cookie = req.cookie(SESSION_COOKIE).map(|c| c.value().to_owned());
    let client_ip = crate::client_ip::client_ip(&req, &state.trusted_proxies);
    let Some(ctx) = ConnectionCtx::admit(
        cookie,
        client_ip,
        &limits,
        cfg.max_connections_per_user.get(),
    ) else {
        record_refused(Refused::TooManyUnauthenticated, origin_header, &peer);
        return Ok(HttpResponse::TooManyRequests()
            .insert_header(("Retry-After", "5"))
            .body(Refused::TooManyUnauthenticated.label()));
    };
    let (response, session, stream) = actix_ws::handle(&req, body)?;
    let stream = stream
        .max_frame_size(MAX_FRAME)
        .aggregate_continuations()
        .max_continuation_size(MAX_FRAME);
    // R2: the RpcSystem is !Send, so it stays on this worker.
    let revalidate = Duration::from_secs(u64::from(cfg.revalidate_secs.get()));
    actix_web::rt::spawn(run(state, ctx, session, stream, peer, revalidate));
    Ok(response)
}

async fn run<S>(
    state: web::Data<AppState>,
    ctx: Rc<ConnectionCtx>,
    session: Session,
    mut inbound: S,
    peer: String,
    revalidate: Duration,
) where
    S: futures_util::Stream<Item = Result<AggregatedMessage, actix_ws::ProtocolError>> + Unpin,
{
    let started = Instant::now();
    let _gauge = OpenGauge::open();
    let conn = Rc::new(Connection::new());
    tracing::info!(connection = conn.id(), peer = %peer, "native api connection opened");

    let (rpc_end, socket_end) = tokio::io::duplex(PIPE_BYTES);
    let (rpc_read, rpc_write) = tokio::io::split(rpc_end);
    let (mut from_rpc, mut to_rpc) = tokio::io::split(socket_end);
    let rpc = super::serve(
        rpc_read,
        rpc_write,
        bootstrap_client(state.clone(), Rc::clone(&ctx), &conn),
    );

    let mut sender = session.clone();
    let outbound = async move {
        let mut buf = vec![0u8; PIPE_BYTES];
        loop {
            match from_rpc.read(&mut buf).await {
                Ok(0) | Err(_) => return CloseCause::RpcEnded("rpc output ended".into()),
                Ok(n) => {
                    if sender.binary(buf[..n].to_vec()).await.is_err() {
                        return CloseCause::SendFailed;
                    }
                }
            }
        }
    };

    // Inbound frames feed the RPC pipe, whose `write_all` waits whenever the
    // RpcSystem is behind. The timers run in their own future so a stalled
    // pipe can never hold off the auth deadline or the idle drop.
    let last_seen = Rc::new(Cell::new(Instant::now()));
    let mut ponger = session.clone();
    let seen = Rc::clone(&last_seen);
    let inbound = async move {
        loop {
            let msg = inbound.next().await;
            seen.set(Instant::now());
            match msg {
                None => return CloseCause::StreamEnd,
                Some(Err(e)) if is_oversized(&e) => return CloseCause::MessageTooLarge,
                Some(Err(e)) => return CloseCause::ReadError(e.to_string()),
                Some(Ok(AggregatedMessage::Binary(bytes))) => {
                    if to_rpc.write_all(&bytes).await.is_err() {
                        return CloseCause::RpcEnded("rpc input closed".into());
                    }
                }
                Some(Ok(AggregatedMessage::Ping(p))) => {
                    if ponger.pong(&p).await.is_err() {
                        return CloseCause::SendFailed;
                    }
                }
                Some(Ok(AggregatedMessage::Pong(_))) => {}
                Some(Ok(AggregatedMessage::Text(_))) => return CloseCause::TextFrame,
                Some(Ok(AggregatedMessage::Close(reason))) => {
                    return CloseCause::PeerClose(reason.map(|r| r.code));
                }
            }
        }
    };

    let mut pinger = session.clone();
    let deadline = ctx.clone();
    // Subscribed before any resume, so no announcement can slip between a
    // resume and the first poll (T025).
    let mut announced = state.revocations.subscribe();
    let timers = async move {
        let mut keepalive = tokio::time::interval(KEEPALIVE);
        keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let auth_deadline = tokio::time::sleep(AUTH_DEADLINE);
        tokio::pin!(auth_deadline);
        let mut checked_auth = false;
        let mut recheck = tokio::time::interval(revalidate);
        recheck.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        recheck.reset();
        let mut announcing = true;
        // An announced re-check the store could not answer, retried on the
        // timer after UNSURE_RETRY under its own trigger.
        let mut retrying: Option<(Recheck, Option<UserId>)> = None;
        loop {
            let trigger = tokio::select! {
                _ = recheck.tick() => Some(retrying.take().unwrap_or((Recheck::Timer, None))),
                event = announced.recv(), if announcing => match event {
                    Ok(user) if deadline.serves(user) => Some((Recheck::Announced, Some(user))),
                    Ok(_) => None,
                    Err(broadcast::error::RecvError::Lagged(_)) => Some((Recheck::Lagged, None)),
                    Err(broadcast::error::RecvError::Closed) => {
                        announcing = false;
                        None
                    }
                },
                () = deadline.signed_out.notified() => {
                    // A stale wake (signed in again since) changes nothing.
                    if !deadline.is_signed_in() {
                        auth_deadline
                            .as_mut()
                            .reset(tokio::time::Instant::now() + AUTH_DEADLINE);
                        checked_auth = false;
                    }
                    None
                }
                () = &mut auth_deadline, if !checked_auth => {
                    if !deadline.is_signed_in() {
                        return CloseCause::AuthDeadline;
                    }
                    checked_auth = true;
                    None
                }
                _ = keepalive.tick() => {
                    if last_seen.get().elapsed() > IDLE_DROP {
                        return CloseCause::IdleTimeout;
                    }
                    if pinger.ping(b"").await.is_err() {
                        return CloseCause::SendFailed;
                    }
                    None
                }
            };
            if let Some((trigger, user)) = trigger {
                match still_valid(&state, &deadline.resumed_tokens(user)).await {
                    Verdict::Revoked => return CloseCause::Revoked(trigger),
                    Verdict::Unsure if trigger != Recheck::Timer => {
                        // A pending retry for another user widens to all
                        // tokens rather than being overwritten.
                        retrying = Some(match retrying {
                            Some((_, pending)) if pending != user => (trigger, None),
                            _ => (trigger, user),
                        });
                        recheck.reset_after(UNSURE_RETRY);
                    }
                    Verdict::Valid | Verdict::Unsure => {}
                }
            }
        }
    };

    let cause = tokio::select! {
        cause = inbound => cause,
        cause = timers => cause,
        cause = outbound => cause,
        result = rpc => CloseCause::RpcEnded(match result {
            Ok(()) => "rpc system finished".into(),
            Err(e) => e.to_string(),
        }),
    };
    let _ = session.close(cause.reason()).await;
    record_closed(&cause, conn.id(), ctx.is_signed_in(), started.elapsed());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_cause_and_refusal_labels_are_distinct() {
        let causes = [
            CloseCause::PeerClose(None),
            CloseCause::StreamEnd,
            CloseCause::ReadError(String::new()),
            CloseCause::MessageTooLarge,
            CloseCause::SendFailed,
            CloseCause::TextFrame,
            CloseCause::AuthDeadline,
            CloseCause::IdleTimeout,
            CloseCause::Revoked(Recheck::Timer),
            CloseCause::RpcEnded(String::new()),
        ];
        let labels: std::collections::HashSet<_> = causes.iter().map(CloseCause::label).collect();
        assert_eq!(labels.len(), causes.len());

        let triggers = [Recheck::Timer, Recheck::Announced, Recheck::Lagged];
        let labels: std::collections::HashSet<_> = triggers.iter().map(|t| t.label()).collect();
        assert_eq!(labels.len(), triggers.len());

        let refused = [
            Refused::Origin(origin::OriginRefusal::NotAllowed),
            Refused::Origin(origin::OriginRefusal::Malformed),
            Refused::TooManyUnauthenticated,
        ];
        let labels: std::collections::HashSet<_> = refused.iter().map(|r| r.label()).collect();
        assert_eq!(labels.len(), refused.len());
    }
}
