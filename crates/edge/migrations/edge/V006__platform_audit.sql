-- The platform's own action log: what the deployment's operators and its
-- services (the billing package) did to tenants, append-only. The tenant
-- admin trail (`edge_audit`) is a tenant's, under row-level security; this
-- one is the platform's, read on the platform host alone, so it carries no
-- policy and no foreign key: a deleted tenant's entries outlive the tenant.
CREATE TABLE platform_audit (
    id BIGSERIAL PRIMARY KEY,
    principal TEXT NOT NULL,
    action TEXT NOT NULL,
    tenant TEXT NOT NULL DEFAULT '',
    detail TEXT NOT NULL DEFAULT '',
    at TIMESTAMPTZ NOT NULL DEFAULT now()
);
