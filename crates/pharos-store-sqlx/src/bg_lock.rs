//! Singleton background-work leadership (Phase B2, zero-downtime deploys).
//!
//! When >1 replica runs against one Postgres (the rolling-deploy surge
//! window), exactly one must run the DB-writing background loops — library
//! rescan, trickplay / subtitle warm, image janitor — or they duplicate work
//! and contend on shared-storage I/O. A Postgres session-scoped advisory
//! lock elects that replica: whoever holds it leads; the rest serve reads.
//!
//! SQLite is always single-writer (single replica), so its holder is
//! unconditional.
//!
//! `try_acquire_bg_leadership` is a single non-blocking attempt — the retry
//! cadence lives in the caller (`pharos-server`), which owns a timer. On
//! success it returns a [`BgLeadership`] guard; for Postgres that guard owns
//! the connection holding the lock, so dropping it (process exit) releases
//! the lock and lets another replica take over.

use crate::StoreError;

/// Opaque guard: while it lives, this process holds background-work
/// leadership. For Postgres it owns the *detached* connection that holds the
/// advisory lock — detached so that dropping the guard CLOSES the session
/// (releasing the session-scoped lock) rather than returning the connection
/// to the pool with the lock still held. Process exit therefore hands
/// leadership to another replica.
pub struct BgLeadership {
    #[cfg(feature = "postgres")]
    _conn: Option<sqlx::PgConnection>,
}

/// Fixed 64-bit advisory-lock key for the background-work singleton. Picked
/// once; never reused for any other advisory lock.
#[cfg(feature = "postgres")]
const BG_LOCK_KEY: i64 = 0x7048_4152_4f53_4247; // "pHAROSBG"

#[cfg(feature = "sqlite")]
impl crate::sqlite::SqliteStore {
    /// SQLite is single-writer: the sole replica always leads.
    pub async fn try_acquire_bg_leadership(&self) -> Result<Option<BgLeadership>, StoreError> {
        Ok(Some(BgLeadership {
            #[cfg(feature = "postgres")]
            _conn: None,
        }))
    }
}

#[cfg(feature = "postgres")]
impl crate::postgres::PostgresStore {
    /// One non-blocking `pg_try_advisory_lock` attempt on a dedicated pooled
    /// connection. `Some` = we won and the returned guard holds the lock;
    /// `None` = another replica holds it (the connection is returned to the
    /// pool, releasing nothing since we never locked).
    pub async fn try_acquire_bg_leadership(&self) -> Result<Option<BgLeadership>, StoreError> {
        // Detach from the pool: this connection must own the lock for as long
        // as the guard lives and CLOSE (not recycle) on drop, so the lock is
        // released on process exit / voluntary drop rather than lingering on a
        // pooled connection.
        let mut conn = self
            .pool()
            .acquire()
            .await
            .map_err(StoreError::Sqlx)?
            .detach();
        let (locked,): (bool,) = sqlx::query_as("SELECT pg_try_advisory_lock($1)")
            .bind(BG_LOCK_KEY)
            .fetch_one(&mut conn)
            .await
            .map_err(StoreError::Sqlx)?;
        if locked {
            Ok(Some(BgLeadership { _conn: Some(conn) }))
        } else {
            Ok(None)
        }
    }
}

#[cfg(all(feature = "sqlite", feature = "postgres"))]
impl crate::any::AnyStore {
    pub async fn try_acquire_bg_leadership(&self) -> Result<Option<BgLeadership>, StoreError> {
        match self {
            crate::any::AnyStore::Sqlite(s) => s.try_acquire_bg_leadership().await,
            crate::any::AnyStore::Postgres(p) => p.try_acquire_bg_leadership().await,
        }
    }
}
