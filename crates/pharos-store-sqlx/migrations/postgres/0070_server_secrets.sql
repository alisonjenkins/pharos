-- B762 (V712): server-side secrets that must survive restarts, such as the
-- key that signs image tags (ADR-0073). One row per named secret.
CREATE TABLE IF NOT EXISTS server_secrets (
    name       TEXT   PRIMARY KEY,
    value      BYTEA  NOT NULL,
    created_at BIGINT NOT NULL
);
