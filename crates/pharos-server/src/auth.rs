//! `BuiltinAuth` — verifies passwords against an Argon2id hash stored in
//! a `UserStore`. The store sees the *hash*, not the plaintext, so the
//! domain layer never needs `argon2` as a dependency (V12).

use crate::config::LoginThrottleConfig;
use argon2::{
    password_hash::{phc::PasswordHash, PasswordHasher, PasswordVerifier},
    Argon2,
};
use pharos_core::{
    AuthBackend, AuthError, AuthResult, SecretString, User, UserId, UserPolicy, UserRecord,
    UserStore,
};

use std::collections::HashMap;
use std::net::{IpAddr, Ipv6Addr};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Result of [`BuiltinAuth::create_user`] — distinguishes a fresh create
/// from an idempotent no-op so the caller can report accurately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreateUserOutcome {
    Created,
    AlreadyExists,
}

pub struct BuiltinAuth<U: UserStore> {
    users: U,
    argon: Argon2<'static>,
}

impl<U: UserStore> BuiltinAuth<U> {
    pub fn new(users: U) -> Self {
        Self {
            users,
            argon: Argon2::default(),
        }
    }

    pub fn users(&self) -> &U {
        &self.users
    }

    /// Hash a plaintext password with a fresh random salt. Use this to
    /// build a `UserRecord` before calling `UserStore::create`.
    pub fn hash_password(&self, password: &SecretString) -> AuthResult<SecretString> {
        // password-hash 0.6's `hash_password` generates its own fresh random
        // salt of the recommended length via the `getrandom` feature — no
        // reason to hand-roll that ourselves as the pre-0.6 API required.
        let hash = self
            .argon
            .hash_password(password.expose().as_bytes())
            .map_err(|e| AuthError::Backend(format!("hash: {e}")))?
            .to_string();
        Ok(SecretString::new(hash))
    }

    /// Create a user with the given plaintext password, hashing it first.
    /// Idempotent: a name collision yields [`CreateUserOutcome::AlreadyExists`]
    /// rather than an error, so re-running a bootstrap command is safe.
    /// This is the supported way to bootstrap the first admin
    /// (`pharos admin create-user --name … --password … --admin`).
    pub async fn create_user(
        &self,
        name: &str,
        password: &SecretString,
        admin: bool,
    ) -> AuthResult<CreateUserOutcome> {
        let hash = self.hash_password(password)?;
        let record = UserRecord {
            id: UserId::new(),
            name: name.to_string(),
            password_hash: hash,
            policy: UserPolicy {
                admin,
                ..Default::default()
            },
        };
        match self.users.create(record).await {
            Ok(()) => Ok(CreateUserOutcome::Created),
            Err(AuthError::Conflict) => Ok(CreateUserOutcome::AlreadyExists),
            Err(e) => Err(e),
        }
    }
}

impl<U: UserStore> AuthBackend for BuiltinAuth<U> {
    #[tracing::instrument(skip(self, password), fields(user.name = %name))]
    async fn authenticate(&self, name: &str, password: &SecretString) -> AuthResult<User> {
        let record = match self.users.lookup_by_name(name).await {
            Ok(r) => r,
            Err(AuthError::UserNotFound) => return Err(AuthError::InvalidCredentials),
            Err(other) => return Err(other),
        };
        let parsed = PasswordHash::new(record.password_hash.expose())
            .map_err(|e| AuthError::Backend(format!("parse hash: {e}")))?;
        self.argon
            .verify_password(password.expose().as_bytes(), &parsed)
            .map_err(|_| AuthError::InvalidCredentials)?;
        // T68 — a disabled user must not authenticate even with valid
        // credentials. Checked after the password verify so a disabled account
        // is indistinguishable from a wrong password (no account-state
        // enumeration): both yield InvalidCredentials → 401.
        if record.policy.is_disabled {
            return Err(AuthError::InvalidCredentials);
        }
        Ok(record.into_user())
    }
}

/// Which counter refused a login attempt. The label is a dashboard contract
/// (`pharos_login_throttled_total{reason}`); keep the strings stable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThrottleReason {
    User,
    Ip,
}

impl ThrottleReason {
    pub fn label(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Ip => "ip",
        }
    }
}

/// A refused login attempt: how long the caller must wait, and which counter
/// said so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryAfter {
    pub reason: ThrottleReason,
    /// Whole seconds, rounded up, never zero.
    pub secs: u64,
}

