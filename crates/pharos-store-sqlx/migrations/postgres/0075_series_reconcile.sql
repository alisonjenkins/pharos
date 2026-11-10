-- Spec 033 (V940): an episode whose show's stored match changed is marked for
-- reconciliation; the mark is the unix second it was set, cleared only when
-- the episode has been rebuilt against the new match.
ALTER TABLE media_items ADD COLUMN series_reconcile_pending BIGINT;
CREATE INDEX IF NOT EXISTS idx_media_items_series_reconcile_pending
    ON media_items(series_reconcile_pending)
    WHERE series_reconcile_pending IS NOT NULL;
