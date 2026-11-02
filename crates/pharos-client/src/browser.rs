//! The browser transport (017 T049): capnp over a WebSocket to `/pharos/rpc`.
//!
//! The browser sends the session cookie with the upgrade itself, so the
//! socket opens unauthenticated and [`crate::resume`] signs it in. No token
//! passes through script.

use crate::byte_stream::{ChunkReader, ChunkWriter};
use futures::channel::{mpsc, oneshot};
use pharos_api::pharos_capnp::bootstrap;
use std::cell::RefCell;
use std::io;
use std::rc::Rc;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast as _;
use web_sys::{BinaryType, CloseEvent, Event, MessageEvent, WebSocket};

/// Why the socket closed: the close frame's code and reason, so the UI can
/// tell a refused origin or a server restart from a network drop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Closed {
    pub code: u16,
    pub reason: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    /// The browser refused to create the socket (a malformed URL, or a
    /// policy such as CSP `connect-src`).
    #[error("websocket refused by the browser: {0}")]
    Create(String),
    /// The socket closed before it opened: the server refused the upgrade
    /// (native API off, origin not allowed) or the network failed.
    #[error("websocket closed before opening (code {}: {})", .0.code, .0.reason)]
    NotOpened(Closed),
}

/// An open connection. Dropping it closes the socket.
pub struct Connection {
    pub bootstrap: bootstrap::Client,
    closed: Option<oneshot::Receiver<Closed>>,
    socket: WebSocket,
    _handlers: Handlers,
}

struct Handlers {
    _message: Closure<dyn FnMut(MessageEvent)>,
    _close: Closure<dyn FnMut(CloseEvent)>,
    _open: Closure<dyn FnMut(Event)>,
}

impl Connection {
    /// Resolves once the socket closes, with its close frame. Resolves at
    /// most once; later calls resolve immediately with code 1006.
    pub async fn closed(&mut self) -> Closed {
        let abnormal = || Closed {
            code: 1006,
            reason: String::new(),
        };
        match self.closed.take() {
            Some(rx) => rx.await.unwrap_or_else(|_| abnormal()),
            None => abnormal(),
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        let _ = self.socket.close();
    }
}

/// Open `url` (`ws[s]://<host>/pharos/rpc`) and start capnp on it.
pub async fn connect(url: &str) -> Result<Connection, ConnectError> {
    let socket = WebSocket::new(url).map_err(|e| ConnectError::Create(format!("{e:?}")))?;
    socket.set_binary_type(BinaryType::Arraybuffer);

    let (chunks_tx, chunks_rx) = mpsc::unbounded::<Vec<u8>>();
    let (opened_tx, opened_rx) = oneshot::channel::<()>();
    let (closed_tx, closed_rx) = oneshot::channel::<Closed>();
    let opened_tx = Rc::new(RefCell::new(Some(opened_tx)));
    let closed_tx = Rc::new(RefCell::new(Some(closed_tx)));

    let message = {
        let chunks_tx = chunks_tx.clone();
        Closure::<dyn FnMut(MessageEvent)>::new(move |ev: MessageEvent| {
            if let Ok(buf) = ev.data().dyn_into::<js_sys::ArrayBuffer>() {
                let _ = chunks_tx.unbounded_send(js_sys::Uint8Array::new(&buf).to_vec());
            }
        })
    };
    let open = {
        let opened_tx = opened_tx.clone();
        Closure::<dyn FnMut(Event)>::new(move |_| {
            if let Some(tx) = opened_tx.borrow_mut().take() {
                let _ = tx.send(());
            }
        })
    };
    let close = Closure::<dyn FnMut(CloseEvent)>::new(move |ev: CloseEvent| {
        chunks_tx.close_channel();
        // Dropping the open sender fails a pending `connect`.
        opened_tx.borrow_mut().take();
        if let Some(tx) = closed_tx.borrow_mut().take() {
            let _ = tx.send(Closed {
                code: ev.code(),
                reason: ev.reason(),
            });
        }
    });
    socket.set_onmessage(Some(message.as_ref().unchecked_ref()));
    socket.set_onopen(Some(open.as_ref().unchecked_ref()));
    socket.set_onclose(Some(close.as_ref().unchecked_ref()));
    let handlers = Handlers {
        _message: message,
        _close: close,
        _open: open,
    };

    let mut closed = Some(closed_rx);
    if opened_rx.await.is_err() {
        let why = match closed.take() {
            Some(rx) => rx.await.unwrap_or(Closed {
                code: 1006,
                reason: String::new(),
            }),
            None => Closed {
                code: 1006,
                reason: String::new(),
            },
        };
        return Err(ConnectError::NotOpened(why));
    }

    let sender = socket.clone();
    let writer = ChunkWriter::new(move |bytes: &[u8]| {
        sender
            .send_with_u8_array(bytes)
            .map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, format!("{e:?}")))
    });
    let (client, system) = crate::connect_stream(ChunkReader::new(chunks_rx), writer);
    wasm_bindgen_futures::spawn_local(async move {
        let _ = system.await;
    });
    Ok(Connection {
        bootstrap: client,
        closed,
        socket,
        _handlers: handlers,
    })
}
