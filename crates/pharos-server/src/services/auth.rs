//! Token to [`User`]: the decision every authenticated call on either API
//! makes, implemented once (V172).

use pharos_core::{AuthError, TokenScope, TokenSelector, TokenStore, User, UserId, UserStore};
use tokio::sync::broadcast;

use crate::state::{AppState, Stores};

/// Why a token did not yield a user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenRefusal {
    /// The store does not know the token.
    InvalidToken,
    /// The token resolved, but its user no longer exists.
    UserRevoked,
    /// The store could not answer (pool timeout, database down). Transient and
    /// ours, not the caller's: they keep their session and retry (B216).
    Unavailable(String),
    /// The token is live but restricted (a cast TV's
    /// [`TokenScope::Playback`]), and the caller needs a full credential: a
    /// restricted token reaches only the allowlisted HTTP routes (V229).
    ScopeRestricted,
    /// The user's access schedule is closed right now (V578). Jellyfin
    /// answers 403 here, not 401: the credential is fine, the hour is not.
    OutsideSchedule,
}

impl TokenRefusal {
    /// The `reason` label of `pharos_auth_rejected_total`.
    pub fn label(&self) -> &'static str {
        match self {
            TokenRefusal::InvalidToken => "invalid_token",
            TokenRefusal::UserRevoked => "user_revoked",
            TokenRefusal::Unavailable(_) => "store_unavailable",
            TokenRefusal::ScopeRestricted => "scope_restricted",
            TokenRefusal::OutsideSchedule => "outside_schedule",
        }
    }
}

/// Resolves a full-scope `token` to its user: one store call for an unknown
/// token, two for a known one. A restricted token (a cast TV's) is refused
/// with [`TokenRefusal::ScopeRestricted`]: every caller of this function
/// grants the user's whole account, and a restricted token may only reach the
/// routes [`scoped_user_for_token`]'s callers check it against.
pub async fn user_for_token(stores: &Stores, token: &str) -> Result<User, TokenRefusal> {
    let (user, scope) = scoped_user_for_token(stores, token).await?;
    if scope != TokenScope::Full {
        metrics::counter!("pharos_token_scope_refused_total", "scope" => scope.label())
            .increment(1);
        tracing::warn!(
            scope = scope.label(),
            user.id = %user.id,
            "token scope does not permit a full-account credential check; refusing"
        );
        return Err(TokenRefusal::ScopeRestricted);
    }
    Ok(user)
}

/// [`user_for_token`], with the scope the token was issued with: what an
/// HTTP request checks its route against.
pub async fn scoped_user_for_token(
    stores: &Stores,
    token: &str,
) -> Result<(User, TokenScope), TokenRefusal> {
    let resolved = stores
        .resolve_scoped(token)
        .await
        .map_err(|e| classify(e, TokenRefusal::InvalidToken))?;
    Ok((live_user(stores, resolved.user).await?, resolved.scope))
}

/// [`user_for_token`] for a token known only by its
/// [`token_id`](TokenStore::token_id), as a play session holds it: refused
/// once that token is revoked or expired, or its user is gone or disabled.
pub async fn user_for_token_id(stores: &Stores, id: &str) -> Result<User, TokenRefusal> {
    let uid = stores
        .resolve_token_id(id)
        .await
        .map_err(|e| classify(e, TokenRefusal::InvalidToken))?;
    live_user(stores, uid).await
}

async fn live_user(stores: &Stores, uid: UserId) -> Result<User, TokenRefusal> {
    let record = stores
        .get(uid)
        .await
        .map_err(|e| classify(e, TokenRefusal::UserRevoked))?;
    // A disabled user's tokens stop working, not only their password.
    if record.policy.is_disabled {
        return Err(TokenRefusal::UserRevoked);
    }
    // Jellyfin DefaultAuthorizationHandler: admins skip the schedule on a
    // request; everyone else is refused outside their windows (V578).
    if !record.policy.admin {
        let (weekday, hour) = crate::local_time::now_local();
        if !record.policy.within_access_schedule(weekday, hour) {
            return Err(TokenRefusal::OutsideSchedule);
        }
    }
    Ok(record.into_user())
}

/// Why a user who proved who they are was still refused a new session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginGate {
    /// Their access schedule is closed right now (V578).
    OutsideSchedule,
    /// They already hold `held` of at most `max` sessions (V579).
    SessionLimit { held: u32, max: u32 },
}

