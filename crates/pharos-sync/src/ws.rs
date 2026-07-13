//! Extended `/sync/v1/ws` WebSocket handler. Bridges raw WS frames to the
//! group actor's `ClientMsg` / `ServerMsg`. Jellyfin-shaped `/socket` is
//! T16 phase 2; this path is for pharos-native clients (Dioxus, etc.).
//!
//! Auth is delegated to the `TokenResolver` trait — server impls it over
//! its concrete `TokenStore` and registers as actix
//! `web::Data<Arc<dyn TokenResolver>>` so this crate stays free of
//! `pharos-server::AppState`.

use super::delivery::MemberSinks;
use super::group::{GroupMsg, Joined};
use super::host::TokenResolver;
use super::messages::{ClientMsg, ErrorCode, MemberId, ServerMsg};
use super::registry::GroupRegistry;
use actix_web::{web, HttpRequest, HttpResponse};
use actix_ws::{AggregatedMessage, Session};
use futures_util::StreamExt;
use pharos_core::SecretString;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;

/// Actix `web::Data` carrier for the `Arc<dyn TokenResolver>`. Newtype
/// keeps the `register` signature explicit and the type unambiguous when
/// multiple `Arc<dyn _>`s are registered.
pub type TokenResolverData = Arc<dyn TokenResolver>;

pub fn register(cfg: &mut web::ServiceConfig) {
    cfg.route("/sync/v1/ws", web::get().to(ws_entry));
}

async fn ws_entry(
    req: HttpRequest,
    body: web::Payload,
    resolver: web::Data<TokenResolverData>,
    registry: web::Data<GroupRegistry>,
    sinks: web::Data<MemberSinks>,
) -> Result<HttpResponse, actix_web::Error> {
    let (response, session, stream) = actix_ws::handle(&req, body)?;
    let stream = stream
        .aggregate_continuations()
        .max_continuation_size(64 * 1024);
    actix_web::rt::spawn(handle_connection(
        session,
        stream,
        resolver.get_ref().clone(),
        registry.get_ref().clone(),
        sinks.get_ref().clone(),
    ));
    Ok(response)
}

#[derive(Debug, thiserror::Error)]
enum WsError {
    #[error("auth failed")]
    AuthFailed,
    #[error("protocol violation: {0}")]
    Protocol(String),
}

async fn handle_connection<S>(
    mut session: Session,
    mut stream: S,
    resolver: TokenResolverData,
    registry: GroupRegistry,
    sinks: MemberSinks,
) where
    S: futures_util::Stream<Item = Result<AggregatedMessage, actix_ws::ProtocolError>> + Unpin,
{
    let started = Instant::now();
    // 1) wait for Hello, authenticate.
    let (member_name, member_id) =
        match expect_hello(&mut session, &mut stream, resolver.as_ref()).await {
            Ok(v) => v,
            Err(e) => {
                close_with_error(&mut session, ErrorCode::AuthFailed, &e.to_string()).await;
                return;
            }
        };

    // 2) send Welcome.
    let _ = send(
        &mut session,
        ServerMsg::Welcome {
            member_id,
            server_ms: started.elapsed().as_millis() as u64,
        },
    )
    .await;

    // 3) per-connection outbound channel from group actor to this WS sink.
    let (out_tx, mut out_rx) = mpsc::channel::<ServerMsg>(64);

    // 4) wait for Join (or CreateAndJoin).
    let group_handle = match expect_join(&mut session, &mut stream, &registry).await {
        Ok(h) => h,
        Err(e) => {
            close_with_error(&mut session, ErrorCode::UnknownGroup, &e.to_string()).await;
            return;
        }
    };
    // Register this connection's sink into the per-replica delivery table
    // BEFORE AddMember, so the actor's join catch-up reaches this socket.
    sinks.insert(member_id, out_tx.clone());
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    if group_handle
        .tx
        .send(GroupMsg::AddMember {
            member_id,
            name: member_name.clone(),
            reply: reply_tx,
        })
        .await
        .is_err()
    {
        close_with_error(&mut session, ErrorCode::Internal, "group actor dropped").await;
        return;
    }
    let Joined {
        group_id,
        leader,
        members,
    } = match reply_rx.await {
        Ok(j) => j,
        Err(_) => {
            close_with_error(&mut session, ErrorCode::Internal, "join reply dropped").await;
            return;
        }
    };
    let _ = send(
        &mut session,
        ServerMsg::Joined {
            group_id,
            leader,
            members,
        },
    )
    .await;

    // 5) bidirectional pump: WS frames → group; group outbound → WS.
    let group_tx = group_handle.tx.clone();
    // Per-connection pending NTP clock sample (T1,T2,T3) awaiting T4.
    let mut clock_pending: Option<(u64, u64, u64)> = None;
    let mut closed = false;
    while !closed {
        tokio::select! {
            biased;
            Some(out) = out_rx.recv() => {
                if send(&mut session, out).await.is_err() {
                    closed = true;
                }
            }
            frame = stream.next() => {
                match frame {
                    Some(Ok(AggregatedMessage::Text(txt))) => {
                        match sonic_rs::from_str::<ClientMsg>(&txt) {
                            Ok(msg) => dispatch_client_msg(msg, member_id, &group_tx, &mut session, &started, &mut clock_pending).await,
                            Err(e) => {
                                let _ = send(&mut session, ServerMsg::Error {
                                    code: ErrorCode::Internal,
                                    detail: format!("parse: {e}"),
                                }).await;
                            }
                        }
                    }
                    Some(Ok(AggregatedMessage::Close(_))) | None => {
                        closed = true;
                    }
                    Some(Ok(AggregatedMessage::Ping(p))) => {
                        let _ = session.pong(&p).await;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) => {
                        closed = true;
                    }
                }
            }
        }
    }

    // 6) drop membership + deregister the sink.
    let _ = group_tx.send(GroupMsg::RemoveMember { member_id }).await;
    sinks.remove(member_id);
    let _ = session.clone().close(None).await;
}

