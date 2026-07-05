//! `BuiltinAuth` — verifies passwords against an Argon2id hash stored in
//! a `UserStore`. The store sees the *hash*, not the plaintext, so the
//! domain layer never needs `argon2` as a dependency (V12).

use argon2::{
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use pharos_core::{
    AuthBackend, AuthError, AuthResult, SecretString, User, UserId, UserPolicy, UserRecord,
    UserStore,
};

/// Result of [`BuiltinAuth::create_user`] — distinguishes a fresh create
/// from an idempotent no-op so the caller can report accurately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreateUserOutcome {
    Created,
    AlreadyExists,
}

pub struct BuiltinAuth<U: UserStore> {
    users: U,
    argon: Argon2<'static>,
}

impl<U: UserStore> BuiltinAuth<U> {
    pub fn new(users: U) -> Self {
        Self {
            users,
            argon: Argon2::default(),
        }
    }

    pub fn users(&self) -> &U {
        &self.users
    }

    /// Hash a plaintext password with a fresh random salt. Use this to
    /// build a `UserRecord` before calling `UserStore::create`.
    pub fn hash_password(&self, password: &SecretString) -> AuthResult<SecretString> {
        let mut salt_bytes = [0u8; 16];
        getrandom::getrandom(&mut salt_bytes)
            .map_err(|e| AuthError::Backend(format!("salt rng: {e}")))?;
        let salt = SaltString::encode_b64(&salt_bytes)
            .map_err(|e| AuthError::Backend(format!("salt encode: {e}")))?;
        let hash = self
            .argon
            .hash_password(password.expose().as_bytes(), &salt)
            .map_err(|e| AuthError::Backend(format!("hash: {e}")))?
            .to_string();
        Ok(SecretString::new(hash))
    }

    /// Create a user with the given plaintext password, hashing it first.
    /// Idempotent: a name collision yields [`CreateUserOutcome::AlreadyExists`]
    /// rather than an error, so re-running a bootstrap command is safe.
    /// This is the supported way to bootstrap the first admin
    /// (`pharos admin create-user --name … --password … --admin`).
    pub async fn create_user(
        &self,
        name: &str,
        password: &SecretString,
        admin: bool,
    ) -> AuthResult<CreateUserOutcome> {
        let hash = self.hash_password(password)?;
        let record = UserRecord {
            id: UserId::new(),
            name: name.to_string(),
            password_hash: hash,
            policy: UserPolicy { admin },
        };
        match self.users.create(record).await {
            Ok(()) => Ok(CreateUserOutcome::Created),
            Err(AuthError::Conflict) => Ok(CreateUserOutcome::AlreadyExists),
            Err(e) => Err(e),
        }
    }
}

