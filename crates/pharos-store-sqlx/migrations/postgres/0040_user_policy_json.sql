-- T68 — full UserPolicy field set. The `admin` column stays the authoritative
-- fast-path flag (indexed, read by the last-admin guard); everything else
-- (IsDisabled, EnabledFolders, parental control, session limits, feature
-- flags) serializes to this JSON blob. NULL for pre-migration rows →
-- hydrated as a permissive UserPolicy::default at read time.
ALTER TABLE users ADD COLUMN policy_json TEXT;
