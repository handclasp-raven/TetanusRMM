-- Phase 11: agents are classified as a server, a desktop or other, and
-- report their operating system.
--
-- classification is an admin's choice; while it is NULL the agent's
-- reported device kind decides (server: server, workstation: desktop,
-- unknown: other). os is reported by the agent (protocol 9), e.g.
-- "Windows 11 Pro (build 26100)"; NULL until known.

CREATE TYPE classification AS ENUM ('server', 'desktop', 'other');

ALTER TABLE agents
    ADD COLUMN classification classification,
    ADD COLUMN os             TEXT;
