-- Native `Prefs`: one opaque value per (user, key). The server decides which
-- keys exist and how large a value may be; the table holds whatever it wrote.
CREATE TABLE IF NOT EXISTS user_prefs (
    user_id    BLOB NOT NULL,
    key        TEXT NOT NULL,
    value      BLOB NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (user_id, key),
    FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
) STRICT;