impl LoginGate {
    /// The `reason` label of `pharos_login_gated_total`.
    pub fn label(self) -> &'static str {
        match self {
            LoginGate::OutsideSchedule => "outside_schedule",
            LoginGate::SessionLimit { .. } => "session_limit",
        }
    }

    /// Jellyfin's `SecurityException` text for this refusal.
    pub fn message(self) -> &'static str {
        match self {
            LoginGate::OutsideSchedule => "User is not allowed access at this time.",
            LoginGate::SessionLimit { .. } => "User is at their maximum number of sessions.",
        }
    }
}

/// [`login_gate`] gave no verdict or refused.
#[derive(Debug)]
pub enum LoginGateError {
    Refused(LoginGate),
    /// The token store could not count the user's sessions; the login fails
    /// closed rather than skipping the cap.
    Store(AuthError),
}

impl From<LoginGate> for LoginGateError {
    fn from(gate: LoginGate) -> Self {
        Self::Refused(gate)
    }
}

impl From<LoginGateError> for PasswordLoginError {
    fn from(e: LoginGateError) -> Self {
        match e {
            LoginGateError::Refused(gate) => Self::Gated(gate),
            LoginGateError::Store(e) => Self::Issue(e),
        }
    }
}

impl From<LoginGateError> for QuickConnectLoginError {
    fn from(e: LoginGateError) -> Self {
        match e {
            LoginGateError::Refused(gate) => Self::Gated(gate),
            LoginGateError::Store(e) => Self::Unavailable(e.to_string()),
        }
    }
}

/// Entries kept before idle per-user locks are dropped from [`SessionSlots`].
const SESSION_SLOTS_PRUNE_AT: usize = 256;

/// One lock per user, so the session count and the token insert of a capped
/// login happen as a unit: two concurrent logins cannot both see room for
/// the last session (V579). In-process only, like [`Revocations`].
#[derive(Debug, Default)]
pub struct SessionSlots {
    locks:
        std::sync::Mutex<std::collections::HashMap<UserId, std::sync::Arc<tokio::sync::Mutex<()>>>>,
}

impl SessionSlots {
    async fn acquire(&self, user: UserId) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = {
            let mut locks = self
                .locks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if locks.len() >= SESSION_SLOTS_PRUNE_AT {
                // An entry nobody else holds or waits on is idle.
                locks.retain(|_, l| std::sync::Arc::strong_count(l) > 1);
            }
            std::sync::Arc::clone(locks.entry(user).or_default())
        };
        lock.lock_owned().await
    }
}

/// Proof a capped login's count still holds. Keep it alive until the token
/// is issued; dropping it lets the user's next login count.
#[derive(Debug)]
pub struct SessionSlot {
    _guard: Option<tokio::sync::OwnedMutexGuard<()>>,
}

/// The policy checks Jellyfin makes after a correct password and before it
/// issues a session (`UserManager.AuthenticateUser`, then
/// `SessionManager.AuthenticateNewSessionInternal`): the access schedule,
/// then `MaxActiveSessions`. Unlike a request, a login is gated for admins
/// too. Only called with a verified user, so the refusal can say why without
/// telling a guesser anything (V578, V579). `device_id` is the device the
/// new token is for: signing in again from a device that already has a
/// session does not count against the cap. For a capped user the returned
/// [`SessionSlot`] serialises this login's count with its token insert.
pub async fn login_gate(
    state: &AppState,
    user: &User,
    device_id: &str,
) -> Result<SessionSlot, LoginGateError> {
    let (weekday, hour) = crate::local_time::now_local();
    let capped = user.policy.max_active_sessions >= 1;
    let guard = if capped {
        Some(state.session_slots.acquire(user.id).await)
    } else {
        None
    };
    let held = if capped {
        state
            .stores
            .count_sessions_excluding_device(user.id, device_id)
            .await
            .map_err(LoginGateError::Store)?
    } else {
        0
    };
    login_gate_at(user, weekday, hour, held).map_err(LoginGateError::Refused)?;
    Ok(SessionSlot { _guard: guard })
}

fn login_gate_at(user: &User, weekday: u8, hour: f64, held: u32) -> Result<(), LoginGate> {
    let gate = if !user.policy.within_access_schedule(weekday, hour) {
        LoginGate::OutsideSchedule
    } else if let Some(max) = u32::try_from(user.policy.max_active_sessions)
        .ok()
        .filter(|max| *max >= 1 && held >= *max)
    {
        LoginGate::SessionLimit { held, max }
    } else {
        return Ok(());
    };
    metrics::counter!("pharos_login_gated_total", "reason" => gate.label()).increment(1);
    tracing::warn!(
        user.id = %user.id,
        reason = gate.label(),
        weekday,
        hour,
        "login refused by user policy"
    );
    Err(gate)
}

