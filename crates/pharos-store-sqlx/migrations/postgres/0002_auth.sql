CREATE TABLE IF NOT EXISTS users (
    id            BYTEA   PRIMARY KEY,
    name          TEXT    NOT NULL UNIQUE,
    password_hash TEXT    NOT NULL,
    admin         INTEGER NOT NULL DEFAULT 0 CHECK (admin IN (0, 1))
);

CREATE TABLE IF NOT EXISTS auth_tokens (
    token      TEXT    PRIMARY KEY,
    user_id    BYTEA   NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    device_id  TEXT    NOT NULL,
    created_at BIGINT  NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_auth_tokens_user ON auth_tokens(user_id);
