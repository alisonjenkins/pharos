//! CASTV2 wire format and transport: one protobuf message, 4-byte big-endian
//! length framing, TLS to port 8009.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use futures_util::{SinkExt, StreamExt};
use prost::Message;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::DigitallySignedStruct;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_rustls::TlsConnector;
use tokio_util::codec::{Decoder, Encoder, Framed, LengthDelimitedCodec};

use crate::messages::ReceiverCommand;
use crate::transport::{AppSession, CastError, CastTransport, TransportEvent};

/// Chromium's own receiver rejects frames above 64 KiB; 1 MiB leaves room for
/// large status payloads while a hostile length prefix cannot force a big
/// allocation.
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

pub const NS_CONNECTION: &str = "urn:x-cast:com.google.cast.tp.connection";
pub const NS_HEARTBEAT: &str = "urn:x-cast:com.google.cast.tp.heartbeat";
pub const NS_RECEIVER: &str = "urn:x-cast:com.google.cast.receiver";
pub const NS_CONNECTSDK: &str = "urn:x-cast:com.connectsdk";

/// Field numbers and requiredness follow the public `cast_channel.proto`
/// (Chromium `components/cast_channel/proto`, as vendored by rust-cast):
///
/// ```text
/// required ProtocolVersion protocol_version = 1;   // CASTV2_1_0 = 0
/// required string source_id = 2;
/// required string destination_id = 3;
/// required string namespace = 4;
/// required PayloadType payload_type = 5;           // STRING = 0, BINARY = 1
/// optional string payload_utf8 = 6;
/// optional bytes payload_binary = 7;
/// ```
///
/// Fields 8 (`continued`) and 9 (`remaining_length`) belong to the deprecated
/// chunking versions and are not used by CASTV2_1_0.
#[derive(Clone, PartialEq, Message)]
pub struct CastMessage {
    #[prost(enumeration = "ProtocolVersion", required, tag = "1")]
    pub protocol_version: i32,
    #[prost(string, required, tag = "2")]
    pub source_id: String,
    #[prost(string, required, tag = "3")]
    pub destination_id: String,
    #[prost(string, required, tag = "4")]
    pub namespace: String,
    #[prost(enumeration = "PayloadType", required, tag = "5")]
    pub payload_type: i32,
    #[prost(string, optional, tag = "6")]
    pub payload_utf8: Option<String>,
    #[prost(bytes = "vec", optional, tag = "7")]
    pub payload_binary: Option<Vec<u8>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, prost::Enumeration)]
#[repr(i32)]
pub enum ProtocolVersion {
    Castv21_0 = 0,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, prost::Enumeration)]
#[repr(i32)]
pub enum PayloadType {
    String = 0,
    Binary = 1,
}

