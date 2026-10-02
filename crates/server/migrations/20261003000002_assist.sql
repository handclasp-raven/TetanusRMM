-- Phase 12: quick assist. A technician makes a six-digit code; a user on a
-- machine with no agent runs the quick assist client and types it. The code
-- is traded for a short-lived certificate for a throwaway agent, which
-- lives only as long as the session.

CREATE TABLE assist_sessions (
    id          BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    -- SHA-256 of the code while it can still be used; NULL once it has
    -- been redeemed, has expired or was voided. Unique, so two usable
    -- codes are never the same.
    code_hash   BYTEA UNIQUE CHECK (octet_length(code_hash) = 32),
    -- The technician who made the code, and the only support engineer who
    -- may use the session.
    user_id     BIGINT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at  TIMESTAMPTZ NOT NULL,
    redeemed_at TIMESTAMPTZ,
    -- Set when the session's agent is removed.
    ended_at    TIMESTAMPTZ
);

-- Set for a quick assist client's throwaway agent; NULL for installed agents.
ALTER TABLE agents
    ADD COLUMN assist_session_id BIGINT UNIQUE REFERENCES assist_sessions (id) ON DELETE CASCADE;
