//! `SecretString` — structural V8 enforcement. Wrap tokens, passwords, API
//! keys in this newtype; its `Debug` and `Display` impls emit `<redacted>`,
//! so an accidental `tracing::info!(token = %tok)` cannot leak.
//!
//! Deliberate access via `expose()` only. Not `Serialize`/`Deserialize` —
//! callers must opt in explicitly with their own encoding.

#[derive(Clone, Default, PartialEq, Eq)]
pub struct SecretString(String);

impl SecretString {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// Read the underlying string. Use sparingly; the name is a flag for
    /// reviewers.
    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for SecretString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<redacted>")
    }
}

impl std::fmt::Display for SecretString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<redacted>")
    }
}

impl From<String> for SecretString {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for SecretString {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn display_is_redacted() {
        let s = SecretString::new("supersecret-token-xyz");
        assert_eq!(format!("{s}"), "<redacted>");
    }

    #[test]
    fn debug_is_redacted() {
        let s = SecretString::new("supersecret-token-xyz");
        assert_eq!(format!("{s:?}"), "<redacted>");
    }

    #[test]
    fn expose_returns_original() {
        let s = SecretString::new("v");
        assert_eq!(s.expose(), "v");
    }

    #[test]
    fn equality_works() {
        assert_eq!(SecretString::new("a"), SecretString::new("a"));
        assert_ne!(SecretString::new("a"), SecretString::new("b"));
    }

    #[test]
    fn nested_struct_debug_still_redacted() {
        #[derive(Debug)]
        #[allow(dead_code)]
        struct Login {
            user: String,
            token: SecretString,
        }
        let l = Login {
            user: "ali".into(),
            token: SecretString::new("xyz"),
        };
        let dbg = format!("{l:?}");
        assert!(dbg.contains("ali"));
        assert!(!dbg.contains("xyz"));
        assert!(dbg.contains("<redacted>"));
    }
}
