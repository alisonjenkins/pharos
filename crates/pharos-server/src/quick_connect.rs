//! Jellyfin QuickConnect server-side flow.
//!
//! Three steps:
//! 1. Unauthenticated client POSTs `/QuickConnect/Initiate` and gets
//!    back `{Code, Secret, DeviceId}`. Code is 6 digits the user reads
//!    aloud + types on a *signed-in* device.
//! 2. Signed-in user POSTs `/QuickConnect/Authorize?Code=…` (admin
//!    endpoint in jellyfin-web; pharos gates on any authenticated
//!    user — a non-admin can vouch only for themselves). This marks
//!    the pending request as authorized + records the bearer's user_id.
//! 3. Client polls `/QuickConnect/Connect?Secret=…` (READ-ONLY). While
//!    `Authenticated:false` they keep polling; once `true`, the response
//!    echoes the `Secret` back but mints no token — the poll never
//!    consumes the request.
//! 4. Client finalizes at `POST /Users/AuthenticateWithQuickConnect`
//!    with `{Secret}` (see `api::jellyfin::users`). That step consumes
//!    the authorized request (one-shot, via [`QcMsg::Consume`]) and
//!    issues the `AccessToken` against the authorizing user. This split
//!    mirrors jellyfin-web, which polls Connect then calls
//!    `apiClient.quickConnect(secret)` → AuthenticateWithQuickConnect.
//!
//! State lives in an in-memory `QuickConnectRegistry` actor (no DB
//! persistence — pending requests die with the process; that's fine
//! because the TTL is short and the user just retries).

use pharos_core::UserId;
use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};

/// How long a pending request lives before it's GC'd.
pub const PENDING_TTL: Duration = Duration::from_secs(300);

/// Live pending requests the registry will hold. Initiate is unauthenticated,
/// so without a bound anyone could grow the map until the process OOMs; 1024
/// is far above any household's concurrent sign-ins.
pub const MAX_PENDING: usize = 1024;

/// Live pending requests one client address (IPv6: one /64) may hold. A TV
/// that retries a few times is fine; a flood from one host is not.
pub const MAX_PENDING_PER_IP: usize = 8;

/// Unknown-code Authorize attempts one signed-in user may make within
/// [`PENDING_TTL`]. The code space is 1M and a code lives 5 minutes, so an
/// unthrottled user could enumerate it; 10 leaves room for typos.
pub const MAX_AUTHORIZE_FAILURES: usize = 10;

/// `Retry-After` sent with a cap refusal: long enough for a stuck request to
/// be consumed or a client to back off, short enough for a retrying TV.
pub const CAP_RETRY_AFTER_SECS: u64 = 30;

#[derive(Debug, Clone)]
pub struct PendingRequest {
    pub code: String,
    pub secret: String,
    pub device_id: String,
    /// The initiating client's identity, echoed on BOTH Initiate and the
    /// Connect poll. Non-nullable strings in Jellyfin's `QuickConnectResult`:
    /// the Android/Google-TV kotlin SDK rejects a poll response missing them,
    /// which makes the TV hide the code seconds after it appears (B61).
    pub device_name: String,
    pub app_name: String,
    pub app_version: String,
    pub created_at: Instant,
    /// Wall-clock creation time (unix seconds) — the wire `DateAdded`.
    /// Jellyfin's C# QuickConnectResult marks DateAdded as a non-nullable
    /// DateTime, so BOTH Initiate and the Connect poll must carry it (the
    /// kotlin SDK rejects the poll response without it).
    pub created_unix_secs: i64,
    /// Set once an authorized user vouches for the code.
    pub authorized_by: Option<UserId>,
    /// Initiating client address (IPv6 on its /64), for the per-address cap.
    pub client: IpAddr,
}

impl PendingRequest {
    fn expired(&self, now: Instant) -> bool {
        now.duration_since(self.created_at) > PENDING_TTL
    }
}

/// Source of random bytes; `getrandom::fill` in production, a stub in tests.
pub type FillRandom = fn(&mut [u8]) -> Result<(), getrandom::Error>;

