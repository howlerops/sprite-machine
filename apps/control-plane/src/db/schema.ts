/**
 * SQLite schema for the control plane. Inlined as a string so it ships with
 * the bundled artifact without requiring filesystem access at runtime.
 * Postgres-compatible subset for future migration.
 */
export const SCHEMA_SQL = `
create table if not exists sprites (
  id text primary key,
  status text not null,
  agent_base_url text,
  agent_token text not null,
  from_checkpoint_id text,
  created_at text not null,
  updated_at text not null,
  last_active_at text,
  metadata text not null default '{}'
);

create table if not exists runs (
  id text primary key,
  sprite_id text not null references sprites(id),
  sidecar_run_id text not null,
  status text not null,
  harness text not null default 'claude',
  prompt text not null,
  title text,
  workdir text,
  session_id text,
  created_at text not null,
  updated_at text not null,
  last_event_at text,
  total_cost_usd real not null default 0,
  total_input_tokens integer not null default 0,
  total_output_tokens integer not null default 0,
  metadata text not null default '{}',
  unique (sprite_id, sidecar_run_id)
);

create table if not exists run_events (
  run_id text not null references runs(id),
  seq integer not null,
  event text not null,
  created_at text not null,
  primary key (run_id, seq)
);

create table if not exists push_tokens (
  expo_token text primary key,
  label text,
  created_at text not null,
  last_used_at text
);

create index if not exists idx_runs_sprite on runs(sprite_id);
create index if not exists idx_runs_status on runs(status);
create index if not exists idx_runs_updated on runs(updated_at);
`;
