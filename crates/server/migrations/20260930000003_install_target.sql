-- Phase 11: download links can be fetched as an MSI that installs and
-- enrolls the agent unattended. The MSI needs to know where the agent
-- should connect: chosen when the link is made, kept with its token.
-- NULL for links made before this (their MSI uses the server's defaults).

ALTER TABLE enrollment_tokens
    ADD COLUMN install_server      TEXT,
    ADD COLUMN install_server_name TEXT;
