-- What `edge.toml` holds for an install, per tenant: the file seeds the
-- `default` tenant's row on first boot, then the row owns it. The JSONB
-- columns hold each config section without its secrets; the secrets sit
-- beside them in BYTEA columns sealed under `EDGE_MASTER_KEY` (AES-256-GCM,
-- bound to the tenant and column), so a dump of this table yields no key.
-- `llm_status` is the last outcome of the tenant's key: the admin banner.

CREATE TABLE tenant_settings (
    tenant_id             TEXT PRIMARY KEY REFERENCES tenants (id) ON DELETE CASCADE,
    llm                   JSONB NOT NULL,
    llm_api_key           BYTEA,
    llm_status            JSONB,
    grader                JSONB,
    token_exchange        JSONB,
    token_exchange_secret BYTEA,
    admin                 JSONB,
    admin_client_secret   BYTEA,
    limits                JSONB NOT NULL,
    audit                 JSONB,
    outbound              TEXT NOT NULL DEFAULT 'private' CHECK (outbound IN ('private', 'public')),
    updated_at            TIMESTAMPTZ NOT NULL DEFAULT now()
);

ALTER TABLE tenant_settings ENABLE ROW LEVEL SECURITY;
ALTER TABLE tenant_settings FORCE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON tenant_settings
    USING (tenant_id = current_setting('app.tenant_id', true))
    WITH CHECK (tenant_id = current_setting('app.tenant_id', true));
