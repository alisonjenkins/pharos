//! 045 — e-reader passwords (ADR-0128): create, list and revoke them for
//! their owner, and check one presented over HTTP Basic auth on `/opds/`.
//!
//! A reader password opens the OPDS catalogue and nothing else: the only
//! caller of [`authenticate`] is the OPDS extractor, and no other route reads
//! a Basic `Authorization` header. The account password is never accepted
//! there, because a check only ever compares against a reader password's own
//! hash.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::Engine as _;
use pharos_core::reader_password::{
    READER_PASSWORD_ALPHABET, READER_PASSWORD_ID_LEN, READER_PASSWORD_SECRET_LEN,
};
use pharos_core::{
    format_reader_password, AuthError, ReaderPassword, ReaderPasswordId, ReaderPasswordRecord,
    ReaderPasswordStore, SecretString, TypedReaderPassword, User,
};
use sha2::{Digest, Sha256};

use crate::auth::{Denial, LoginFailure, LoginRefusal, NameMismatch};
use crate::services::auth::TokenRefusal;
use crate::state::AppState;

/// Live reader passwords one user may hold: one per device is the use, and
/// a bound keeps a signed-in session from filling the table.
pub const MAX_LIVE_PER_USER: usize = 10;
/// Longest label kept, in characters.
pub const MAX_LABEL_CHARS: usize = 64;
/// How long a revoked row is kept so a refusal can say "revoked"; older
/// ones are deleted when their owner next creates one.
const REVOKED_KEPT_SECS: i64 = 30 * 24 * 3600;
/// How long a verified password is trusted without another argon2 verify.
/// The row is still read on every request, so a revocation takes effect at
/// once; this bounds only how long a secret's digest stays in memory.
const VERIFIED_TTL: Duration = Duration::from_secs(15 * 60);
/// Verified entries kept; past this the oldest goes.
const VERIFIED_MAX: usize = 1024;
/// `last_used_at` is written at most this often per password: a page of
/// covers is one write, not fifty.
const TOUCH_EVERY: Duration = Duration::from_secs(5 * 60);
/// Draws of a fresh id before giving up on a collision.
const ID_DRAWS: usize = 5;
/// Longest `Authorization` header read; anything longer is malformed.
const MAX_BASIC_HEADER: usize = 1024;

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// `n` characters from [`READER_PASSWORD_ALPHABET`], uniformly: a byte is
/// kept only below the largest multiple of the alphabet's length.
fn random_chars(n: usize) -> Result<String, getrandom::Error> {
    let len = READER_PASSWORD_ALPHABET.len();
    let limit = 256 - 256 % len;
    let mut out = String::with_capacity(n);
    let mut buf = [0u8; 64];
    while out.len() < n {
        getrandom::fill(&mut buf)?;
        for b in buf {
            let b = usize::from(b);
            if b < limit && out.len() < n {
                if let Some(c) = READER_PASSWORD_ALPHABET.get(b % len) {
                    out.push(char::from(*c));
                }
            }
        }
    }
    Ok(out)
}

/// Why [`create`] made no password.
#[derive(Debug)]
pub enum CreateError {
    /// The label is empty, too long or holds a control character.
    Label(&'static str),
    /// The user already holds [`MAX_LIVE_PER_USER`] live passwords.
    Limit { held: usize },
    /// No random bytes.
    Rng(getrandom::Error),
    /// Hashing failed or the hash slots were busy.
    Hash(AuthError),
    /// The store failed.
    Store(pharos_core::DomainError),
    /// [`ID_DRAWS`] ids in a row were taken.
    NoFreeId,
}

impl CreateError {
    /// The `outcome` label of `pharos_reader_password_total` for this refusal.
    pub fn label(&self) -> &'static str {
        match self {
            CreateError::Label(_) => "invalid_label",
            CreateError::Limit { .. } => "limit",
            CreateError::Rng(_) | CreateError::Hash(_) | CreateError::NoFreeId => "failed",
            CreateError::Store(_) => "store_unavailable",
        }
    }
}

/// A new reader password: the listing entry, and the password itself, shown
/// once and never again.
#[derive(Debug)]
pub struct Created {
    pub listed: ReaderPassword,
    pub password: SecretString,
}