/// A backend failure is `Unavailable` whatever stage it hit; any other error
/// at that stage is `otherwise`.
fn classify(e: AuthError, otherwise: TokenRefusal) -> TokenRefusal {
    match e {
        AuthError::Backend(detail) => TokenRefusal::Unavailable(detail),
        _ => otherwise,
    }
}

/// Why a user's tokens may have stopped resolving. The label is the `cause`
/// of `pharos_auth_revocation_announced_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevokeCause {
    /// The caller signed out, revoking its own token.
    Logout,
    /// An admin deleted a device, revoking its tokens.
    DeviceDeleted,
    /// An admin revoked an API key.
    ApiKeyRevoked,
    /// The user was deleted.
    UserDeleted,
    /// The user's policy changed (it may now be disabled).
    PolicyChanged,
    /// The user's password changed, so tokens issued under the old one go.
    PasswordChanged,
    /// Too many wrong passwords disabled the account (V577).
    LockedOut,
}

impl RevokeCause {
    pub fn label(self) -> &'static str {
        match self {
            RevokeCause::Logout => "logout",
            RevokeCause::DeviceDeleted => "device_deleted",
            RevokeCause::ApiKeyRevoked => "api_key_revoked",
            RevokeCause::UserDeleted => "user_deleted",
            RevokeCause::PolicyChanged => "policy_changed",
            RevokeCause::PasswordChanged => "password_changed",
            RevokeCause::LockedOut => "locked_out",
        }
    }
}

/// Announces a lockout the sign-in just caused and answers it as the plain
/// wrong password it looks like to the client: the account is now disabled,
/// so its open native sockets must re-check (V577). Any other refusal passes
/// through.
pub fn settle_lockout(
    state: &AppState,
    refusal: crate::auth::LoginRefusal,
) -> crate::auth::LoginRefusal {
    use crate::auth::LoginRefusal;
    match refusal {
        LoginRefusal::LockedOut(user) => {
            state.revocations.announce(user, RevokeCause::LockedOut);
            LoginRefusal::Invalid
        }
        other => other,
    }
}

/// Announced events a channel holds before a slow subscriber lags. A lagged
/// connection re-checks all its tokens, so overflow costs a lookup, never a
/// missed revocation.
const ANNOUNCE_CAPACITY: usize = 256;

/// Tells open native connections (T025) that some of a user's tokens may no
/// longer resolve. The message is a trigger, not a verdict: each of that
/// user's connections re-checks its own tokens against the store, so revoking
/// one device's token leaves the user's other devices connected. In-process
/// only: with more than one pharos replica, connections on the others still
/// wait for their timer (`revalidate_secs`).
#[derive(Debug, Clone)]
pub struct Revocations {
    tx: broadcast::Sender<UserId>,
}

impl Default for Revocations {
    fn default() -> Self {
        Self {
            tx: broadcast::channel(ANNOUNCE_CAPACITY).0,
        }
    }
}

impl Revocations {
    /// Counts and logs the announcement, then sends it to every subscriber.
    pub fn announce(&self, user: UserId, cause: RevokeCause) {
        metrics::counter!("pharos_auth_revocation_announced_total", "cause" => cause.label())
            .increment(1);
        // No subscribers is the normal case with the native API off.
        // Every open native connection subscribes, whoever its user.
        let subscribers = self.tx.send(user).unwrap_or(0);
        tracing::info!(
            user_id = %user.0,
            cause = cause.label(),
            subscribers,
            "auth: revocation announced"
        );
    }

    pub fn subscribe(&self) -> broadcast::Receiver<UserId> {
        self.tx.subscribe()
    }
}

/// How many Media tokens a revoke of `selector` is about to take with its
/// parents. Only feeds the metric and log: the database cascade removes them
/// whether or not this runs, so a failed count is `0`, never a failed revoke.
async fn count_children(
    state: &AppState,
    user: UserId,
    selector: TokenSelector<'_>,
    cause: RevokeCause,
) -> u64 {
    match state.stores.count_live_children(user, selector).await {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!(
                user_id = %user.0,
                cause = cause.label(),
                error = %e,
                "auth: could not count child tokens before revoke"
            );
            0
        }
    }
}

/// Counts and logs the Media tokens that went with their parents
/// (`pharos_media_token_revoked_total{cause}`, B543).
fn record_children_revoked(user: UserId, cause: RevokeCause, children: u64) {
    metrics::counter!("pharos_media_token_revoked_total", "cause" => cause.label())
        .increment(children);
    if children > 0 {
        tracing::info!(
            user_id = %user.0,
            cause = cause.label(),
            children,
            "auth: media tokens revoked with their parent"
        );
    }
}

