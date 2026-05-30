-- LIB-B4: full-text search over title + overview (postgres tsvector).
-- Mirror of migrations/sqlite/0029_media_fts.sql — same SUPERSET-of-
-- substring, prefix-friendly, ranked contract, expressed in postgres FTS.
--
-- A GENERATED ALWAYS … STORED tsvector column is the postgres analogue of
-- the sqlite fts5 external-content vtable + sync triggers: the column is
-- recomputed by the engine on every INSERT / UPDATE and removed on DELETE,
-- so it can NEVER drift from title/overview (no trigger to forget). The
-- GIN index over it makes `search_tsv @@ to_tsquery(...)` an indexed scan;
-- ts_rank ranks the hits best-first.
--
-- Config 'simple' (no stemming / stop-words) keeps parity with the sqlite
-- unicode61 tokenizer: every alphanumeric token is indexed verbatim, so a
-- short query token isn't dropped as a stop word and "news"/"new" aren't
-- conflated by a stemmer. The search code appends `:*` per token for the
-- prefix match (`pok:*` → "Pokemon"); pure mid-word substrings are covered
-- by the substring arm search() UNIONs in, so the result is a SUPERSET of
-- the legacy substring match — identical to the sqlite path.
--
-- `coalesce(...,'')` so a NULL title/overview yields an empty (never NULL)
-- vector; the `||' '||` join keeps title + overview tokens distinct.
ALTER TABLE media_items
    ADD COLUMN IF NOT EXISTS search_tsv tsvector
    GENERATED ALWAYS AS (
        to_tsvector(
            'simple',
            coalesce(title, '') || ' ' || coalesce(overview, '')
        )
    ) STORED;

CREATE INDEX IF NOT EXISTS idx_media_items_search_tsv
    ON media_items USING GIN (search_tsv);
