-- Phase 8: agents report their hostname on connect (protocol 6), shown to
-- technicians in the TUI. NULL until a new-enough agent connects.

ALTER TABLE agents ADD COLUMN hostname TEXT;
