-- Phase 5: short-lived, single-use tokens that let a viewer attach to one
-- agent's screen through the relay.

CREATE TABLE viewer_sessions (
    id              BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    -- SHA-256 of the token. The token itself is never stored.
    token_hash      BYTEA NOT NULL UNIQUE CHECK (octet_length(token_hash) = 32),
    user_id         BIGINT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    agent_id        TEXT NOT NULL REFERENCES agents (id) ON DELETE CASCADE,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- The token must be used before this; it is only good once.
    expires_at      TIMESTAMPTZ NOT NULL,
    connected_at    TIMESTAMPTZ,
    ended_at        TIMESTAMPTZ
);
CREATE INDEX viewer_sessions_agent_id_idx ON viewer_sessions (agent_id);
