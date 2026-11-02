//! T047/T049: `pharos-client` against a real server. The crate's byte
//! stream over WebSocket binary messages carries capnp, and `resume` with
//! an empty token signs in from the cookie the upgrade carried, as the
//! browser transport does.

use awc::ws;
use futures_util::{SinkExt as _, StreamExt as _};
use pharos_api::pharos_capnp::{bootstrap, me_result};
use pharos_client::byte_stream::{ChunkReader, ChunkWriter};
use pharos_client::{CallInfo, ClientError};
use pharos_server::api::pharos::rpc;
use std::io;

use crate::revocation::start;

/// The browser transport's shape, with awc in place of `web_sys::WebSocket`.
async fn connect(server: &actix_test::TestServer, cookie: &str) -> bootstrap::Client {
    let url = server.url("/pharos/rpc").replacen("http", "ws", 1);
    let (_resp, framed) = awc::Client::new()
        .ws(url)
        .cookie(actix_web::cookie::Cookie::new(rpc::SESSION_COOKIE, cookie))
        .connect()
        .await
        .expect("upgrade");
    let (mut sink, stream) = framed.split();
    // A close frame or a broken socket ends the byte stream, as the
    // browser transport's onclose does.
    let chunks = Box::pin(
        stream
            .take_while(|frame| {
                std::future::ready(!matches!(frame, Ok(ws::Frame::Close(_)) | Err(_)))
            })
            .filter_map(|frame| async move {
                match frame {
                    Ok(ws::Frame::Binary(bytes)) => Some(bytes.to_vec()),
                    _ => None,
                }
            }),
    );
    let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    actix_web::rt::spawn(async move {
        while let Some(bytes) = out_rx.recv().await {
            if sink.send(ws::Message::Binary(bytes.into())).await.is_err() {
                break;
            }
        }
    });
    let writer = ChunkWriter::new(move |bytes: &[u8]| {
        out_tx
            .send(bytes.to_vec())
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "socket closed"))
    });
    let (client, system) = pharos_client::connect_stream(ChunkReader::new(chunks), writer);
    actix_web::rt::spawn(async move {
        let _ = system.await;
    });
    client
}

#[actix_web::test]
async fn resume_signs_in_from_the_upgrade_cookie() {
    let (server, fx) = start().await;
    let client = connect(&server, &fx.admin_token).await;
    let session = pharos_client::resume(&client, &CallInfo::default())
        .await
        .expect("resume from the cookie");
    let mut me = session.me_request();
    me.get().init_ctx();
    let reply = me.send().promise.await.unwrap();
    let me_result::Me(me) = reply.get().unwrap().get_result().unwrap().which().unwrap() else {
        panic!("me failed after resume");
    };
    assert_eq!(
        me.unwrap().get_id().unwrap().get_uuid().unwrap(),
        fx.admin_id.0.as_bytes()
    );
}

/// A stale cookie surfaces as an API error the UI routes to sign-in, not a
/// transport failure.
#[actix_web::test]
async fn a_stale_cookie_asks_for_sign_in() {
    let (server, _fx) = start().await;
    let client = connect(&server, "not-a-token").await;
    let err = pharos_client::resume(&client, &CallInfo::default())
        .await
        .err()
        .expect("a stale cookie must not resume");
    assert!(
        matches!(
            err,
            ClientError::Api {
                kind: "revoked",
                ..
            }
        ),
        "got {err:?}"
    );
    assert!(err.needs_sign_in());
}
