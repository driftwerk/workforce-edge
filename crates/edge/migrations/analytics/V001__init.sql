-- Edge usage/cost analytics — the edge's own `edge_analytics` schema.
--
-- Runs as its own MigrationSet (isolated refinery history) so a roll-back here
-- can't touch the `sessions` schema it rides alongside on the shared pool. The
-- runner has already `SET search_path TO edge_analytics`, so these tables are
-- created UNQUALIFIED; every runtime query schema-qualifies `edge_analytics.*`
-- because the pool pins `search_path = sessions, public`.

-- Subject → name/email, upserted once per turn. `owner` is the join key to
-- `sessions.owner` (`ext:{issuer}␟{sub}`). `org` is DEFERRED — the column
-- exists but is not populated from a real claim yet; `issuer` is stored as its
-- documented fallback source until the true org source is confirmed.
CREATE TABLE IF NOT EXISTS chat_identities (
    owner        TEXT PRIMARY KEY,
    display_name TEXT,
    email        TEXT,
    org          TEXT,
    issuer       TEXT,
    first_seen   TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_seen    TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- One row per terminal turn: the raw token buckets (so cost is always
-- recomputable) plus the cost as-charged, the run shape, and the outcome.
CREATE TABLE IF NOT EXISTS usage_events (
    id                BIGSERIAL PRIMARY KEY,
    ts                TIMESTAMPTZ NOT NULL DEFAULT now(),
    agent_id          TEXT NOT NULL,
    owner             TEXT NOT NULL,
    conversation      TEXT NOT NULL,
    message_id        TEXT,
    model             TEXT NOT NULL,
    input_tokens      INT NOT NULL,
    output_tokens     INT NOT NULL,
    cache_read_tokens INT NOT NULL,
    cache_write_tokens INT NOT NULL,
    rounds            INT NOT NULL,
    tool_count        INT NOT NULL,
    tool_failures     INT NOT NULL,
    duration_ms       BIGINT NOT NULL,
    cost_usd          NUMERIC(12,6) NOT NULL,
    outcome           TEXT NOT NULL,
    org               TEXT
);

CREATE INDEX IF NOT EXISTS usage_events_agent_ts ON usage_events (agent_id, ts);
CREATE INDEX IF NOT EXISTS usage_events_owner_ts ON usage_events (owner, ts);
CREATE INDEX IF NOT EXISTS usage_events_org_ts ON usage_events (org, ts);

-- Thumbs ±1 (+ optional comment) per assistant message. Created now; written in
-- Phase 3. `UNIQUE(owner, message_id)` makes a re-rate an upsert.
CREATE TABLE IF NOT EXISTS feedback (
    id           BIGSERIAL PRIMARY KEY,
    ts           TIMESTAMPTZ NOT NULL DEFAULT now(),
    agent_id     TEXT NOT NULL,
    owner        TEXT NOT NULL,
    conversation TEXT NOT NULL,
    message_id   TEXT NOT NULL,
    rating       SMALLINT NOT NULL,
    comment      TEXT,
    org          TEXT,
    UNIQUE (owner, message_id)
);
