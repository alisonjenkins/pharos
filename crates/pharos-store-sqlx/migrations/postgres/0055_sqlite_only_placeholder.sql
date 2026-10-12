-- SQLite's 0055 re-scopes the media_fts update trigger (an fts5 detail with no
-- Postgres counterpart). This empty migration keeps the two sequences
-- numbered alike; sqlx tolerates a gap, readers should not have to.
SELECT 1;
