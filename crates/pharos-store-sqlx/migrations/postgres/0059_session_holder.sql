-- Who negotiated this play session, and with which token. A PlaySessionId
-- works as a capability: a stream URL carrying it plays without a credential
-- (B75). Holding the user and the token's id (its at-rest hash, never the
-- token) lets each use check that the token is still live and the user may
-- still see the item, so the capability ends at logout or revocation.
--
-- NULL for rows written before this column existed; the server treats such a
-- session as expired rather than as one nobody holds.
ALTER TABLE transcode_sessions ADD COLUMN holder_user_id TEXT;
ALTER TABLE transcode_sessions ADD COLUMN holder_token_id TEXT;