/// Why [`LoginThrottle::verify`] returned no user.
#[derive(Debug)]
pub enum LoginRefusal {
    /// Refused before any hash: answer 429 with `Retry-After`.
    Throttled(RetryAfter),
    /// Wrong password or no such user; counted against both keys.
    Invalid,
    /// The user store failed; not counted.
    Backend(AuthError),
}

/// Running failure counts after an attempt was admitted, for logging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FailureCounts {
    pub user: u32,
    pub ip: u32,
}

/// Longest username kept as a key; anything longer is truncated so a hostile
/// client cannot make each entry arbitrarily large.
const MAX_KEY_CHARS: usize = 128;

/// Entries kept per table (usernames, addresses). Past this, expired entries
/// go first, then the stalest, so spraying distinct usernames cannot grow the
/// tables without bound.
const MAX_ENTRIES: usize = 10_000;

#[derive(Debug, Clone, Copy)]
struct Entry {
    failures: u32,
    last_failure: Instant,
}

/// Shared per-username and per-address login throttle (V174).
///
/// Every code path that verifies a password calls [`begin`](Self::begin)
/// before the (expensive) hash and settles the returned [`Attempt`] after it.
/// `begin` checks and reserves under one lock, counting the attempt as a
/// failure up front, so concurrent guesses cannot all pass the check while the
/// first hash is still running. Time is a parameter so tests are
/// deterministic. Accounts are never locked; a throttled key is told to retry
/// later and its delay doubles with each further failure. A success clears the
/// username's count but only takes back its own reservation on the address, so
/// an attacker with a valid account cannot reset the address's budget.
///
/// The per-address limit applies only when the server knows its proxies
/// ([`with_per_ip`](Self::with_per_ip)): behind an ingress that is not in
/// `trusted_proxies`, every client shares the ingress's address, and one
/// typo-prone TV would 429 the whole household.
#[derive(Debug)]
pub struct LoginThrottle {
    policy: LoginThrottleConfig,
    per_ip: bool,
    tables: Mutex<Tables>,
}

#[derive(Debug, Default)]
struct Tables {
    users: HashMap<String, Entry>,
    ips: HashMap<IpAddr, Entry>,
}

impl Default for LoginThrottle {
    fn default() -> Self {
        Self::new(LoginThrottleConfig::default())
    }
}

fn user_key(user: &str) -> String {
    user.chars()
        .take(MAX_KEY_CHARS)
        .flat_map(char::to_lowercase)
        .collect()
}

/// IPv6 clients usually hold a whole /64, so key on that prefix; otherwise one
/// host could rotate through 2^64 addresses to dodge the limit.
fn ip_key(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(_) => ip,
        IpAddr::V6(v6) => {
            let mut o = v6.octets();
            o[8..].fill(0);
            IpAddr::V6(Ipv6Addr::from(o))
        }
    }
}

impl LoginThrottle {
    pub fn new(policy: LoginThrottleConfig) -> Self {
        Self {
            policy,
            per_ip: true,
            tables: Mutex::new(Tables::default()),
        }
    }

    /// Whether the per-address limit is enforced. Failures are still counted
    /// per address either way, so the "login failed" line keeps both counts.
    #[must_use]
    pub fn with_per_ip(mut self, per_ip: bool) -> Self {
        self.per_ip = per_ip;
        self
    }

    pub fn per_ip(&self) -> bool {
        self.per_ip
    }

    /// Logs and publishes `pharos_login_throttle_per_ip_active` (0 or 1);
    /// call once at startup. Inactive is a WARN naming why, so an operator
    /// finds it by query rather than by a household's failed logins.
    pub fn record_state(&self) {
        metrics::gauge!("pharos_login_throttle_per_ip_active")
            .set(f64::from(u8::from(self.per_ip)));
        if self.per_ip {
            tracing::info!(
                max_failures_per_user = self.policy.max_failures_per_user,
                max_failures_per_ip = self.policy.max_failures_per_ip,
                "login throttle: per-user and per-address limits active"
            );
        } else {
            tracing::warn!(
                max_failures_per_user = self.policy.max_failures_per_user,
                reason = "trusted_proxies is empty",
                "login throttle: per-address limit INACTIVE; only the per-user limit applies. \
                 Set [server].trusted_proxies to the ingress CIDR to enable it"
            );
        }
    }