/// Why the registry refused a request. The label is a dashboard contract
/// (`pharos_quick_connect_refused_total{reason}`): keep strings stable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QcRefusal {
    /// The OS CSPRNG failed, so no unpredictable code/secret can be minted.
    RngFailed,
    /// [`MAX_PENDING`] live requests already exist.
    GlobalCap,
    /// The client address already holds [`MAX_PENDING_PER_IP`] live requests.
    IpCap,
    /// The authorizing user guessed [`MAX_AUTHORIZE_FAILURES`] unknown codes
    /// within [`PENDING_TTL`].
    AuthorizeThrottled { retry_after_secs: u64 },
}

impl QcRefusal {
    pub fn label(self) -> &'static str {
        match self {
            Self::RngFailed => "rng_failed",
            Self::GlobalCap => "global_cap",
            Self::IpCap => "ip_cap",
            Self::AuthorizeThrottled { .. } => "authorize_throttled",
        }
    }

    fn record(self) {
        metrics::counter!("pharos_quick_connect_refused_total", "reason" => self.label())
            .increment(1);
    }
}

#[derive(Debug)]
pub enum QcMsg {
    Initiate {
        device_id: String,
        device_name: String,
        app_name: String,
        app_version: String,
        client_ip: IpAddr,
        reply: oneshot::Sender<Result<PendingRequest, QcRefusal>>,
    },
    Authorize {
        code: String,
        by: UserId,
        reply: oneshot::Sender<Result<bool, QcRefusal>>,
    },
    /// Poll for a pending request's state. READ-ONLY — never consumes the
    /// entry: jellyfin-web polls this every 5s and, once `Authenticated`,
    /// finalizes via a SEPARATE `/Users/AuthenticateWithQuickConnect` call
    /// which is where one-shot consumption happens ([`QcMsg::Consume`]).
    Connect {
        secret: String,
        reply: oneshot::Sender<Option<PendingRequest>>,
    },
    /// Finalize: return + REMOVE the pending request, but only if it has been
    /// authorized (single-shot exchange). An unauthorized/unknown secret
    /// yields `None` and leaves any entry in place (client keeps polling).
    Consume {
        secret: String,
        reply: oneshot::Sender<Option<PendingRequest>>,
    },
    /// Drop entries past their TTL. Called periodically + on every
    /// other op for cheap eager cleanup.
    Gc,
}

/// Failed (unknown-code) Authorize attempts per authorizing user, as a
/// sliding window of [`PENDING_TTL`]. Only failures are recorded; a success
/// resets nothing, so interleaving a valid code cannot buy more guesses.
#[derive(Default)]
struct AuthorizeFailures {
    by_user: HashMap<UserId, VecDeque<Instant>>,
}

impl AuthorizeFailures {
    fn prune(&mut self, now: Instant) {
        self.by_user.retain(|_, q| {
            while q
                .front()
                .is_some_and(|t| now.duration_since(*t) > PENDING_TTL)
            {
                q.pop_front();
            }
            !q.is_empty()
        });
    }

    /// Time until `user` may try again, or `None` if they may now.
    fn blocked(&mut self, user: UserId, now: Instant) -> Option<Duration> {
        self.prune(now);
        let q = self.by_user.get(&user)?;
        if q.len() < MAX_AUTHORIZE_FAILURES {
            return None;
        }
        let oldest = *q.front()?;
        Some(PENDING_TTL.saturating_sub(now.duration_since(oldest)))
    }

    fn record(&mut self, user: UserId, now: Instant) {
        self.by_user.entry(user).or_default().push_back(now);
    }

    fn count(&self, user: UserId) -> usize {
        self.by_user.get(&user).map_or(0, VecDeque::len)
    }
}

#[derive(Clone)]
pub struct QuickConnectRegistry {
    pub tx: mpsc::Sender<QcMsg>,
}

impl QuickConnectRegistry {
    pub fn spawn() -> Self {
        Self::spawn_with(getrandom::fill)
    }

