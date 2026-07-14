-- T86/ADR-0018 — detected intro/outro segments + cached fingerprints.
CREATE TABLE IF NOT EXISTS media_segments (
    item_id        BIGINT           NOT NULL,
    kind           TEXT             NOT NULL,
    start_ms       BIGINT           NOT NULL,
    end_ms         BIGINT           NOT NULL,
    detector       TEXT             NOT NULL,
    confidence     DOUBLE PRECISION NOT NULL,
    schema_version BIGINT           NOT NULL,
    PRIMARY KEY (item_id, kind)
);
CREATE INDEX IF NOT EXISTS idx_media_segments_item ON media_segments(item_id);

CREATE TABLE IF NOT EXISTS episode_fingerprints (
    item_id        BIGINT NOT NULL,
    kind           TEXT   NOT NULL,
    points         BYTEA  NOT NULL,
    schema_version BIGINT NOT NULL,
    PRIMARY KEY (item_id, kind)
);
