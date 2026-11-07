-- Spec 028: the style director's effect plan for one subtitle track per video
-- item, written by the background styling job and read when ASS is compiled
-- on request. `source_mtime`/`source_size` (and `sidecar_*` when the track is
-- an external file; NULL for an embedded stream) are the signature the plan
-- was read against: a mismatch means the file changed and the item is styled
-- again. `director_version` lets a newer director supersede an older plan.
-- `plan_json` is NULL for a version-only row: an undecodable file or no usable
-- source, kept so the item is not retried every pass. One row per item in
-- practice (a new source choice replaces the old); the key keeps the stream.
-- BIGINT for postgres' strict INT8 decode.
CREATE TABLE IF NOT EXISTS subtitle_styling (
    item_id          BIGINT NOT NULL REFERENCES media_items(id) ON DELETE CASCADE,
    source_index     BIGINT NOT NULL,
    source_mtime     BIGINT NOT NULL,
    source_size      BIGINT NOT NULL,
    sidecar_mtime    BIGINT,
    sidecar_size     BIGINT,
    director_version BIGINT NOT NULL,
    outcome          TEXT NOT NULL,
    plan_json        TEXT,
    styled_at        BIGINT NOT NULL,
    PRIMARY KEY (item_id, source_index)
);
