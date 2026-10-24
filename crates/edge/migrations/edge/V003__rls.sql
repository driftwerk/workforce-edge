-- Row-level security as the backstop to the tenant predicate every query
-- carries. A policy reads the transaction's `app.tenant_id`, which
-- `TenantPool` sets; a query that reaches a table without it sees no rows
-- and can write none. FORCE makes the table owner — the edge's own role —
-- subject to the policy; a superuser or BYPASSRLS role is not, which boot
-- checks (`tenant::rls_enforced`).
--
-- The session store's tables are covered here, not in the shared session
-- set: the hub never sets `app.tenant_id`, so a policy there would blank
-- it. The edge's database is its own; it is never the hub's.

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
    'edge_agents', 'edge_playbooks', 'edge_operators', 'edge_audit',
    'edge_revisions', 'edge_knowledge',
    'sessions', 'session_archive', 'turn_claims', 'turn_journal'
]::regclass[]) AS t;

DROP FUNCTION tenant_isolation_policy(regclass);
