-- T103 — a detector change overwrites `media_segments` in place, so after a
-- re-run there is no way to answer "did recall drop?". When SEGMENT_DETECT_VERSION
-- 2->3 ran (B131), the question was asked immediately afterwards and could not be
-- answered at all: the only prior figure was an informal note whose query shape
-- was unknown. The sweep now copies the table here BEFORE it replaces anything,
-- once per detect version, so before/after is a diff rather than a recollection.
CREATE TABLE IF NOT EXISTS media_segments_snapshot (
    label          TEXT   NOT NULL,
    item_id        BIGINT NOT NULL,
    kind           TEXT   NOT NULL,
    start_ms       BIGINT NOT NULL,
    end_ms         BIGINT NOT NULL,
    detector       TEXT   NOT NULL,
    confidence     REAL NOT NULL,
    schema_version BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS media_segments_snapshot_label ON media_segments_snapshot (label);