    pub fn spawn_with(fill: FillRandom) -> Self {
        let (tx, mut rx) = mpsc::channel::<QcMsg>(64);
        // `by_secret` is the polling lookup. `by_code` is the
        // Authorize lookup. Both index into the same logical record;
        // we hold two HashMaps so neither path scans the other.
        let mut by_secret: HashMap<String, PendingRequest> = HashMap::new();
        let mut by_code: HashMap<String, String> = HashMap::new();
        let mut authorize_failures = AuthorizeFailures::default();
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                gc_expired(&mut by_secret, &mut by_code);
                match msg {
                    QcMsg::Initiate {
                        device_id,
                        device_name,
                        app_name,
                        app_version,
                        client_ip,
                        reply,
                    } => {
                        let client = crate::auth::ip_key(client_ip);
                        let ip_live = by_secret.values().filter(|e| e.client == client).count();
                        let cap_hit = if ip_live >= MAX_PENDING_PER_IP {
                            Some((QcRefusal::IpCap, ip_live, MAX_PENDING_PER_IP))
                        } else if by_secret.len() >= MAX_PENDING {
                            Some((QcRefusal::GlobalCap, by_secret.len(), MAX_PENDING))
                        } else {
                            None
                        };
                        if let Some((refusal, live, cap)) = cap_hit {
                            refusal.record();
                            tracing::warn!(
                                reason = refusal.label(),
                                client_ip = %client,
                                live,
                                cap,
                                "quickconnect: initiate refused, pending cap reached"
                            );
                            let _ = reply.send(Err(refusal));
                            continue;
                        }
                        let entry = match mint_pending(
                            device_id,
                            device_name,
                            app_name,
                            app_version,
                            client,
                            &by_code,
                            fill,
                        ) {
                            Ok(entry) => entry,
                            Err(e) => {
                                QcRefusal::RngFailed.record();
                                tracing::error!(
                                    error = %e,
                                    live_pending = by_secret.len(),
                                    "quickconnect: initiate refused, CSPRNG failed"
                                );
                                let _ = reply.send(Err(QcRefusal::RngFailed));
                                continue;
                            }
                        };
                        by_code.insert(entry.code.clone(), entry.secret.clone());
                        by_secret.insert(entry.secret.clone(), entry.clone());
                        // B62 — QC observability: the mint was a total blind spot
                        // (no log), so a code/secret that later 404s couldn't be
                        // correlated to its Initiate. Log code + secret PREFIX
                        // (never the full secret) + device + the live pool size.
                        tracing::info!(
                            code = %entry.code,
                            secret.prefix = %secret_prefix(&entry.secret),
                            device.id = %entry.device_id,
                            device.name = %entry.device_name,
                            live_pending = by_secret.len(),
                            "quickconnect: initiated (code minted)"
                        );
                        let _ = reply.send(Ok(entry));
                    }
                    QcMsg::Authorize { code, by, reply } => {
                        let now = Instant::now();
                        if let Some(wait) = authorize_failures.blocked(by, now) {
                            let refusal = QcRefusal::AuthorizeThrottled {
                                retry_after_secs: wait.as_secs().saturating_add(1),
                            };
                            refusal.record();
                            tracing::warn!(
                                reason = refusal.label(),
                                user = %by,
                                failures = authorize_failures.count(by),
                                cap = MAX_AUTHORIZE_FAILURES,
                                retry_after_secs = wait.as_secs().saturating_add(1),
                                "quickconnect: authorize refused, too many unknown codes"
                            );
                            let _ = reply.send(Err(refusal));
                            continue;
                        }
                        let mut ok = false;
                        if let Some(secret) = by_code.get(&code).cloned() {
                            if let Some(entry) = by_secret.get_mut(&secret) {
                                entry.authorized_by = Some(by);
                                ok = true;
                            }
                        }
                        if !ok {
                            authorize_failures.record(by, now);
                        }
                        tracing::info!(
                            code = %code,
                            authorized = ok,
                            live_codes = by_code.len(),
                            "quickconnect: authorize {}",
                            if ok { "matched" } else { "UNKNOWN CODE" }
                        );
                        let _ = reply.send(Ok(ok));
                    }
                    QcMsg::Connect { secret, reply } => {
                        // Read-only poll: jellyfin-web reads `Authenticated`
                        // here but finalizes via /Users/AuthenticateWithQuick
                        // Connect (QcMsg::Consume), so the entry MUST survive
                        // this call — consuming here would delete the record
                        // before the finalize could find it.
                        let hit = by_secret.get(&secret).cloned();
                        tracing::debug!(
                            secret.prefix = %secret_prefix(&secret),
                            found = hit.is_some(),
                            authenticated = hit.as_ref().map(|e| e.authorized_by.is_some()),
                            live_pending = by_secret.len(),
                            "quickconnect: connect poll"
                        );
                        let _ = reply.send(hit);
                    }
                    QcMsg::Consume { secret, reply } => {
                        // Single-shot: hand back + remove the record only once
                        // it's authorized. Unauthorized/unknown → None, entry
                        // left in place so the client can keep polling.
                        let present = by_secret.get(&secret).cloned();
                        let result = present
                            .as_ref()
                            .and_then(|e| e.authorized_by.is_some().then(|| e.clone()));
                        if let Some(ref entry) = result {
                            by_secret.remove(&secret);
                            by_code.remove(&entry.code);
                        }
                        tracing::info!(
                            secret.prefix = %secret_prefix(&secret),
                            secret_known = present.is_some(),
                            was_authorized = present.as_ref().map(|e| e.authorized_by.is_some()),
                            consumed = result.is_some(),
                            "quickconnect: finalize (AuthenticateWithQuickConnect)"
                        );
                        let _ = reply.send(result);
                    }
                    QcMsg::Gc => {}
                }
            }
        });
        Self { tx }
    }
}