impl CastMessage {
    pub fn string(
        source_id: &str,
        destination_id: &str,
        namespace: &str,
        payload: impl Into<String>,
    ) -> Self {
        Self {
            protocol_version: ProtocolVersion::Castv21_0 as i32,
            source_id: source_id.to_owned(),
            destination_id: destination_id.to_owned(),
            namespace: namespace.to_owned(),
            payload_type: PayloadType::String as i32,
            payload_utf8: Some(payload.into()),
            payload_binary: None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("cast frame exceeds the {MAX_FRAME_BYTES}-byte limit: {0}")]
    Codec(#[source] std::io::Error),
    #[error("cast frame is not a valid CastMessage: {0}")]
    Decode(#[from] prost::DecodeError),
}

impl From<std::io::Error> for FrameError {
    fn from(e: std::io::Error) -> Self {
        FrameError::Codec(e)
    }
}

/// Length-delimited codec for CASTV2: 4-byte big-endian prefix, bounded.
#[derive(Debug)]
pub struct CastCodec {
    inner: LengthDelimitedCodec,
}

impl Default for CastCodec {
    fn default() -> Self {
        Self {
            inner: LengthDelimitedCodec::builder()
                .big_endian()
                .length_field_length(4)
                .max_frame_length(MAX_FRAME_BYTES)
                .new_codec(),
        }
    }
}

impl Encoder<CastMessage> for CastCodec {
    type Error = FrameError;

    fn encode(&mut self, item: CastMessage, dst: &mut BytesMut) -> Result<(), FrameError> {
        let body = Bytes::from(item.encode_to_vec());
        Ok(self.inner.encode(body, dst)?)
    }
}

impl Decoder for CastCodec {
    type Item = CastMessage;
    type Error = FrameError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<CastMessage>, FrameError> {
        match self.inner.decode(src)? {
            Some(frame) => Ok(Some(CastMessage::decode(frame.freeze())?)),
            None => Ok(None),
        }
    }
}

// ---------------------------------------------------------------------------
// TLS policy
// ---------------------------------------------------------------------------

/// How the device's certificate is judged.
///
/// Chromecasts present a per-device self-signed certificate (issued by a
/// Google-internal device CA that no public trust store contains), so ordinary
/// chain validation can never succeed. The two policies replace chain
/// validation for this one connection type only; TLS signatures are still
/// verified, so the peer must hold the key for the certificate it presents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TlsPolicy {
    /// Accept whatever certificate the device presents (LAN, trust-on-first-use
    /// is the caller's decision).
    AcceptAny,
    /// Accept only the certificate whose DER encoding has this SHA-256.
    PinSha256([u8; 32]),
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn sha256(der: &[u8]) -> [u8; 32] {
    Sha256::digest(der).into()
}

#[derive(Debug)]
struct DeviceCertVerifier {
    policy: TlsPolicy,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for DeviceCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let actual = sha256(end_entity.as_ref());
        match &self.policy {
            TlsPolicy::AcceptAny => {
                tracing::debug!(
                    fingerprint = %hex(&actual),
                    "accepting self-signed cast device certificate"
                );
                Ok(ServerCertVerified::assertion())
            }
            TlsPolicy::PinSha256(expected) if *expected == actual => {
                Ok(ServerCertVerified::assertion())
            }
            TlsPolicy::PinSha256(expected) => Err(rustls::Error::General(format!(
                "cast device certificate fingerprint mismatch: expected sha256 {}, got {}",
                hex(expected),
                hex(&actual)
            ))),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn tls_connector(policy: &TlsPolicy) -> Result<TlsConnector, rustls::Error> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(Arc::clone(&provider))
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(DeviceCertVerifier {
            policy: policy.clone(),
            provider,
        }))
        .with_no_client_auth();
    Ok(TlsConnector::from(Arc::new(config)))
}

// ---------------------------------------------------------------------------
// Receiver payloads
// ---------------------------------------------------------------------------

const SENDER_ID: &str = "sender-0";
const RECEIVER_ID: &str = "receiver-0";
/// The Chromecast home screen; it is reported as a running application.
const BACKDROP_APP_ID: &str = "E8C28D3C";
pub const DEFAULT_PORT: u16 = 8009;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningApp {
    pub app_id: String,
    pub transport_id: Option<String>,
    pub session_id: Option<String>,
    pub display_name: Option<String>,
    pub is_idle_screen: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReceiverStatus {
    pub applications: Vec<RunningApp>,
}

/// What a receiver is doing right now, judged against the app we would launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Occupancy {
    Idle,
    /// Our own app is already running. The receiver does not say which sender
    /// started it, so the caller treats this as possibly another sender's.
    OurApp {
        session_id: Option<String>,
    },
    ForeignApp {
        app_id: String,
        display_name: Option<String>,
    },
}

impl ReceiverStatus {
    pub fn occupancy(&self, our_app_id: &str) -> Occupancy {
        let busy = self
            .applications
            .iter()
            .filter(|a| !a.is_idle_screen && a.app_id != BACKDROP_APP_ID);
        let mut ours = None;
        for app in busy {
            if app.app_id == our_app_id {
                ours = Some(app);
            } else {
                return Occupancy::ForeignApp {
                    app_id: app.app_id.clone(),
                    display_name: app.display_name.clone(),
                };
            }
        }
        match ours {
            Some(app) => Occupancy::OurApp {
                session_id: app.session_id.clone(),
            },
            None => Occupancy::Idle,
        }
    }
}

#[derive(Debug, serde::Deserialize)]
struct ReceiverEnvelope {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(rename = "requestId", default)]
    request_id: u32,
    #[serde(default)]
    status: Option<StatusBody>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct StatusBody {
    #[serde(default)]
    applications: Vec<AppEntry>,
}

#[derive(Debug, serde::Deserialize)]
struct AppEntry {
    #[serde(rename = "appId", default)]
    app_id: String,
    #[serde(rename = "transportId", default)]
    transport_id: Option<String>,
    #[serde(rename = "sessionId", default)]
    session_id: Option<String>,
    #[serde(rename = "displayName", default)]
    display_name: Option<String>,
    #[serde(rename = "isIdleScreen", default)]
    is_idle_screen: bool,
}

impl ReceiverEnvelope {
    fn into_status(self) -> ReceiverStatus {
        ReceiverStatus {
            applications: self
                .status
                .unwrap_or_default()
                .applications
                .into_iter()
                .map(|a| RunningApp {
                    app_id: a.app_id,
                    transport_id: a.transport_id.filter(|s| !s.is_empty()),
                    session_id: a.session_id.filter(|s| !s.is_empty()),
                    display_name: a.display_name,
                    is_idle_screen: a.is_idle_screen,
                })
                .collect(),
        }
    }
}

/// Payloads can embed device and app names; keep log lines and errors bounded.
fn truncate(s: &str) -> String {
    const MAX: usize = 512;
    if s.len() <= MAX {
        return s.to_owned();
    }
    let cut = (0..=MAX)
        .rev()
        .find(|i| s.is_char_boundary(*i))
        .unwrap_or(0);
    format!("{}...", s.get(..cut).unwrap_or(""))
}

fn parse_receiver(msg: &CastMessage) -> Option<ReceiverEnvelope> {
    let payload = msg.payload_utf8.as_deref()?;
    match serde_json::from_str(payload) {
        Ok(env) => Some(env),
        Err(e) => {
            tracing::warn!(error = %e, payload = %truncate(payload), "unparseable cast receiver payload");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Connection task
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct CastV2Config {
    /// Bound on the TCP connect and, separately, the TLS handshake.
    pub connect_timeout: Duration,
    /// Bound on LAUNCH to a usable transportId, and on GET_STATUS.
    pub request_timeout: Duration,
}

impl Default for CastV2Config {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(5),
            // A cold Default Media Receiver launch takes a few seconds.
            request_timeout: Duration::from_secs(15),
        }
    }
}

enum Cmd {
    Send(CastMessage),
    /// Flush what is queued, then end the task.
    Shutdown,
}

enum Inbound {
    Frame(CastMessage),
    Closed(String),
}

struct Conn {
    out: mpsc::Sender<Cmd>,
    inbound: mpsc::UnboundedReceiver<Inbound>,
    task: JoinHandle<()>,
}

impl Drop for Conn {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn io_task<S>(
    mut framed: Framed<S, CastCodec>,
    mut out: mpsc::Receiver<Cmd>,
    inbound: mpsc::UnboundedSender<Inbound>,
) where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let reason = loop {
        tokio::select! {
            frame = framed.next() => match frame {
                Some(Ok(msg)) => {
                    if inbound.send(Inbound::Frame(msg)).is_err() {
                        return;
                    }
                }
                Some(Err(e)) => break format!("reading from the cast device failed: {e}"),
                None => break "the cast device closed the connection".to_owned(),
            },
            cmd = out.recv() => match cmd {
                Some(Cmd::Send(msg)) => {
                    if let Err(e) = framed.send(msg).await {
                        break format!("writing to the cast device failed: {e}");
                    }
                }
                Some(Cmd::Shutdown) | None => {
                    let _ = framed.close().await;
                    return;
                }
            },
        }
    };
    let _ = inbound.send(Inbound::Closed(reason));
}

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

pub struct CastV2Transport {
    host: String,
    port: u16,
    policy: TlsPolicy,
    config: CastV2Config,
    conn: Option<Conn>,
    request_id: u32,
    app: Option<AppSession>,
    terminal: Option<TransportEvent>,
}

impl std::fmt::Debug for CastV2Transport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CastV2Transport")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("policy", &self.policy)
            .field("connected", &self.conn.is_some())
            .finish()
    }
}

impl CastV2Transport {
    pub fn new(host: impl Into<String>, port: u16, policy: TlsPolicy) -> Self {
        Self::with_config(host, port, policy, CastV2Config::default())
    }

    pub fn from_addr(addr: SocketAddr, policy: TlsPolicy) -> Self {
        Self::new(addr.ip().to_string(), addr.port(), policy)
    }

    pub fn with_config(
        host: impl Into<String>,
        port: u16,
        policy: TlsPolicy,
        config: CastV2Config,
    ) -> Self {
        Self {
            host: host.into(),
            port,
            policy,
            config,
            conn: None,
            request_id: 0,
            app: None,
            terminal: None,
        }
    }

    fn addr_label(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    fn next_request_id(&mut self) -> u32 {
        self.request_id = self.request_id.saturating_add(1);
        self.request_id
    }

    fn connect_error(&self, source: io::Error) -> CastError {
        CastError::Connect {
            addr: self.addr_label(),
            source,
        }
    }

    async fn send_frame(&self, msg: CastMessage) -> Result<(), CastError> {
        let conn = self.conn.as_ref().ok_or(CastError::Closed)?;
        conn.out
            .send(Cmd::Send(msg))
            .await
            .map_err(|_| CastError::Closed)
    }

    async fn send_json(
        &self,
        destination: &str,
        namespace: &str,
        body: &serde_json::Value,
    ) -> Result<(), CastError> {
        self.send_frame(CastMessage::string(
            SENDER_ID,
            destination,
            namespace,
            body.to_string(),
        ))
        .await
    }

    async fn virtual_connect(&self, destination: &str) -> Result<(), CastError> {
        self.send_json(
            destination,
            NS_CONNECTION,
            &serde_json::json!({"type": "CONNECT"}),
        )
        .await
    }

    /// Next frame from the device, or why the connection ended.
    async fn next_frame(&mut self, deadline: Instant) -> Result<Option<CastMessage>, CastError> {
        if self.terminal.is_some() {
            return Err(CastError::Closed);
        }
        let conn = self.conn.as_mut().ok_or(CastError::Closed)?;
        match tokio::time::timeout_at(deadline, conn.inbound.recv()).await {
            Err(_) => Ok(None),
            Ok(Some(Inbound::Frame(msg))) => Ok(Some(msg)),
            Ok(Some(Inbound::Closed(reason))) => {
                self.terminal = Some(TransportEvent::Closed { reason });
                Err(CastError::Closed)
            }
            Ok(None) => {
                self.terminal = Some(TransportEvent::Closed {
                    reason: "the connection task ended".to_owned(),
                });
                Err(CastError::Closed)
            }
        }
    }

    /// `GET_STATUS` on the receiver namespace.
    pub async fn get_status(&mut self) -> Result<ReceiverStatus, CastError> {
        let request_id = self.next_request_id();
        self.send_json(
            RECEIVER_ID,
            NS_RECEIVER,
            &serde_json::json!({"type": "GET_STATUS", "requestId": request_id}),
        )
        .await?;
        let deadline = Instant::now() + self.config.request_timeout;
        loop {
            let Some(msg) = self.next_frame(deadline).await? else {
                return Err(CastError::Protocol(format!(
                    "no RECEIVER_STATUS for GET_STATUS {request_id} within {:?}",
                    self.config.request_timeout
                )));
            };
            if msg.namespace != NS_RECEIVER {
                continue;
            }
            if let Some(env) = parse_receiver(&msg) {
                if env.kind == "RECEIVER_STATUS" && env.request_id == request_id {
                    return Ok(env.into_status());
                }
            }
        }
    }

    /// Whether a different app is running on the receiver, to be asked before
    /// `launch`: a second sender silently steals the receiver's replies.
    pub async fn busy_with_foreign_app(
        &mut self,
        our_app_id: &str,
    ) -> Result<Occupancy, CastError> {
        let occupancy = self.get_status().await?.occupancy(our_app_id);
        tracing::debug!(addr = %self.addr_label(), ?occupancy, "cast receiver occupancy");
        Ok(occupancy)
    }
}

impl CastTransport for CastV2Transport {
    async fn connect(&mut self) -> Result<(), CastError> {
        self.conn = None;
        self.app = None;
        self.terminal = None;

        let timeout_error = |what: &str, after: Duration| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                format!("{what} did not complete within {after:?}"),
            )
        };
        let limit = self.config.connect_timeout;

        let tcp = tokio::time::timeout(limit, TcpStream::connect((self.host.as_str(), self.port)))
            .await
            .map_err(|_| self.connect_error(timeout_error("TCP connect", limit)))?
            .map_err(|e| self.connect_error(e))?;
        tcp.set_nodelay(true).map_err(|e| self.connect_error(e))?;

        let connector =
            tls_connector(&self.policy).map_err(|e| self.connect_error(io::Error::other(e)))?;
        let server_name = ServerName::try_from(self.host.clone()).map_err(|e| {
            self.connect_error(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid cast host name {:?}: {e}", self.host),
            ))
        })?;
        let tls = tokio::time::timeout(limit, connector.connect(server_name, tcp))
            .await
            .map_err(|_| self.connect_error(timeout_error("TLS handshake", limit)))?
            .map_err(|e| self.connect_error(e))?;

        let (out_tx, out_rx) = mpsc::channel(32);
        let (in_tx, in_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(io_task(
            Framed::new(tls, CastCodec::default()),
            out_rx,
            in_tx,
        ));
        self.conn = Some(Conn {
            out: out_tx,
            inbound: in_rx,
            task,
        });

        self.virtual_connect(RECEIVER_ID).await?;
        tracing::info!(addr = %self.addr_label(), "cast receiver connected");
        Ok(())
    }

    async fn launch(&mut self, app_id: &str) -> Result<AppSession, CastError> {
        let launch_error = |detail: String| CastError::Launch {
            app_id: app_id.to_owned(),
            detail,
        };
        let request_id = self.next_request_id();
        self.send_json(
            RECEIVER_ID,
            NS_RECEIVER,
            &serde_json::json!({"type": "LAUNCH", "appId": app_id, "requestId": request_id}),
        )
        .await?;

        let deadline = Instant::now() + self.config.request_timeout;
        let mut last_status = String::from("none received");
        let session = loop {
            let Some(msg) = self.next_frame(deadline).await? else {
                return Err(launch_error(format!(
                    "timed out after {:?} waiting for RECEIVER_STATUS with a transportId; last status: {last_status}",
                    self.config.request_timeout
                )));
            };
            if msg.namespace != NS_RECEIVER {
                continue;
            }
            let payload = msg.payload_utf8.clone().unwrap_or_default();
            let Some(env) = parse_receiver(&msg) else {
                continue;
            };
            match env.kind.as_str() {
                "LAUNCH_ERROR" | "INVALID_REQUEST" if env.request_id == request_id => {
                    return Err(launch_error(format!("{} {}", env.kind, truncate(&payload))));
                }
                "RECEIVER_STATUS" => {
                    last_status = truncate(&payload);
                    let found = env
                        .into_status()
                        .applications
                        .into_iter()
                        .find(|a| a.app_id == app_id);
                    if let Some(RunningApp {
                        transport_id: Some(transport_id),
                        session_id: Some(session_id),
                        ..
                    }) = found
                    {
                        break AppSession {
                            transport_id,
                            session_id,
                        };
                    }
                }
                _ => {}
            }
        };

        self.virtual_connect(&session.transport_id).await?;
        self.app = Some(session.clone());
        tracing::info!(
            addr = %self.addr_label(),
            app_id,
            transport_id = %session.transport_id,
            session_id = %session.session_id,
            "cast app launched"
        );
        Ok(session)
    }

    async fn send(&mut self, command: &ReceiverCommand) -> Result<(), CastError> {
        let app = self.app.as_ref().ok_or_else(|| {
            CastError::Protocol("send before launch: no app transport to address".to_owned())
        })?;
        let payload = serde_json::to_string(command)
            .map_err(|e| CastError::Protocol(format!("encoding {}: {e}", command.body.name())))?;
        self.send_frame(CastMessage::string(
            SENDER_ID,
            &app.transport_id,
            NS_CONNECTSDK,
            payload,
        ))
        .await
    }

    async fn recv(&mut self) -> TransportEvent {
        if let Some(event) = &self.terminal {
            return event.clone();
        }
        loop {
            match self
                .next_frame(Instant::now() + Duration::from_secs(3600))
                .await
            {
                Ok(_) => {}
                Err(_) => {
                    return self.terminal.clone().unwrap_or(TransportEvent::Closed {
                        reason: "not connected".to_owned(),
                    });
                }
            }
        }
    }

    async fn close(&mut self) {
        let Some(mut conn) = self.conn.take() else {
            return;
        };
        let mut targets = vec![RECEIVER_ID.to_owned()];
        if let Some(app) = self.app.take() {
            targets.insert(0, app.transport_id);
        }
        for target in targets {
            let msg = CastMessage::string(
                SENDER_ID,
                &target,
                NS_CONNECTION,
                serde_json::json!({"type": "CLOSE"}).to_string(),
            );
            if conn.out.send(Cmd::Send(msg)).await.is_err() {
                break;
            }
        }
        if conn.out.send(Cmd::Shutdown).await.is_ok() {
            // Bounded so a wedged socket cannot stall shutdown; Drop aborts it.
            let _ = tokio::time::timeout(Duration::from_secs(1), &mut conn.task).await;
        }
    }
}

#[cfg(test)]
mod codec_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn sample() -> CastMessage {
        CastMessage::string("sender-0", "receiver-0", NS_HEARTBEAT, r#"{"type":"PING"}"#)
    }

