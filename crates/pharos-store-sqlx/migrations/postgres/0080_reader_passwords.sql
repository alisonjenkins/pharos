-- 045 (ADR-0128): e-reader passwords, which open only the OPDS catalogue.
-- `id` is the public first part of the password and picks the row; `hash` is
-- the argon2 hash of the secret part. A revoked row opens nothing and is kept
-- a while only so a refusal can say "revoked".
CREATE TABLE IF NOT EXISTS reader_passwords (
    id           TEXT    NOT NULL PRIMARY KEY,
    user_id      BYTEA   NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    label        TEXT    NOT NULL,
    hash         TEXT    NOT NULL,
    created_at   BIGINT  NOT NULL,
    last_used_at BIGINT,
    revoked_at   BIGINT
);

CREATE INDEX IF NOT EXISTS idx_reader_passwords_user ON reader_passwords(user_id, created_at DESC);