fn mint_pending(
    device_id: String,
    device_name: String,
    app_name: String,
    app_version: String,
    client: IpAddr,
    by_code: &HashMap<String, String>,
    fill: FillRandom,
) -> Result<PendingRequest, getrandom::Error> {
    let now = Instant::now();
    let created_unix_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    // V8/security: codes must be unique among *live* requests. Blindly
    // overwriting an existing code let an attacker spam Initiate to
    // collide a victim's code and bind the victim's later Authorize to
    // the attacker's secret → account takeover. Generate until free.
    let code = unique_code(by_code, fill)?;
    let secret = secret_from(&mut { fill })?;
    Ok(PendingRequest {
        code,
        secret,
        device_id,
        device_name,
        app_name,
        app_version,
        created_at: now,
        created_unix_secs,
        authorized_by: None,
        client,
    })
}

/// Draw codes until one is not currently live. The 6-digit space is 1M
/// and live requests are few (short TTL), so this terminates in ~1 draw;
/// the bound is a safety belt against pathological saturation.
fn unique_code(
    by_code: &HashMap<String, String>,
    fill: FillRandom,
) -> Result<String, getrandom::Error> {
    let mut fill = fill;
    for _ in 0..10_000 {
        let c = code_from(&mut fill)?;
        if !by_code.contains_key(&c) {
            return Ok(c);
        }
    }
    // Astronomically unlikely (would need ~1M live requests). Fall back
    // to a fresh draw rather than loop forever.
    code_from(&mut fill)
}

fn gc_expired(
    by_secret: &mut HashMap<String, PendingRequest>,
    by_code: &mut HashMap<String, String>,
) {
    let now = Instant::now();
    by_secret.retain(|_, e| {
        let alive = !e.expired(now);
        if !alive {
            by_code.remove(&e.code);
        }
        alive
    });
}

/// Largest multiple of 1_000_000 that fits in a `u32`; draws at or above it
/// are redrawn so `% 1_000_000` has no modulo bias.
const CODE_DRAW_LIMIT: u32 = 4_294_000_000;

/// Redraws before giving up on a (practically impossible) run of rejected
/// draws; each is rejected with probability ~1.6e-4.
const CODE_DRAW_ATTEMPTS: usize = 16;

/// Six-digit numeric code the user reads aloud. Drawn from a CSPRNG
/// (`getrandom`) — the old wall-clock `xxh3` seed was predictable, which
/// (combined with the collision overwrite, now fixed) enabled a code
/// pre-image / collision attack. Uniqueness is enforced by the caller
/// ([`unique_code`]); unpredictability is enforced here. A CSPRNG error is
/// returned, never papered over with a fixed code.
fn code_from(
    fill: &mut impl FnMut(&mut [u8]) -> Result<(), getrandom::Error>,
) -> Result<String, getrandom::Error> {
    for _ in 0..CODE_DRAW_ATTEMPTS {
        let mut b = [0u8; 4];
        fill(&mut b)?;
        let n = u32::from_le_bytes(b);
        if n < CODE_DRAW_LIMIT {
            return Ok(format!("{:06}", n % 1_000_000));
        }
    }
    Err(getrandom::Error::UNEXPECTED)
}

#[cfg(test)]
fn generate_code() -> Result<String, getrandom::Error> {
    code_from(&mut getrandom::fill)
}

