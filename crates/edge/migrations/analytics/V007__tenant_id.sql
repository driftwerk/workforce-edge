-- The tenant every analytics row belongs to. Rows from before tenancy are
-- `default`'s, stamped by the column default as it is added. The registry
-- lives in the `sessions` schema with the edge's other tables; this schema
-- carries the id without a reference to it, so the two sets stay separate.
-- No row may carry the empty string: it is what the policies' setting reads
-- once the transaction that set it has ended, and must match nothing.

ALTER TABLE chat_identities ADD COLUMN tenant_id TEXT NOT NULL DEFAULT 'default' CHECK (tenant_id <> '');
ALTER TABLE chat_identities
    DROP CONSTRAINT chat_identities_pkey,
    ADD PRIMARY KEY (tenant_id, owner);

ALTER TABLE usage_events ADD COLUMN tenant_id TEXT NOT NULL DEFAULT 'default' CHECK (tenant_id <> '');
DROP INDEX IF EXISTS usage_events_agent_ts;
DROP INDEX IF EXISTS usage_events_owner_ts;
DROP INDEX IF EXISTS usage_events_org_ts;
CREATE INDEX usage_events_agent_ts ON usage_events (tenant_id, agent_id, ts);
CREATE INDEX usage_events_owner_ts ON usage_events (tenant_id, owner, ts);
CREATE INDEX usage_events_org_ts ON usage_events (tenant_id, org, ts);

ALTER TABLE feedback ADD COLUMN tenant_id TEXT NOT NULL DEFAULT 'default' CHECK (tenant_id <> '');
ALTER TABLE feedback
    DROP CONSTRAINT feedback_owner_message_id_key,
    ADD UNIQUE (tenant_id, owner, message_id);

ALTER TABLE answer_grades ADD COLUMN tenant_id TEXT NOT NULL DEFAULT 'default' CHECK (tenant_id <> '');
ALTER TABLE answer_grades
    DROP CONSTRAINT answer_grades_owner_message_id_key,
    ADD UNIQUE (tenant_id, owner, message_id);
DROP INDEX IF EXISTS answer_grades_agent_ts;
DROP INDEX IF EXISTS answer_grades_quality_ts;
DROP INDEX IF EXISTS answer_grades_topic;
CREATE INDEX answer_grades_agent_ts ON answer_grades (tenant_id, agent_id, ts);
CREATE INDEX answer_grades_quality_ts ON answer_grades (tenant_id, quality, ts);
CREATE INDEX answer_grades_topic ON answer_grades (tenant_id, gap_topic);

ALTER TABLE active_turns ADD COLUMN tenant_id TEXT NOT NULL DEFAULT 'default' CHECK (tenant_id <> '');
ALTER TABLE active_turns
    DROP CONSTRAINT active_turns_pkey,
    ADD PRIMARY KEY (tenant_id, turn_key);
DROP INDEX IF EXISTS active_turns_started_at;
CREATE INDEX active_turns_started_at ON active_turns (tenant_id, started_at);

ALTER TABLE turn_tool_evidence ADD COLUMN tenant_id TEXT NOT NULL DEFAULT 'default' CHECK (tenant_id <> '');
ALTER TABLE turn_tool_evidence
    DROP CONSTRAINT turn_tool_evidence_pkey,
    ADD PRIMARY KEY (tenant_id, message_id);
DROP INDEX IF EXISTS turn_tool_evidence_ts;
CREATE INDEX turn_tool_evidence_ts ON turn_tool_evidence (tenant_id, ts);
