-- Store only a SHA-256 hash of each session token, never the token itself,
-- so a DB dump / backup leak cannot yield usable session tokens. Also bound
-- token lifetime with `expires_at` (NULL = never expires) so a leaked token
-- has a finite validity window.
--
-- Existing plaintext tokens cannot be re-hashed (the plaintext is the only
-- thing that maps to the hash), so drop them; affected clients simply
-- re-authenticate. No production deployment exists at this migration.
DROP TABLE IF EXISTS auth_tokens;

CREATE TABLE auth_tokens (
    token_hash TEXT    PRIMARY KEY,
    user_id    BLOB    NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    device_id  TEXT    NOT NULL,
    created_at INTEGER NOT NULL,
    expires_at INTEGER
) STRICT;

CREATE INDEX IF NOT EXISTS idx_auth_tokens_user ON auth_tokens(user_id);
