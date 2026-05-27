-- T-fix-20: UserConfiguration + DisplayPreferences persistence.
--
-- `user_configuration` is per-user — captures the body posted to
-- `/Users/{u}/Configuration`. Stored as JSON so the schema can grow
-- without further migrations (Jellyfin's `UserConfiguration` shape
-- changes every minor).
CREATE TABLE IF NOT EXISTS user_configuration (
    user_id BLOB    PRIMARY KEY NOT NULL,
    config  TEXT    NOT NULL,
    updated_at INTEGER NOT NULL,
    FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
) STRICT;

-- `display_preferences` is keyed by (user, dp_id, client). dp_id is a
-- jellyfin-web internal token ("usersettings", "home", "movies-1234",
-- etc); client distinguishes web vs mobile.
CREATE TABLE IF NOT EXISTS display_preferences (
    user_id BLOB NOT NULL,
    dp_id   TEXT NOT NULL,
    client  TEXT NOT NULL,
    prefs   TEXT NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (user_id, dp_id, client),
    FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
) STRICT;
