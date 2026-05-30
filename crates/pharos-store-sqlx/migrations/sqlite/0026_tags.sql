-- LIB-C6: tags as real entities.
--
-- Tags are free-form labels: NFO `<tag>` elements (e.g. "cyberpunk") and
-- the filename provider's quality/source tokens ("1080p", "BluRay", …)
-- both land in MetadataResult.tags. Until now they were carried by the
-- resolver but logged-and-dropped at the scanner wire-in (no table). We
-- promote them to rows so they carry a stable wire id, surface on the
-- item DTO's `Tags`, drive `/Items?ParentId=<tag>` + `?Tags=a,b`, and can
-- be mutated manually (POST/DELETE /Items/{id}/Tags).
--
-- tags.wire_id = the 32-hex tag_id_for(name) the Jellyfin DTO emits for a
-- synthesised Tag item, computed via pharos_core::tag_wire_id at upsert
-- (pure hash, never IO). Indexed so `/Items?ParentId=<tag synth id>`
-- resolves the tag row by wire_id, then joins through item_tags to the
-- tagged items — an indexed pivot, not an in-memory scan.
--
-- item_tags is the many-to-many join: one media_item carries several
-- tags. PK (item_id, tag_id) so re-tagging the same pair is a no-op. No
-- FK constraints (mirrors item_genres' additive-only shape); a swept
-- media_item leaves an orphan join row the read path never resolves.
--
-- Unlike genres there is NO backfill: media_items carries no legacy tag
-- column (genres backfill exists only because probe.genre predates the
-- join). Tags are populated purely by the scanner wire-in and the manual
-- add/remove endpoints.
CREATE TABLE IF NOT EXISTS tags (
    id      INTEGER PRIMARY KEY,
    name    TEXT NOT NULL UNIQUE,
    wire_id TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS item_tags (
    item_id INTEGER NOT NULL,
    tag_id  INTEGER NOT NULL,
    PRIMARY KEY (item_id, tag_id)
);

CREATE INDEX IF NOT EXISTS idx_item_tags_tag_id ON item_tags(tag_id);
CREATE INDEX IF NOT EXISTS idx_item_tags_item_id ON item_tags(item_id);
CREATE INDEX IF NOT EXISTS idx_tags_wire_id ON tags(wire_id);
