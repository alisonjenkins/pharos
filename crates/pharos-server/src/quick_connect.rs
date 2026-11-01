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
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};

/// How long a pending request lives before it's GC'd.
pub const PENDING_TTL: Duration = Duration::from_secs(300);

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
}

impl QcRefusal {
    pub fn label(self) -> &'static str {
        match self {
            Self::RngFailed => "rng_failed",
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
        reply: oneshot::Sender<Result<PendingRequest, QcRefusal>>,
    },
    Authorize {
        code: String,
        by: UserId,
        reply: oneshot::Sender<bool>,
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
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                gc_expired(&mut by_secret, &mut by_code);
                match msg {
                    QcMsg::Initiate {
                        device_id,
                        device_name,
                        app_name,
                        app_version,
                        reply,
                    } => {
                        let entry = match mint_pending(
                            device_id,
                            device_name,
                            app_name,
                            app_version,
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
                        let mut ok = false;
                        if let Some(secret) = by_code.get(&code).cloned() {
                            if let Some(entry) = by_secret.get_mut(&secret) {
                                entry.authorized_by = Some(by);
                                ok = true;
                            }
                        }
                        tracing::info!(
                            code = %code,
                            authorized = ok,
                            live_codes = by_code.len(),
                            "quickconnect: authorize {}",
                            if ok { "matched" } else { "UNKNOWN CODE" }
                        );
                        let _ = reply.send(ok);
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
        assert!(rx.await.unwrap(), "authorize should succeed");

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
        assert!(!rx.await.unwrap());
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
        let labels = [QcRefusal::RngFailed.label()];
        assert_eq!(labels, ["rng_failed"]);
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
        };
        assert!(entry.expired(Instant::now()));
    }
}