/// Revokes `user`'s `token` (idempotent) and announces the user.
pub async fn revoke_token(
    state: &AppState,
    user: UserId,
    token: &str,
    cause: RevokeCause,
) -> Result<(), AuthError> {
    let children = count_children(state, user, TokenSelector::Token(token), cause).await;
    state.stores.revoke(token).await?;
    record_children_revoked(user, cause, children);
    state.revocations.announce(user, cause);
    Ok(())
}

/// Revokes every token `user` holds for `device_id`; announces the user when
/// any went. Returns how many.
pub async fn revoke_device(
    state: &AppState,
    user: UserId,
    device_id: &str,
    cause: RevokeCause,
) -> Result<u64, AuthError> {
    let children = count_children(state, user, TokenSelector::Device(device_id), cause).await;
    let dropped = state
        .stores
        .revoke_tokens_by_device(user, device_id)
        .await?;
    record_children_revoked(user, cause, children);
    if dropped > 0 {
        state.revocations.announce(user, cause);
    }
    Ok(dropped)
}

/// How a native sign-in arrived. The label is the `transport` of
/// `pharos_native_login_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginTransport {
    /// `POST /pharos/session`, answered with the `pharos_session` cookie.
    Cookie,
    /// `Bootstrap.login` / `quickConnectPoll` over `/pharos/rpc`.
    Rpc,
}

impl LoginTransport {
    pub fn label(self) -> &'static str {
        match self {
            LoginTransport::Cookie => "cookie",
            LoginTransport::Rpc => "rpc",
        }
    }
}

/// How a native sign-in ended. The label is the `outcome` of
/// `pharos_native_login_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginOutcome {
    Ok,
    /// Wrong password, unknown or disabled user: one answer for all three.
    Invalid,
    /// Refused by the login throttle before any hash (V174).
    Throttled,
    /// The user or token store failed.
    Unavailable,
    /// A browser request from an origin not in `allowed_origins`, or none.
    OriginRefused,
    /// The password was right but the user holds the most connections they
    /// may (FR-010); no token was issued.
    Capped,
    /// The password was right but the user's policy (access schedule) forbids
    /// a session; no token was issued.
    Gated,
    /// A Quick Connect poll for a code nobody has authorized yet. Not
    /// counted: clients poll every few seconds.
    Pending,
}

impl LoginOutcome {
    pub fn label(self) -> &'static str {
        match self {
            LoginOutcome::Ok => "ok",
            LoginOutcome::Invalid => "invalid",
            LoginOutcome::Throttled => "throttled",
            LoginOutcome::Unavailable => "unavailable",
            LoginOutcome::OriginRefused => "origin_refused",
            LoginOutcome::Capped => "capped",
            LoginOutcome::Gated => "gated",
            LoginOutcome::Pending => "pending",
        }
    }
}

/// Counts one native sign-in attempt by transport, method and outcome.
pub fn record_login(transport: LoginTransport, method: &'static str, outcome: LoginOutcome) {
    metrics::counter!(
        "pharos_native_login_total",
        "transport" => transport.label(),
        "method" => method,
        "outcome" => outcome.label()
    )
    .increment(1);
}

/// Why [`password_login`] issued no token.
#[derive(Debug)]
pub enum PasswordLoginError {
    Refused(crate::auth::LoginRefusal),
    /// The password was right but the token could not be stored.
    Issue(AuthError),
    /// The password was right but `admit` refused the user: they hold
    /// `held` of at most `max` connections.
    Capped {
        held: u32,
        max: u32,
    },
    /// The password was right but the user's policy forbids a session now.
    Gated(LoginGate),
}

impl PasswordLoginError {
    pub fn outcome(&self) -> LoginOutcome {
        use crate::auth::LoginRefusal;
        match self {
            PasswordLoginError::Refused(LoginRefusal::Throttled(_)) => LoginOutcome::Throttled,
            PasswordLoginError::Refused(LoginRefusal::Invalid | LoginRefusal::LockedOut(_)) => {
                LoginOutcome::Invalid
            }
            PasswordLoginError::Refused(LoginRefusal::Backend(_))
            | PasswordLoginError::Issue(_) => LoginOutcome::Unavailable,
            PasswordLoginError::Capped { .. } => LoginOutcome::Capped,
            PasswordLoginError::Gated(_) => LoginOutcome::Gated,
        }
    }
}

