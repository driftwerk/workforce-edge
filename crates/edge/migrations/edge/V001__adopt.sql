-- The edge's own tables in the `sessions` schema, as boot created them with
-- CREATE TABLE IF NOT EXISTS until now. Adopting them here is idempotent on
-- an install that already has them and creates them on a fresh one; from this
-- migration on, their shape is changed only by this set.

CREATE TABLE IF NOT EXISTS edge_agents (
    id TEXT PRIMARY KEY,
    config JSONB NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS edge_playbooks (
    id TEXT PRIMARY KEY,
    source TEXT NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS edge_operators (
    kind TEXT NOT NULL,
    value TEXT NOT NULL,
    added_by TEXT NOT NULL,
    added_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (kind, value)
);

CREATE TABLE IF NOT EXISTS edge_audit (
    id BIGSERIAL PRIMARY KEY,
    operator TEXT NOT NULL,
    action TEXT NOT NULL,
    target TEXT NOT NULL,
    detail TEXT NOT NULL DEFAULT '',
    at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS edge_revisions (
    kind TEXT NOT NULL,
    item_id TEXT NOT NULL,
    revision INT NOT NULL,
    content TEXT NOT NULL,
    edited_by TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (kind, item_id, revision)
);

CREATE TABLE IF NOT EXISTS edge_knowledge (
    id TEXT NOT NULL,
    agent_id TEXT NOT NULL,
    title TEXT NOT NULL,
    content TEXT NOT NULL,
    chunks INT NOT NULL DEFAULT 0,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (agent_id, id)
);
