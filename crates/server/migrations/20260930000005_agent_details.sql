-- Phase 11: more of what the viewer's status panel shows, reported by the
-- agent with its status (protocol 9): the DNS servers it is configured
-- with, and every fixed disk as [{"name", "total_bytes", "used_bytes"}].
-- NULL until known; last known values are kept while the agent is offline.

ALTER TABLE agents
    ADD COLUMN dns_servers TEXT[],
    ADD COLUMN disks       JSONB;
