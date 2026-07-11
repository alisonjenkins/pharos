//! Phase B2 — background-work leadership election.
//!
//! SQLite always leads (single writer). Under Postgres the advisory lock is
//! exclusive across sessions: one replica wins, others get `None` until the
//! winner's lease drops (process exit / connection close).

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn sqlite_always_leads() {
    let s = pharos_store_sqlx::sqlite::SqliteStore::connect("sqlite::memory:")
        .await
        .unwrap();
    assert!(
        s.try_acquire_bg_leadership().await.unwrap().is_some(),
        "sqlite is single-writer; it must always win leadership"
    );
}

#[cfg(feature = "postgres")]
#[tokio::test]
async fn postgres_advisory_lock_is_exclusive() {
    let Ok(url) = std::env::var("PHAROS_TEST_POSTGRES_URL") else {
        eprintln!("SKIP postgres_advisory_lock_is_exclusive: PHAROS_TEST_POSTGRES_URL unset");
        return;
    };
    // Two independent stores (separate pools = separate sessions) against the
    // same database, modelling two replicas.
    let a = pharos_store_sqlx::postgres::PostgresStore::connect(&url)
        .await
        .unwrap();
    let b = pharos_store_sqlx::postgres::PostgresStore::connect(&url)
        .await
        .unwrap();

    // A wins; while A holds the lease, B is locked out.
    let lease_a = a.try_acquire_bg_leadership().await.unwrap();
    assert!(lease_a.is_some(), "first replica must win leadership");
    assert!(
        b.try_acquire_bg_leadership().await.unwrap().is_none(),
        "second replica must be locked out while the first leads"
    );

    // A's lease drops → the lock releases → B can now take over.
    drop(lease_a);
    // The connection returns to A's pool and the advisory lock is released
    // with it; give the pool a moment to reclaim it.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(
        b.try_acquire_bg_leadership().await.unwrap().is_some(),
        "after the leader's lease drops, another replica must take over"
    );
}
