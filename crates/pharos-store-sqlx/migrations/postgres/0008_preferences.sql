-- T-fix-20: UserConfiguration + DisplayPreferences persistence.
CREATE TABLE IF NOT EXISTS user_configuration (
    user_id BYTEA   PRIMARY KEY NOT NULL,
    config  TEXT    NOT NULL,
    updated_at BIGINT NOT NULL,
    FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS display_preferences (
    user_id BYTEA NOT NULL,
    dp_id   TEXT NOT NULL,
    client  TEXT NOT NULL,
    prefs   TEXT NOT NULL,
    updated_at BIGINT NOT NULL,
    PRIMARY KEY (user_id, dp_id, client),
    FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
);
