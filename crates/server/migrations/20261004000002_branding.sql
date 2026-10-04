-- Company branding: the name, logo and accent colour an admin puts on
-- what users see (the agent's windows and tray, quick assist, the MSI and
-- the install pages) in place of TetanusRMM's. One per server: the table
-- holds at most one row.

CREATE TABLE branding (
    only_row   BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (only_row),
    name       TEXT NOT NULL,
    -- Red, green, blue; NULL for TetanusRMM's rust.
    accent     BYTEA CHECK (octet_length(accent) = 3),
    -- A PNG; NULL for TetanusRMM's mark.
    logo_png   BYTEA,
    updated_by TEXT NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
