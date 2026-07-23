-- 0044 — series-container metadata (T9-series). A show has no media_items row
-- (Series/Season are synthesised from their episodes), so its provider-sourced
-- overview / genres / rating / poster live here, keyed by the same series_key
-- (series_folder, else series_name) the synthesised Series wire id hashes on.
--
-- match_source mirrors the media_items discipline: NULL = never matched
-- (eligible now); 'search' = an online match; 'none' = searched, nothing over
-- the confidence floor (skip until the TTL); 'manual' = operator override the
-- enricher never clobbers. genres/studios are JSON string arrays; provider_ids
-- is the same JSON shape media_items uses.
CREATE TABLE IF NOT EXISTS series_metadata (
    series_key            TEXT PRIMARY KEY,
    series_name           TEXT NOT NULL,
    match_provider        TEXT,
    match_external_id     TEXT,
    match_source          TEXT,
    match_confidence      REAL,
    metadata_refreshed_at INTEGER,
    overview              TEXT,
    community_rating      REAL,
    premiere_date         INTEGER,
    official_rating       TEXT,
    genres                TEXT NOT NULL DEFAULT '[]',
    studios               TEXT NOT NULL DEFAULT '[]',
    provider_ids          TEXT NOT NULL DEFAULT '{}',
    poster_locator        TEXT,
    backdrop_locator      TEXT
) STRICT;
