-- LIB-C3: studios (production companies / TV networks) as real entities.
--
-- /Studios previously returned a stub that aggregated DISTINCT
-- media_items.album_artist strings (a stand-in borrowed from the music
-- path) — it never reflected an item's real production studios. The
-- MetadataResolver already parses NFO <studio> into MetadataResult.studios
-- but the scanner logged-and-dropped them for want of a table. We promote
-- studios to rows so they carry a stable wire id (the 32-hex
-- studio_id_for(name) the Jellyfin DTO emits as a Studio's `Id`), so
-- /Items?ParentId=<studio id> is an indexed join, and so an item's
-- studios round-trip onto its BaseItemDto.Studios.
--
-- studios.wire_id = the 32-hex studio_id_for(name), computed at upsert
-- time (the hash lives in pharos-core, pure arithmetic, never IO).
-- Indexed so /Items?ParentId=<studio synth id> maps to the studio row by
-- wire_id, then joins through item_studios to the tagged items.
--
-- item_studios is the many-to-many join: one media_item carries several
-- studios. Unlike genres there is NO legacy source column on media_items
-- (probe carries no studio), so there is no backfill — studios are
-- populated purely by the scanner wire-in from MetadataResult on write.
--
-- No FK constraints (mirrors item_genres' additive-only shape); a swept
-- media_item leaves orphan join rows the read path never resolves (a
-- rescan replaces them). Indexed on studio wire_id (ParentId pivot) +
-- both join columns.
CREATE TABLE IF NOT EXISTS studios (
    id      INTEGER PRIMARY KEY,
    name    TEXT NOT NULL UNIQUE,
    wire_id TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS item_studios (
    item_id   INTEGER NOT NULL,
    studio_id INTEGER NOT NULL,
    PRIMARY KEY (item_id, studio_id)
);

CREATE INDEX IF NOT EXISTS idx_item_studios_studio_id ON item_studios(studio_id);
CREATE INDEX IF NOT EXISTS idx_item_studios_item_id ON item_studios(item_id);
CREATE INDEX IF NOT EXISTS idx_studios_wire_id ON studios(wire_id);
