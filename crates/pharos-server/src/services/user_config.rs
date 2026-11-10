//! One string field of a user's Jellyfin configuration blob, read and
//! written on its own. The native settings pages and jellyfin-web share the
//! blob, so a native set must keep every other field, known or not, and must
//! not replace a blob it cannot read as a JSON object.

use pharos_core::{PreferenceStore, UserId};

/// Why a configuration field was not stored.
#[derive(Debug)]
pub enum SetFieldError {
    /// The value is not one the field accepts.
    Invalid,
    /// The stored configuration is not a JSON object, so setting one field
    /// in it would throw the rest away.
    Unreadable,
    /// The configuration could not be read or written.
    Store(String),
}

/// `field` of the stored configuration `raw`, read on its own: a sibling
/// field the typed configuration would reject (a `null` where it wants a
/// list, say) must not lose a valid setting. Missing or not a string is "".
pub fn string_field(raw: &str, field: &str) -> Result<String, serde_json::Error> {
    let v: serde_json::Value = serde_json::from_str(raw)?;
    Ok(v.get(field)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string())
}

/// `field` of `user`'s stored configuration, "" when there is none.
pub async fn get_string_field<S: PreferenceStore>(
    store: &S,
    user: UserId,
    field: &str,
) -> Result<String, String> {
    let raw = store
        .get_user_configuration(user)
        .await
        .map_err(|e| e.to_string())?;
    Ok(raw
        .and_then(|r| string_field(&r, field).ok())
        .unwrap_or_default())
}

/// Held across each read-modify-write, so two fields set at once (audio and
/// subtitle language picked in quick succession) cannot each write back a
/// blob without the other's change. Sets are rare; one lock for all users
/// costs nothing.
static FIELD_WRITE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Store `value` as `field` of `user`'s configuration, keeping every other
/// field as it was. A stored value that is not a JSON object is refused
/// rather than overwritten. Logs the user and the stored value.
pub async fn set_string_field<S: PreferenceStore>(
    store: &S,
    user: UserId,
    field: &'static str,
    value: &str,
) -> Result<(), SetFieldError> {
    let _one_at_a_time = FIELD_WRITE.lock().await;
    let raw = store
        .get_user_configuration(user)
        .await
        .map_err(|e| SetFieldError::Store(e.to_string()))?;
    let mut cfg = match raw {
        None => serde_json::json!({}),
        Some(r) => match serde_json::from_str::<serde_json::Value>(&r) {
            Ok(v) if v.is_object() => v,
            _ => {
                tracing::warn!(user.id = %user, field, size = r.len(), "configuration field not set: stored configuration is not a JSON object");
                return Err(SetFieldError::Unreadable);
            }
        },
    };
    cfg[field] = value.into();
    store
        .set_user_configuration(user, &cfg.to_string())
        .await
        .map_err(|e| SetFieldError::Store(e.to_string()))?;
    tracing::info!(user.id = %user, field, value, "user configuration field set");
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use pharos_core::{SecretString, UserPolicy, UserRecord, UserStore};
    use pharos_store_sqlx::sqlite::SqliteStore;

    const FIELDS: [&str; 8] = ["F0", "F1", "F2", "F3", "F4", "F5", "F6", "F7"];

    /// Disarm check: without `FIELD_WRITE` held, the fields set together
    /// overwrite each other and some are missing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn fields_set_at_once_all_survive() {
        let s = std::sync::Arc::new(SqliteStore::connect("sqlite::memory:").await.unwrap());
        let uid = UserId::new();
        UserStore::create(
            &*s,
            UserRecord {
                id: uid,
                name: "invented".into(),
                password_hash: SecretString::new("$argon2id$fake"),
                policy: UserPolicy::default(),
            },
        )
        .await
        .unwrap();
        let sets = FIELDS.map(|field| {
            let s = std::sync::Arc::clone(&s);
            tokio::spawn(async move { set_string_field(&*s, uid, field, "x").await })
        });
        for set in sets {
            set.await.unwrap().unwrap();
        }
        let raw = s.get_user_configuration(uid).await.unwrap().unwrap();
        for field in FIELDS {
            assert_eq!(
                string_field(&raw, field).unwrap(),
                "x",
                "{field} lost: {raw}"
            );
        }
    }
}