    #[test]
    fn frame_round_trips_with_big_endian_prefix() {
        let mut codec = CastCodec::default();
        let mut buf = BytesMut::new();
        codec.encode(sample(), &mut buf).unwrap();
        let declared = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
        assert_eq!(declared, buf.len() - 4);
        let back = codec.decode(&mut buf).unwrap().unwrap();
        assert_eq!(back, sample());
        assert!(buf.is_empty());
    }

    #[test]
    fn partial_frame_waits_for_more_bytes() {
        let mut codec = CastCodec::default();
        let mut full = BytesMut::new();
        codec.encode(sample(), &mut full).unwrap();
        let mut head = full.split_to(full.len() - 3);
        assert!(codec.decode(&mut head).unwrap().is_none());
        head.extend_from_slice(&full);
        assert_eq!(codec.decode(&mut head).unwrap().unwrap(), sample());
    }

    #[test]
    fn oversized_length_is_rejected_without_allocating_it() {
        let mut codec = CastCodec::default();
        // Declares ~4 GiB but supplies 8 bytes: the check must fire on the
        // prefix alone, before any buffer of that size is reserved.
        let mut buf = BytesMut::from(&[0xFF, 0xFF, 0xFF, 0xF0, 1, 2, 3, 4][..]);
        let err = codec.decode(&mut buf).unwrap_err();
        assert!(matches!(err, FrameError::Codec(_)), "{err}");
        assert!(
            buf.capacity() < MAX_FRAME_BYTES,
            "capacity {}",
            buf.capacity()
        );
    }

