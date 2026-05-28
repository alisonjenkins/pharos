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
//! 3. Client polls `/QuickConnect/Connect?Secret=…`. While
//!    `Authenticated:false` they keep polling; once `true`, the
//!    response carries the `AccessToken` issued via `TokenStore::issue`
//!    against the authorizing user. The pending request is then
//!    consumed (one-shot).
//!
//! State lives in an in-memory `QuickConnectRegistry` actor (no DB
//! persistence — pending requests die with the process; that's fine
//! because the TTL is short and the user just retries).

use pharos_core::{TokenStore, UserId};
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
    pub created_at: Instant,
    /// Set once an authorized user vouches for the code.
    pub authorized_by: Option<UserId>,
}

impl PendingRequest {
    fn expired(&self, now: Instant) -> bool {
        now.duration_since(self.created_at) > PENDING_TTL
    }
}

#[derive(Debug)]
pub enum QcMsg {
    Initiate {
        device_id: String,
        reply: oneshot::Sender<PendingRequest>,
    },
    Authorize {
        code: String,
        by: UserId,
        reply: oneshot::Sender<bool>,
    },
    Connect {
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
                    QcMsg::Initiate { device_id, reply } => {
                        let entry = mint_pending(device_id);
                        by_code.insert(entry.code.clone(), entry.secret.clone());
                        by_secret.insert(entry.secret.clone(), entry.clone());
                        let _ = reply.send(entry);
                    }
                    QcMsg::Authorize { code, by, reply } => {
                        let mut ok = false;
                        if let Some(secret) = by_code.get(&code).cloned() {
                            if let Some(entry) = by_secret.get_mut(&secret) {
                                entry.authorized_by = Some(by);
                                ok = true;
                            }
                        }
                        let _ = reply.send(ok);
                    }
                    QcMsg::Connect { secret, reply } => {
                        let result = by_secret.get(&secret).cloned();
                        // If the request is now authorized + we've
                        // surfaced the user id, consume it so it can't
                        // be reused (V8 — single-shot exchange).
                        if let Some(ref entry) = result {
                            if entry.authorized_by.is_some() {
                                by_secret.remove(&secret);
                                by_code.remove(&entry.code);
                            }
                        }
                        let _ = reply.send(result);
                    }
                    QcMsg::Gc => {}
                }
            }
        });
        Self { tx }
    }
}

fn mint_pending(device_id: String) -> PendingRequest {
    let now = Instant::now();
    let code = generate_code();
    let secret = generate_secret();
    PendingRequest {
        code,
        secret,
        device_id,
        created_at: now,
        authorized_by: None,
    }
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

/// Six-digit numeric code the user reads aloud. Uses `xxh3` as a
/// cheap RNG seeded by wall-clock — no crypto-strength needed since
/// the code is paired with a much larger Secret on the wire.
fn generate_code() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let n = xxhash_rust::xxh3::xxh3_64(&ns.to_le_bytes()) % 1_000_000;
    format!("{n:06}")
}

/// Crypto-style random secret. Uses `uuid::Uuid::new_v4().simple()`.
fn generate_secret() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// Helper for handlers — issues an `AccessToken` against `user`'s
/// account once Connect resolves with an authorized pending request.
pub async fn issue_token<T: TokenStore>(
    tokens: &T,
    user: UserId,
    device_id: &str,
) -> Result<String, String> {
    let t = tokens
        .issue(user, device_id)
        .await
        .map_err(|e| e.to_string())?;
    Ok(t.0.expose().to_string())
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
                reply: tx,
            })
            .await
            .unwrap();
        let entry = rx.await.unwrap();
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

        // Connect after authorize → consumes the entry.
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

        // Subsequent connect with the same secret returns None
        // (one-shot consumption).
        let (tx, rx) = oneshot::channel();
        reg.tx
            .send(QcMsg::Connect {
                secret: entry.secret.clone(),
                reply: tx,
            })
            .await
            .unwrap();
        assert!(rx.await.unwrap().is_none());
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
    fn pending_expired_after_ttl() {
        let entry = PendingRequest {
            code: "000000".into(),
            secret: "x".into(),
            device_id: "d".into(),
            created_at: Instant::now() - Duration::from_secs(400),
            authorized_by: None,
        };
        assert!(entry.expired(Instant::now()));
    }
}
