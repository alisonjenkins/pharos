//! A CASTV2 receiver on a real TCP + TLS socket, for end-to-end tests.
//!
//! [`MockTv`] speaks the wire protocol the stock receiver speaks (length-framed
//! protobuf over TLS, `tp.connection`, `tp.heartbeat`, `receiver`, and the
//! `com.connectsdk` command channel) and plays the commands it is sent through
//! a [`FakeReceiver`], so the playback model is the one the in-process tests
//! already use. A production `CastV2Transport` can be pointed at it unchanged.

use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_rustls::TlsAcceptor;
use tokio_util::codec::Framed;

use crate::castv2::{
    CastCodec, CastMessage, NS_CONNECTION, NS_CONNECTSDK, NS_HEARTBEAT, NS_RECEIVER,
};
use crate::fake::{FakeReceiver, FakeReceiverConfig, FakeReceiverHandle, FakeState};
use crate::messages::{ReceiverCommand, ReceiverReport};
use crate::transport::{CastTransport, TransportEvent};

const APP_ID: &str = "F007D354";
const TRANSPORT_ID: &str = "mock-transport-1";
const SESSION_ID: &str = "mock-session-1";
const RECEIVER_ID: &str = "receiver-0";
const SENDER_ID: &str = "sender-0";
/// A prober that connects and leaves without handshaking must not pin the
/// accept loop.
const TLS_ACCEPT_TIMEOUT: Duration = Duration::from_secs(5);

/// How the mock answers a `LAUNCH`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LaunchBehavior {
    #[default]
    Accept,
    /// Answer after this long (a cold app start).
    Delay(Duration),
    /// Answer `LAUNCH_ERROR`.
    Refuse,
}

#[derive(Debug, Default)]
struct Control {
    launch: LaunchBehavior,
    malformed: Vec<String>,
}

#[derive(Debug, Default)]
struct Counters {
    connections: AtomicUsize,
    launches: AtomicUsize,
    pings: AtomicUsize,
}

#[derive(Debug)]
struct Shared {
    control: Mutex<Control>,
    counters: Counters,
}

impl Shared {
    fn control(&self) -> MutexGuard<'_, Control> {
        self.control.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Test-side handle on a running mock TV. Dropping it stops the server.
#[derive(Debug)]
pub struct MockTv {
    addr: SocketAddr,
    fingerprint: [u8; 32],
    shared: Arc<Shared>,
    model: FakeReceiverHandle,
    task: JoinHandle<()>,
}

impl Drop for MockTv {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl MockTv {
    /// Bind `127.0.0.1:0` and serve with a healthy default receiver.
    pub async fn start() -> io::Result<Self> {
        Self::start_with(FakeReceiverConfig {
            progress_interval_ms: Some(1_000),
            ..FakeReceiverConfig::default()
        })
        .await
    }

    pub async fn start_with(config: FakeReceiverConfig) -> io::Result<Self> {
        let rcgen::CertifiedKey { cert, signing_key } =
            rcgen::generate_simple_self_signed(vec!["localhost".to_owned()])
                .map_err(io::Error::other)?;
        let fingerprint: [u8; 32] = Sha256::digest(cert.der().as_ref()).into();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let tls = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(io::Error::other)?
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.der().clone()],
                PrivateKeyDer::from(PrivatePkcs8KeyDer::from(signing_key.serialize_der())),
            )
            .map_err(io::Error::other)?;
        let acceptor = TlsAcceptor::from(Arc::new(tls));
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;

