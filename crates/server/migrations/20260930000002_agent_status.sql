-- Phase 11: what technicians see about each agent beyond telemetry.
--
-- logged_in_users and local_ip are reported by the agent (protocol 9);
-- remote_ip is the address the server sees it connect from (its public
-- address when behind NAT). All NULL until known; last known values are
-- kept while the agent is offline.

ALTER TABLE agents
    ADD COLUMN logged_in_users TEXT[],
    ADD COLUMN local_ip        TEXT,
    ADD COLUMN remote_ip       TEXT;