async fn expect_hello<S>(
    session: &mut Session,
    stream: &mut S,
    resolver: &dyn TokenResolver,
) -> Result<(String, MemberId), WsError>
where
    S: futures_util::Stream<Item = Result<AggregatedMessage, actix_ws::ProtocolError>> + Unpin,
{
    let frame = stream
        .next()
        .await
        .ok_or_else(|| WsError::Protocol("connection closed before Hello".into()))?
        .map_err(|e| WsError::Protocol(format!("stream: {e}")))?;
    let txt = match frame {
        AggregatedMessage::Text(t) => t,
        _ => return Err(WsError::Protocol("expected text Hello".into())),
    };
    let msg: ClientMsg =
        sonic_rs::from_str(&txt).map_err(|e| WsError::Protocol(format!("hello parse: {e}")))?;
    let ClientMsg::Hello {
        token,
        client: _client,
        device_id: _device,
        name,
    } = msg
    else {
        return Err(WsError::Protocol("first message must be Hello".into()));
    };
    // V8: wrap immediately and drop the wire-side String.
    let token = SecretString::new(token);
    let user_id = resolver.resolve(&token).await.ok_or(WsError::AuthFailed)?;
    // Use the user's UUID as the member id base — but generate a new one
    // per connection so a user with multiple devices doesn't conflict.
    let _ = user_id;
    let _ = session;
    Ok((name, MemberId::new()))
}

async fn expect_join<S>(
    session: &mut Session,
    stream: &mut S,
    registry: &GroupRegistry,
) -> Result<super::group::GroupHandle, WsError>
where
    S: futures_util::Stream<Item = Result<AggregatedMessage, actix_ws::ProtocolError>> + Unpin,
{
    let _ = session;
    let frame = stream
        .next()
        .await
        .ok_or_else(|| WsError::Protocol("connection closed before Join".into()))?
        .map_err(|e| WsError::Protocol(format!("stream: {e}")))?;
    let txt = match frame {
        AggregatedMessage::Text(t) => t,
        _ => return Err(WsError::Protocol("expected text Join".into())),
    };
    let msg: ClientMsg =
        sonic_rs::from_str(&txt).map_err(|e| WsError::Protocol(format!("join parse: {e}")))?;
    match msg {
        ClientMsg::Join { group_id } => registry
            .get_or_create(group_id)
            .await
            .map_err(|_| WsError::Protocol("registry actor down".into())),
        ClientMsg::CreateAndJoin => registry
            .create()
            .await
            .map_err(|_| WsError::Protocol("registry actor down".into())),
        _ => Err(WsError::Protocol("expected Join or CreateAndJoin".into())),
    }
}

