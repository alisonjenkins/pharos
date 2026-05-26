//! Extended `/sync/v1/ws` WebSocket handler. Bridges raw WS frames to the
//! group actor's `ClientMsg` / `ServerMsg`. Jellyfin-shaped `/socket` is
//! T16 phase 2; this path is for pharos-native clients (Dioxus, etc.).

use super::group::{GroupMsg, Joined};
use super::messages::{ClientMsg, ErrorCode, MemberId, ServerMsg};
use super::registry::GroupRegistry;
use crate::state::AppState;
use actix_web::{web, HttpRequest, HttpResponse};
use actix_ws::{AggregatedMessage, Session};
use futures_util::StreamExt;
use pharos_core::{SecretString, TokenStore};
use std::time::Instant;
use tokio::sync::mpsc;

pub fn register(cfg: &mut web::ServiceConfig) {
    cfg.route("/sync/v1/ws", web::get().to(ws_entry));
}

async fn ws_entry(
    req: HttpRequest,
    body: web::Payload,
    state: web::Data<AppState>,
    registry: web::Data<GroupRegistry>,
) -> Result<HttpResponse, actix_web::Error> {
    let (response, session, stream) = actix_ws::handle(&req, body)?;
    let stream = stream.aggregate_continuations().max_continuation_size(64 * 1024);
    actix_web::rt::spawn(handle_connection(
        session,
        stream,
        state.clone(),
        registry.get_ref().clone(),
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
    state: web::Data<AppState>,
    registry: GroupRegistry,
) where
    S: futures_util::Stream<Item = Result<AggregatedMessage, actix_ws::ProtocolError>> + Unpin,
{
    let started = Instant::now();
    // 1) wait for Hello, authenticate.
    let (member_name, member_id) = match expect_hello(&mut session, &mut stream, &state).await {
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
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    if group_handle
        .tx
        .send(GroupMsg::AddMember {
            member_id,
            name: member_name.clone(),
            sink: out_tx.clone(),
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
                            Ok(msg) => dispatch_client_msg(msg, member_id, &group_tx, &mut session, &started).await,
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

    // 6) drop membership.
    let _ = group_tx
        .send(GroupMsg::RemoveMember { member_id })
        .await;
    let _ = session.clone().close(None).await;
}

async fn expect_hello<S>(
    session: &mut Session,
    stream: &mut S,
    state: &AppState,
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
    let msg: ClientMsg = sonic_rs::from_str(&txt)
        .map_err(|e| WsError::Protocol(format!("hello parse: {e}")))?;
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
    let user_id = state
        .stores
        .resolve(token.expose())
        .await
        .map_err(|_| WsError::AuthFailed)?;
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
    let msg: ClientMsg = sonic_rs::from_str(&txt)
        .map_err(|e| WsError::Protocol(format!("join parse: {e}")))?;
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
            let _ = send(
                session,
                ServerMsg::Pong {
                    client_ms_echo: client_ms,
                    server_ms: server_ms_send,
                },
            )
            .await;
            // T1 we don't actually know; observe with what we have.
            let _ = group_tx
                .send(GroupMsg::ObserveClock {
                    member_id,
                    t1: client_ms,
                    t2: server_ms_recv,
                    t3: server_ms_send,
                    t4: client_ms,
                })
                .await;
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
                })
                .await;
        }
        ClientMsg::BufferingEnd { position_ms: _ } => {
            let _ = group_tx
                .send(GroupMsg::BufferingEnd { member_id })
                .await;
        }
        ClientMsg::Leave => {
            let _ = group_tx
                .send(GroupMsg::RemoveMember { member_id })
                .await;
        }
        ClientMsg::Heartbeat => { /* no-op */ }
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
