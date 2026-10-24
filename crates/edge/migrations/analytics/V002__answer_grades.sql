-- Automated answer-quality grades, one row per graded terminal turn. Keyed to a
-- turn by (owner, message_id) — the same key `feedback` uses — so a grade joins
-- to its usage_events row and its transcript. Created unqualified: the migration
-- runner sets search_path to `edge_analytics` for this set.
CREATE TABLE IF NOT EXISTS answer_grades (
    id           BIGSERIAL PRIMARY KEY,
    ts           TIMESTAMPTZ NOT NULL DEFAULT now(),
    agent_id     TEXT NOT NULL,
    owner        TEXT NOT NULL,
    conversation TEXT NOT NULL,
    message_id   TEXT NOT NULL,
    -- good | poor | incorrect | unanswerable
    quality      TEXT NOT NULL,
    -- short topic label for clustering gaps (e.g. "rate card upload"); NULL for `good`
    gap_topic    TEXT,
    -- knowledge | behaviour | tool | none — which fix path a gap needs
    remedy       TEXT,
    -- the grader's one-line justification
    reason       TEXT,
    -- how the grade was derived: llm | outcome | feedback
    source       TEXT NOT NULL,
    -- the grading model, when known; NULL for a deterministic grade OR when the
    -- provider's default model was used. Tell an LLM grade from a deterministic
    -- one by `source`, not by whether `model` is present.
    model        TEXT,
    -- per-turn org attribution, mirroring usage_events.org
    org          TEXT,
    UNIQUE (owner, message_id)
);

CREATE INDEX IF NOT EXISTS answer_grades_agent_ts ON answer_grades (agent_id, ts);
CREATE INDEX IF NOT EXISTS answer_grades_quality_ts ON answer_grades (quality, ts);
CREATE INDEX IF NOT EXISTS answer_grades_topic ON answer_grades (gap_topic);
