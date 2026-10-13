//! Token scope + TTL against a real Postgres, mirroring `auth_sqlite.rs`.
//! Skips itself when `PHAROS_TEST_POSTGRES_URL` is unset. The database is
//! shared across tests, so each test makes its own uniquely named user and
//! only ever touches that user's token rows.

#![cfg(feature = "postgres")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use pharos_core::{
    AuthError, SecretString, TokenScope, TokenStore, UserId, UserPolicy, UserRecord, UserStore,
};
use pharos_store_sqlx::postgres::PostgresStore;

async fn fresh(test: &str) -> Option<(PostgresStore, UserId)> {
    let Ok(url) = std::env::var("PHAROS_TEST_POSTGRES_URL") else {
        eprintln!("SKIP {test}: PHAROS_TEST_POSTGRES_URL unset");
        return None;
    };
    let s = PostgresStore::connect(&url).await.unwrap();
    let id = UserId::new();
    s.create(UserRecord {
        id,
        name: format!("{test}-{}", id.0),
        password_hash: SecretString::new("h"),
        policy: UserPolicy::default(),
    })
    .await
    .unwrap();
    Some((s, id))
}

#[tokio::test]
async fn postgres_playback_token_resolves_with_its_scope_and_a_login_token_is_full() {
    let Some((s, uid)) = fresh("pg_scope_resolve").await else {
        return;
    };
    let tv = s
        .issue_with_ttl(uid, "cast:Den", 3600, TokenScope::Playback)
        .await
        .unwrap();
    let login = s.issue(uid, "dev").await.unwrap();
    let tv = s.resolve_scoped(tv.secret.0.expose()).await.unwrap();
    let login = s.resolve_scoped(login.0.expose()).await.unwrap();
    assert_eq!((tv.user, tv.scope), (uid, TokenScope::Playback));
    assert_eq!((login.user, login.scope), (uid, TokenScope::Full));
}

#[tokio::test]
async fn postgres_row_with_no_scope_is_full_and_an_unknown_scope_is_refused() {
    let Some((s, uid)) = fresh("pg_scope_legacy").await else {
        return;
    };
    let legacy = s.issue(uid, "dev").await.unwrap();
    let stored: (Option<String>,) =
        sqlx::query_as("SELECT scope FROM auth_tokens WHERE user_id = $1")
            .bind(uid.0.as_bytes().to_vec())
            .fetch_one(s.pool())
            .await
            .unwrap();
    assert_eq!(stored.0, None, "Full is stored as NULL, as before scopes");
    assert_eq!(
        s.resolve_scoped(legacy.0.expose()).await.unwrap().scope,
        TokenScope::Full
    );
    sqlx::query("UPDATE auth_tokens SET scope = 'superuser' WHERE user_id = $1")
        .bind(uid.0.as_bytes().to_vec())
        .execute(s.pool())
        .await
        .unwrap();
    match s.resolve_scoped(legacy.0.expose()).await {
        Err(AuthError::Backend(m)) => assert!(m.contains("superuser"), "{m}"),
        other => panic!("expected Backend naming the scope, got {other:?}"),
    }
}

#[tokio::test]
async fn postgres_revoke_by_id_removes_only_that_token_and_is_idempotent() {
    let Some((s, uid)) = fresh("pg_revoke_by_id").await else {
        return;
    };
    let issued = s
        .issue_with_ttl(uid, "cast:Den", 3600, TokenScope::Playback)
        .await
        .unwrap();
    let other = s.issue(uid, "dev").await.unwrap();
    s.revoke_by_id(&issued.id).await.unwrap();
    s.revoke_by_id(&issued.id).await.unwrap();
    assert!(matches!(
        s.resolve(issued.secret.0.expose()).await,
        Err(AuthError::InvalidToken)
    ));
    assert_eq!(s.resolve(other.0.expose()).await.unwrap(), uid);
}

#[tokio::test]
async fn postgres_issue_with_ttl_bounds_lifetime_and_zero_ttl_is_expired() {
    let Some((s, uid)) = fresh("pg_ttl").await else {
        return;
    };
    let issued = s
        .issue_with_ttl(uid, "cast:Den", 3600, TokenScope::Full)
        .await
        .unwrap();
    let (stored, created, expires): (String, i64, i64) = sqlx::query_as(
        "SELECT token_hash, created_at, expires_at FROM auth_tokens WHERE token_hash = $1",
    )
    .bind(&issued.id.0)
    .fetch_one(s.pool())
    .await
    .unwrap();
    assert_eq!(issued.id.0, stored);
    assert_ne!(issued.id.0, issued.secret.0.expose());
    assert_eq!(expires - created, 3600);
    assert_eq!(s.resolve(issued.secret.0.expose()).await.unwrap(), uid);

    let dead = s
        .issue_with_ttl(uid, "cast:Den", 0, TokenScope::Full)
        .await
        .unwrap();
    assert!(matches!(
        s.resolve(dead.secret.0.expose()).await,
        Err(AuthError::InvalidToken)
    ));
}
