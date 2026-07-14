-- T86/ADR-0018 — detected intro/outro segments + the cached episode
-- fingerprints that produced them.
--
-- media_segments: one row per (item, kind). Populated by the fingerprint /
-- black-frame backfill; served by /MediaSegments (union'd with chapter-derived
-- segments, which win on conflict). schema_version stamps the algorithm so a
-- bump re-analyzes. No FK on item_id (additive, mirrors item_genres/artwork).
CREATE TABLE IF NOT EXISTS media_segments (
    item_id        INTEGER NOT NULL,
    kind           TEXT    NOT NULL,
    start_ms       INTEGER NOT NULL,
    end_ms         INTEGER NOT NULL,
    detector       TEXT    NOT NULL,
    confidence     REAL    NOT NULL,
    schema_version INTEGER NOT NULL,
    PRIMARY KEY (item_id, kind)
);
CREATE INDEX IF NOT EXISTS idx_media_segments_item ON media_segments(item_id);

-- episode_fingerprints: the raw chromaprint points per (item, window), cached
-- so a newly-added episode matches an existing season without re-fingerprinting
-- the whole season (ADR-0018 improvement #2). points = little-endian u32 BLOB.
CREATE TABLE IF NOT EXISTS episode_fingerprints (
    item_id        INTEGER NOT NULL,
    kind           TEXT    NOT NULL,   -- 'intro' | 'credits'
    points         BLOB    NOT NULL,
    schema_version INTEGER NOT NULL,
    PRIMARY KEY (item_id, kind)
);
