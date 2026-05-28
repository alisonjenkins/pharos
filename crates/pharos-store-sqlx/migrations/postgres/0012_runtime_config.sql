CREATE TABLE IF NOT EXISTS runtime_config (
    id                INTEGER PRIMARY KEY CHECK (id = 1),
    server_name       TEXT,
    login_disclaimer  TEXT,
    custom_css        TEXT,
    updated_at        BIGINT NOT NULL
);