impl<U: UserStore> AuthBackend for BuiltinAuth<U> {
    #[tracing::instrument(skip(self, password), fields(user.name = %name))]
    async fn authenticate(&self, name: &str, password: &SecretString) -> AuthResult<User> {
        let record = match self.users.lookup_by_name(name).await {
            Ok(r) => r,
            Err(AuthError::UserNotFound) => return Err(AuthError::InvalidCredentials),
            Err(other) => return Err(other),
        };
        let parsed = PasswordHash::new(record.password_hash.expose())
            .map_err(|e| AuthError::Backend(format!("parse hash: {e}")))?;
        self.argon
            .verify_password(password.expose().as_bytes(), &parsed)
            .map_err(|_| AuthError::InvalidCredentials)?;
        Ok(record.into_user())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use pharos_core::{UserId, UserPolicy, UserRecord};
    use std::collections::HashMap;
    use tokio::sync::Mutex;

    #[derive(Default)]
    struct MemUsers {
        by_name: Mutex<HashMap<String, UserRecord>>,
    }

    impl UserStore for MemUsers {
        async fn create(&self, record: UserRecord) -> AuthResult<()> {
            let mut g = self.by_name.lock().await;
            if g.contains_key(&record.name) {
                return Err(AuthError::Conflict);
            }
            g.insert(record.name.clone(), record);
            Ok(())
        }
        async fn lookup_by_name(&self, name: &str) -> AuthResult<UserRecord> {
            self.by_name
                .lock()
                .await
                .get(name)
                .cloned()
                .ok_or(AuthError::UserNotFound)
        }
        async fn get(&self, id: UserId) -> AuthResult<UserRecord> {
            self.by_name
                .lock()
                .await
                .values()
                .find(|r| r.id == id)
                .cloned()
                .ok_or(AuthError::UserNotFound)
        }
        async fn list(&self) -> AuthResult<Vec<UserRecord>> {
            let g = self.by_name.lock().await;
            let mut v: Vec<UserRecord> = g.values().cloned().collect();
            v.sort_by_key(|a| a.name.to_lowercase());
            Ok(v)
        }
        async fn delete(&self, id: UserId) -> AuthResult<()> {
            let mut g = self.by_name.lock().await;
            let Some(key) = g.iter().find(|(_, r)| r.id == id).map(|(k, _)| k.clone()) else {
                return Err(AuthError::UserNotFound);
            };
            g.remove(&key);
            Ok(())
        }
        async fn set_policy(&self, id: UserId, policy: UserPolicy) -> AuthResult<()> {
            let mut g = self.by_name.lock().await;
            for rec in g.values_mut() {
                if rec.id == id {
                    rec.policy = policy;
                    return Ok(());
                }
            }
            Err(AuthError::UserNotFound)
        }
        async fn set_password(&self, id: UserId, password_hash: SecretString) -> AuthResult<()> {
            let mut g = self.by_name.lock().await;
            for rec in g.values_mut() {
                if rec.id == id {
                    rec.password_hash = password_hash;
                    return Ok(());
                }
            }
            Err(AuthError::UserNotFound)
        }
    }

    async fn fresh() -> BuiltinAuth<MemUsers> {
        BuiltinAuth::new(MemUsers::default())
    }

    async fn seed(auth: &BuiltinAuth<MemUsers>, name: &str, pw: &str) -> UserRecord {
        let hash = auth.hash_password(&SecretString::new(pw)).unwrap();
        let rec = UserRecord {
            id: UserId::new(),
            name: name.into(),
            password_hash: hash,
            policy: UserPolicy::default(),
        };
        auth.users().create(rec.clone()).await.unwrap();
        rec
    }

    #[tokio::test]
    async fn authenticate_happy_path() {
        let auth = fresh().await;
        let rec = seed(&auth, "ali", "hunter2").await;
        let user = auth
            .authenticate("ali", &SecretString::new("hunter2"))
            .await
            .unwrap();
        assert_eq!(user.id, rec.id);
        assert_eq!(user.name, "ali");
    }

    #[tokio::test]
    async fn wrong_password_is_invalid_credentials() {
        let auth = fresh().await;
        seed(&auth, "ali", "hunter2").await;
        match auth.authenticate("ali", &SecretString::new("wrong")).await {
            Err(AuthError::InvalidCredentials) => {}
            other => panic!("expected InvalidCredentials, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unknown_user_is_invalid_credentials_not_user_not_found() {
        // V8 spirit: do not leak existence of accounts via differing errors.
        let auth = fresh().await;
        match auth.authenticate("nope", &SecretString::new("x")).await {
            Err(AuthError::InvalidCredentials) => {}
            other => panic!("expected InvalidCredentials, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn hash_is_not_plaintext() {
        let auth = fresh().await;
        let h = auth.hash_password(&SecretString::new("hunter2")).unwrap();
        assert!(h.expose().starts_with("$argon2"));
        assert!(!h.expose().contains("hunter2"));
    }

    #[tokio::test]
    async fn create_user_creates_admin_that_can_authenticate() {
        let auth = fresh().await;
        let outcome = auth
            .create_user("ali", &SecretString::new("hunter2"), true)
            .await
            .unwrap();
        assert_eq!(outcome, CreateUserOutcome::Created);
        // The created user can log in and is an admin.
        let user = auth
            .authenticate("ali", &SecretString::new("hunter2"))
            .await
            .unwrap();
        assert_eq!(user.name, "ali");
        let rec = auth.users().lookup_by_name("ali").await.unwrap();
        assert!(rec.policy.admin);
    }

    #[tokio::test]
    async fn create_user_defaults_to_non_admin() {
        let auth = fresh().await;
        auth.create_user("bob", &SecretString::new("pw"), false)
            .await
            .unwrap();
        let rec = auth.users().lookup_by_name("bob").await.unwrap();
        assert!(!rec.policy.admin);
    }

    #[tokio::test]
    async fn create_user_is_idempotent_on_name_collision() {
        let auth = fresh().await;
        auth.create_user("ali", &SecretString::new("hunter2"), true)
            .await
            .unwrap();
        let again = auth
            .create_user("ali", &SecretString::new("different"), false)
            .await
            .unwrap();
        assert_eq!(again, CreateUserOutcome::AlreadyExists);
        // The original password + admin flag are left untouched.
        let user = auth
            .authenticate("ali", &SecretString::new("hunter2"))
            .await
            .unwrap();
        assert_eq!(user.name, "ali");
    }
}
