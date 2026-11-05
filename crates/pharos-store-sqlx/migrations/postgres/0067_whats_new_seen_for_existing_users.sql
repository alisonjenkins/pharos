-- ADR-0065: an account with no `whats_new.seen` pref is new and gets the
-- first-run setup. Every account that exists when this ships has already
-- set Pharos up, so it starts at entry 0 and gets what's new instead.
INSERT INTO user_prefs (user_id, key, value, updated_at)
SELECT id, 'whats_new.seen', convert_to('0', 'UTF8'), EXTRACT(EPOCH FROM now())::BIGINT
FROM users
ON CONFLICT (user_id, key) DO NOTHING;
