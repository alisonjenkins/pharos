//! B836 — the pool a store opens takes its size and acquire timeout from the
//! caller; unconfigured it keeps the sizes the stores had before.

use std::time::Duration;

use pharos_store_sqlx::{sqlite::SqliteStore, PoolConfig};

#[tokio::test]
async fn the_default_pool_is_eight_connections_and_a_thirty_second_wait() {
    let store = SqliteStore::connect("sqlite::memory:").await.unwrap();
    let options = store.pool().options();
    assert_eq!(options.get_max_connections(), 8);
    assert_eq!(options.get_acquire_timeout(), Duration::from_secs(30));
}

#[tokio::test]
async fn a_configured_pool_gets_its_size_and_timeout() {
    let store = SqliteStore::connect_with(
        "sqlite::memory:",
        PoolConfig {
            max_connections: 3,
            acquire_timeout: Duration::from_secs(2),
        },
    )
    .await
    .unwrap();
    let options = store.pool().options();
    assert_eq!(options.get_max_connections(), 3);
    assert_eq!(options.get_acquire_timeout(), Duration::from_secs(2));
}
