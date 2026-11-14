//! `ReaderPasswordStore` for both backends (045, ADR-0128, migration 0080).
//! The hash column holds the auth backend's hash of the secret half; the id
//! is the public half and the primary key.

use pharos_core::{
    DomainError, DomainResult, ReaderPassword, ReaderPasswordId, ReaderPasswordRecord,
    SecretString, UserId,
};

const INSERT: &str = "INSERT INTO reader_passwords \
       (id, user_id, label, hash, created_at, last_used_at, revoked_at) \
     VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT (id) DO NOTHING";

const LIST: &str = "SELECT id, label, created_at, last_used_at FROM reader_passwords \
     WHERE user_id = $1 AND revoked_at IS NULL ORDER BY created_at DESC, id";

const GET: &str = "SELECT id, user_id, label, hash, created_at, last_used_at, revoked_at \
     FROM reader_passwords WHERE id = $1";

const REVOKE: &str = "UPDATE reader_passwords SET revoked_at = $3 \
     WHERE user_id = $1 AND id = $2 AND revoked_at IS NULL";

const TOUCH: &str = "UPDATE reader_passwords SET last_used_at = $2 WHERE id = $1";

const PRUNE: &str = "DELETE FROM reader_passwords \
     WHERE user_id = $1 AND revoked_at IS NOT NULL AND revoked_at < $2";

type ListRow = (String, String, i64, Option<i64>);
type GetRow = (
    String,
    Vec<u8>,
    String,
    String,
    i64,
    Option<i64>,
    Option<i64>,
);

fn backend(e: sqlx::Error) -> DomainError {
    DomainError::Backend(e.to_string())
}

fn id_of(stored: &str) -> DomainResult<ReaderPasswordId> {
    ReaderPasswordId::parse(stored).ok_or_else(|| {
        DomainError::Backend(format!(
            "reader_passwords.id {stored:?} is not a reader password id"
        ))
    })
}

fn from_list_row(r: ListRow) -> DomainResult<ReaderPassword> {
    Ok(ReaderPassword {
        id: id_of(&r.0)?,
        label: r.1,
        created_at: r.2,
        last_used_at: r.3,
    })
}

fn from_get_row(r: GetRow) -> DomainResult<ReaderPasswordRecord> {
    let user = uuid::Uuid::from_slice(&r.1)
        .map_err(|e| DomainError::Backend(format!("reader_passwords.user_id: {e}")))?;
    Ok(ReaderPasswordRecord {
        id: id_of(&r.0)?,
        user: UserId(user),
        label: r.2,
        hash: SecretString::new(r.3),
        created_at: r.4,
        last_used_at: r.5,
        revoked_at: r.6,
    })
}

fn user_key(user: UserId) -> Vec<u8> {
    user.0.as_bytes().to_vec()
}

macro_rules! impl_store {
    ($store:ty) => {
        impl pharos_core::ReaderPasswordStore for $store {
            #[tracing::instrument(skip(self, record), fields(user.id = %record.user.0, id = %record.id))]
            async fn create_reader_password(
                &self,
                record: ReaderPasswordRecord,
            ) -> DomainResult<bool> {
                let done = sqlx::query(INSERT)
                    .bind(record.id.as_str().to_owned())
                    .bind(user_key(record.user))
                    .bind(record.label)
                    .bind(record.hash.expose().to_owned())
                    .bind(record.created_at)
                    .bind(record.last_used_at)
                    .bind(record.revoked_at)
                    .execute(self.pool())
                    .await
                    .map_err(backend)?;
                Ok(done.rows_affected() == 1)
            }

            #[tracing::instrument(skip(self), fields(user.id = %user.0))]
            async fn reader_passwords_for(&self, user: UserId) -> DomainResult<Vec<ReaderPassword>> {
                let rows: Vec<ListRow> = sqlx::query_as(LIST)
                    .bind(user_key(user))
                    .fetch_all(self.pool())
                    .await
                    .map_err(backend)?;
                rows.into_iter().map(from_list_row).collect()
            }

            #[tracing::instrument(skip(self), fields(id = %id))]
            async fn reader_password(
                &self,
                id: &ReaderPasswordId,
            ) -> DomainResult<Option<ReaderPasswordRecord>> {
                let row: Option<GetRow> = sqlx::query_as(GET)
                    .bind(id.as_str().to_owned())
                    .fetch_optional(self.pool())
                    .await
                    .map_err(backend)?;
                row.map(from_get_row).transpose()
            }

            #[tracing::instrument(skip(self), fields(user.id = %user.0, id = %id))]
            async fn revoke_reader_password(
                &self,
                user: UserId,
                id: &ReaderPasswordId,
                at: i64,
            ) -> DomainResult<bool> {
                let done = sqlx::query(REVOKE)
                    .bind(user_key(user))
                    .bind(id.as_str().to_owned())
                    .bind(at)
                    .execute(self.pool())
                    .await
                    .map_err(backend)?;
                Ok(done.rows_affected() > 0)
            }

            #[tracing::instrument(skip(self), fields(id = %id))]
            async fn touch_reader_password(&self, id: &ReaderPasswordId, at: i64) -> DomainResult<()> {
                sqlx::query(TOUCH)
                    .bind(id.as_str().to_owned())
                    .bind(at)
                    .execute(self.pool())
                    .await
                    .map_err(backend)?;
                Ok(())
            }

            #[tracing::instrument(skip(self), fields(user.id = %user.0))]
            async fn prune_revoked_reader_passwords(
                &self,
                user: UserId,
                before: i64,
            ) -> DomainResult<u64> {
                let done = sqlx::query(PRUNE)
                    .bind(user_key(user))
                    .bind(before)
                    .execute(self.pool())
                    .await
                    .map_err(backend)?;
                Ok(done.rows_affected())
            }
        }
    };
}

#[cfg(feature = "sqlite")]
impl_store!(crate::sqlite::SqliteStore);

#[cfg(feature = "postgres")]
impl_store!(crate::postgres::PostgresStore);
