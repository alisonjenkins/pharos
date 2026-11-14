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
    #[error("audio cache {op}: {why}")]
    Cache { op: String, why: String },
    #[error("resume file {op}: {why}")]
    Resume { op: String, why: String },
    #[error("the content worker thread is gone")]
    WorkerGone,
}

impl ContentError {
    /// A stable class for log fields; unlike `Display` it never names the
    /// server, a path or a TLS reason.
    pub fn label(&self) -> &'static str {
        match self {
            ContentError::Url(_) => "bad_url",
            ContentError::Connect { .. } => "connect",
            ContentError::Timeout { .. } => "timeout",
            ContentError::Closed { .. } => "closed",
            ContentError::Api { .. } => "api",
            ContentError::Rpc { .. } => "rpc",
            ContentError::NeedsSignIn => "needs_sign_in",
            ContentError::NoCode { .. } => "no_code",
            ContentError::BadId(_) => "bad_id",
            ContentError::TokenFile { .. } => "token_file",
            ContentError::Cache { .. } => "cache",
            ContentError::Resume { .. } => "resume",
            ContentError::WorkerGone => "worker_gone",
        }
    }

    /// The error for a log line or a notice the car shows: `Display` minus
    /// anything that names the server, a URL, a path or a TLS reason.
    pub fn redacted(&self) -> String {
        match self {
            ContentError::Timeout { .. }
            | ContentError::Closed { .. }
            | ContentError::Api { .. }
            | ContentError::NeedsSignIn
            | ContentError::BadId(_)
            | ContentError::Cache { .. }
            | ContentError::Resume { .. }
            | ContentError::WorkerGone => self.to_string(),
            ContentError::Rpc { op, .. } => format!("{op}: rpc failed"),
            ContentError::TokenFile { why, .. } => format!("token file: {why}"),
            ContentError::Url(_) | ContentError::Connect { .. } | ContentError::NoCode { .. } => {
                self.label().to_owned()
            }
        }
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacted_never_names_the_server_or_the_tls_reason() {
        let host = "pharos.example";
        for e in [
            ContentError::Url(format!("bad scheme: http://{host}")),
            ContentError::Connect {
                url: format!("wss://{host}/rpc"),
                why: "invalid peer certificate: UnknownIssuer".into(),
            },
            ContentError::Rpc {
                op: "search".into(),
                why: format!("io error talking to {host}"),
            },
            ContentError::NoCode {
                server: format!("https://{host}"),
                last: "refused".into(),
            },
            ContentError::TokenFile {
                path: PathBuf::from(format!("/data/{host}/device-token")),
                why: "permission denied".into(),
            },
        ] {
            let r = e.redacted();
            assert!(!r.contains(host), "{r}");
            assert!(!r.contains("certificate"), "{r}");
        }
        let t = ContentError::Timeout {
            op: "children".into(),
            secs: 8,
        };
        assert_eq!(t.redacted(), t.to_string());
    }
}