/// A native password sign-in: the shared throttle checks the password
/// (V174), `admit` may still refuse the user (`Err((held, max))`, a
/// connection cap), then a device token is issued for `device_id` in the one
/// token store (ADR-0011). Counted and logged with its outcome either way;
/// a refused login issues no token.
pub async fn password_login(
    state: &AppState,
    transport: LoginTransport,
    name: &str,
    password: &pharos_core::SecretString,
    client_ip: std::net::IpAddr,
    device_id: &str,
    admit: impl FnOnce(&User) -> Result<(), (u32, u32)>,
) -> Result<(User, pharos_core::AuthToken), PasswordLoginError> {
    let result: Result<_, PasswordLoginError> = async {
        // An empty password is never a valid native credential: refuse it
        // before any hash, with the same answer as a wrong password (B271).
        if password.expose().is_empty() {
            tracing::warn!(
                transport = transport.label(),
                user = %name,
                client_ip = %client_ip,
                reason = "empty_password",
                "native login refused"
            );
            return Err(PasswordLoginError::Refused(
                crate::auth::LoginRefusal::Invalid,
            ));
        }
        let user = state
            .login_throttle
            .verify(&state.auth, name, password, client_ip)
            .await
            .map_err(|r| PasswordLoginError::Refused(settle_lockout(state, r)))?;
        let _slot = login_gate(state, &user, device_id).await?;
        admit(&user).map_err(|(held, max)| PasswordLoginError::Capped { held, max })?;
        let token = state
            .stores
            .issue(user.id, device_id)
            .await
            .map_err(PasswordLoginError::Issue)?;
        Ok((user, token))
    }
    .await;
    let outcome = match &result {
        Ok(_) => LoginOutcome::Ok,
        Err(e) => e.outcome(),
    };
    record_login(transport, "password", outcome);
    match &result {
        Ok((user, _)) => {
            tracing::info!(
                transport = transport.label(),
                user = %user.name,
                client_ip = %client_ip,
                device_id,
                "native login"
            );
            state.record_activity(
                &format!("{} logged in", user.name),
                "SessionStarted",
                Some(&user.id.0.simple().to_string()),
                Some("pharos"),
            );
        }
        Err(e) => tracing::info!(
            transport = transport.label(),
            outcome = outcome.label(),
            client_ip = %client_ip,
            error = ?e,
            "native login refused"
        ),
    }
    result
}

/// Startup check (B271): sets `pharos_users_empty_password` to the number of
/// accounts whose password is "", and says which ones. The argon2 checks run on
/// the blocking pool, so a large user table does not hold a worker.
pub async fn report_empty_password_accounts(
    state: actix_web::web::Data<AppState>,
) -> Result<(), AuthError> {
    let records = state.auth.users().list().await?;
    let checker = state.clone();
    let names =
        tokio::task::spawn_blocking(move || checker.auth.names_with_empty_password(&records))
            .await
            .map_err(|e| AuthError::Backend(format!("empty-password check task: {e}")))?;
    metrics::gauge!("pharos_users_empty_password").set(names.len() as f64);
    if names.is_empty() {
        tracing::info!(count = 0, "no account has an empty password");
    } else {
        tracing::warn!(
            count = names.len(),
            users = %names.join(", "),
            "accounts with an empty password: native sign-in refuses them; set a password"
        );
    }
    Ok(())
}

/// Revokes every session token `user` holds (API keys excepted) but `keep_token`; announces the user
/// when any went and logs how many. Returns the count.
pub async fn revoke_other_tokens(
    state: &AppState,
    user: UserId,
    keep_token: Option<&str>,
    cause: RevokeCause,
) -> Result<u64, AuthError> {
    let children = count_children(state, user, TokenSelector::AllExcept(keep_token), cause).await;
    let revoked = state
        .stores
        .revoke_sessions_except(user, keep_token)
        .await?;
    record_children_revoked(user, cause, children);
    tracing::info!(
        user_id = %user.0,
        cause = cause.label(),
        revoked,
        "auth: user tokens revoked"
    );
    if revoked > 0 {
        state.revocations.announce(user, cause);
    }
    Ok(revoked)
}

/// Why [`change_own_password`] stored no new password, or did but could
/// not sign the user's other sessions out.
#[derive(Debug)]
pub enum ChangePasswordError {
    /// The new password is empty: no account may hold one (V211).
    EmptyNew,
    /// The current password was refused: wrong (or empty), throttled, or
    /// the user store could not check it.
    Refused(crate::auth::LoginRefusal),
    /// Hashing or storing the new password failed; the old one stands.
    Store(AuthError),
    /// The new password is stored, but the other sessions were not revoked.
    Revoke(AuthError),
}

