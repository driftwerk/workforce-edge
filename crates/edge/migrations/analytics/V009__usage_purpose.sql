-- Which LLM call a usage row meters. Until now only the customer turn wrote a
-- row; the background calls a turn causes — the answer grader (and the lesson
-- it may extract from a behaviour-gap verdict), lesson extraction, passive
-- memory capture and memory consolidation — now record one each, on the same
-- key, so a tenant's admin shows spend by purpose. Rows from before are turns.
-- The turn KPIs filter on `purpose = 'turn'`, so their meaning is unchanged.

ALTER TABLE usage_events ADD COLUMN purpose TEXT NOT NULL DEFAULT 'turn'
    CHECK (purpose IN ('turn', 'grader', 'lessons', 'passive_memory', 'consolidation'));
CREATE INDEX usage_events_purpose_ts ON usage_events (tenant_id, purpose, ts);
