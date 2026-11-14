//! 045 — e-reader passwords: per-user credentials that open only the OPDS
//! catalogue (ADR-0128).
//!
//! A reader password is `<id>-<secret>`. The id is public and picks the row,
//! so a check costs one hash verify however many passwords a user holds; the
//! secret is stored only as the auth backend's password hash. Hashing lives
//! in the server adapter, as it does for account passwords.

use crate::{DomainResult, SecretString, UserId};

/// The public half of a reader password, which names its row. Lowercase
/// letters and digits from [`READER_PASSWORD_ALPHABET`], [`READER_PASSWORD_ID_LEN`] long.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ReaderPasswordId(String);

/// Characters a reader password is made of: lowercase letters and digits
/// without `0 1 i l o`, so a password copied by eye onto an e-reader keyboard
/// has no look-alikes.
pub const READER_PASSWORD_ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";
/// Characters in a password's id.
pub const READER_PASSWORD_ID_LEN: usize = 6;
/// Characters in a password's secret: 16 from a 31-letter alphabet is about
/// 79 bits.
pub const READER_PASSWORD_SECRET_LEN: usize = 16;

impl ReaderPasswordId {
    /// `s` as an id, or `None` when it is not one: the wrong length or a
    /// character outside the alphabet.
    pub fn parse(s: &str) -> Option<Self> {
        (s.len() == READER_PASSWORD_ID_LEN
            && s.bytes().all(|b| READER_PASSWORD_ALPHABET.contains(&b)))
        .then(|| Self(s.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ReaderPasswordId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A typed reader password split into its id and secret. Case, spaces and
/// dashes are ignored, so `ABCDEF-ghjk mnpq-rstu-vwxy` reads like the shown
/// form.
#[derive(Debug)]
pub struct TypedReaderPassword {
    pub id: ReaderPasswordId,
    pub secret: SecretString,
}

impl TypedReaderPassword {
    /// `typed` split into id and secret, or `None` when it cannot be a reader
    /// password at all.
    pub fn parse(typed: &str) -> Option<Self> {
        let folded: String = typed
            .chars()
            .filter(|c| !matches!(c, '-' | ' '))
            .flat_map(char::to_lowercase)
            .collect();
        if folded.len() != READER_PASSWORD_ID_LEN + READER_PASSWORD_SECRET_LEN
            || !folded
                .bytes()
                .all(|b| READER_PASSWORD_ALPHABET.contains(&b))
        {
            return None;
        }
        let (id, secret) = folded.split_at(READER_PASSWORD_ID_LEN);
        Some(Self {
            id: ReaderPasswordId::parse(id)?,
            secret: SecretString::new(secret.to_owned()),
        })
    }
}

/// The password as shown once to its owner: the id, then the secret in
/// groups of four, joined by dashes.
pub fn format_reader_password(id: &ReaderPasswordId, secret: &str) -> String {
    let mut out = id.as_str().to_owned();
    for (i, c) in secret.chars().enumerate() {
        if i % 4 == 0 {
            out.push('-');
        }
        out.push(c);
    }
    out
}

/// A reader password as its owner sees it listed: never the hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReaderPassword {
    pub id: ReaderPasswordId,
    pub label: String,
    /// Unix seconds.
    pub created_at: i64,
    /// Unix seconds of the last request it opened, to the minute or so.
    pub last_used_at: Option<i64>,
}

/// A stored reader password, hash included: what a check reads.
#[derive(Debug, Clone)]
pub struct ReaderPasswordRecord {
    pub id: ReaderPasswordId,
    pub user: UserId,
    pub label: String,
    /// The auth backend's hash of the secret half.
    pub hash: SecretString,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
    /// Unix seconds it was revoked; such a row opens nothing and is kept a
    /// while only so a refusal can say "revoked".
    pub revoked_at: Option<i64>,
}

pub trait ReaderPasswordStore: Send + Sync {
    /// Insert `record`. `Ok(false)` when its id is taken (the caller draws
    /// another); nothing is written then.
    fn create_reader_password(
        &self,
        record: ReaderPasswordRecord,
    ) -> impl std::future::Future<Output = DomainResult<bool>> + Send;

    /// `user`'s live reader passwords, newest first.
    fn reader_passwords_for(
        &self,
        user: UserId,
    ) -> impl std::future::Future<Output = DomainResult<Vec<ReaderPassword>>> + Send;

    /// The row with `id`, revoked or not.
    fn reader_password(
        &self,
        id: &ReaderPasswordId,
    ) -> impl std::future::Future<Output = DomainResult<Option<ReaderPasswordRecord>>> + Send;

    /// Revoke `user`'s live password `id` at `at`. `Ok(false)` when `user`
    /// holds no live password with that id.
    fn revoke_reader_password(
        &self,
        user: UserId,
        id: &ReaderPasswordId,
        at: i64,
    ) -> impl std::future::Future<Output = DomainResult<bool>> + Send;

    /// Record that `id` opened a request at `at`.
    fn touch_reader_password(
        &self,
        id: &ReaderPasswordId,
        at: i64,
    ) -> impl std::future::Future<Output = DomainResult<()>> + Send;

    /// Delete `user`'s passwords revoked before `before`; returns how many.
    fn prune_revoked_reader_passwords(
        &self,
        user: UserId,
        before: i64,
    ) -> impl std::future::Future<Output = DomainResult<u64>> + Send;
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_shown_password_parses_back_however_it_is_typed() {
        let id = ReaderPasswordId::parse("abcdef").unwrap();
        let shown = format_reader_password(&id, "ghjkmnpqrstuvwxy");
        assert_eq!(shown, "abcdef-ghjk-mnpq-rstu-vwxy");
        for typed in [
            shown.as_str(),
            "ABCDEF-GHJK-MNPQ-RSTU-VWXY",
            "abcdefghjkmnpqrstuvwxy",
            "abcdef ghjk mnpq rstu vwxy",
        ] {
            let parsed = TypedReaderPassword::parse(typed).unwrap();
            assert_eq!(parsed.id.as_str(), "abcdef", "{typed}");
            assert_eq!(parsed.secret.expose(), "ghjkmnpqrstuvwxy", "{typed}");
        }
    }

    #[test]
    fn anything_else_is_not_a_reader_password() {
        for typed in [
            "",
            "hunter2",
            "abcdef-ghjk-mnpq-rstu-vwx",
            "abcdef-ghjk-mnpq-rstu-vwxyz",
            // `0`, `1`, `l`, `o` and `i` are not in the alphabet.
            "abcde0-ghjk-mnpq-rstu-vwxy",
            "abcdef-ghjk-mnpq-rstu-vwx1",
            "abcdef-ghjk-mnpq-rstu-vwx\u{e9}",
        ] {
            assert!(TypedReaderPassword::parse(typed).is_none(), "{typed:?}");
        }
        assert!(ReaderPasswordId::parse("abcde").is_none());
        assert!(ReaderPasswordId::parse("../etc").is_none());
    }
}
