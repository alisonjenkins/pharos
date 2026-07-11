-- Phase B1 (zero-downtime deploys): persist the per-PlaySessionId transcode
-- negotiation so a viewer whose replica goes away mid-stream can be served
-- segment N+1 by another replica without a 410 "play session expired".
--
-- `decision_json` / `source_probe_json` are opaque serde payloads (the
-- device_profile::Decision + pharos_core::MediaProbe) — the store layer stays
-- codec-agnostic and treats them as text, mirroring the PreferenceStore JSON
-- convention. `updated_at` is unix-seconds, touched on every read/write so the
-- background pruner can drop sessions idle past the in-memory expiry window.
CREATE TABLE IF NOT EXISTS transcode_sessions (
    play_session_id   TEXT PRIMARY KEY,
    media_id          INTEGER NOT NULL,
    decision_json     TEXT NOT NULL,
    source_probe_json TEXT NOT NULL,
    updated_at        INTEGER NOT NULL
) STRICT;

CREATE INDEX IF NOT EXISTS idx_transcode_sessions_updated_at
    ON transcode_sessions (updated_at);
