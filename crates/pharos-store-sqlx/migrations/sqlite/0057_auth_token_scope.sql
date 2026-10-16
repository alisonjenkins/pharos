-- NULL = full scope (every token issued before scopes existed, and every login
-- token); 'playback' limits a token to playback endpoints (pharos_core::TokenScope).
ALTER TABLE auth_tokens ADD COLUMN scope TEXT;
