-- 004-books — admit 'book' to the `kind` CHECK constraint from migration 0001.
--
-- Postgres can drop and re-add a named constraint, so no table rewrite and no
-- writable_schema trickery is needed (the sqlite migration of the same number
-- explains why that backend cannot do this).
--
-- The constraint is unnamed in 0001, so postgres generated `media_items_kind_check`
-- (table_column_check). IF EXISTS keeps this idempotent in case a database was
-- created before the constraint existed or under a different name.
ALTER TABLE media_items DROP CONSTRAINT IF EXISTS media_items_kind_check;

ALTER TABLE media_items
  ADD CONSTRAINT media_items_kind_check
  CHECK (kind IN ('movie', 'episode', 'audio', 'book'));
