-- Phase 13: deployment keys. Unlike a download link's token, a deployment
-- key enrolls any number of agents, so one MSI can be pushed to many
-- machines (GPO, Intune). It works until it expires or is revoked.

CREATE TABLE deployment_keys (
    id                  BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    -- SHA-256 of the key. The key itself is never stored.
    token_hash          BYTEA NOT NULL UNIQUE CHECK (octet_length(token_hash) = 32),
    name                TEXT NOT NULL,
    created_by          TEXT NOT NULL,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- NULL: never expires.
    expires_at          TIMESTAMPTZ,
    -- Set when revoked; a key is usable only while NULL.
    revoked_at          TIMESTAMPTZ,
    revoked_by          TEXT,
    CHECK ((revoked_at IS NULL) = (revoked_by IS NULL)),
    -- Groups an agent joins when it enrolls with the key.
    group_ids           BIGINT[] NOT NULL DEFAULT '{}',
    -- Where the MSI's agent connects.
    install_server      TEXT NOT NULL,
    install_server_name TEXT NOT NULL,
    enrolled_count      BIGINT NOT NULL DEFAULT 0,
    last_enrolled_at    TIMESTAMPTZ
);
