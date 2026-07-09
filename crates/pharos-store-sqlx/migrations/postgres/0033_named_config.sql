-- T72: runtime-mutable named configuration sections (see the sqlite
-- migration of the same name for rationale). Opaque JSON blob per
-- section name, overlaid on the served defaults.
CREATE TABLE IF NOT EXISTS named_config (
    key         TEXT PRIMARY KEY,
    value       TEXT NOT NULL,
    updated_at  BIGINT NOT NULL
);
