-- T72: runtime-mutable named configuration sections. jellyfin-web's
-- dashboard POSTs section blobs to `/System/Configuration/{encoding,
-- network,metadata,livetv,…}` and expects them to survive restart and
-- reflect on the matching GET. pharos's toml config stays authoritative
-- for the fields it owns; each named section is stored as an opaque JSON
-- blob keyed by its section name and overlaid on the served defaults.
CREATE TABLE IF NOT EXISTS named_config (
    key         TEXT PRIMARY KEY,
    value       TEXT NOT NULL,
    updated_at  INTEGER NOT NULL
) STRICT;
