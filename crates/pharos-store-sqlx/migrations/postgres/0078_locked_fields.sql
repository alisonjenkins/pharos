-- Mirror of migrations/sqlite/0078_locked_fields.sql.
-- Spec 015 US3 (ADR-0097, V1030): the fields an admin edited, as a bit mask
-- (pharos_core::EditableField). A scan's put and the series enricher's upsert
-- keep a locked column's stored value instead of writing their own.
ALTER TABLE media_items ADD COLUMN locked_fields BIGINT NOT NULL DEFAULT 0;
ALTER TABLE series_metadata ADD COLUMN locked_fields BIGINT NOT NULL DEFAULT 0;