/// The label as stored, or why it cannot be.
pub fn clean_label(label: &str) -> Result<String, &'static str> {
    let label = label.trim();
    if label.is_empty() {
        return Err("a label is required");
    }
    if label.chars().count() > MAX_LABEL_CHARS {
        return Err("the label is longer than 64 characters");
    }
    if label.chars().any(char::is_control) {
        return Err("the label holds a control character");
    }
    Ok(label.to_owned())
}

fn record_action(action: &'static str, outcome: &'static str) {
    metrics::counter!("pharos_reader_password_total", "action" => action, "outcome" => outcome)
        .increment(1);
}

/// Make `user` a reader password labelled `label`. Its secret is hashed with
/// the account-password hasher and only the hash is stored.
pub async fn create(state: &AppState, user: &User, label: &str) -> Result<Created, CreateError> {
    let result = create_inner(state, user, label).await;
    match &result {
        Ok(created) => {
            record_action("create", "ok");
            tracing::info!(
                user = %user.name,
                reader_password = %created.listed.id,
                label = %created.listed.label,
                "reader password created"
            );
        }
        Err(e) => {
            record_action("create", e.label());
            tracing::warn!(user = %user.name, outcome = e.label(), error = ?e, "reader password not created");
        }
    }
    result
}

async fn create_inner(state: &AppState, user: &User, label: &str) -> Result<Created, CreateError> {
    let label = clean_label(label).map_err(CreateError::Label)?;
    let held = state
        .stores
        .reader_passwords_for(user.id)
        .await
        .map_err(CreateError::Store)?
        .len();
    if held >= MAX_LIVE_PER_USER {
        return Err(CreateError::Limit { held });
    }
    let now = unix_now();
    let pruned = state
        .stores
        .prune_revoked_reader_passwords(user.id, now.saturating_sub(REVOKED_KEPT_SECS))
        .await
        .map_err(CreateError::Store)?;
    if pruned > 0 {
        tracing::debug!(user = %user.name, pruned, "old revoked reader passwords deleted");
    }
    let secret =
        SecretString::new(random_chars(READER_PASSWORD_SECRET_LEN).map_err(CreateError::Rng)?);
    let hash = state
        .auth
        .hash_password_off_worker(&secret)
        .await
        .map_err(CreateError::Hash)?;
    for _ in 0..ID_DRAWS {
        let drawn = random_chars(READER_PASSWORD_ID_LEN).map_err(CreateError::Rng)?;
        let Some(id) = ReaderPasswordId::parse(&drawn) else {
            continue;
        };
        let record = ReaderPasswordRecord {
            id: id.clone(),
            user: user.id,
            label: label.clone(),
            hash: hash.clone(),
            created_at: now,
            last_used_at: None,
            revoked_at: None,
        };
        if state
            .stores
            .create_reader_password(record)
            .await
            .map_err(CreateError::Store)?
        {
            let password = SecretString::new(format_reader_password(&id, secret.expose()));
            return Ok(Created {
                listed: ReaderPassword {
                    id,
                    label,
                    created_at: now,
                    last_used_at: None,
                },
                password,
            });
        }
        tracing::debug!(reader_password = %id, "reader password id taken; drawing another");
    }
    Err(CreateError::NoFreeId)
}

/// `user`'s live reader passwords, newest first.
pub async fn list(state: &AppState, user: &User) -> pharos_core::DomainResult<Vec<ReaderPassword>> {
    state.stores.reader_passwords_for(user.id).await
}

/// Revoke `user`'s reader password `id`. `Ok(false)` when they hold no live
/// one by that id. Takes effect on the next request: every check reads the
/// row.
pub async fn revoke(
    state: &AppState,
    user: &User,
    id: &ReaderPasswordId,
) -> pharos_core::DomainResult<bool> {
    let result = state
        .stores
        .revoke_reader_password(user.id, id, unix_now())
        .await;
    state.reader_passwords.forget(id);
    let outcome = match &result {
        Ok(true) => "ok",
        Ok(false) => "not_found",
        Err(_) => "store_unavailable",
    };
    record_action("revoke", outcome);
    match &result {
        Ok(_) => {
            tracing::info!(user = %user.name, reader_password = %id, outcome, "reader password revoke")
        }
        Err(e) => {
            tracing::warn!(user = %user.name, reader_password = %id, error = %e, "reader password revoke failed")
        }
    }
    result
}

