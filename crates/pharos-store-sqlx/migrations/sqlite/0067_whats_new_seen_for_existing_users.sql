-- ADR-0065: an account with no `whats_new.seen` pref is new and gets the
-- first-run setup. Every account that exists when this ships has already
-- set Pharos up, so it starts at entry 0 and gets what's new instead.
INSERT OR IGNORE INTO user_prefs (user_id, key, value, updated_at)
SELECT id, 'whats_new.seen', CAST('0' AS BLOB), CAST(strftime('%s', 'now') AS INTEGER)
FROM users;
