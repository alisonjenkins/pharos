CREATE TABLE IF NOT EXISTS system_identity (
    -- Single-row table; `id = 1` is the only valid value.
    id         INTEGER PRIMARY KEY CHECK (id = 1),
    server_id  TEXT    NOT NULL UNIQUE,
    created_at BIGINT  NOT NULL
);