async fn dispatch_client_msg(
    msg: ClientMsg,
    member_id: MemberId,
    group_tx: &mpsc::Sender<GroupMsg>,
    session: &mut Session,
    started: &Instant,
    // Pending NTP sample (T1, T2, T3) awaiting the client's T4 via
    // `ClockReport`. One outstanding ping at a time per connection.
    clock_pending: &mut Option<(u64, u64, u64)>,
) {
    let server_ms_recv = started.elapsed().as_millis() as u64;
    match msg {
        ClientMsg::Hello { .. } | ClientMsg::Join { .. } | ClientMsg::CreateAndJoin => {
            let _ = send(
                session,
                ServerMsg::Error {
                    code: ErrorCode::Internal,
                    detail: "Hello/Join only valid at connection start".into(),
                },
            )
            .await;
        }
        ClientMsg::Ping { client_ms } => {
            let server_ms_send = started.elapsed().as_millis() as u64;
            // Stash (T1, T2, T3); the real ObserveClock fires when the
            // client reports T4 in a ClockReport.
            *clock_pending = Some((client_ms, server_ms_recv, server_ms_send));
            let _ = send(
                session,
                ServerMsg::Pong {
                    client_ms_echo: client_ms,
                    server_ms: server_ms_send,
                },
            )
            .await;
        }
        ClientMsg::ClockReport {
            client_ms,
            client_recv_ms,
        } => {
            // Match the report to the outstanding ping (by T1) and feed a
            // complete NTP sample so RTT/offset are real.
            if let Some((t1, t2, t3, t4)) =
                correlate_clock(*clock_pending, client_ms, client_recv_ms)
            {
                *clock_pending = None;
                let _ = group_tx
                    .send(GroupMsg::ObserveClock {
                        member_id,
                        t1,
                        t2,
                        t3,
                        t4,
                    })
                    .await;
            }
        }
        ClientMsg::LeaderPlay { position_ms } => {
            let _ = group_tx
                .send(GroupMsg::LeaderPlay {
                    sender: member_id,
                    position_ms,
                })
                .await;
        }
        ClientMsg::LeaderPause => {
            let _ = group_tx
                .send(GroupMsg::LeaderPause { sender: member_id })
                .await;
        }
        ClientMsg::LeaderSeek { position_ms } => {
            let _ = group_tx
                .send(GroupMsg::LeaderSeek {
                    sender: member_id,
                    position_ms,
                })
                .await;
        }
        ClientMsg::BufferingStart { position_ms } => {
            let _ = group_tx
                .send(GroupMsg::BufferingStart {
                    member_id,
                    position_ms,
                    // ws-native framing has no queue concept yet — never stale.
                    playlist_item_id: None,
                })
                .await;
        }
        ClientMsg::BufferingEnd { position_ms: _ } => {
            let _ = group_tx.send(GroupMsg::BufferingEnd { member_id }).await;
        }
        ClientMsg::Leave => {
            let _ = group_tx.send(GroupMsg::RemoveMember { member_id }).await;
        }
        ClientMsg::Heartbeat => { /* no-op */ }
    }
}

/// Correlate a `ClockReport` (echo of T1 + the client's T4) against the
/// outstanding ping sample `(T1,T2,T3)`. Returns the complete NTP sample
/// only when the echoed T1 matches the pending ping — guards against a
/// stale/duplicate report polluting the offset estimator.
fn correlate_clock(
    pending: Option<(u64, u64, u64)>,
    echo_t1: u64,
    t4: u64,
) -> Option<(u64, u64, u64, u64)> {
    match pending {
        Some((t1, t2, t3)) if t1 == echo_t1 => Some((t1, t2, t3, t4)),
        _ => None,
    }
}

async fn send(session: &mut Session, msg: ServerMsg) -> Result<(), actix_ws::Closed> {
    let txt = match sonic_rs::to_string(&msg) {
        Ok(s) => s,
        Err(_) => return Err(actix_ws::Closed),
    };
    session.text(txt).await
}

async fn close_with_error(session: &mut Session, code: ErrorCode, detail: &str) {
    let _ = send(
        session,
        ServerMsg::Error {
            code,
            detail: detail.into(),
        },
    )
    .await;
    let _ = session.clone().close(None).await;
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::correlate_clock;
    use crate::clock::ClockOffset;

    #[test]
    fn correlate_matches_pending_ping() {
        // T1=100 (client send), T2=150 (server recv), T3=160 (server
        // send), T4=300 (client recv) → rtt = (300-100)-(160-150) = 190.
        let s = correlate_clock(Some((100, 150, 160)), 100, 300).unwrap();
        assert_eq!(s, (100, 150, 160, 300));
        let mut c = ClockOffset::default();
        c.observe(s.0, s.1, s.2, s.3);
        assert_eq!(
            c.max_rtt_ms(),
            190,
            "RTT must be the real round-trip, not 0"
        );
    }

    #[test]
    fn correlate_rejects_mismatched_or_absent() {
        // Stale report (echoed T1 doesn't match the pending ping).
        assert!(correlate_clock(Some((100, 150, 160)), 999, 300).is_none());
        // No outstanding ping.
        assert!(correlate_clock(None, 100, 300).is_none());
    }
}
