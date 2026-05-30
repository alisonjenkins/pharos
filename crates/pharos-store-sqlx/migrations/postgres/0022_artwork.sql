-- LIB-D4: sidecar artwork as real rows (postgres mirror of the sqlite 0022
-- migration). See the sqlite copy for the full rationale.
--
-- One row per (item_id, role); highest-priority source wins on conflict.
-- role = ArtworkRole::as_str token; source = 'local' | 'url'; locator =
-- absolute sidecar path or remote URL. item_id is BIGINT to match
-- media_items' u64 id range. No FK constraint (additive-only discipline,
-- mirroring item_genres). Indexed on item_id so artwork_for(item) is a
-- single indexed lookup.
CREATE TABLE IF NOT EXISTS artwork (
    item_id BIGINT NOT NULL,
    role    TEXT NOT NULL,
    source  TEXT NOT NULL,
    locator TEXT NOT NULL,
    PRIMARY KEY (item_id, role)
);

CREATE INDEX IF NOT EXISTS idx_artwork_item_id ON artwork(item_id);
