-- Cross-process registry of in-flight turns.
--
-- In-flight turn state (`host::active_turns`) is a per-process map, so with more
-- than one replica a reconnecting socket that lands on a different task cannot be
-- told that its turn is still running, and a new message there cannot interrupt
-- it. This table makes that ownership visible across processes: it is the minimum
-- shared state needed to DETECT the split today, and the registry the eventual
-- shared-turn transport will claim against.
--
-- Deliberately not authoritative: a turn runs from its owner's memory, and this
-- row is best-effort bookkeeping written off the critical path. A crashed replica
-- leaves an orphan row, so every reader treats a row older than the turn ceiling
-- as absent and `sweep_stale` removes it.
--
-- Created unqualified: the migration runner sets search_path to `edge_analytics`.
CREATE TABLE IF NOT EXISTS active_turns (
    -- Same identity the in-process map keys on (owner + conversation), so the two
    -- views of "is this conversation busy" can never disagree about scope.
    turn_key     TEXT        PRIMARY KEY,
    conversation TEXT        NOT NULL,
    owner        TEXT        NOT NULL,
    agent_id     TEXT        NOT NULL,
    -- Which process owns the running turn. A reader comparing this against its own
    -- id is what distinguishes "my turn" from "running on another replica".
    replica      TEXT        NOT NULL,
    started_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Supports the stale sweep; the primary key already covers point lookups.
CREATE INDEX IF NOT EXISTS active_turns_started_at ON active_turns (started_at);