/// How one OPDS credential check ended. The `outcome` label of
/// `pharos_opds_auth_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthOutcome {
    Ok,
    /// No `Authorization: Basic` header.
    Missing,
    /// A Basic header that does not decode to `name:password`.
    Malformed,
    UnknownUser,
    /// Not a reader password of this user: wrong, malformed, someone
    /// else's, or the account password.
    BadPassword,
    /// The user's reader password, revoked.
    Revoked,
    /// The account is disabled.
    Disabled,
    /// The user's access schedule is closed now (V578).
    OutsideSchedule,
    /// Refused by the throttle before any check (V174).
    Locked,
    /// The store or the hash slots failed; nothing was judged.
    Unavailable,
}

impl AuthOutcome {
    pub const ALL: [Self; 10] = [
        Self::Ok,
        Self::Missing,
        Self::Malformed,
        Self::UnknownUser,
        Self::BadPassword,
        Self::Revoked,
        Self::Disabled,
        Self::OutsideSchedule,
        Self::Locked,
        Self::Unavailable,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Missing => "missing",
            Self::Malformed => "malformed",
            Self::UnknownUser => "unknown_user",
            Self::BadPassword => "bad_password",
            Self::Revoked => "revoked",
            Self::Disabled => "disabled",
            Self::OutsideSchedule => "outside_schedule",
            Self::Locked => "locked",
            Self::Unavailable => "unavailable",
        }
    }
}

/// A refused OPDS credential: the outcome, and for a throttled one how long
/// to wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refused {
    pub outcome: AuthOutcome,
    pub retry_after_secs: Option<u64>,
}

