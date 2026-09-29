-- Phase 9: agent groups and per-agent access grants (RBAC).
--
-- Roles decide what kind of thing a user may do: admins everything,
-- auditors nothing but read. For support engineers, grants decide *where*:
-- each grant gives one engineer some capabilities on one agent, on every
-- agent in one group, or on all agents.

CREATE TABLE agent_groups (
    id          BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name        TEXT NOT NULL UNIQUE CHECK (char_length(name) BETWEEN 1 AND 64),
    description TEXT NOT NULL DEFAULT '' CHECK (char_length(description) <= 500),
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- An agent may be in any number of groups.
CREATE TABLE agent_group_members (
    group_id    BIGINT NOT NULL REFERENCES agent_groups (id) ON DELETE CASCADE,
    agent_id    TEXT NOT NULL REFERENCES agents (id) ON DELETE CASCADE,
    added_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (group_id, agent_id)
);
CREATE INDEX agent_group_members_agent_id_idx ON agent_group_members (agent_id);

CREATE TABLE access_grants (
    id           BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    user_id      BIGINT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    -- Exactly one scope: an agent, a group, or all agents.
    agent_id     TEXT REFERENCES agents (id) ON DELETE CASCADE,
    group_id     BIGINT REFERENCES agent_groups (id) ON DELETE CASCADE,
    all_agents   BOOLEAN NOT NULL DEFAULT false,
    capabilities TEXT[] NOT NULL CHECK (
        cardinality(capabilities) > 0
        AND capabilities <@ ARRAY['desktop', 'shell', 'script', 'file_transfer']
    ),
    created_by   TEXT NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK (num_nonnulls(agent_id, group_id, NULLIF(all_agents, false)) = 1)
);
CREATE INDEX access_grants_user_id_idx ON access_grants (user_id);

-- Support engineers who existed before grants keep the access they had
-- (every capability on every agent), so upgrading locks nobody out; admins
-- can narrow it. Engineers created from now on start with no access.
INSERT INTO access_grants (user_id, all_agents, capabilities, created_by)
SELECT id, true, ARRAY['desktop', 'shell', 'script', 'file_transfer'], 'migration'
FROM users WHERE role = 'support_engineer';

-- Enrollment links can put the new agent straight into groups.
ALTER TABLE enrollment_tokens ADD COLUMN group_ids BIGINT[] NOT NULL DEFAULT '{}';
