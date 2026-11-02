-- Native `Prefs`: one opaque value per (user, key). The server decides which
-- keys exist and how large a value may be; the table holds whatever it wrote.
CREATE TABLE IF NOT EXISTS user_prefs (
    user_id    BYTEA NOT NULL,
    key        TEXT NOT NULL,
    value      BYTEA NOT NULL,
    updated_at BIGINT NOT NULL,
    PRIMARY KEY (user_id, key),
    FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
);
