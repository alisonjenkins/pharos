-- 0044 — series-container metadata (T9-series). See the sqlite migration of
-- the same name for rationale. genres/studios are JSON string arrays and
-- provider_ids the JSON provider map (stored as text, parsed in the store
-- layer — same treatment as media_items).
CREATE TABLE IF NOT EXISTS series_metadata (
    series_key            TEXT PRIMARY KEY,
    series_name           TEXT NOT NULL,
    match_provider        TEXT,
    match_external_id     TEXT,
    match_source          TEXT,
    match_confidence      REAL,
    metadata_refreshed_at BIGINT,
    overview              TEXT,
    community_rating      REAL,
    premiere_date         BIGINT,
    official_rating       TEXT,
    genres                TEXT NOT NULL DEFAULT '[]',
    studios               TEXT NOT NULL DEFAULT '[]',
    provider_ids          TEXT NOT NULL DEFAULT '{}',
    poster_locator        TEXT,
    backdrop_locator      TEXT
);
