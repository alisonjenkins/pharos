-- T68 (V577): consecutive failed password logins per user, for
-- LoginAttemptsBeforeLockout. Kept out of policy_json so a policy edit
-- cannot clobber it and the increment is a single atomic UPDATE.
ALTER TABLE users ADD COLUMN failed_login_attempts INTEGER NOT NULL DEFAULT 0;