/// `name` and password from an `Authorization` header value, or why not.
fn parse_basic(header: Option<&str>) -> Result<(String, SecretString), AuthOutcome> {
    let Some(value) = header else {
        return Err(AuthOutcome::Missing);
    };
    if value.len() > MAX_BASIC_HEADER {
        return Err(AuthOutcome::Malformed);
    }
    let Some((scheme, b64)) = value.trim().split_once(' ') else {
        return Err(AuthOutcome::Missing);
    };
    if !scheme.eq_ignore_ascii_case("basic") {
        // A bearer or Jellyfin header is not a reader credential.
        return Err(AuthOutcome::Missing);
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .map_err(|_| AuthOutcome::Malformed)?;
    let decoded = String::from_utf8(decoded).map_err(|_| AuthOutcome::Malformed)?;
    let (name, password) = decoded.split_once(':').ok_or(AuthOutcome::Malformed)?;
    if name.trim().is_empty() {
        return Err(AuthOutcome::Malformed);
    }
    Ok((name.to_owned(), SecretString::new(password.to_owned())))
}

/// The typed name as logged: at most 64 characters, so a pasted secret in
/// the name field cannot fill a log line.
fn loggable_name(name: &str) -> String {
    name.trim().chars().take(64).collect()
}

/// Check the Basic credential in `authorization`: the user, and the reader
/// password that opened the request. Every check, refused or not, counts one
/// `pharos_opds_auth_total{outcome}` and logs the typed name and outcome,
/// never the password.
pub async fn authenticate(
    state: &AppState,
    authorization: Option<&str>,
    client_ip: IpAddr,
) -> Result<(User, ReaderPasswordId), Refused> {
    let (name, password) = match parse_basic(authorization) {
        Ok(parsed) => parsed,
        Err(outcome) => {
            record_auth(outcome);
            // Every anonymous crawler sends no credential: the counter keeps
            // the rate, the log stays quiet.
            tracing::debug!(outcome = outcome.label(), client_ip = %client_ip, "opds credential refused");
            return Err(Refused {
                outcome,
                retry_after_secs: None,
            });
        }
    };
    let mut outcome = AuthOutcome::Unavailable;
    let verdict = state
        .reader_throttle
        .verify_with(
            &name,
            client_ip,
            judge(state, &name, &password, &mut outcome),
        )
        .await;
    let result = match verdict {
        Ok(found) => found.map_err(|refusal| Refused {
            outcome: refusal,
            retry_after_secs: None,
        }),
        Err(LoginRefusal::Throttled(retry)) => Err(Refused {
            outcome: AuthOutcome::Locked,
            retry_after_secs: Some(retry.secs),
        }),
        Err(LoginRefusal::Invalid | LoginRefusal::LockedOut(_) | LoginRefusal::Backend(_)) => {
            Err(Refused {
                outcome,
                retry_after_secs: None,
            })
        }
    };
    let final_outcome = match &result {
        Ok(_) => AuthOutcome::Ok,
        Err(r) => r.outcome,
    };
    record_auth(final_outcome);
    match &result {
        Ok((user, id)) => tracing::debug!(
            user = %user.name,
            reader_password = %id,
            outcome = final_outcome.label(),
            client_ip = %client_ip,
            "opds credential accepted"
        ),
        Err(_) => tracing::warn!(
            user = %loggable_name(&name),
            outcome = final_outcome.label(),
            client_ip = %client_ip,
            "opds credential refused"
        ),
    }
    result
}

fn record_auth(outcome: AuthOutcome) {
    metrics::counter!("pharos_opds_auth_total", "outcome" => outcome.label()).increment(1);
}

fn refused(reason: LoginFailure) -> Denial {
    Denial::Refused {
        reason,
        name_mismatch: NameMismatch::None,
    }
}

/// The throttle's judge. A password that was right settles the attempt as
/// a success even when the account is then refused for the hour (`Ok(Err)`),
/// as a sign-in does. `outcome` names a refusal finer than the throttle's
/// classes.
async fn judge(
    state: &AppState,
    name: &str,
    password: &SecretString,
    outcome: &mut AuthOutcome,
) -> Result<Result<(User, ReaderPasswordId), AuthOutcome>, Denial> {
    let account = state
        .auth
        .account_named(name)
        .await
        .map_err(Denial::Backend)?;
    let record = match account {
        Ok(record) => record,
        Err(reason) => {
            state
                .auth
                .verify_nothing(password)
                .await
                .map_err(Denial::Backend)?;
            *outcome = AuthOutcome::UnknownUser;
            return Err(refused(reason));
        }
    };
    let typed = TypedReaderPassword::parse(password.expose());
    let row = match &typed {
        Some(typed) => state
            .stores
            .reader_password(&typed.id)
            .await
            .map_err(|e| Denial::Backend(AuthError::Backend(e.to_string())))?
            .filter(|row| row.user == record.id),
        None => None,
    };
    let (Some(typed), Some(row)) = (typed, row) else {
        state
            .auth
            .verify_nothing(password)
            .await
            .map_err(Denial::Backend)?;
        *outcome = AuthOutcome::BadPassword;
        return Err(refused(LoginFailure::BadPassword));
    };
    let now = Instant::now();
    let cached = state.reader_passwords.check(&typed, now);
    let matched = match cached {
        Some(_) => true,
        None => state
            .auth
            .verify_secret(row.hash.expose(), &typed.secret)
            .await
            .map_err(Denial::Backend)?,
    };
    if !matched {
        *outcome = AuthOutcome::BadPassword;
        return Err(refused(LoginFailure::BadPassword));
    }
    if row.revoked_at.is_some() {
        state.reader_passwords.forget(&typed.id);
        *outcome = AuthOutcome::Revoked;
        return Err(refused(LoginFailure::BadPassword));
    }
    // The same per-request policy a token gets: disabled refuses, a closed
    // access schedule refuses non-admins (V578).
    let user = match crate::services::auth::live_user(&state.stores, record.id).await {
        Ok(user) => user,
        Err(TokenRefusal::OutsideSchedule) => {
            return Ok(Err(AuthOutcome::OutsideSchedule));
        }
        Err(TokenRefusal::Unavailable(detail)) => {
            return Err(Denial::Backend(AuthError::Backend(detail)));
        }
        Err(_) => {
            *outcome = AuthOutcome::Disabled;
            return Err(refused(LoginFailure::Disabled));
        }
    };
    let touch = match cached {
        Some(touch) => touch,
        None => {
            state.reader_passwords.remember(&typed, now);
            true
        }
    };
    if touch {
        if let Err(e) = state
            .stores
            .touch_reader_password(&typed.id, unix_now())
            .await
        {
            tracing::warn!(reader_password = %typed.id, error = %e, "reader password last-used not recorded");
        }
    }
    Ok(Ok((user, typed.id)))
}

/// Reader passwords verified lately, by a keyed digest of the secret: a hit
/// skips the argon2 verify, never the row read. The key is drawn at start,
/// so the digests mean nothing outside this process.
pub struct VerifiedCache {
    key: [u8; 32],
    entries: Mutex<HashMap<ReaderPasswordId, Verified>>,
}

struct Verified {
    digest: [u8; 32],
    at: Instant,
    touched: Instant,
}

impl std::fmt::Debug for VerifiedCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VerifiedCache").finish_non_exhaustive()
    }
}

