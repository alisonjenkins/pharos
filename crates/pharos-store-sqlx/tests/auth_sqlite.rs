#![cfg(feature = "sqlite")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use pharos_core::{
    AuthError, SecretString, TokenStore, UserId, UserPolicy, UserRecord, UserStore,
};
use pharos_store_sqlx::sqlite::SqliteStore;

async fn fresh() -> SqliteStore {
    SqliteStore::connect("sqlite::memory:").await.unwrap()
}

fn record(name: &str, hash: &str, admin: bool) -> UserRecord {
    UserRecord {
        id: UserId::new(),
        name: name.into(),
        password_hash: SecretString::new(hash),
        policy: UserPolicy { admin },
    }
}

#[tokio::test]
async fn create_then_lookup_user() {
    let s = fresh().await;
    let r = record("ali", "$argon2id$fake", true);
    s.create(r.clone()).await.unwrap();
    let got = s.lookup_by_name("ali").await.unwrap();
    assert_eq!(got.id, r.id);
    assert!(got.policy.admin);
    assert_eq!(got.password_hash.expose(), "$argon2id$fake");
}

#[tokio::test]
async fn duplicate_name_is_conflict() {
    let s = fresh().await;
    s.create(record("ali", "h1", false)).await.unwrap();
    match s.create(record("ali", "h2", false)).await {
        Err(AuthError::Conflict) => {}
        other => panic!("expected Conflict, got {other:?}"),
    }
}

#[tokio::test]
async fn unknown_user_is_user_not_found() {
    let s = fresh().await;
    match s.lookup_by_name("nope").await {
        Err(AuthError::UserNotFound) => {}
        other => panic!("expected UserNotFound, got {other:?}"),
    }
}

#[tokio::test]
async fn issue_then_resolve_token() {
    let s = fresh().await;
    let r = record("ali", "h", false);
    let uid = r.id;
    s.create(r).await.unwrap();
    let token = s.issue(uid, "test-device").await.unwrap();
    let resolved = s.resolve(token.0.expose()).await.unwrap();
    assert_eq!(resolved, uid);
}

#[tokio::test]
async fn resolve_unknown_token_is_invalid_token() {
    let s = fresh().await;
    match s.resolve("nope-not-a-token").await {
        Err(AuthError::InvalidToken) => {}
        other => panic!("expected InvalidToken, got {other:?}"),
    }
}

#[tokio::test]
async fn revoke_removes_token() {
    let s = fresh().await;
    let r = record("ali", "h", false);
    let uid = r.id;
    s.create(r).await.unwrap();
    let token = s.issue(uid, "dev").await.unwrap();
    s.revoke(token.0.expose()).await.unwrap();
    match s.resolve(token.0.expose()).await {
        Err(AuthError::InvalidToken) => {}
        other => panic!("expected InvalidToken, got {other:?}"),
    }
}

#[tokio::test]
async fn cascade_delete_on_user_drop() {
    // FK ON DELETE CASCADE: deleting user wipes its tokens. Verified at
    // schema level by attempting to resolve afterwards.
    let s = fresh().await;
    let r = record("ali", "h", false);
    let uid = r.id;
    s.create(r).await.unwrap();
    let token = s.issue(uid, "dev").await.unwrap();
    sqlx::query("DELETE FROM users WHERE id = ?")
        .bind(uid.0.as_bytes().to_vec())
        .execute(s.pool())
        .await
        .unwrap();
    match s.resolve(token.0.expose()).await {
        Err(AuthError::InvalidToken) => {}
        other => panic!("expected InvalidToken, got {other:?}"),
    }
}
