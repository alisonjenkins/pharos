use std::future::Future;
use std::io;

use thiserror::Error;

use crate::messages::{ReceiverCommand, ReceiverReport};

#[derive(Debug, Error)]
pub enum CastError {
    #[error("connecting to cast receiver {addr} failed: {source}")]
    Connect {
        addr: String,
        #[source]
        source: io::Error,
    },
    #[error("launching cast app {app_id} failed: {detail}")]
    Launch { app_id: String, detail: String },
    #[error("cast protocol error: {0}")]
    Protocol(String),
    #[error("cast transport i/o error: {0}")]
    Io(#[from] io::Error),
    #[error("cast connection is closed")]
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppSession {
    pub transport_id: String,
    pub session_id: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TransportEvent {
    Report(ReceiverReport),
    HeartbeatLost,
    Closed { reason: String },
}

pub trait CastTransport: Send {
    fn connect(&mut self) -> impl Future<Output = Result<(), CastError>> + Send;

    fn launch(
        &mut self,
        app_id: &str,
    ) -> impl Future<Output = Result<AppSession, CastError>> + Send;

    fn send(
        &mut self,
        command: &ReceiverCommand,
    ) -> impl Future<Output = Result<(), CastError>> + Send;

    /// Cancel-safe: dropping the future before it resolves loses no event.
    fn recv(&mut self) -> impl Future<Output = TransportEvent> + Send;

    fn close(&mut self) -> impl Future<Output = ()> + Send;
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn errors_carry_their_cause() {
        let e = CastError::Connect {
            addr: "10.0.0.5:8009".into(),
            source: io::Error::new(io::ErrorKind::ConnectionRefused, "refused"),
        };
        let shown = e.to_string();
        assert!(
            shown.contains("10.0.0.5:8009") && shown.contains("refused"),
            "{shown}"
        );
        assert!(std::error::Error::source(&e).is_some());

        let l = CastError::Launch {
            app_id: "F007D354".into(),
            detail: "NOT_FOUND".into(),
        };
        let shown = l.to_string();
        assert!(
            shown.contains("F007D354") && shown.contains("NOT_FOUND"),
            "{shown}"
        );
    }

    #[test]
    fn io_error_converts() {
        let e: CastError = io::Error::new(io::ErrorKind::BrokenPipe, "pipe").into();
        assert!(matches!(e, CastError::Io(_)));
    }

    #[tokio::test]
    async fn trait_futures_are_send_so_a_transport_can_be_spawned() {
        struct Nop;
        impl CastTransport for Nop {
            async fn connect(&mut self) -> Result<(), CastError> {
                Ok(())
            }
            async fn launch(&mut self, app_id: &str) -> Result<AppSession, CastError> {
                Err(CastError::Launch {
                    app_id: app_id.into(),
                    detail: "nop".into(),
                })
            }
            async fn send(&mut self, _: &ReceiverCommand) -> Result<(), CastError> {
                Ok(())
            }
            async fn recv(&mut self) -> TransportEvent {
                TransportEvent::Closed {
                    reason: "nop".into(),
                }
            }
            async fn close(&mut self) {}
        }
        let handle = tokio::spawn(async move {
            let mut t = Nop;
            t.connect().await.is_ok() && t.launch("X").await.is_err()
        });
        assert!(handle.await.unwrap());
    }
}
