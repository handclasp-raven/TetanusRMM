-- Phase 6: the agent reports whether it is a workstation or a server, which
-- picks the default consent mode (notify / unattended) until an admin sets
-- the device's policy explicitly.

CREATE TYPE device_kind AS ENUM ('workstation', 'server');

ALTER TABLE agents ADD COLUMN device_kind device_kind;

-- True once an admin has set this device's policy; from then on the reported
-- device kind no longer changes it.
ALTER TABLE device_policies ADD COLUMN admin_set BOOLEAN NOT NULL DEFAULT false;