/// A signed-in user changing their own password: the current one is
/// checked through the login throttle (V174), so a stolen session cannot
/// buy unthrottled guesses with it, the new one is stored in one update,
/// and every other session token the user holds is revoked (B235);
/// `keep_token`, the caller's own, survives. Returns how many went.
pub async fn change_own_password(
    state: &AppState,
    user: &User,
    current: &pharos_core::SecretString,
    new: &pharos_core::SecretString,
    client_ip: std::net::IpAddr,
    keep_token: &str,
) -> Result<u64, ChangePasswordError> {
    let result = async {
        // Both checked before the throttle, so a malformed request spends
        // no attempt; an empty current password is refused as a wrong one.
        if new.expose().is_empty() {
            return Err(ChangePasswordError::EmptyNew);
        }
        if current.expose().is_empty() {
            return Err(ChangePasswordError::Refused(
                crate::auth::LoginRefusal::Invalid,
            ));
        }
        state
            .login_throttle
            .verify_recheck(&state.auth, &user.name, current, client_ip)
            .await
            .map_err(ChangePasswordError::Refused)?;
        let hash = state
            .auth
            .hash_password_off_worker(new)
            .await
            .map_err(ChangePasswordError::Store)?;
        state
            .stores
            .set_password(user.id, hash)
            .await
            .map_err(ChangePasswordError::Store)?;
        revoke_other_tokens(
            state,
            user.id,
            Some(keep_token),
            RevokeCause::PasswordChanged,
        )
        .await
        .map_err(ChangePasswordError::Revoke)
    }
    .await;
    match &result {
        Ok(revoked) => tracing::info!(
            user = %user.name,
            client_ip = %client_ip,
            revoked,
            "password changed"
        ),
        Err(e) => tracing::warn!(
            user = %user.name,
            client_ip = %client_ip,
            error = ?e,
            "password change refused"
        ),
    }
    result
}

/// Why [`quick_connect_login`] issued no token.
#[derive(Debug)]
pub enum QuickConnectLoginError {
    /// No live request has this secret: never issued, expired, or already
    /// signed in.
    Unknown,
    /// The code has not been authorized yet; the client polls again.
    Pending,
    /// The authorizing user is disabled (V176); the code is spent.
    Disabled,
    /// `admit` refused the authorizing user (a connection cap); the code is
    /// kept, so a retry can still sign in.
    Capped { held: u32, max: u32 },
    /// The authorizing user's policy forbids a session now; the code is kept.
    Gated(LoginGate),
    /// The registry, user store or token store failed.
    Unavailable(String),
}

impl QuickConnectLoginError {
    pub fn outcome(&self) -> LoginOutcome {
        match self {
            Self::Unknown | Self::Disabled => LoginOutcome::Invalid,
            Self::Pending => LoginOutcome::Pending,
            Self::Capped { .. } => LoginOutcome::Capped,
            Self::Gated(_) => LoginOutcome::Gated,
            Self::Unavailable(_) => LoginOutcome::Unavailable,
        }
    }
}