        let (model_transport, model) = FakeReceiver::new(config);
        let shared = Arc::new(Shared {
            control: Mutex::new(Control::default()),
            counters: Counters::default(),
        });
        let task = tokio::spawn(accept_loop(
            listener,
            acceptor,
            model_transport,
            Arc::clone(&shared),
        ));
        Ok(Self {
            addr,
            fingerprint,
            shared,
            model,
            task,
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// SHA-256 of the DER certificate, for `TlsPolicy::PinSha256`.
    pub fn fingerprint(&self) -> [u8; 32] {
        self.fingerprint
    }

    /// Every connectsdk command received, decoded from the wire.
    pub fn commands(&self) -> Vec<ReceiverCommand> {
        self.model.commands()
    }

    pub fn state(&self) -> FakeState {
        self.model.state()
    }

    /// The playback model, to drive the TV remote (`user_seek`, `user_stop`, ...)
    /// or change its drift and latency.
    pub fn receiver(&self) -> &FakeReceiverHandle {
        &self.model
    }

    /// Close the TCP connection now; the TV keeps playing.
    pub fn drop_connection(&self) {
        self.model.drop_connection();
    }

    pub fn set_launch(&self, behavior: LaunchBehavior) {
        self.shared.control().launch = behavior;
    }

    /// TLS connections accepted so far.
    pub fn connections(&self) -> usize {
        self.shared.counters.connections.load(Ordering::SeqCst)
    }

    /// `LAUNCH` requests received so far, answered or not.
    pub fn launches(&self) -> usize {
        self.shared.counters.launches.load(Ordering::SeqCst)
    }

    /// Heartbeat `PING`s received from the sender.
    pub fn pings(&self) -> usize {
        self.shared.counters.pings.load(Ordering::SeqCst)
    }

    /// Connectsdk payloads that did not decode as a command, with the error.
    pub fn malformed(&self) -> Vec<String> {
        self.shared.control().malformed.clone()
    }
}

async fn accept_loop(
    listener: TcpListener,
    acceptor: TlsAcceptor,
    mut model: FakeReceiver,
    shared: Arc<Shared>,
) {
    while let Ok((tcp, peer)) = listener.accept().await {
        let tls = match tokio::time::timeout(TLS_ACCEPT_TIMEOUT, acceptor.accept(tcp)).await {
            Ok(Ok(tls)) => tls,
            Ok(Err(e)) => {
                tracing::debug!(%peer, error = %e, "mock tv: TLS handshake failed (a probe?)");
                continue;
            }
            Err(_) => {
                tracing::debug!(%peer, "mock tv: TLS handshake timed out");
                continue;
            }
        };
        shared.counters.connections.fetch_add(1, Ordering::SeqCst);
        if model.connect().await.is_err() {
            continue;
        }
        let ended = serve(tls, &mut model, &shared).await;
        tracing::debug!(%peer, %ended, "mock tv: connection ended");
        model.close().await;
    }
}

fn status(request_id: u64, applications: Value) -> String {
    json!({"type": "RECEIVER_STATUS", "requestId": request_id, "status": {"applications": applications}})
        .to_string()
}

fn app_entry() -> Value {
    json!({"appId": APP_ID, "transportId": TRANSPORT_ID, "sessionId": SESSION_ID, "displayName": "Jellyfin"})
}

/// Serve one connection until either side ends it; says why.
async fn serve<S>(stream: S, model: &mut FakeReceiver, shared: &Shared) -> String
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut framed = Framed::new(stream, CastCodec::default());
    let mut app_running = false;
    loop {
        tokio::select! {
            frame = framed.next() => match frame {
                Some(Ok(msg)) => {
                    if let Err(why) =
                        on_frame(&mut framed, model, shared, &mut app_running, msg).await
                    {
                        return why;
                    }
                }
                Some(Err(e)) => return format!("reading from the sender failed: {e}"),
                None => return "the sender closed the connection".to_owned(),
            },
            event = model.recv() => match event {
                TransportEvent::Report(report) => {
                    if let Err(why) = send_report(&mut framed, &report).await {
                        return why;
                    }
                }
                TransportEvent::Closed { reason } => return reason,
                TransportEvent::HeartbeatLost => return "heartbeat lost".to_owned(),
            },
        }
    }
}

async fn send_report<S>(
    framed: &mut Framed<S, CastCodec>,
    report: &ReceiverReport,
) -> Result<(), String>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let payload =
        serde_json::to_string(report).map_err(|e| format!("encoding a report failed: {e}"))?;
    framed
        .send(CastMessage::string(
            TRANSPORT_ID,
            SENDER_ID,
            NS_CONNECTSDK,
            payload,
        ))
        .await
        .map_err(|e| format!("writing to the sender failed: {e}"))
}

async fn on_frame<S>(
    framed: &mut Framed<S, CastCodec>,
    model: &mut FakeReceiver,
    shared: &Shared,
    app_running: &mut bool,
    msg: CastMessage,
) -> Result<(), String>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let payload = msg.payload_utf8.as_deref().unwrap_or("{}");
    let body: Value = serde_json::from_str(payload).unwrap_or(Value::Null);
    let kind = body.get("type").and_then(Value::as_str).unwrap_or("");
    let request_id = body.get("requestId").and_then(Value::as_u64).unwrap_or(0);
    let reply = |namespace: &str, from: &str, payload: String| {
        CastMessage::string(from, &msg.source_id, namespace, payload)
    };
    let write_failed = |e: crate::castv2::FrameError| format!("writing to the sender failed: {e}");

    match (msg.namespace.as_str(), kind) {
        (NS_HEARTBEAT, "PING") => {
            shared.counters.pings.fetch_add(1, Ordering::SeqCst);
            framed
                .send(reply(
                    NS_HEARTBEAT,
                    RECEIVER_ID,
                    json!({"type": "PONG"}).to_string(),
                ))
                .await
                .map_err(write_failed)?;
        }
        (NS_RECEIVER, "GET_STATUS") => {
            let apps = if *app_running {
                json!([app_entry()])
            } else {
                json!([])
            };
            framed
                .send(reply(NS_RECEIVER, RECEIVER_ID, status(request_id, apps)))
                .await
                .map_err(write_failed)?;
        }
        (NS_RECEIVER, "LAUNCH") => {
            shared.counters.launches.fetch_add(1, Ordering::SeqCst);
            let behavior = shared.control().launch;
            if let LaunchBehavior::Delay(wait) = behavior {
                tokio::time::sleep(wait).await;
            }
            let launched = behavior != LaunchBehavior::Refuse && model.launch(APP_ID).await.is_ok();
            if launched {
                *app_running = true;
                // The app is starting (no transportId yet), then up.
                let starting = json!([{"appId": APP_ID, "displayName": "Jellyfin"}]);
                for apps in [starting, json!([app_entry()])] {
                    framed
                        .send(reply(NS_RECEIVER, RECEIVER_ID, status(request_id, apps)))
                        .await
                        .map_err(write_failed)?;
                }
            } else {
                framed
                    .send(reply(
                        NS_RECEIVER,
                        RECEIVER_ID,
                        json!({"type": "LAUNCH_ERROR", "requestId": request_id, "reason": "NOT_FOUND"})
                            .to_string(),
                    ))
                    .await
                    .map_err(write_failed)?;
            }
        }
        (NS_CONNECTION, "CLOSE") if msg.destination_id == TRANSPORT_ID => *app_running = false,
        (NS_CONNECTSDK, _) => match serde_json::from_str::<ReceiverCommand>(payload) {
            Ok(command) => {
                if model.send(&command).await.is_err() {
                    return Err("the playback model refused a command".to_owned());
                }
            }
            Err(e) => {
                shared.control().malformed.push(format!(
                    "{e}: {}",
                    payload.chars().take(200).collect::<String>()
                ));
            }
        },
        _ => {}
    }
    Ok(())
}
