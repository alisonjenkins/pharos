-- Spec 028: the style director's effect plan for one subtitle track per video
-- item, written by the background styling job and read when ASS is compiled
-- on request. `source_mtime`/`source_size` (and `sidecar_*` when the track is
-- an external file; NULL for an embedded stream) are the signature the plan
-- was read against: a mismatch means the file changed and the item is styled
-- again. `director_version` lets a newer director supersede an older plan.
-- `plan_json` is NULL for a version-only row: an undecodable file or no usable
-- source, kept so the item is not retried every pass. One row per item in
-- practice (a new source choice replaces the old); the key keeps the stream.
-- INTEGER: the table is STRICT.
CREATE TABLE IF NOT EXISTS subtitle_styling (
    item_id          INTEGER NOT NULL REFERENCES media_items(id) ON DELETE CASCADE,
    source_index     INTEGER NOT NULL,
    source_mtime     INTEGER NOT NULL,
    source_size      INTEGER NOT NULL,
    sidecar_mtime    INTEGER,
    sidecar_size     INTEGER,
    director_version INTEGER NOT NULL,
    outcome          TEXT NOT NULL,
    plan_json        TEXT,
    styled_at        INTEGER NOT NULL,
    PRIMARY KEY (item_id, source_index)
) STRICT;