/// A native Quick Connect sign-in: once a code is authorized, its secret is
/// spent (one-shot, as the Jellyfin finalize) and a token is issued to the
/// authorizer under the device id the request was initiated with. A
/// disabled authorizer gets none (V176). Counted under
/// `method="quick_connect"` and logged, except while the code is pending.
pub async fn quick_connect_login(
    state: &AppState,
    transport: LoginTransport,
    secret: &str,
    admit: impl FnOnce(&User) -> Result<(), (u32, u32)>,
) -> Result<(User, pharos_core::AuthToken), QuickConnectLoginError> {
    use crate::quick_connect::QcMsg;
    use QuickConnectLoginError as E;
    let result: Result<_, E> = async {
        let ask = |consume: bool| async move {
            let (reply, rx) = tokio::sync::oneshot::channel();
            let secret = secret.to_owned();
            let msg = if consume {
                QcMsg::Consume { secret, reply }
            } else {
                QcMsg::Connect { secret, reply }
            };
            state
                .quick_connect
                .tx
                .send(msg)
                .await
                .map_err(|e| E::Unavailable(e.to_string()))?;
            rx.await.map_err(|e| E::Unavailable(e.to_string()))
        };
        // A read first, so a pending code is told apart from an unknown one.
        let entry = ask(false).await?.ok_or(E::Unknown)?;
        let by = entry.authorized_by.ok_or(E::Pending)?;
        let record = state
            .stores
            .get(by)
            .await
            .map_err(|e| E::Unavailable(e.to_string()))?;
        if record.policy.is_disabled {
            // Spend the code, as the Jellyfin finalize does.
            if let Err(e) = ask(true).await {
                tracing::warn!(error = ?e, "quick connect: could not spend a disabled user's code");
            }
            return Err(E::Disabled);
        }
        let user = record.into_user();
        let _slot = login_gate(state, &user, &entry.device_id).await?;
        admit(&user).map_err(|(held, max)| E::Capped { held, max })?;
        // One-shot: of two racing polls, only one consumes the entry.
        let entry = ask(true).await?.ok_or(E::Unknown)?;
        let token = state
            .stores
            .issue(user.id, &entry.device_id)
            .await
            .map_err(|e| E::Unavailable(e.to_string()))?;
        Ok((user, token))
    }
    .await;
    match &result {
        Err(E::Pending) => {}
        Ok((user, _)) => {
            record_login(transport, "quick_connect", LoginOutcome::Ok);
            tracing::info!(
                transport = transport.label(),
                user = %user.name,
                "native quick connect login"
            );
            state.record_activity(
                &format!("{} logged in via Quick Connect", user.name),
                "SessionStarted",
                Some(&user.id.0.simple().to_string()),
                Some("pharos"),
            );
        }
        Err(e) => {
            record_login(transport, "quick_connect", e.outcome());
            tracing::info!(
                transport = transport.label(),
                outcome = e.outcome().label(),
                error = ?e,
                "native quick connect login refused"
            );
        }
    }
    result
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// B216: a store that could not answer is not a bad token.
    #[test]
    fn a_backend_failure_is_unavailable_and_anything_else_is_the_stage_refusal() {
        assert_eq!(
            classify(
                AuthError::Backend("pool timed out".into()),
                TokenRefusal::InvalidToken
            ),
            TokenRefusal::Unavailable("pool timed out".into())
        );
        for e in [
            AuthError::InvalidToken,
            AuthError::UserNotFound,
            AuthError::InvalidCredentials,
        ] {
            assert_eq!(
                classify(e, TokenRefusal::UserRevoked),
                TokenRefusal::UserRevoked
            );
        }
    }

    #[test]
    fn login_labels_are_distinct() {
        let outcomes = [
            LoginOutcome::Ok,
            LoginOutcome::Invalid,
            LoginOutcome::Throttled,
            LoginOutcome::Unavailable,
            LoginOutcome::OriginRefused,
            LoginOutcome::Capped,
            LoginOutcome::Pending,
        ];
        let unique: std::collections::HashSet<_> = outcomes.iter().map(|o| o.label()).collect();
        assert_eq!(unique.len(), outcomes.len());
        assert_ne!(LoginTransport::Cookie.label(), LoginTransport::Rpc.label());
    }

    #[test]
    fn revoke_cause_labels_are_distinct() {
        let causes = [
            RevokeCause::Logout,
            RevokeCause::DeviceDeleted,
            RevokeCause::ApiKeyRevoked,
            RevokeCause::UserDeleted,
            RevokeCause::PolicyChanged,
            RevokeCause::PasswordChanged,
            RevokeCause::LockedOut,
        ];
        let unique: std::collections::HashSet<_> = causes.iter().map(|c| c.label()).collect();
        assert_eq!(unique.len(), causes.len());
    }

    /// The announcement is queryable: an operator can see revocations happen
    /// without a native connection open to notice them.
    #[test]
    fn an_announcement_is_counted_by_cause_and_reaches_subscribers() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};
        let recorder = DebuggingRecorder::new();
        let snapshotter = pharos_test_metrics::snapshotter(&recorder);
        let bus = Revocations::default();
        let mut rx = bus.subscribe();
        let user = UserId::new();
        metrics::with_local_recorder(&recorder, || bus.announce(user, RevokeCause::Logout));
        assert_eq!(rx.try_recv().unwrap(), user);
        let counted = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .find(|(k, ..)| {
                k.key().name() == "pharos_auth_revocation_announced_total"
                    && k.key()
                        .labels()
                        .any(|l| l.key() == "cause" && l.value() == "logout")
            });
        assert!(
            matches!(counted, Some((.., DebugValue::Counter(1)))),
            "{counted:?}"
        );
    }

    /// `pharos_login_gated_total{reason}` is a dashboard contract: one
    /// distinct label per gate, and each refusal bumps its own series.
    #[test]
    fn login_gate_labels_are_distinct_and_each_refusal_is_counted() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};
        let gates = [
            LoginGate::OutsideSchedule,
            LoginGate::SessionLimit { held: 1, max: 1 },
        ];
        let labels: std::collections::HashSet<_> = gates.iter().map(|g| g.label()).collect();
        assert_eq!(labels.len(), gates.len());

        let recorder = DebuggingRecorder::new();
        let snapshotter = pharos_test_metrics::snapshotter(&recorder);
        metrics::with_local_recorder(&recorder, || {
            let mut user = User {
                id: UserId::new(),
                name: "u".into(),
                policy: pharos_core::UserPolicy::default(),
                has_password: true,
            };
            user.policy.max_active_sessions = 1;
            // Open all week, and one session held: only the cap refuses.
            assert!(matches!(
                login_gate_at(&user, 3, 12.0, 1),
                Err(LoginGate::SessionLimit { .. })
            ));
            user.policy.access_schedules = vec![pharos_core::AccessSchedule {
                day_of_week: "Everyday".into(),
                start_hour: 1.0,
                end_hour: 1.5,
            }];
            assert!(matches!(
                login_gate_at(&user, 3, 12.0, 0),
                Err(LoginGate::OutsideSchedule)
            ));
        });
        let snapshot = snapshotter.snapshot().into_vec();
        for gate in gates {
            let counted = snapshot.iter().find(|(k, ..)| {
                k.key().name() == "pharos_login_gated_total"
                    && k.key()
                        .labels()
                        .any(|l| l.key() == "reason" && l.value() == gate.label())
            });
            assert!(
                matches!(counted, Some((.., DebugValue::Counter(1)))),
                "{}: {counted:?}",
                gate.label()
            );
        }
    }

    #[test]
    fn refusal_labels_are_distinct() {
        let labels = [
            TokenRefusal::InvalidToken.label(),
            TokenRefusal::UserRevoked.label(),
            TokenRefusal::Unavailable(String::new()).label(),
            TokenRefusal::ScopeRestricted.label(),
            TokenRefusal::OutsideSchedule.label(),
        ];
        let unique: std::collections::HashSet<_> = labels.iter().collect();
        assert_eq!(unique.len(), labels.len());
    }

    #[actix_web::test]
    async fn an_unknown_token_is_invalid() {
        let stores = Stores::connect("sqlite::memory:").await.unwrap();
        assert_eq!(
            user_for_token(&stores, "nope").await.unwrap_err(),
            TokenRefusal::InvalidToken
        );
    }

    #[actix_web::test]
    async fn a_disabled_users_token_is_refused() {
        use pharos_core::{SecretString, TokenStore, UserId, UserPolicy, UserRecord, UserStore};
        let stores = Stores::connect("sqlite::memory:").await.unwrap();
        let hash = crate::auth::BuiltinAuth::new(stores.clone())
            .hash_password(&SecretString::new("p"))
            .unwrap();
        let id = UserId::new();
        stores
            .create(UserRecord {
                id,
                name: "off".into(),
                password_hash: hash,
                policy: UserPolicy {
                    is_disabled: true,
                    ..Default::default()
                },
            })
            .await
            .unwrap();
        let token = stores.issue(id, "dev").await.unwrap();
        assert_eq!(
            user_for_token(&stores, token.0.expose()).await.unwrap_err(),
            TokenRefusal::UserRevoked
        );
    }

    /// A cast TV's playback token is refused wherever a full credential is
    /// checked (the native API's resume, session calls and socket re-check
    /// all use `user_for_token`), and the refusal is counted; the scoped
    /// lookup still resolves it for the allowlisted HTTP routes (V229).
    #[actix_web::test]
    async fn a_playback_token_is_not_a_full_credential() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};
        use pharos_core::{SecretString, TokenStore, UserId, UserPolicy, UserRecord, UserStore};
        let stores = Stores::connect("sqlite::memory:").await.unwrap();
        let hash = crate::auth::BuiltinAuth::new(stores.clone())
            .hash_password(&SecretString::new("p"))
            .unwrap();
        let id = UserId::new();
        stores
            .create(UserRecord {
                id,
                name: "owner".into(),
                password_hash: hash,
                policy: UserPolicy::default(),
            })
            .await
            .unwrap();
        let playback = stores
            .issue_with_ttl(id, "cast:Den", 3600, TokenScope::Playback)
            .await
            .unwrap()
            .secret;
        let full = stores.issue(id, "web").await.unwrap();

        let recorder = DebuggingRecorder::new();
        let snapshotter = pharos_test_metrics::snapshotter(&recorder);
        let refused = {
            let _guard = metrics::set_default_local_recorder(&recorder);
            user_for_token(&stores, playback.0.expose()).await
        };
        assert_eq!(refused.unwrap_err(), TokenRefusal::ScopeRestricted);
        let counted = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .find(|(k, ..)| {
                k.key().name() == "pharos_token_scope_refused_total"
                    && k.key()
                        .labels()
                        .any(|l| l.key() == "scope" && l.value() == "playback")
            });
        assert!(
            matches!(counted, Some((.., DebugValue::Counter(1)))),
            "{counted:?}"
        );

        assert_eq!(
            user_for_token(&stores, full.0.expose()).await.unwrap().id,
            id
        );
        let (user, scope) = scoped_user_for_token(&stores, playback.0.expose())
            .await
            .unwrap();
        assert_eq!((user.id, scope), (id, TokenScope::Playback));
    }
}
