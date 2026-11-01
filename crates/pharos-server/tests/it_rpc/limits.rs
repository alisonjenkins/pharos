//! T026 (FR-010): limits enforced at the socket. The call-concurrency cap
//! lives in the generated dispatch and is tested in `pharos-api`'s
//! `it_instrument/limits.rs`.

use awc::ws;
use futures_util::{SinkExt as _, StreamExt as _};

use crate::connect::{start, ws_url};

/// Sends `messages` and answers the close frame the server sends back.
async fn close_after(messages: Vec<ws::Message>) -> ws::CloseReason {
    let server = start(Vec::new()).await;
    let (_resp, mut framed) = awc::Client::new()
        .ws(ws_url(&server))
        .connect()
        .await
        .expect("upgrade");
    for m in messages {
        if framed.send(m).await.is_err() {
            break;
        }
    }
    let mut closed = None;
    while let Some(Ok(frame)) = framed.next().await {
        if let ws::Frame::Close(reason) = frame {
            closed = Some(reason);
            break;
        }
    }
    closed.expect("server closes").expect("with a reason")
}

/// A message over the frame cap closes the socket with `Size`, not a bare
/// read error.
#[actix_web::test]
async fn an_oversized_message_closes_with_size() {
    let reason = close_after(vec![ws::Message::Binary(vec![0u8; (1 << 20) + 1].into())]).await;
    assert_eq!(reason.code, ws::CloseCode::Size);
    assert_eq!(reason.description.as_deref(), Some("message_too_large"));
}

/// Continuation frames each under the cap but adding up past it hit the same
/// limit. actix-ws reports this one only as error text, so this test is what
/// notices a change in that text.
#[actix_web::test]
async fn continuations_adding_up_past_the_cap_close_with_size() {
    use actix_ws::Item;
    let half = || vec![0u8; (1 << 19) + 1].into();
    let reason = close_after(vec![
        ws::Message::Continuation(Item::FirstBinary(half())),
        ws::Message::Continuation(Item::Continue(half())),
        ws::Message::Continuation(Item::Last(vec![0u8; 1].into())),
    ])
    .await;
    assert_eq!(reason.code, ws::CloseCode::Size);
    assert_eq!(reason.description.as_deref(), Some("message_too_large"));
}
