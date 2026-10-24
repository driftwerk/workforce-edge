-- The tenants one deployment serves, and the tenant every edge row belongs
-- to. Rows from before tenancy belong to `default`, the one tenant a
-- self-hosted install has; the column's default stamps them as the column
-- is added, with no rewrite of the table.

CREATE TABLE tenants (
    id           TEXT PRIMARY KEY CHECK (id ~ '^[a-z0-9][a-z0-9-]{0,62}$'),
    display_name TEXT NOT NULL,
    status       TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'suspended')),
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE tenant_hostnames (
    hostname  TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants (id) ON DELETE CASCADE
);
CREATE INDEX tenant_hostnames_tenant ON tenant_hostnames (tenant_id);

INSERT INTO tenants (id, display_name) VALUES ('default', 'Default');

ALTER TABLE edge_agents
    ADD COLUMN tenant_id TEXT NOT NULL DEFAULT 'default' REFERENCES tenants (id);
ALTER TABLE edge_agents
    DROP CONSTRAINT edge_agents_pkey,
    ADD PRIMARY KEY (tenant_id, id);

ALTER TABLE edge_playbooks
    ADD COLUMN tenant_id TEXT NOT NULL DEFAULT 'default' REFERENCES tenants (id);
ALTER TABLE edge_playbooks
    DROP CONSTRAINT edge_playbooks_pkey,
    ADD PRIMARY KEY (tenant_id, id);

ALTER TABLE edge_operators
    ADD COLUMN tenant_id TEXT NOT NULL DEFAULT 'default' REFERENCES tenants (id);
ALTER TABLE edge_operators
    DROP CONSTRAINT edge_operators_pkey,
    ADD PRIMARY KEY (tenant_id, kind, value);

ALTER TABLE edge_audit
    ADD COLUMN tenant_id TEXT NOT NULL DEFAULT 'default' REFERENCES tenants (id);
CREATE INDEX edge_audit_tenant_recent ON edge_audit (tenant_id, id DESC);

ALTER TABLE edge_revisions
    ADD COLUMN tenant_id TEXT NOT NULL DEFAULT 'default' REFERENCES tenants (id);
ALTER TABLE edge_revisions
    DROP CONSTRAINT edge_revisions_pkey,
    ADD PRIMARY KEY (tenant_id, kind, item_id, revision);

ALTER TABLE edge_knowledge
    ADD COLUMN tenant_id TEXT NOT NULL DEFAULT 'default' REFERENCES tenants (id);
ALTER TABLE edge_knowledge
    DROP CONSTRAINT edge_knowledge_pkey,
    ADD PRIMARY KEY (tenant_id, agent_id, id);

-- The session tables: the second step of the shared set's V011, which added
-- the tenant-first indexes beside the old ones so a hub could roll out
-- replica by replica. This database is the edge's alone, so here the old
-- unique keys go and the primary keys lead with the tenant — two tenants may
-- hold the same (agent, channel, owner), the same archived message id, the
-- same turn claim. The store's ON CONFLICT clauses bind to what remains.

DROP INDEX sessions_active_uniq;
DROP INDEX sessions_agent_updated;
DROP INDEX sessions_owner_updated;

DROP INDEX session_archive_message_uniq;
ALTER TABLE session_archive
    DROP CONSTRAINT session_archive_pkey,
    ADD PRIMARY KEY (tenant_id, session_id, seq);

-- The turn-claim upsert names its arbiter `turn_claims_pkey`, so the
-- tenant-first key keeps that name.
ALTER TABLE turn_claims
    DROP CONSTRAINT turn_claims_pkey,
    ADD CONSTRAINT turn_claims_pkey PRIMARY KEY (tenant_id, agent_id, session_id);

DROP INDEX turn_journal_session;
ALTER TABLE turn_journal
    DROP CONSTRAINT turn_journal_pkey,
    ADD PRIMARY KEY (tenant_id, agent_id, session_id, call_id, entry_key);