    #[test]
    fn garbage_body_is_a_decode_error_not_a_panic() {
        let mut codec = CastCodec::default();
        let mut buf = BytesMut::from(&[0, 0, 0, 3, 0xFF, 0xFF, 0xFF][..]);
        let err = codec.decode(&mut buf).unwrap_err();
        assert!(matches!(err, FrameError::Decode(_)), "{err}");
    }

    #[test]
    fn wire_tags_match_cast_channel_proto() {
        // Field 1 varint 0, field 2 "a", field 3 "b", field 4 "n",
        // field 5 varint 0, field 6 "p".
        let m = CastMessage::string("a", "b", "n", "p");
        assert_eq!(
            m.encode_to_vec(),
            [0x08, 0, 0x12, 1, b'a', 0x1A, 1, b'b', 0x22, 1, b'n', 0x28, 0, 0x32, 1, b'p']
        );
    }
}

#[cfg(test)]
mod transport_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::Mutex;

    use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
    use serde_json::{json, Value};
    use tokio::net::TcpListener;
    use tokio_rustls::TlsAcceptor;

    use super::*;
    use crate::messages::{AccessToken, CommandBody};

    const APP: &str = "F007D354";
    const TRANSPORT: &str = "transport-1";

    /// What the in-process receiver does; the defaults are a healthy device.
    #[derive(Clone)]
    struct Script {
        launch_error: bool,
        /// Status for GET_STATUS.
        applications: Value,
    }

