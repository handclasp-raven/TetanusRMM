-- Phase 2 initial schema.

CREATE TYPE user_role AS ENUM ('admin', 'support_engineer', 'auditor');

CREATE TABLE users (
    id              BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    username        TEXT NOT NULL UNIQUE,
    -- PHC string, always $argon2id$...
    password_hash   TEXT NOT NULL,
    -- Base32 TOTP secret.
    totp_secret     TEXT NOT NULL,
    -- Last TOTP time step accepted for this user. A code is only ever accepted
    -- once (RFC 6238 section 5.2), so steps <= this value are rejected.
    totp_last_step  BIGINT,
    role            user_role NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- A login is two steps. The password step creates a short-lived 'pending_totp'
-- row; the TOTP step deletes it and creates a fresh 'active' row with a new token.
CREATE TYPE session_stage AS ENUM ('pending_totp', 'active');

CREATE TABLE sessions (
    -- SHA-256 of the bearer token. The token itself is never stored.
    token_hash      BYTEA PRIMARY KEY CHECK (octet_length(token_hash) = 32),
    user_id         BIGINT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    stage           session_stage NOT NULL,
    failed_attempts INT NOT NULL DEFAULT 0,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at      TIMESTAMPTZ NOT NULL
);
CREATE INDEX sessions_user_id_idx ON sessions (user_id);
CREATE INDEX sessions_expires_at_idx ON sessions (expires_at);

CREATE TYPE enrollment_state AS ENUM ('pending', 'enrolled', 'revoked');

CREATE TABLE agents (
    id                  TEXT PRIMARY KEY,
    enrollment_state    enrollment_state NOT NULL DEFAULT 'pending',
    -- Hex SHA-256 of the agent's client certificate (DER).
    cert_fingerprint    TEXT,
    last_seen           TIMESTAMPTZ,
    -- Telemetry, reported from Phase 4 on.
    cpu_percent         REAL,
    mem_used_bytes      BIGINT,
    mem_total_bytes     BIGINT,
    disk_used_bytes     BIGINT,
    disk_total_bytes    BIGINT,
    uptime_secs         BIGINT,
    telemetry_at        TIMESTAMPTZ,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TYPE consent_mode AS ENUM ('require', 'notify', 'unattended');
CREATE TYPE on_no_user AS ENUM ('deny', 'allow');

CREATE TABLE device_policies (
    agent_id                TEXT PRIMARY KEY REFERENCES agents (id) ON DELETE CASCADE,
    consent_mode            consent_mode NOT NULL DEFAULT 'notify',
    on_no_user              on_no_user NOT NULL DEFAULT 'deny',
    consent_timeout_secs    INT NOT NULL DEFAULT 30 CHECK (consent_timeout_secs > 0),
    updated_at              TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Hash-chained, append-only. See src/audit.rs for how `hash` is computed.
-- ids are assigned by the application (1, 2, 3, ...) so a gap means a deleted row.
CREATE TABLE audit_log (
    id          BIGINT PRIMARY KEY CHECK (id > 0),
    ts          TIMESTAMPTZ NOT NULL,
    actor       TEXT NOT NULL,
    action      TEXT NOT NULL,
    target      TEXT,
    detail      JSONB NOT NULL,
    prev_hash   BYTEA NOT NULL CHECK (octet_length(prev_hash) = 32),
    hash        BYTEA NOT NULL UNIQUE CHECK (octet_length(hash) = 32)
);
