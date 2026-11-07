//! Client-supplied identifiers that travel as opaque strings on the wire.
//!
//! Both are `#[serde(transparent)]`, so the JSON form is the bare string a
//! plain `String` field produced before.

use serde::{Deserialize, Serialize};

/// A play session id: minted at PlaybackInfo and echoed back by the client on
/// every stream and report. It authorizes a stream on its own (B75), so
/// `Debug` shows only the log-safe prefix and there is no `Display`.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PlaySessionId(String);

impl PlaySessionId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

impl std::fmt::Debug for PlaySessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PlaySessionId({}..)", crate::secret_prefix(&self.0))
    }
}

impl From<String> for PlaySessionId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for PlaySessionId {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

/// The client-chosen device id from the `Emby-Authorization` header. Not the
/// transcoder's hardware device (`pharos_transcode::protocol::DeviceId`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ClientDeviceId(String);

impl ClientDeviceId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

impl std::fmt::Display for ClientDeviceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for ClientDeviceId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for ClientDeviceId {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn wire_form_is_the_bare_string() {
        let p = PlaySessionId::new("abc123");
        assert_eq!(serde_json::to_string(&p).unwrap(), r#""abc123""#);
        let back: PlaySessionId = serde_json::from_str(r#""abc123""#).unwrap();
        assert_eq!(back, p);

        let d = ClientDeviceId::new("dev-1");
        assert_eq!(serde_json::to_string(&d).unwrap(), r#""dev-1""#);
        let back: ClientDeviceId = serde_json::from_str(r#""dev-1""#).unwrap();
        assert_eq!(back, d);
    }

    #[test]
    fn wire_form_inside_a_struct_is_unchanged() {
        #[derive(Serialize)]
        struct Old {
            id: String,
        }
        #[derive(Serialize)]
        struct New {
            id: PlaySessionId,
        }
        let old = serde_json::to_string(&Old { id: "x y".into() }).unwrap();
        let new = serde_json::to_string(&New { id: "x y".into() }).unwrap();
        assert_eq!(old, new);
    }

    #[test]
    fn play_session_id_debug_never_shows_the_full_id() {
        let p = PlaySessionId::new("0123456789abcdef");
        let dbg = format!("{p:?}");
        assert!(dbg.contains("01234567"), "{dbg}");
        assert!(!dbg.contains("89abcdef"), "{dbg}");
    }
}
