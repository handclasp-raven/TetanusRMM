-- Phase 3: one-time enrollment tokens minted with each agent download link.

CREATE TABLE enrollment_tokens (
    id          BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    -- SHA-256 of the token. The token itself is never stored.
    token_hash  BYTEA NOT NULL UNIQUE CHECK (octet_length(token_hash) = 32),
    created_by  TEXT NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at  TIMESTAMPTZ NOT NULL,
    -- Set when the token is consumed; a token is usable only while NULL.
    used_at     TIMESTAMPTZ,
    agent_id    TEXT REFERENCES agents (id),
    CHECK ((used_at IS NULL) = (agent_id IS NULL))
);