impl Default for VerifiedCache {
    fn default() -> Self {
        Self::new()
    }
}

impl VerifiedCache {
    pub fn new() -> Self {
        let mut key = [0u8; 32];
        if let Err(e) = getrandom::fill(&mut key) {
            // A fixed key still keys a digest no one outside sees; the cache
            // only ever compares digests it made itself.
            tracing::error!(error = %e, "reader password cache: no random key; using a fixed one");
        }
        Self {
            key,
            entries: Mutex::new(HashMap::new()),
        }
    }

    fn digest(&self, typed: &TypedReaderPassword) -> [u8; 32] {
        let mut h = Sha256::new();
        h.update(self.key);
        h.update(typed.id.as_str().as_bytes());
        h.update([0u8]);
        h.update(typed.secret.expose().as_bytes());
        h.finalize().into()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<ReaderPasswordId, Verified>> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// `Some(touch)` when `typed` was verified within the TTL, `touch` saying
    /// whether its last use is due to be written.
    fn check(&self, typed: &TypedReaderPassword, now: Instant) -> Option<bool> {
        let digest = self.digest(typed);
        let mut entries = self.lock();
        let entry = entries.get_mut(&typed.id)?;
        if now.saturating_duration_since(entry.at) >= VERIFIED_TTL {
            entries.remove(&typed.id);
            return None;
        }
        // Constant-time: the comparison does not stop at the first mismatch.
        let same = entry
            .digest
            .iter()
            .zip(digest.iter())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0;
        if !same {
            return None;
        }
        let touch = now.saturating_duration_since(entry.touched) >= TOUCH_EVERY;
        if touch {
            entry.touched = now;
        }
        Some(touch)
    }

    fn remember(&self, typed: &TypedReaderPassword, now: Instant) {
        let digest = self.digest(typed);
        let mut entries = self.lock();
        if entries.len() >= VERIFIED_MAX && !entries.contains_key(&typed.id) {
            entries.retain(|_, e| now.saturating_duration_since(e.at) < VERIFIED_TTL);
            if entries.len() >= VERIFIED_MAX {
                let oldest = entries
                    .iter()
                    .min_by_key(|(_, e)| e.at)
                    .map(|(k, _)| k.clone());
                if let Some(k) = oldest {
                    entries.remove(&k);
                }
            }
        }
        entries.insert(
            typed.id.clone(),
            Verified {
                digest,
                at: now,
                touched: now,
            },
        );
    }

    /// Drop `id`, so its next use verifies again.
    pub fn forget(&self, id: &ReaderPasswordId) {
        self.lock().remove(id);
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.lock().len()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn basic(raw: &str) -> String {
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(raw)
        )
    }

    #[test]
    fn a_basic_header_gives_its_name_and_password() {
        let (name, pw) = parse_basic(Some(&basic("ali:abc:def"))).unwrap();
        assert_eq!(name, "ali");
        assert_eq!(pw.expose(), "abc:def", "only the first colon splits");
        let (name, _) = parse_basic(Some(&format!(
            "basic {}",
            base64::engine::general_purpose::STANDARD.encode("ali:x")
        )))
        .unwrap();
        assert_eq!(name, "ali", "the scheme is not case sensitive");
    }

    #[test]
    fn other_headers_are_missing_or_malformed() {
        assert_eq!(parse_basic(None).unwrap_err(), AuthOutcome::Missing);
        assert_eq!(
            parse_basic(Some("MediaBrowser Token=\"abc\"")).unwrap_err(),
            AuthOutcome::Missing
        );
        assert_eq!(
            parse_basic(Some("Bearer abc")).unwrap_err(),
            AuthOutcome::Missing
        );
        assert_eq!(
            parse_basic(Some("Basic !!!")).unwrap_err(),
            AuthOutcome::Malformed
        );
        assert_eq!(
            parse_basic(Some(&basic("no-colon"))).unwrap_err(),
            AuthOutcome::Malformed
        );
        assert_eq!(
            parse_basic(Some(&basic(":pw"))).unwrap_err(),
            AuthOutcome::Malformed
        );
        let long = format!("Basic {}", "A".repeat(MAX_BASIC_HEADER));
        assert_eq!(
            parse_basic(Some(&long)).unwrap_err(),
            AuthOutcome::Malformed
        );
    }

    #[test]
    fn auth_outcome_labels_are_distinct() {
        let labels: std::collections::HashSet<_> =
            AuthOutcome::ALL.iter().map(|o| o.label()).collect();
        assert_eq!(labels.len(), AuthOutcome::ALL.len());
        for want in ["ok", "bad_password", "unknown_user", "revoked", "locked"] {
            assert!(labels.contains(want), "{want}");
        }
    }

    #[test]
    fn random_passwords_use_only_the_alphabet() {
        let s = random_chars(500).unwrap();
        assert_eq!(s.len(), 500);
        assert!(s.bytes().all(|b| READER_PASSWORD_ALPHABET.contains(&b)));
        assert_ne!(random_chars(16).unwrap(), random_chars(16).unwrap());
    }

    #[test]
    fn labels_are_trimmed_and_bounded() {
        assert_eq!(clean_label("  Kobo  ").unwrap(), "Kobo");
        assert!(clean_label("   ").is_err());
        assert!(clean_label(&"x".repeat(65)).is_err());
        assert!(
            clean_label(&"é".repeat(64)).is_ok(),
            "characters, not bytes"
        );
        assert!(clean_label("Ko\nbo").is_err());
    }

    fn typed(s: &str) -> TypedReaderPassword {
        TypedReaderPassword::parse(s).unwrap()
    }

    #[test]
    fn the_cache_hits_only_the_same_secret_within_its_ttl() {
        let cache = VerifiedCache::new();
        let t0 = Instant::now();
        let right = typed("abcdef-ghjk-mnpq-rstu-vwxy");
        let wrong = typed("abcdef-ghjk-mnpq-rstu-vwxz");
        assert_eq!(cache.check(&right, t0), None);
        cache.remember(&right, t0);
        assert_eq!(cache.check(&right, t0), Some(false), "just touched");
        assert_eq!(
            cache.check(&wrong, t0),
            None,
            "another secret for the id misses"
        );
        assert_eq!(cache.check(&right, t0 + TOUCH_EVERY), Some(true));
        assert_eq!(
            cache.check(&right, t0 + TOUCH_EVERY),
            Some(false),
            "one touch per interval"
        );
        assert_eq!(cache.check(&right, t0 + VERIFIED_TTL), None, "expired");
        cache.remember(&right, t0);
        cache.forget(&right.id);
        assert_eq!(cache.check(&right, t0), None, "forgotten on revoke");
    }

    #[test]
    fn the_cache_is_bounded() {
        let cache = VerifiedCache::new();
        let t0 = Instant::now();
        for i in 0..(VERIFIED_MAX + 10) {
            let id = random_chars(READER_PASSWORD_ID_LEN).unwrap();
            let p = typed(&format!("{id}{}", "a".repeat(16)));
            cache.remember(&p, t0 + Duration::from_millis(i as u64));
        }
        assert!(cache.len() <= VERIFIED_MAX);
    }
}
