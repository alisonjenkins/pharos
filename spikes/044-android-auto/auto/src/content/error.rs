//! Every failure the content layer reports. Each variant carries the value
//! that failed (the URL, the id, the call) so a logcat line alone says what
//! to fix; none carries a token or a QuickConnect secret.

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum ContentError {
    #[error("{0}")]
    Url(String),
    #[error("connect {url}: {why}")]
    Connect { url: String, why: String },
    #[error("{op}: no answer within {secs} s")]
    Timeout { op: String, secs: u64 },
    #[error("{op}: connection closed")]
    Closed { op: String },
    #[error("{op}: server answered {kind}: {reason}")]
    Api {
        op: String,
        kind: &'static str,
        reason: String,
    },
    #[error("{op}: rpc failed: {why}")]
    Rpc { op: String, why: String },
    #[error("not signed in; approve the QuickConnect code in Pharos")]
    NeedsSignIn,
    #[error("no QuickConnect code from {server} yet: {last}")]
    NoCode { server: String, last: String },
    #[error("not a media id: {0:?}")]
    BadId(String),
    #[error("token file {}: {why}", path.display())]
    TokenFile { path: PathBuf, why: String },
    #[error("the content worker thread is gone")]
    WorkerGone,
}

impl ContentError {
    /// The server refused because the session or device token is revoked.
    pub fn is_revoked(&self) -> bool {
        matches!(
            self,
            ContentError::Api {
                kind: "revoked",
                ..
            }
        )
    }

    /// Map a client error from call `op`, naming the call.
    pub(crate) fn from_client(op: &str, e: pharos_client::ClientError) -> Self {
        match e {
            pharos_client::ClientError::Api { kind, reason, .. } => ContentError::Api {
                op: op.to_owned(),
                kind,
                reason,
            },
            other => ContentError::Rpc {
                op: op.to_owned(),
                why: other.to_string(),
            },
        }
    }
}