    fn window(&self) -> Duration {
        Duration::from_secs(self.policy.window_secs)
    }

    /// Seconds a key with `failures` failures must wait after its last one;
    /// zero below the threshold.
    fn delay(&self, failures: u32, threshold: u32) -> Duration {
        if failures < threshold.max(1) {
            return Duration::ZERO;
        }
        // 1 s at the threshold, doubling per further failure; the exponent is
        // clamped so the shift cannot overflow before the cap applies.
        let exp = (failures - threshold.max(1)).min(20);
        Duration::from_secs((1_u64 << exp).min(self.policy.max_backoff_secs))
    }

    fn remaining(&self, e: Option<&Entry>, threshold: u32, now: Instant) -> Duration {
        let Some(e) = e else {
            return Duration::ZERO;
        };
        let since = now.saturating_duration_since(e.last_failure);
        if since >= self.window() {
            return Duration::ZERO;
        }
        self.delay(e.failures, threshold).saturating_sub(since)
    }

    /// Admit or refuse one password attempt for `user` from `ip`, atomically.
    ///
    /// Refused: returns how long to wait, records
    /// `pharos_login_throttled_total{reason}` and WARNs with the reason,
    /// username, address and wait. Admitted: the attempt is counted as a
    /// failure immediately; call [`Attempt::succeeded`] if the password was
    /// right, or [`Attempt::abandon`] if it was never judged (backend error).
    /// Dropping the attempt otherwise leaves it counted and logs
    /// "login failed" with both running counts.
    ///
    /// Private: callers go through [`verify`](Self::verify), so no path can
    /// check a password without the throttle (V174).
    fn begin(&self, user: &str, ip: IpAddr, now: Instant) -> Result<Attempt<'_>, RetryAfter> {
        let uk = user_key(user);
        let ik = ip_key(ip);
        let window = self.window();
        let admitted = {
            let mut t = self.lock();
            let by_user = self.remaining(t.users.get(&uk), self.policy.max_failures_per_user, now);
            let by_ip = if self.per_ip {
                self.remaining(t.ips.get(&ik), self.policy.max_failures_per_ip, now)
            } else {
                Duration::ZERO
            };
            let (reason, wait) = if by_ip > by_user {
                (ThrottleReason::Ip, by_ip)
            } else {
                (ThrottleReason::User, by_user)
            };
            if wait.is_zero() {
                let user = bump(&mut t.users, uk.clone(), now, window);
                let ip = bump(&mut t.ips, ik, now, window);
                Ok(FailureCounts { user, ip })
            } else {
                Err((reason, wait))
            }
        };
        match admitted {
            Ok(counts) => Ok(Attempt {
                throttle: self,
                user_key: uk,
                ip_key: ik,
                client_ip: ip,
                counts,
                settled: false,
            }),
            Err((reason, wait)) => {
                let secs = wait.as_secs() + u64::from(wait.subsec_nanos() > 0);
                metrics::counter!("pharos_login_throttled_total", "reason" => reason.label())
                    .increment(1);
                tracing::warn!(
                    reason = reason.label(),
                    user = %uk,
                    client_ip = %ip,
                    retry_after_secs = secs,
                    "login throttled: too many failed attempts"
                );
                Err(RetryAfter { reason, secs })
            }
        }
    }

    /// The one way to check a password (V174): admit the attempt, verify it
    /// with `auth`, and settle it. A wrong password or unknown user stays
    /// counted; a store failure is given back, because the password was never
    /// judged.
    pub async fn verify<A: AuthBackend>(
        &self,
        auth: &A,
        user: &str,
        password: &SecretString,
        ip: IpAddr,
    ) -> Result<User, LoginRefusal> {
        let attempt = self
            .begin(user, ip, Instant::now())
            .map_err(LoginRefusal::Throttled)?;
        match auth.authenticate(user, password).await {
            Ok(u) => {
                attempt.succeeded();
                Ok(u)
            }
            Err(AuthError::InvalidCredentials | AuthError::UserNotFound) => {
                // Dropped unsettled: stays counted as a failed login.
                drop(attempt);
                Err(LoginRefusal::Invalid)
            }
            Err(e) => {
                attempt.abandon();
                Err(LoginRefusal::Backend(e))
            }
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Tables> {
        // A panic while holding the lock leaves only counters behind; they
        // are still safe to use.
        self.tables
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[cfg(test)]
    fn sizes(&self) -> (usize, usize) {
        let t = self.lock();
        (t.users.len(), t.ips.len())
    }
}

/// Take back one counted failure, forgetting the key once it reaches zero.
fn release<K: std::hash::Hash + Eq>(table: &mut HashMap<K, Entry>, key: &K) {
    if let Some(e) = table.get_mut(key) {
        e.failures = e.failures.saturating_sub(1);
        if e.failures == 0 {
            table.remove(key);
        }
    }
}

/// One admitted password attempt, counted as a failure until settled.
#[derive(Debug)]
#[must_use = "an unsettled attempt stays counted as a failed login"]
pub struct Attempt<'a> {
    throttle: &'a LoginThrottle,
    user_key: String,
    ip_key: IpAddr,
    client_ip: IpAddr,
    counts: FailureCounts,
    settled: bool,
}

impl Attempt<'_> {
    /// Running counts including this attempt.
    pub fn counts(&self) -> FailureCounts {
        self.counts
    }

    /// The password was right: clear the username's failures and give back
    /// this attempt's own count on the address (earlier failures from the
    /// address stay, so a valid account cannot launder guesses at others).
    pub fn succeeded(mut self) {
        self.settled = true;
        let mut t = self.throttle.lock();
        t.users.remove(&self.user_key);
        release(&mut t.ips, &self.ip_key);
    }

    /// The password was never judged (e.g. the user store failed): take the
    /// reservation back instead of counting a failure.
    pub fn abandon(mut self) {
        self.settled = true;
        let mut t = self.throttle.lock();
        release(&mut t.users, &self.user_key);
        release(&mut t.ips, &self.ip_key);
    }
}