    impl Default for Script {
        fn default() -> Self {
            Self {
                launch_error: false,
                applications: json!([]),
            }
        }
    }

    struct FakeDevice {
        port: u16,
        fingerprint: [u8; 32],
        /// `"<type> -> <destination>"` for every tp.connection message.
        connections: Arc<Mutex<Vec<String>>>,
        /// connectsdk payloads received.
        #[allow(dead_code)]
        commands: Arc<Mutex<Vec<Value>>>,
        _task: JoinHandle<()>,
    }

    fn reply(request_id: u64, kind: &str, applications: Value) -> String {
        json!({"type": kind, "requestId": request_id, "status": {"applications": applications}})
            .to_string()
    }

    fn app_entry() -> Value {
        json!([{"appId": APP, "transportId": TRANSPORT, "sessionId": "sess-1", "displayName": "Jellyfin"}])
    }

    async fn start_device(script: Script) -> FakeDevice {
        let rcgen::CertifiedKey { cert, signing_key } =
            rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
        let fingerprint = sha256(cert.der().as_ref());
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.der().clone()],
                PrivateKeyDer::from(PrivatePkcs8KeyDer::from(signing_key.serialize_der())),
            )
            .unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let connections = Arc::new(Mutex::new(Vec::new()));
        let commands = Arc::new(Mutex::new(Vec::new()));
        let (conns, cmds) = (Arc::clone(&connections), Arc::clone(&commands));
        let task = tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let Ok(tls) = acceptor.accept(tcp).await else {
                    continue;
                };
                serve(tls, script.clone(), Arc::clone(&conns), Arc::clone(&cmds)).await;
            }
        });
        FakeDevice {
            port,
            fingerprint,
            connections,
            commands,
            _task: task,
        }
    }

    async fn serve<S: AsyncRead + AsyncWrite + Unpin>(
        stream: S,
        script: Script,
        connections: Arc<Mutex<Vec<String>>>,
        commands: Arc<Mutex<Vec<Value>>>,
    ) {
        let mut framed = Framed::new(stream, CastCodec::default());
        while let Some(Ok(msg)) = framed.next().await {
            let body: Value =
                serde_json::from_str(msg.payload_utf8.as_deref().unwrap_or("{}")).unwrap();
            let kind = body["type"].as_str().unwrap_or("").to_owned();
            let request_id = body["requestId"].as_u64().unwrap_or(0);
            let answer = |ns: &str, from: &str, payload: String| {
                CastMessage::string(from, &msg.source_id, ns, payload)
            };
            match (msg.namespace.as_str(), kind.as_str()) {
                (NS_CONNECTION, _) => {
                    connections
                        .lock()
                        .unwrap()
                        .push(format!("{kind} -> {}", msg.destination_id));
                }
                (NS_RECEIVER, "GET_STATUS") => {
                    let m = answer(
                        NS_RECEIVER,
                        RECEIVER_ID,
                        reply(request_id, "RECEIVER_STATUS", script.applications.clone()),
                    );
                    framed.send(m).await.unwrap();
                }
                (NS_RECEIVER, "LAUNCH") if script.launch_error => {
                    let m = answer(
                        NS_RECEIVER,
                        RECEIVER_ID,
                        json!({"type": "LAUNCH_ERROR", "requestId": request_id, "reason": "NOT_FOUND"})
                            .to_string(),
                    );
                    framed.send(m).await.unwrap();
                }
                (NS_RECEIVER, "LAUNCH") => {
                    // First status: the app is starting and has no transportId yet.
                    let starting = json!([{"appId": APP, "displayName": "Jellyfin"}]);
                    for apps in [starting, app_entry()] {
                        let m = answer(
                            NS_RECEIVER,
                            RECEIVER_ID,
                            reply(request_id, "RECEIVER_STATUS", apps),
                        );
                        framed.send(m).await.unwrap();
                    }
                }
                (NS_CONNECTSDK, _) => {
                    commands.lock().unwrap().push(body);
                }
                _ => {}
            }
        }
    }

    async fn wait_for_len<T: Clone>(log: &Mutex<Vec<T>>, len: usize) -> Vec<T> {
        for _ in 0..200 {
            {
                let seen = log.lock().unwrap();
                if seen.len() >= len {
                    return seen.clone();
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        log.lock().unwrap().clone()
    }

    fn fast_config() -> CastV2Config {
        CastV2Config {
            connect_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_millis(600),
        }
    }

    fn transport(device: &FakeDevice, policy: TlsPolicy) -> CastV2Transport {
        CastV2Transport::with_config("127.0.0.1", device.port, policy, fast_config())
    }

    #[tokio::test]
    async fn connect_then_launch_returns_the_app_transport() {
        let device = start_device(Script::default()).await;
        let mut t = transport(&device, TlsPolicy::AcceptAny);
        t.connect().await.unwrap();
        let session = t.launch(APP).await.unwrap();
        assert_eq!(
            session,
            AppSession {
                transport_id: TRANSPORT.into(),
                session_id: "sess-1".into()
            }
        );
        t.close().await;
        let seen = wait_for_len(&device.connections, 4).await;
        assert_eq!(
            seen,
            [
                "CONNECT -> receiver-0",
                "CONNECT -> transport-1",
                "CLOSE -> transport-1",
                "CLOSE -> receiver-0"
            ]
        );
    }

    #[tokio::test]
    async fn launch_error_names_the_payload() {
        let device = start_device(Script {
            launch_error: true,
            ..Script::default()
        })
        .await;
        let mut t = transport(&device, TlsPolicy::AcceptAny);
        t.connect().await.unwrap();
        let err = t.launch(APP).await.unwrap_err();
        let shown = err.to_string();
        assert!(matches!(err, CastError::Launch { .. }), "{shown}");
        assert!(
            shown.contains(APP) && shown.contains("LAUNCH_ERROR") && shown.contains("NOT_FOUND"),
            "{shown}"
        );
    }

    #[tokio::test]
    async fn pinned_fingerprint_mismatch_names_expected_and_actual() {
        let device = start_device(Script::default()).await;
        let wrong = [7u8; 32];
        let mut t = transport(&device, TlsPolicy::PinSha256(wrong));
        let err = t.connect().await.unwrap_err();
        let shown = err.to_string();
        assert!(matches!(err, CastError::Connect { .. }), "{shown}");
        assert!(shown.contains(&hex(&wrong)), "{shown}");
        assert!(shown.contains(&hex(&device.fingerprint)), "{shown}");
    }

    #[tokio::test]
    async fn pinned_fingerprint_match_connects() {
        let device = start_device(Script::default()).await;
        let mut t = transport(&device, TlsPolicy::PinSha256(device.fingerprint));
        t.connect().await.unwrap();
        t.close().await;
    }

    #[tokio::test]
    async fn connection_refused_is_a_connect_error_with_the_address() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let mut t =
            CastV2Transport::with_config("127.0.0.1", port, TlsPolicy::AcceptAny, fast_config());
        let shown = t.connect().await.unwrap_err().to_string();
        assert!(shown.contains(&format!("127.0.0.1:{port}")), "{shown}");
    }

    #[tokio::test]
    async fn get_status_classifies_foreign_and_idle_receivers() {
        let foreign = start_device(Script {
            applications: json!([{"appId": "CC1AD845", "displayName": "Default Media Receiver"}]),
            ..Script::default()
        })
        .await;
        let mut t = transport(&foreign, TlsPolicy::AcceptAny);
        t.connect().await.unwrap();
        assert_eq!(
            t.busy_with_foreign_app(APP).await.unwrap(),
            Occupancy::ForeignApp {
                app_id: "CC1AD845".into(),
                display_name: Some("Default Media Receiver".into())
            }
        );

        let idle = start_device(Script {
            applications: json!([{"appId": "E8C28D3C", "isIdleScreen": true}]),
            ..Script::default()
        })
        .await;
        let mut t = transport(&idle, TlsPolicy::AcceptAny);
        t.connect().await.unwrap();
        assert_eq!(t.busy_with_foreign_app(APP).await.unwrap(), Occupancy::Idle);

        let ours = start_device(Script {
            applications: app_entry(),
            ..Script::default()
        })
        .await;
        let mut t = transport(&ours, TlsPolicy::AcceptAny);
        t.connect().await.unwrap();
        assert_eq!(
            t.busy_with_foreign_app(APP).await.unwrap(),
            Occupancy::OurApp {
                session_id: Some("sess-1".into())
            }
        );
    }

    #[tokio::test]
    async fn request_ids_increase_monotonically() {
        let mut t = CastV2Transport::new("h", 1, TlsPolicy::AcceptAny);
        let ids = [
            t.next_request_id(),
            t.next_request_id(),
            t.next_request_id(),
        ];
        assert_eq!(ids, [1, 2, 3]);
    }

    #[tokio::test]
    async fn send_before_launch_is_a_protocol_error() {
        let mut t = CastV2Transport::new("127.0.0.1", 1, TlsPolicy::AcceptAny);
        let cmd = ReceiverCommand {
            body: CommandBody::Pause,
            server_address: "http://x".into(),
            access_token: AccessToken::new("tok"),
            receiver_name: None,
            user_id: None,
            device_id: None,
            server_id: None,
            server_version: None,
            max_bitrate: None,
        };
        assert!(matches!(
            t.send(&cmd).await.unwrap_err(),
            CastError::Protocol(_)
        ));
    }
}
