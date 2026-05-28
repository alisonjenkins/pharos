-- T-fix-RC1: runtime-mutable branding fields. jellyfin-web's
-- `Branding` dashboard POSTs ServerName / LoginDisclaimer / CustomCss
-- to `/System/Configuration` and expects them to survive restart.
-- Single-row table same shape as system_identity.
CREATE TABLE IF NOT EXISTS runtime_config (
    id                INTEGER PRIMARY KEY CHECK (id = 1),
    server_name       TEXT,
    login_disclaimer  TEXT,
    custom_css        TEXT,
    updated_at        INTEGER NOT NULL
) STRICT;
