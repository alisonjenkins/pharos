-- A token issued on the strength of another (a Media token, ADR-0058) names
-- that token in `parent_hash`. Deleting the parent deletes the child in the
-- same statement, so every revoke path (sign-out, device revoke, session
-- sweep, revoke by id, user delete) takes the child with it, and so does any
-- path added later (B543, V438). NULL = a token issued on its own.
ALTER TABLE auth_tokens ADD COLUMN parent_hash TEXT REFERENCES auth_tokens(token_hash) ON DELETE CASCADE;

CREATE INDEX IF NOT EXISTS idx_auth_tokens_parent ON auth_tokens(parent_hash);