impl Drop for Attempt<'_> {
    fn drop(&mut self) {
        if !self.settled {
            tracing::info!(
                user = %self.user_key,
                client_ip = %self.client_ip,
                user_failures = self.counts.user,
                ip_failures = self.counts.ip,
                "login failed"
            );
        }
    }
}

fn bump<K: std::hash::Hash + Eq + Clone>(
    table: &mut HashMap<K, Entry>,
    key: K,
    now: Instant,
    window: Duration,
) -> u32 {
    if table.len() >= MAX_ENTRIES && !table.contains_key(&key) {
        table.retain(|_, e| now.saturating_duration_since(e.last_failure) < window);
        if table.len() >= MAX_ENTRIES {
            let stalest = table
                .iter()
                .min_by_key(|(_, e)| e.last_failure)
                .map(|(k, _)| k.clone());
            if let Some(k) = stalest {
                table.remove(&k);
            }
        }
    }
    let e = table.entry(key).or_insert(Entry {
        failures: 0,
        last_failure: now,
    });
    if now.saturating_duration_since(e.last_failure) >= window {
        e.failures = 0;
    }
    e.failures = e.failures.saturating_add(1);
    e.last_failure = now;
    e.failures
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use pharos_core::{UserId, UserPolicy, UserRecord};
    use std::collections::HashMap;
    use tokio::sync::Mutex;

    #[derive(Default)]
    struct MemUsers {
        by_name: Mutex<HashMap<String, UserRecord>>,
    }

    impl UserStore for MemUsers {
        async fn create(&self, record: UserRecord) -> AuthResult<()> {
            let mut g = self.by_name.lock().await;
            if g.contains_key(&record.name) {
                return Err(AuthError::Conflict);
            }
            g.insert(record.name.clone(), record);
            Ok(())
        }
        async fn lookup_by_name(&self, name: &str) -> AuthResult<UserRecord> {
            self.by_name
                .lock()
                .await
                .get(name)
                .cloned()
                .ok_or(AuthError::UserNotFound)
        }
        async fn get(&self, id: UserId) -> AuthResult<UserRecord> {
            self.by_name
                .lock()
                .await
                .values()
                .find(|r| r.id == id)
                .cloned()
                .ok_or(AuthError::UserNotFound)
        }
        async fn list(&self) -> AuthResult<Vec<UserRecord>> {
            let g = self.by_name.lock().await;
            let mut v: Vec<UserRecord> = g.values().cloned().collect();
            v.sort_by_key(|a| a.name.to_lowercase());
            Ok(v)
        }
        async fn delete(&self, id: UserId) -> AuthResult<()> {
            let mut g = self.by_name.lock().await;
            let Some(key) = g.iter().find(|(_, r)| r.id == id).map(|(k, _)| k.clone()) else {
                return Err(AuthError::UserNotFound);
            };
            g.remove(&key);
            Ok(())
        }
        async fn set_policy(&self, id: UserId, policy: UserPolicy) -> AuthResult<()> {
            let mut g = self.by_name.lock().await;
            for rec in g.values_mut() {
                if rec.id == id {
                    rec.policy = policy;
                    return Ok(());
                }
            }
            Err(AuthError::UserNotFound)
        }
        async fn set_password(&self, id: UserId, password_hash: SecretString) -> AuthResult<()> {
            let mut g = self.by_name.lock().await;
            for rec in g.values_mut() {
                if rec.id == id {
                    rec.password_hash = password_hash;
                    return Ok(());
                }
            }
            Err(AuthError::UserNotFound)
        }
    }

    async fn fresh() -> BuiltinAuth<MemUsers> {
        BuiltinAuth::new(MemUsers::default())
    }

    async fn seed(auth: &BuiltinAuth<MemUsers>, name: &str, pw: &str) -> UserRecord {
        let hash = auth.hash_password(&SecretString::new(pw)).unwrap();
        let rec = UserRecord {
            id: UserId::new(),
            name: name.into(),
            password_hash: hash,
            policy: UserPolicy::default(),
        };
        auth.users().create(rec.clone()).await.unwrap();
        rec
    }

    #[tokio::test]
    async fn authenticate_happy_path() {
        let auth = fresh().await;
        let rec = seed(&auth, "ali", "hunter2").await;
        let user = auth
            .authenticate("ali", &SecretString::new("hunter2"))
            .await
            .unwrap();
        assert_eq!(user.id, rec.id);
        assert_eq!(user.name, "ali");
    }

    #[tokio::test]
    async fn wrong_password_is_invalid_credentials() {
        let auth = fresh().await;
        seed(&auth, "ali", "hunter2").await;
        match auth.authenticate("ali", &SecretString::new("wrong")).await {
            Err(AuthError::InvalidCredentials) => {}
            other => panic!("expected InvalidCredentials, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unknown_user_is_invalid_credentials_not_user_not_found() {
        // V8 spirit: do not leak existence of accounts via differing errors.
        let auth = fresh().await;
        match auth.authenticate("nope", &SecretString::new("x")).await {
            Err(AuthError::InvalidCredentials) => {}
            other => panic!("expected InvalidCredentials, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn hash_is_not_plaintext() {
        let auth = fresh().await;
        let h = auth.hash_password(&SecretString::new("hunter2")).unwrap();
        assert!(h.expose().starts_with("$argon2"));
        assert!(!h.expose().contains("hunter2"));
    }

    #[tokio::test]
    async fn create_user_creates_admin_that_can_authenticate() {
        let auth = fresh().await;
        let outcome = auth
            .create_user("ali", &SecretString::new("hunter2"), true)
            .await
            .unwrap();
        assert_eq!(outcome, CreateUserOutcome::Created);
        // The created user can log in and is an admin.
        let user = auth
            .authenticate("ali", &SecretString::new("hunter2"))
            .await
            .unwrap();
        assert_eq!(user.name, "ali");
        let rec = auth.users().lookup_by_name("ali").await.unwrap();
        assert!(rec.policy.admin);
    }

    #[tokio::test]
    async fn create_user_defaults_to_non_admin() {
        let auth = fresh().await;
        auth.create_user("bob", &SecretString::new("pw"), false)
            .await
            .unwrap();
        let rec = auth.users().lookup_by_name("bob").await.unwrap();
        assert!(!rec.policy.admin);
    }

    #[tokio::test]
    async fn create_user_is_idempotent_on_name_collision() {
        let auth = fresh().await;
        auth.create_user("ali", &SecretString::new("hunter2"), true)
            .await
            .unwrap();
        let again = auth
            .create_user("ali", &SecretString::new("different"), false)
            .await
            .unwrap();
        assert_eq!(again, CreateUserOutcome::AlreadyExists);
        // The original password + admin flag are left untouched.
        let user = auth
            .authenticate("ali", &SecretString::new("hunter2"))
            .await
            .unwrap();
        assert_eq!(user.name, "ali");
    }

    mod throttle {
        use super::super::*;
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};
        use std::net::Ipv4Addr;

        fn cfg() -> LoginThrottleConfig {
            LoginThrottleConfig {
                max_failures_per_user: 3,
                max_failures_per_ip: 5,
                window_secs: 60,
                max_backoff_secs: 8,
            }
        }

        fn ip(n: u8) -> IpAddr {
            IpAddr::V4(Ipv4Addr::new(192, 0, 2, n))
        }

        fn secs(n: u64) -> Duration {
            Duration::from_secs(n)
        }

        /// One failed attempt: admitted, then dropped unsettled.
        fn fail(t: &LoginThrottle, user: &str, from: IpAddr, at: Instant) {
            drop(t.begin(user, from, at).expect("admitted"));
        }

        /// `n` failures spaced by whatever backoff applies, so each is
        /// admitted; returns the time of the last one.
        fn fail_paced(
            t: &LoginThrottle,
            user: &str,
            from: IpAddr,
            n: u32,
            mut at: Instant,
        ) -> Instant {
            for _ in 0..n {
                loop {
                    match t.begin(user, from, at) {
                        Ok(a) => {
                            drop(a);
                            break;
                        }
                        Err(r) => at += secs(r.secs),
                    }
                }
            }
            at
        }

        #[test]
        fn allows_up_to_the_threshold_then_refuses_with_a_retry_after() {
            let t = LoginThrottle::new(cfg());
            let t0 = Instant::now();
            for _ in 0..3 {
                fail(&t, "bob", ip(1), t0);
            }
            assert_eq!(
                t.begin("bob", ip(1), t0).unwrap_err(),
                RetryAfter {
                    reason: ThrottleReason::User,
                    secs: 1
                }
            );
            assert!(t.begin("bob", ip(1), t0 + secs(1)).is_ok());
        }

        #[test]
        fn backoff_doubles_per_failure_and_is_capped() {
            let t = LoginThrottle::new(cfg());
            let mut at = Instant::now();
            let mut waits = Vec::new();
            for _ in 0..8 {
                // Wait out whatever is asked, fail once, then see the next ask.
                at = fail_paced(&t, "bob", ip(1), 1, at);
                // Probe without leaving a reservation behind.
                waits.push(match t.begin("bob", ip(1), at) {
                    Ok(a) => {
                        a.abandon();
                        None
                    }
                    Err(r) => Some(r.secs),
                });
            }
            assert_eq!(
                waits,
                [
                    None,
                    None,
                    Some(1),
                    Some(2),
                    Some(4),
                    Some(8),
                    Some(8),
                    Some(8)
                ]
            );
        }

        #[test]
        fn wait_shrinks_as_time_passes_and_rounds_up() {
            let t = LoginThrottle::new(cfg());
            let last = fail_paced(&t, "bob", ip(1), 5, Instant::now()); // 4 s delay now
            let r = t
                .begin("bob", ip(1), last + Duration::from_millis(1500))
                .unwrap_err();
            assert_eq!(r.secs, 3, "2.5 s left rounds up to 3");
        }

        #[test]
        fn failures_are_forgotten_after_the_window() {
            let t = LoginThrottle::new(cfg());
            let last = fail_paced(&t, "bob", ip(1), 6, Instant::now());
            let a = t
                .begin("bob", ip(1), last + secs(60))
                .expect("window lapsed");
            assert_eq!(a.counts(), FailureCounts { user: 1, ip: 1 });
        }

        #[test]
        fn success_clears_that_user_only() {
            let t = LoginThrottle::new(cfg());
            let t0 = Instant::now();
            for _ in 0..2 {
                fail(&t, "bob", ip(1), t0);
            }
            for _ in 0..3 {
                fail(&t, "amy", ip(2), t0);
            }
            t.begin("bob", ip(1), t0).unwrap().succeeded();
            assert_eq!(
                t.begin("bob", ip(1), t0).unwrap().counts().user,
                1,
                "bob's count restarted"
            );
            assert!(t.begin("amy", ip(2), t0).is_err(), "other users untouched");
        }

        /// Without trusted proxies every client may share the ingress's
        /// address, so the address limit is off and only the user limit holds.
        #[test]
        fn without_per_ip_one_address_is_never_held_but_a_user_still_is() {
            let t = LoginThrottle::new(cfg()).with_per_ip(false);
            let t0 = Instant::now();
            for i in 0..50 {
                fail(&t, &format!("u{i}"), ip(7), t0);
            }
            assert!(
                t.begin("fresh", ip(7), t0).is_ok(),
                "address must not be held"
            );
            for _ in 0..3 {
                fail(&t, "amy", ip(8), t0);
            }
            assert_eq!(
                t.begin("amy", ip(9), t0).unwrap_err().reason,
                ThrottleReason::User
            );
        }

        #[test]
        fn a_valid_account_cannot_reset_the_address_budget() {
            let t = LoginThrottle::new(cfg());
            let t0 = Instant::now();
            // Four guesses at other users from one address (budget is 5).
            for u in ["a", "b", "c", "d"] {
                fail(&t, u, ip(7), t0);
            }
            // The attacker logs into its own account in between.
            t.begin("mine", ip(7), t0).unwrap().succeeded();
            // That success must not have wiped the four: one more guess uses
            // the last of the budget and the address is then held.
            fail(&t, "e", ip(7), t0);
            let r = t.begin("fresh", ip(7), t0).unwrap_err();
            assert_eq!(r.reason, ThrottleReason::Ip);
            // The address count still decays with the window.
            assert!(t.begin("fresh", ip(7), t0 + secs(60)).is_ok());
        }

        #[test]
        fn an_abandoned_attempt_is_not_counted() {
            let t = LoginThrottle::new(cfg());
            let t0 = Instant::now();
            for _ in 0..10 {
                t.begin("bob", ip(1), t0).unwrap().abandon();
            }
            assert_eq!(t.sizes(), (0, 0));
        }

        #[test]
        fn user_and_ip_limits_are_independent() {
            let t = LoginThrottle::new(cfg());
            let t0 = Instant::now();
            // One user guessed from three addresses: the user is held, the
            // addresses and other users are not.
            for n in 1..=3 {
                fail(&t, "bob", ip(n), t0);
            }
            let r = t.begin("bob", ip(9), t0).unwrap_err();
            assert_eq!(r.reason, ThrottleReason::User);
            assert!(t.begin("amy", ip(9), t0).is_ok());
            // One address spraying five usernames: the address is held, and
            // no single username is.
            for u in ["a", "b", "c", "d", "e"] {
                fail(&t, u, ip(7), t0);
            }
            let r = t.begin("fresh", ip(7), t0).unwrap_err();
            assert_eq!(r.reason, ThrottleReason::Ip);
            assert!(t.begin("fresh", ip(8), t0).is_ok());
        }

        #[test]
        fn usernames_are_case_insensitive() {
            let t = LoginThrottle::new(cfg());
            let t0 = Instant::now();
            fail(&t, "Bob", ip(1), t0);
            fail(&t, "BOB", ip(2), t0);
            fail(&t, "bob", ip(3), t0);
            assert!(t.begin("bOb", ip(4), t0).is_err());
        }

        #[test]
        fn ipv6_clients_share_a_slash_64() {
            let t = LoginThrottle::new(cfg());
            let t0 = Instant::now();
            let a: IpAddr = "2001:db8:1:2::1".parse().unwrap();
            let b: IpAddr = "2001:db8:1:2:ffff::9".parse().unwrap();
            let other: IpAddr = "2001:db8:1:3::1".parse().unwrap();
            for (i, u) in ["a", "b", "c", "d", "e"].iter().enumerate() {
                fail(&t, u, if i % 2 == 0 { a } else { b }, t0);
            }
            assert!(t.begin("fresh", a, t0).is_err());
            assert!(t.begin("fresh", b, t0).is_err());
            assert!(t.begin("fresh", other, t0).is_ok());
        }

        #[test]
        fn memory_is_bounded_when_usernames_are_sprayed() {
            let t = LoginThrottle::new(cfg());
            let t0 = Instant::now();
            for i in 0..(MAX_ENTRIES + 500) {
                let a = IpAddr::V4(Ipv4Addr::from(u32::try_from(i).unwrap()));
                fail(
                    &t,
                    &format!("user{i}"),
                    a,
                    t0 + Duration::from_millis(i as u64),
                );
            }
            let (users, ips) = t.sizes();
            assert!(users <= MAX_ENTRIES && ips <= MAX_ENTRIES, "{users} {ips}");
            // The newest entry survived; the stalest was the one evicted.
            assert!(!t.lock().users.contains_key("user0"));
            assert!(t
                .lock()
                .users
                .contains_key(&format!("user{}", MAX_ENTRIES + 499)));
        }

        #[test]
        fn very_long_usernames_do_not_make_big_keys() {
            let t = LoginThrottle::new(cfg());
            let t0 = Instant::now();
            let long = "x".repeat(100_000);
            fail(&t, &long, ip(1), t0);
            assert!(t.lock().users.keys().all(|k| k.len() <= MAX_KEY_CHARS));
        }

        #[test]
        fn attempts_in_flight_count_against_the_budget() {
            // Sequentially: the first `threshold` attempts are admitted and
            // still running (nothing settled); every further one is refused.
            let t = LoginThrottle::new(cfg());
            let t0 = Instant::now();
            let held: Vec<_> = (0..50).map(|_| t.begin("bob", ip(1), t0)).collect();
            assert_eq!(held.iter().filter(|r| r.is_ok()).count(), 3);
        }

        #[test]
        fn concurrent_attempts_cannot_exceed_the_budget() {
            let t = LoginThrottle::new(cfg());
            let t0 = Instant::now();
            let admitted = std::sync::atomic::AtomicUsize::new(0);
            let go = std::sync::Barrier::new(16);
            std::thread::scope(|s| {
                for _ in 0..16 {
                    s.spawn(|| {
                        go.wait();
                        for _ in 0..20 {
                            // Held until the thread ends: an unfinished hash.
                            if let Ok(a) = t.begin("bob", ip(1), t0) {
                                admitted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                                std::mem::forget(a);
                            }
                        }
                    });
                }
            });
            assert_eq!(
                admitted.load(std::sync::atomic::Ordering::SeqCst),
                3,
                "only max_failures_per_user guesses may be in flight"
            );
        }

        #[test]
        fn reason_labels_are_distinct_and_stable() {
            let labels = [ThrottleReason::User.label(), ThrottleReason::Ip.label()];
            assert_eq!(labels, ["user", "ip"]);
        }

        fn throttled_count(rec: &DebuggingRecorder, reason: &str) -> u64 {
            rec.snapshotter()
                .snapshot()
                .into_vec()
                .into_iter()
                .find_map(|(key, _, _, v)| {
                    let hit = key.key().name() == "pharos_login_throttled_total"
                        && key
                            .key()
                            .labels()
                            .any(|l| l.key() == "reason" && l.value() == reason);
                    match v {
                        DebugValue::Counter(n) if hit => Some(n),
                        _ => None,
                    }
                })
                .unwrap_or(0)
        }

        #[derive(Clone, Default)]
        struct Buf(std::sync::Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Buf {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Buf {
            type Writer = Buf;
            fn make_writer(&'a self) -> Buf {
                self.clone()
            }
        }

        #[test]
        fn every_refusal_is_counted_by_reason_and_warned_with_its_context() {
            let rec = DebuggingRecorder::new();
            let _m = metrics::set_default_local_recorder(&rec);
            let buf = Buf::default();
            let sub = tracing_subscriber::fmt()
                .with_writer(buf.clone())
                .with_ansi(false)
                .with_max_level(tracing::Level::DEBUG)
                .finish();
            let _g = tracing::subscriber::set_default(sub);

            let t = LoginThrottle::new(cfg());
            let t0 = Instant::now();
            for _ in 0..3 {
                fail(&t, "Mallory", ip(1), t0);
            }
            assert!(t.begin("mallory", ip(1), t0).is_err());
            let log = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();

            assert!(log.contains("login failed"), "failure is visible: {log}");
            assert!(log.contains("user_failures=3"), "{log}");
            let warn = log
                .lines()
                .find(|l| l.contains("login throttled"))
                .unwrap_or_else(|| panic!("no throttle WARN in {log}"));
            for want in [
                "WARN",
                "reason=\"user\"",
                "user=mallory",
                "client_ip=192.0.2.1",
                "retry_after_secs=1",
            ] {
                assert!(warn.contains(want), "missing {want} in {warn}");
            }
            assert_eq!(throttled_count(&rec, "user"), 1);
        }

        #[test]
        fn ip_refusals_are_counted_under_their_own_label() {
            let rec = DebuggingRecorder::new();
            let _m = metrics::set_default_local_recorder(&rec);
            let t = LoginThrottle::new(cfg());
            let t0 = Instant::now();
            for u in ["a", "b", "c", "d", "e"] {
                fail(&t, u, ip(7), t0);
            }
            assert!(t.begin("fresh", ip(7), t0).is_err());
            assert_eq!(throttled_count(&rec, "ip"), 1);
        }
    }
}
