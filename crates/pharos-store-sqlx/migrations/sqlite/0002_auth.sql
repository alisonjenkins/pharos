CREATE TABLE IF NOT EXISTS users (
    id            BLOB    PRIMARY KEY,
    name          TEXT    NOT NULL UNIQUE,
    password_hash TEXT    NOT NULL,
    admin         INTEGER NOT NULL DEFAULT 0 CHECK (admin IN (0, 1))
) STRICT;

CREATE TABLE IF NOT EXISTS auth_tokens (
    token      TEXT    PRIMARY KEY,
    user_id    BLOB    NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    device_id  TEXT    NOT NULL,
    created_at INTEGER NOT NULL
) STRICT;

CREATE INDEX IF NOT EXISTS idx_auth_tokens_user ON auth_tokens(user_id);
