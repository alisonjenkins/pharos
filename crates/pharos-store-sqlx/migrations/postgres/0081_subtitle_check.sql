-- Spec 040: each subtitle track's check against its video. source_key is
-- stream:<index> for an embedded stream or sidecar:<hash> for a sidecar file.
-- source_* is the video's signature and sidecar_* the sidecar's; a mismatch,
-- or an older check_version, means the track is checked again. verdict is NULL
-- while an aligner failure waits for retry_at (FR-015a); attempts counts them.
-- scores_json holds every probe and decoy score, opaque to the store.
CREATE TABLE IF NOT EXISTS subtitle_check (
    item_id       BIGINT NOT NULL REFERENCES media_items(id) ON DELETE CASCADE,
    source_key    TEXT NOT NULL,
    source_mtime  BIGINT NOT NULL,
    source_size   BIGINT NOT NULL,
    sidecar_mtime BIGINT,
    sidecar_size  BIGINT,
    check_version BIGINT NOT NULL,
    verdict       TEXT,
    reason        TEXT NOT NULL,
    offset_ms     BIGINT,
    ratio         DOUBLE PRECISION,
    scores_json   TEXT,
    attempts      BIGINT NOT NULL,
    retry_at      BIGINT,
    checked_at    BIGINT NOT NULL,
    PRIMARY KEY (item_id, source_key)
);