/// Random 32-char hex secret in the UUIDv4 layout the clients have always seen.
fn secret_from(
    fill: &mut impl FnMut(&mut [u8]) -> Result<(), getrandom::Error>,
) -> Result<String, getrandom::Error> {
    let mut b = [0u8; 16];
    fill(&mut b)?;
    Ok(uuid::Builder::from_random_bytes(b)
        .into_uuid()
        .simple()
        .to_string())
}

/// First 8 chars of a secret, for correlating trace lines WITHOUT logging the
/// full credential (a full secret in logs would be a bearer-token leak).
fn secret_prefix(s: &str) -> &str {
    &s[..s.len().min(8)]
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[tokio::test]
    async fn initiate_authorize_connect_cycle() {
        let reg = QuickConnectRegistry::spawn();
        let (tx, rx) = oneshot::channel();
        reg.tx
            .send(QcMsg::Initiate {
                device_id: "dev-1".into(),
                device_name: "Living Room TV".into(),
                app_name: "Jellyfin Android TV".into(),
                app_version: "0.18".into(),
                client_ip: ip(1),
                reply: tx,
            })
            .await
            .unwrap();
        let entry = rx.await.unwrap().unwrap();
        assert_eq!(entry.code.len(), 6, "code is 6 digits");
        assert!(entry.code.chars().all(|c| c.is_ascii_digit()));
        assert_eq!(entry.secret.len(), 32, "secret is 32-char hex");

        // Connect before authorize → returns Some with no `by`.
        let (tx, rx) = oneshot::channel();
        reg.tx
            .send(QcMsg::Connect {
                secret: entry.secret.clone(),
                reply: tx,
            })
            .await
            .unwrap();
        let mid = rx.await.unwrap();
        assert!(mid.is_some());
        assert!(mid.unwrap().authorized_by.is_none());

        // Authorize.
        let by = UserId::new();
        let (tx, rx) = oneshot::channel();
        reg.tx
            .send(QcMsg::Authorize {
                code: entry.code.clone(),
                by,
                reply: tx,
            })
            .await
            .unwrap();
        assert!(rx.await.unwrap().unwrap(), "authorize should succeed");

        // Connect after authorize → reports authorized but is READ-ONLY:
        // the client polls this repeatedly, so it must not consume.
        for _ in 0..2 {
            let (tx, rx) = oneshot::channel();
            reg.tx
                .send(QcMsg::Connect {
                    secret: entry.secret.clone(),
                    reply: tx,
                })
                .await
                .unwrap();
            let resolved = rx.await.unwrap().unwrap();
            assert_eq!(resolved.authorized_by, Some(by));
        }

        // Consume (the /Users/AuthenticateWithQuickConnect step) resolves the
        // authorized request once, then it's gone (single-shot exchange).
        let (tx, rx) = oneshot::channel();
        reg.tx
            .send(QcMsg::Consume {
                secret: entry.secret.clone(),
                reply: tx,
            })
            .await
            .unwrap();
        assert_eq!(rx.await.unwrap().unwrap().authorized_by, Some(by));

        // Second consume with the same secret → None (already spent).
        let (tx, rx) = oneshot::channel();
        reg.tx
            .send(QcMsg::Consume {
                secret: entry.secret.clone(),
                reply: tx,
            })
            .await
            .unwrap();
        assert!(rx.await.unwrap().is_none());
    }

    #[tokio::test]
    async fn consume_before_authorize_returns_none_and_keeps_entry() {
        let reg = QuickConnectRegistry::spawn();
        let (tx, rx) = oneshot::channel();
        reg.tx
            .send(QcMsg::Initiate {
                device_id: "dev-1".into(),
                device_name: "Living Room TV".into(),
                app_name: "Jellyfin Android TV".into(),
                app_version: "0.18".into(),
                client_ip: ip(1),
                reply: tx,
            })
            .await
            .unwrap();
        let entry = rx.await.unwrap().unwrap();

        // Not yet authorized → Consume yields None and must NOT delete the
        // entry (the client is still polling / about to be authorized).
        let (tx, rx) = oneshot::channel();
        reg.tx
            .send(QcMsg::Consume {
                secret: entry.secret.clone(),
                reply: tx,
            })
            .await
            .unwrap();
        assert!(rx.await.unwrap().is_none());

        // Still there: a Connect poll finds it.
        let (tx, rx) = oneshot::channel();
        reg.tx
            .send(QcMsg::Connect {
                secret: entry.secret.clone(),
                reply: tx,
            })
            .await
            .unwrap();
        assert!(
            rx.await.unwrap().is_some(),
            "unauthorized Consume must not delete the entry"
        );
    }

    #[tokio::test]
    async fn authorize_unknown_code_returns_false() {
        let reg = QuickConnectRegistry::spawn();
        let (tx, rx) = oneshot::channel();
        reg.tx
            .send(QcMsg::Authorize {
                code: "999999".into(),
                by: UserId::new(),
                reply: tx,
            })
            .await
            .unwrap();
        assert!(!rx.await.unwrap().unwrap());
    }

    #[tokio::test]
    async fn connect_unknown_secret_returns_none() {
        let reg = QuickConnectRegistry::spawn();
        let (tx, rx) = oneshot::channel();
        reg.tx
            .send(QcMsg::Connect {
                secret: "nope".into(),
                reply: tx,
            })
            .await
            .unwrap();
        assert!(rx.await.unwrap().is_none());
    }

    #[test]
    fn codes_are_unique_across_many_initiates() {
        // Security regression: distinct Initiate calls must never collide
        // onto the same code (which previously let an attacker overwrite a
        // victim's code→secret mapping). With a 1M space + uniqueness loop,
        // a batch of mints must all differ.
        let mut by_code: HashMap<String, String> = HashMap::new();
        for _ in 0..2000 {
            let e = mint_pending(
                "d".into(),
                "tv".into(),
                "app".into(),
                "1.0".into(),
                ip(1),
                &by_code,
                getrandom::fill,
            )
            .unwrap();
            assert!(
                !by_code.contains_key(&e.code),
                "mint produced a colliding code"
            );
            by_code.insert(e.code.clone(), e.secret.clone());
        }
        assert_eq!(by_code.len(), 2000);
    }

    fn ip(n: u8) -> IpAddr {
        IpAddr::from([192, 0, 2, n])
    }

    async fn initiate_from(
        reg: &QuickConnectRegistry,
        client_ip: IpAddr,
    ) -> Result<PendingRequest, QcRefusal> {
        let (tx, rx) = oneshot::channel();
        reg.tx
            .send(QcMsg::Initiate {
                device_id: "d".into(),
                device_name: "tv".into(),
                app_name: "app".into(),
                app_version: "1".into(),
                client_ip,
                reply: tx,
            })
            .await
            .unwrap();
        rx.await.unwrap()
    }

    fn refused_count(rec: &metrics_util::debugging::DebuggingRecorder, reason: &str) -> u64 {
        use metrics_util::debugging::DebugValue;
        pharos_test_metrics::snapshotter(rec)
            .snapshot()
            .into_vec()
            .into_iter()
            .find_map(|(ck, _, _, v)| {
                let hit = ck.key().name() == "pharos_quick_connect_refused_total"
                    && ck
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

    #[tokio::test]
    async fn one_address_cannot_hold_more_than_the_per_ip_cap() {
        let rec = metrics_util::debugging::DebuggingRecorder::new();
        let _g = metrics::set_default_local_recorder(&rec);
        let reg = QuickConnectRegistry::spawn();
        for _ in 0..MAX_PENDING_PER_IP {
            initiate_from(&reg, ip(1)).await.unwrap();
        }
        assert_eq!(
            initiate_from(&reg, ip(1)).await.unwrap_err(),
            QcRefusal::IpCap
        );
        // another client is unaffected
        initiate_from(&reg, ip(2)).await.unwrap();
        assert_eq!(refused_count(&rec, "ip_cap"), 1);
    }

    #[tokio::test]
    async fn ipv6_clients_share_the_cap_across_their_slash_64() {
        let reg = QuickConnectRegistry::spawn();
        let a: IpAddr = "2001:db8:1:2::1".parse().unwrap();
        for i in 0..MAX_PENDING_PER_IP {
            let b: IpAddr = format!("2001:db8:1:2::{:x}", i + 10).parse().unwrap();
            initiate_from(&reg, b).await.unwrap();
        }
        assert_eq!(initiate_from(&reg, a).await.unwrap_err(), QcRefusal::IpCap);
    }

    #[tokio::test]
    async fn the_pool_is_bounded_globally() {
        let rec = metrics_util::debugging::DebuggingRecorder::new();
        let _g = metrics::set_default_local_recorder(&rec);
        let reg = QuickConnectRegistry::spawn();
        for i in 0..MAX_PENDING {
            // spread over IPv4 so the per-IP cap is never the one that trips
            let o = i / MAX_PENDING_PER_IP;
            let client = IpAddr::from([10, (o / 256) as u8, (o % 256) as u8, 1]);
            initiate_from(&reg, client).await.unwrap();
        }
        assert_eq!(
            initiate_from(&reg, ip(200)).await.unwrap_err(),
            QcRefusal::GlobalCap
        );
        assert_eq!(refused_count(&rec, "global_cap"), 1);
    }

    #[tokio::test]
    async fn consuming_frees_a_slot_under_the_cap() {
        let reg = QuickConnectRegistry::spawn();
        let mut first = None;
        for _ in 0..MAX_PENDING_PER_IP {
            first.get_or_insert(initiate_from(&reg, ip(1)).await.unwrap());
        }
        let first = first.unwrap();
        let (tx, rx) = oneshot::channel();
        reg.tx
            .send(QcMsg::Authorize {
                code: first.code.clone(),
                by: UserId::new(),
                reply: tx,
            })
            .await
            .unwrap();
        assert!(rx.await.unwrap().unwrap());
        let (tx, rx) = oneshot::channel();
        reg.tx
            .send(QcMsg::Consume {
                secret: first.secret,
                reply: tx,
            })
            .await
            .unwrap();
        assert!(rx.await.unwrap().is_some());
        initiate_from(&reg, ip(1)).await.unwrap();
    }

    #[tokio::test]
    async fn cap_refusal_warns_with_the_offending_values() {
        #[derive(Clone, Default)]
        struct Buf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
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
        let buf = Buf::default();
        let sub = tracing_subscriber::fmt()
            .with_writer(buf.clone())
            .with_ansi(false)
            .finish();
        let _g = tracing::subscriber::set_default(sub);
        let reg = QuickConnectRegistry::spawn();
        for _ in 0..=MAX_PENDING_PER_IP {
            let _ = initiate_from(&reg, ip(7)).await;
        }
        let log = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        let warn = log
            .lines()
            .find(|l| l.contains("WARN"))
            .unwrap_or_else(|| panic!("no WARN in {log}"));
        for want in [
            "reason=\"ip_cap\"",
            "client_ip=192.0.2.7",
            "live=8",
            "cap=8",
        ] {
            assert!(warn.contains(want), "missing {want} in {warn}");
        }
    }

    async fn authorize_as(
        reg: &QuickConnectRegistry,
        code: &str,
        by: UserId,
    ) -> Result<bool, QcRefusal> {
        let (tx, rx) = oneshot::channel();
        reg.tx
            .send(QcMsg::Authorize {
                code: code.into(),
                by,
                reply: tx,
            })
            .await
            .unwrap();
        rx.await.unwrap()
    }

    #[test]
    fn failure_window_blocks_at_the_limit_and_clears_after_the_ttl() {
        let mut f = AuthorizeFailures::default();
        let u = UserId::new();
        let t0 = Instant::now();
        for _ in 0..MAX_AUTHORIZE_FAILURES {
            assert!(f.blocked(u, t0).is_none());
            f.record(u, t0);
        }
        assert_eq!(f.blocked(u, t0), Some(PENDING_TTL));
        assert_eq!(
            f.blocked(u, t0 + Duration::from_secs(100)),
            Some(PENDING_TTL - Duration::from_secs(100))
        );
        assert!(f
            .blocked(u, t0 + PENDING_TTL + Duration::from_secs(1))
            .is_none());
        assert!(
            f.by_user.is_empty(),
            "expired window is dropped, not leaked"
        );
    }

    #[test]
    fn failure_windows_are_per_user() {
        let mut f = AuthorizeFailures::default();
        let (a, b) = (UserId::new(), UserId::new());
        let t0 = Instant::now();
        for _ in 0..MAX_AUTHORIZE_FAILURES {
            f.record(a, t0);
        }
        assert!(f.blocked(a, t0).is_some());
        assert!(f.blocked(b, t0).is_none());
    }

    #[tokio::test]
    async fn guessing_codes_is_throttled_per_user_and_a_success_resets_nobody() {
        let rec = metrics_util::debugging::DebuggingRecorder::new();
        let _g = metrics::set_default_local_recorder(&rec);
        let reg = QuickConnectRegistry::spawn();
        let (a, b) = (UserId::new(), UserId::new());
        let real = initiate_from(&reg, ip(1)).await.unwrap();

        for _ in 0..MAX_AUTHORIZE_FAILURES - 1 {
            assert!(!authorize_as(&reg, "999999", a).await.unwrap());
        }
        // a's own success, and b's, must not wipe a's count
        assert!(authorize_as(&reg, &real.code, a).await.unwrap());
        assert!(authorize_as(&reg, &real.code, b).await.unwrap());
        assert!(!authorize_as(&reg, "999999", a).await.unwrap());

        // a is now at the limit: even a correct code is refused
        let err = authorize_as(&reg, &real.code, a).await.unwrap_err();
        assert!(
            matches!(err, QcRefusal::AuthorizeThrottled { .. }),
            "{err:?}"
        );
        // b is untouched
        assert!(authorize_as(&reg, &real.code, b).await.unwrap());
        assert_eq!(refused_count(&rec, "authorize_throttled"), 1);
    }

    fn failing_fill(_: &mut [u8]) -> Result<(), getrandom::Error> {
        Err(getrandom::Error::UNSUPPORTED)
    }

    #[test]
    fn code_draw_fails_closed_when_the_rng_fails() {
        assert!(code_from(&mut failing_fill).is_err());
        assert!(secret_from(&mut failing_fill).is_err());
    }

    #[test]
    fn code_draw_rejects_the_biased_tail() {
        // 4_294_967_295 sits in the final partial block of u32 and would skew
        // low codes; it must be redrawn, not reduced.
        let mut draws = [u32::MAX, 42].into_iter();
        let mut fill = |b: &mut [u8]| {
            b.copy_from_slice(&draws.next().unwrap().to_le_bytes());
            Ok(())
        };
        assert_eq!(code_from(&mut fill).unwrap(), "000042");
    }

    #[tokio::test]
    async fn initiate_with_failing_rng_is_refused_and_counted() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};
        let recorder = DebuggingRecorder::new();
        let snapshotter = pharos_test_metrics::snapshotter(&recorder);
        let _guard = metrics::set_default_local_recorder(&recorder);

        let reg = QuickConnectRegistry::spawn_with(failing_fill);
        let (tx, rx) = oneshot::channel();
        reg.tx
            .send(QcMsg::Initiate {
                device_id: "d".into(),
                device_name: "tv".into(),
                app_name: "app".into(),
                app_version: "1".into(),
                client_ip: ip(1),
                reply: tx,
            })
            .await
            .unwrap();
        assert_eq!(rx.await.unwrap().unwrap_err(), QcRefusal::RngFailed);

        let n = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .find_map(|(ck, _, _, v)| {
                let hit = ck.key().name() == "pharos_quick_connect_refused_total"
                    && ck
                        .key()
                        .labels()
                        .any(|l| l.key() == "reason" && l.value() == "rng_failed");
                match v {
                    DebugValue::Counter(n) if hit => Some(n),
                    _ => None,
                }
            });
        assert_eq!(n, Some(1));
    }

    #[test]
    fn refusal_labels_are_distinct_and_stable() {
        let labels = [
            QcRefusal::RngFailed.label(),
            QcRefusal::GlobalCap.label(),
            QcRefusal::IpCap.label(),
            QcRefusal::AuthorizeThrottled {
                retry_after_secs: 1,
            }
            .label(),
        ];
        assert_eq!(
            labels,
            ["rng_failed", "global_cap", "ip_cap", "authorize_throttled"]
        );
    }

    #[test]
    fn generated_codes_are_six_digits() {
        for _ in 0..100 {
            let c = generate_code().unwrap();
            assert_eq!(c.len(), 6);
            assert!(c.chars().all(|c| c.is_ascii_digit()));
        }
    }

    #[test]
    fn pending_expired_after_ttl() {
        let entry = PendingRequest {
            code: "000000".into(),
            secret: "x".into(),
            device_id: "d".into(),
            device_name: "tv".into(),
            app_name: "app".into(),
            app_version: "1.0".into(),
            created_at: Instant::now() - Duration::from_secs(400),
            created_unix_secs: 0,
            authorized_by: None,
            client: ip(1),
        };
        assert!(entry.expired(Instant::now()));
    }
}
