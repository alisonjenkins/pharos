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

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn sqlite_owns_every_group() {
    let s = pharos_store_sqlx::sqlite::SqliteStore::connect("sqlite::memory:")
        .await
        .unwrap();
    assert!(s.try_acquire_group_ownership("g1").await.unwrap().is_some());
    assert!(s.try_acquire_group_ownership("g2").await.unwrap().is_some());
}

#[cfg(feature = "postgres")]
#[tokio::test]
async fn postgres_group_ownership_is_per_group_exclusive() {
    let Ok(url) = std::env::var("PHAROS_TEST_POSTGRES_URL") else {
        eprintln!(
            "SKIP postgres_group_ownership_is_per_group_exclusive: PHAROS_TEST_POSTGRES_URL unset"
        );
        return;
    };
    let a = pharos_store_sqlx::postgres::PostgresStore::connect(&url)
        .await
        .unwrap();
    let b = pharos_store_sqlx::postgres::PostgresStore::connect(&url)
        .await
        .unwrap();

    // A owns group "movie-night"; B is locked out of THAT group...
    let lease_a = a.try_acquire_group_ownership("movie-night").await.unwrap();
    assert!(lease_a.is_some(), "first replica owns the group");
    assert!(
        b.try_acquire_group_ownership("movie-night")
            .await
            .unwrap()
            .is_none(),
        "a second replica must not own the same group"
    );
    // ...but a DIFFERENT group is independently ownable by B (no false sharing).
    assert!(
        b.try_acquire_group_ownership("other-party")
            .await
            .unwrap()
            .is_some(),
        "a different group must be ownable concurrently"
    );

    // A's lease drops → its group frees → B can take it over.
    drop(lease_a);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(
        b.try_acquire_group_ownership("movie-night")
            .await
            .unwrap()
            .is_some(),
        "after the owner drops, another replica takes over the group"
    );
}
