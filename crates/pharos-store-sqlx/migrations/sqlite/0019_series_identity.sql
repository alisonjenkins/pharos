-- LIB-C11: folder-keyed series identity. The synthesised Series/Season
-- wire ids previously hashed the bare series NAME, so two distinct shows
-- sharing a name (e.g. "Cosmos (1980)" and "Cosmos (2014)") collapsed to
-- one id and interleaved their episodes. We now key identity on the show
-- FOLDER path (stable + unique per show on disk) and surface the parsed
-- release year so clients can tell same-name shows apart.
--
-- series_folder = canonical filesystem path of the show's root directory
-- (the closest non-"Season NN" ancestor of the episode), captured by the
-- scanner. series_year = 4-digit year parsed from a "Show Name (YYYY)"
-- folder convention. Both additive + nullable: rows predating this
-- migration carry NULL and fall back to the legacy name-keyed identity.
ALTER TABLE media_items ADD COLUMN series_folder TEXT;
ALTER TABLE media_items ADD COLUMN series_year INTEGER;
