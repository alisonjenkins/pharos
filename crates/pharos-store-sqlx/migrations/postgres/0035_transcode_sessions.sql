-- Phase B1 (zero-downtime deploys): persist the per-PlaySessionId transcode
-- negotiation (see the sqlite migration of the same name for rationale) so a
-- failed-over replica serves segment N+1 without a 410.
CREATE TABLE IF NOT EXISTS transcode_sessions (
    play_session_id   TEXT PRIMARY KEY,
    media_id          BIGINT NOT NULL,
    decision_json     TEXT NOT NULL,
    source_probe_json TEXT NOT NULL,
    updated_at        BIGINT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_transcode_sessions_updated_at
    ON transcode_sessions (updated_at);
