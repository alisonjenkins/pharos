-- Spec 033 (V893, V896): decisions about a show's grouping that a rescan must
-- not undo. A 'scanner' row records which group of a split folder keeps the
-- plain folder key (chosen once, at the first split); an 'admin' row is a hand
-- fix (move files, keep together) and always wins over a 'scanner' row.
-- `path` is one file, or NULL for a row about the whole folder.
CREATE TABLE IF NOT EXISTS series_overrides (
    id            BIGSERIAL PRIMARY KEY,
    series_folder TEXT    NOT NULL,
    path          TEXT,
    target_key    TEXT    NOT NULL,
    target_name   TEXT,
    target_year   INTEGER,
    source        TEXT    NOT NULL CHECK (source IN ('scanner', 'admin')),
    created_by    TEXT,
    created_at    BIGINT  NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_series_overrides_folder ON series_overrides(series_folder);
CREATE UNIQUE INDEX IF NOT EXISTS idx_series_overrides_one_owner
    ON series_overrides(series_folder, source) WHERE path IS NULL;
CREATE UNIQUE INDEX IF NOT EXISTS idx_series_overrides_one_per_file
    ON series_overrides(path, source) WHERE path IS NOT NULL;
