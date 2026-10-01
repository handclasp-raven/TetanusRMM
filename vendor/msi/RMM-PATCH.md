# Vendored `msi` 0.10.0 (MIT, see LICENSE)

Unmodified except `src/internal/table.rs` (`write_rows`, `stored_order`):
rows are written ordered by their stored primary key (string-pool number or
encoded integer). Upstream orders them by decoded value, so any table whose
key strings were added to the pool out of alphabetical order (e.g. a table
named `Property` next to `_Validation`) produces a database Windows
Installer refuses to open (error 2219, msiexec 1620). Drop this copy once
upstream writes rows in stored order.
