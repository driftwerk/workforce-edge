-- The platform surface: what the billing package sets on a tenant, and the
-- key a tenant's vector store is reached with.
--
-- `plan` holds the caps the platform API sets (turns a month, active users,
-- knowledge chunks, memory objects, upload size, concurrent turns); an empty
-- object is a tenant with no caps, which every existing tenant is. The
-- tenants table carries no row-level policy, so this sits beside the rest
-- of the registry rather than in `tenant_settings`.
--
-- `vellev_api_key` is the key issued to the tenant's own vellev tenant when
-- the platform API created it, sealed like the row's other secrets. A tenant
-- without one is served on the install's key only when it is `default`.

ALTER TABLE tenants ADD COLUMN plan JSONB NOT NULL DEFAULT '{}';

ALTER TABLE tenant_settings ADD COLUMN vellev_api_key BYTEA;
