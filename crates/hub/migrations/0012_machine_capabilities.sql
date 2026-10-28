-- M2B-002b: the runner's capabilities from its last `hello` (tunnel.md §3.1), the org-wide read
-- model of `GET /v1/machines` (client-api §5.1 `capabilities`). JSON text; NULL until the runner
-- has said hello. The Machine DO keeps the same object as its source of truth.

ALTER TABLE machines ADD COLUMN capabilities TEXT CHECK (capabilities IS NULL OR json_valid(capabilities));
