-- The tool evidence behind one answer, kept so its grounding can be checked
-- after the fact.
--
-- The transcript keeps the reply and nothing about the calls that produced it,
-- so "did it invent that figure?" has been unanswerable once a turn ended — the
-- evidence existed only inside the running turn. The grader saw it live and kept
-- only its verdict, which is why a `poor` grade could not be distinguished from
-- a grader that had been shown the wrong end of a long turn.
--
-- Written only when `[audit] enabled` is set: this is a second copy of customer
-- data (leads, workers, shifts, names), already redacted and bounded to the same
-- per-turn budget the judge sees, and swept after the configured retention.
--
-- Created unqualified: the migration runner sets search_path to `edge_analytics`.
CREATE TABLE IF NOT EXISTS turn_tool_evidence (
    -- The assistant message this evidence produced, so a row joins to both
    -- `usage_events` and `answer_grades` on the same key.
    message_id   TEXT        PRIMARY KEY,
    conversation TEXT        NOT NULL,
    owner        TEXT        NOT NULL,
    agent_id     TEXT        NOT NULL,
    org          TEXT,
    -- Redacted, truncated tool input+output for the turn — byte-for-byte what
    -- the grader was shown, so a verdict can be re-read against its evidence.
    evidence     TEXT        NOT NULL,
    ts           TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Drives the retention sweep; the primary key already covers point lookups.
CREATE INDEX IF NOT EXISTS turn_tool_evidence_ts ON turn_tool_evidence (ts);
