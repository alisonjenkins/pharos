-- B123 — record that an episode WAS analysed, separately from what the
-- analysis found. `media_segments` holds results only, so a season whose
-- episodes have no detectable intro leaves no trace and is re-analysed on
-- every pass, forever; and `media_segments.schema_version` cannot be read to
-- force re-detection, because an episode with no rows has no version either.
CREATE TABLE IF NOT EXISTS media_segment_scans (
    item_id        INTEGER NOT NULL,
    schema_version INTEGER NOT NULL,
    PRIMARY KEY (item_id)
);
