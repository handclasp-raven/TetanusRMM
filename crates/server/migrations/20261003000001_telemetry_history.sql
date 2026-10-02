-- Telemetry history: a rolling window of the health samples agents send on
-- their heartbeats, for the technicians' stats panel. At most one sample
-- per agent per step is kept (see registry::TELEMETRY_STEP), and samples
-- older than the retention are pruned. The latest values stay on the
-- agents row as before.

CREATE TABLE agent_telemetry (
    agent_id         TEXT        NOT NULL REFERENCES agents (id) ON DELETE CASCADE,
    ts               TIMESTAMPTZ NOT NULL DEFAULT now(),
    cpu_percent      REAL        NOT NULL,
    mem_used_bytes   BIGINT      NOT NULL,
    mem_total_bytes  BIGINT      NOT NULL,
    disk_used_bytes  BIGINT      NOT NULL,
    disk_total_bytes BIGINT      NOT NULL,
    PRIMARY KEY (agent_id, ts)
);

-- For pruning.
CREATE INDEX agent_telemetry_ts ON agent_telemetry (ts);
