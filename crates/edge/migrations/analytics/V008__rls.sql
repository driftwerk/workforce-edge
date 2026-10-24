-- Row-level security on the analytics tables, as on the edge's tables in
-- `sessions` (see the edge set's V003): the backstop to the tenant predicate
-- every query carries, keyed on the transaction's `app.tenant_id`.

CREATE OR REPLACE FUNCTION tenant_isolation_policy(table_name regclass)
RETURNS void LANGUAGE plpgsql AS $$
BEGIN
    EXECUTE format('ALTER TABLE %s ENABLE ROW LEVEL SECURITY', table_name);
    EXECUTE format('ALTER TABLE %s FORCE ROW LEVEL SECURITY', table_name);
    EXECUTE format(
        'CREATE POLICY tenant_isolation ON %s '
        'USING (tenant_id = current_setting(''app.tenant_id'', true)) '
        'WITH CHECK (tenant_id = current_setting(''app.tenant_id'', true))',
        table_name);
END
$$;

SELECT tenant_isolation_policy(t)
FROM unnest(ARRAY[
    'chat_identities', 'usage_events', 'feedback', 'answer_grades',
    'active_turns', 'turn_tool_evidence'
]::regclass[]) AS t;

DROP FUNCTION tenant_isolation_policy(regclass);
