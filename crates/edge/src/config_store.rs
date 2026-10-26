//! Runtime agent config, in the edge's own Postgres.
//!
//! `edge.toml` seeds the store on first boot; after that the DB is the source of
//! truth so an operator can edit agents in the admin UI and have the change
//! persist and hot-apply — no redeploy, no file push. Only the `[[agents]]`
//! config moves to the DB; the server/data-plane/LLM/admin sections stay in the
//! file (they are bootstrap concerns, not operator knobs).
//!
//! Every store here is bound to one tenant at construction: each query runs on
//! a [`TenantPool`] connection scoped to it and names it explicitly, so one
//! tenant's rows are never read or written through another's store.

use std::sync::Arc;

use base64::Engine;
use eyre::{eyre, Result, WrapErr};
use workforce_crypto::{MasterKey, Sealed};

use crate::agent::EdgeAgent;
use crate::tenant::{TenantId, TenantPool};

/// Agent config persisted in `sessions.edge_agents` (one row per agent, the full
/// [`EdgeAgent`] as JSON). Shares the session pool, whose `search_path` is
/// pinned to the `sessions` schema.
///
/// The secrets in an agent, each MCP server's `discovery_credential`, are
/// sealed inside the JSON under the install's master key when there is one —
/// stored as `{"sealed": "<base64>"}` in the field's place, bound to the
/// tenant, the agent and (for an additional server) the server's id — and
/// opened on load. A cleartext value from before the key is read
/// as it is and sealed by the next write. Without a master key the field is
/// stored as it always was.
#[derive(Clone)]
pub struct EdgeConfigStore {
    pool: TenantPool,
    tenant: TenantId,
    master: Option<Arc<MasterKey>>,
}

const DISCOVERY_CREDENTIAL: &str = "discovery_credential";
const SEALED: &str = "sealed";

impl EdgeConfigStore {
    pub fn new(pool: TenantPool, tenant: TenantId, master: Option<Arc<MasterKey>>) -> Self {
        Self {
            pool,
            tenant,
            master,
        }
    }

    fn credential_aad(&self, agent: &str, field: &str) -> Vec<u8> {
        format!("edge-agent/v1/{}/{agent}/{field}", self.tenant).into_bytes()
    }

    /// Every credential slot in an agent's JSON, as its JSON pointer, the
    /// field its seal is bound to, and the server it belongs to. The primary
    /// keeps the field it always had; an additional server's is bound to its
    /// id, so reordering servers keeps every seal openable while a seal moved
    /// to another server does not open.
    fn credential_slots(config: &serde_json::Value) -> Vec<(String, String, String)> {
        let primary = (
            format!("/embed/mcp/{DISCOVERY_CREDENTIAL}"),
            DISCOVERY_CREDENTIAL.to_string(),
            crate::agent::PRIMARY_MCP_ID.to_string(),
        );
        let additional = config
            .pointer("/embed/mcp_servers")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
            .filter_map(|(i, server)| {
                let id = server.get("id")?.as_str()?;
                Some((
                    format!("/embed/mcp_servers/{i}/{DISCOVERY_CREDENTIAL}"),
                    format!("mcp_servers/{id}/{DISCOVERY_CREDENTIAL}"),
                    id.to_string(),
                ))
            });
        std::iter::once(primary).chain(additional).collect()
    }

    /// Seal every cleartext credential inside an agent's JSON, when there is a
    /// key.
    fn seal_credentials(&self, agent: &str, config: &mut serde_json::Value) -> Result<()> {
        let Some(master) = &self.master else {
            return Ok(());
        };
        for (pointer, field, server) in Self::credential_slots(config) {
            let Some(slot) = config.pointer_mut(&pointer) else {
                continue;
            };
            let Some(cleartext) = slot.as_str() else {
                continue;
            };
            let sealed = master
                .seal(&self.credential_aad(agent, &field), cleartext.as_bytes())
                .wrap_err_with(|| {
                    format!("sealing the discovery credential of agent {agent} (server {server})")
                })?;
            let encoded = base64::engine::general_purpose::STANDARD.encode(sealed.to_bytes());
            *slot = serde_json::json!({ SEALED: encoded });
        }
        Ok(())
    }

    /// Open every sealed credential inside an agent's JSON back into the
    /// string the agent struct expects. A sealed value with no key to open it
    /// is an error, not a silently empty credential.
    fn open_credentials(&self, agent: &str, config: &mut serde_json::Value) -> Result<()> {
        for (pointer, field, server) in Self::credential_slots(config) {
            let Some(slot) = config.pointer_mut(&pointer) else {
                continue;
            };
            let Some(encoded) = slot.get(SEALED).and_then(|sealed| sealed.as_str()) else {
                continue;
            };
            let Some(master) = &self.master else {
                return Err(eyre!(
                    "agent {agent} holds a sealed discovery credential (server {server}) but EDGE_MASTER_KEY is not set"
                ));
            };
            let opened = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .map_err(|_| eyre!("malformed sealed credential"))
                .and_then(|bytes| Ok(Sealed::from_bytes(&bytes)?))
                .and_then(|sealed| Ok(master.open(&self.credential_aad(agent, &field), &sealed)?))
                .wrap_err_with(|| {
                    format!(
                        "opening the discovery credential of agent {agent} (server {server}): is \
                         EDGE_MASTER_KEY the key it was sealed under?"
                    )
                })?;
            let cleartext = String::from_utf8(opened.to_vec()).wrap_err_with(|| {
                format!("the discovery credential of agent {agent} (server {server}) is not text")
            })?;
            *slot = serde_json::Value::String(cleartext);
        }
        Ok(())
    }

    /// Seed the store from `edge.toml` agents the FIRST time only — once any row
    /// exists the DB owns agent config and the file's `[[agents]]` are ignored.
    /// Returns whether seeding ran.
    pub async fn seed_if_empty(&self, agents: &[EdgeAgent]) -> Result<bool> {
        if self.count().await? > 0 {
            return Ok(false);
        }
        for agent in agents {
            self.upsert(agent).await?;
        }
        tracing::info!(count = agents.len(), "seeded edge_agents from edge.toml");
        Ok(true)
    }

    async fn count(&self) -> Result<i64> {
        let conn = self.pool.conn(&self.tenant).await?;
        let count: i64 = conn
            .query_one(
                "SELECT count(*) FROM edge_agents WHERE tenant_id = $1",
                &[&self.tenant],
            )
            .await?
            .get(0);
        conn.commit().await?;
        Ok(count)
    }

    /// All agents, ordered by id.
    pub async fn load_all(&self) -> Result<Vec<EdgeAgent>> {
        let conn = self.pool.conn(&self.tenant).await?;
        let rows = conn
            .query(
                "SELECT id, config FROM edge_agents WHERE tenant_id = $1 ORDER BY id",
                &[&self.tenant],
            )
            .await?;
        conn.commit().await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let id: String = row.get(0);
            let mut json: serde_json::Value = row.get(1);
            self.open_credentials(&id, &mut json)?;
            let agent: EdgeAgent = serde_json::from_value(json)
                .wrap_err("decoding a stored agent config (schema drift?)")?;
            out.push(agent);
        }
        Ok(out)
    }

    /// Insert or replace one agent.
    pub async fn upsert(&self, agent: &EdgeAgent) -> Result<()> {
        let mut json = serde_json::to_value(agent).wrap_err("encoding agent config")?;
        self.seal_credentials(&agent.id, &mut json)?;
        let conn = self.pool.conn(&self.tenant).await?;
        conn.execute(
            "INSERT INTO edge_agents (tenant_id, id, config, updated_at) \
             VALUES ($1, $2, $3, now()) \
             ON CONFLICT (tenant_id, id) DO UPDATE SET config = EXCLUDED.config, updated_at = now()",
            &[&self.tenant, &agent.id, &json],
        )
        .await
        .wrap_err("persisting agent config")?;
        conn.commit().await
    }

    /// Remove one agent's stored config. Returns whether a row was deleted.
    pub async fn delete(&self, id: &str) -> Result<bool> {
        let conn = self.pool.conn(&self.tenant).await?;
        let n = conn
            .execute(
                "DELETE FROM edge_agents WHERE tenant_id = $1 AND id = $2",
                &[&self.tenant, &id],
            )
            .await
            .wrap_err("deleting agent config")?;
        conn.commit().await?;
        Ok(n > 0)
    }

    /// Make the store hold exactly `agents` — upsert each and delete any not
    /// present. Used by git-managed installs to mirror the repo on every boot
    /// (git is authoritative). Not transactional, which is fine: boot has no
    /// concurrent writer.
    pub async fn replace_all(&self, agents: &[EdgeAgent]) -> Result<usize> {
        let keep: std::collections::HashSet<&str> = agents.iter().map(|a| a.id.as_str()).collect();
        for existing in self.load_all().await? {
            if !keep.contains(existing.id.as_str()) {
                self.delete(&existing.id).await?;
            }
        }
        for agent in agents {
            self.upsert(agent).await?;
        }
        Ok(agents.len())
    }

    /// Whether a stored agent with this id exists.
    pub async fn exists(&self, id: &str) -> Result<bool> {
        let conn = self.pool.conn(&self.tenant).await?;
        let row = conn
            .query_one(
                "SELECT EXISTS (SELECT 1 FROM edge_agents WHERE tenant_id = $1 AND id = $2)",
                &[&self.tenant, &id],
            )
            .await?;
        conn.commit().await?;
        Ok(row.get(0))
    }
}

/// Guided-flow playbooks persisted in `sessions.edge_playbooks` (one row per
/// playbook, the authored TOML source as text). Seeded from `playbooks_dir` on
/// first boot, then edited in the admin UI. The parsed definitions feed the
/// hot-swappable [`crate::playbook::PlaybookCatalog`].
#[derive(Clone)]
pub struct EdgePlaybookStore {
    pool: TenantPool,
    tenant: TenantId,
}

impl EdgePlaybookStore {
    pub fn new(pool: TenantPool, tenant: TenantId) -> Self {
        Self { pool, tenant }
    }

    /// Whether the store already holds any playbook (so boot can skip the
    /// `playbooks_dir` read entirely once seeded).
    pub async fn is_seeded(&self) -> Result<bool> {
        let conn = self.pool.conn(&self.tenant).await?;
        let count: i64 = conn
            .query_one(
                "SELECT count(*) FROM edge_playbooks WHERE tenant_id = $1",
                &[&self.tenant],
            )
            .await?
            .get(0);
        conn.commit().await?;
        Ok(count > 0)
    }

    /// Seed from the `playbooks_dir` TOMLs the first time only.
    pub async fn seed_if_empty(&self, sources: &[(String, String)]) -> Result<bool> {
        if self.is_seeded().await? {
            return Ok(false);
        }
        for (id, source) in sources {
            self.upsert(id, source).await?;
        }
        tracing::info!(
            count = sources.len(),
            "seeded edge_playbooks from playbooks_dir"
        );
        Ok(true)
    }

    /// Make the store hold exactly `sources` — upsert each and delete any not
    /// present. Git-managed installs mirror the repo's shared `playbooks/` pool
    /// on every boot.
    pub async fn replace_all(&self, sources: &[(String, String)]) -> Result<usize> {
        let keep: std::collections::HashSet<&str> =
            sources.iter().map(|(id, _)| id.as_str()).collect();
        for (existing_id, _) in self.load_all().await? {
            if !keep.contains(existing_id.as_str()) {
                self.delete(&existing_id).await?;
            }
        }
        for (id, source) in sources {
            self.upsert(id, source).await?;
        }
        Ok(sources.len())
    }

    /// All playbook sources as `(id, source)`, ordered by id.
    pub async fn load_all(&self) -> Result<Vec<(String, String)>> {
        let conn = self.pool.conn(&self.tenant).await?;
        let rows = conn
            .query(
                "SELECT id, source FROM edge_playbooks WHERE tenant_id = $1 ORDER BY id",
                &[&self.tenant],
            )
            .await?;
        conn.commit().await?;
        Ok(rows.iter().map(|r| (r.get(0), r.get(1))).collect())
    }

    pub async fn upsert(&self, id: &str, source: &str) -> Result<()> {
        let conn = self.pool.conn(&self.tenant).await?;
        conn.execute(
            "INSERT INTO edge_playbooks (tenant_id, id, source, updated_at) \
             VALUES ($1, $2, $3, now()) \
             ON CONFLICT (tenant_id, id) DO UPDATE SET source = EXCLUDED.source, updated_at = now()",
            &[&self.tenant, &id, &source],
        )
        .await
        .wrap_err("persisting playbook")?;
        conn.commit().await
    }

    /// Delete a playbook; returns whether a row was removed.
    pub async fn delete(&self, id: &str) -> Result<bool> {
        let conn = self.pool.conn(&self.tenant).await?;
        let n = conn
            .execute(
                "DELETE FROM edge_playbooks WHERE tenant_id = $1 AND id = $2",
                &[&self.tenant, &id],
            )
            .await?;
        conn.commit().await?;
        Ok(n > 0)
    }
}

/// The operator allowlist: who may sign into the admin surface. Rows are either
/// an exact `email` or an email `domain` (e.g. `harbor.example`). Seeded from
/// `[admin]` config on first boot, then managed in the UI.
#[derive(Clone)]
pub struct EdgeOperatorStore {
    pool: TenantPool,
    tenant: TenantId,
}

/// One allowlist entry.
#[derive(Debug, serde::Serialize)]
pub struct Operator {
    pub kind: String,
    pub value: String,
    pub added_by: String,
    pub added_at: String,
}

impl Operator {
    /// From a `SELECT kind, value, added_by, added_at` row.
    fn from_row(row: &tokio_postgres::Row) -> Self {
        let at: chrono::DateTime<chrono::Utc> = row.get(3);
        Self {
            kind: row.get(0),
            value: row.get(1),
            added_by: row.get(2),
            added_at: at.to_rfc3339(),
        }
    }
}

impl EdgeOperatorStore {
    pub fn new(pool: TenantPool, tenant: TenantId) -> Self {
        Self { pool, tenant }
    }

    /// Seed the allowlist from config the first time only.
    pub async fn seed_if_empty(&self, emails: &[String], domains: &[String]) -> Result<bool> {
        if self.count().await? > 0 {
            return Ok(false);
        }
        for e in emails.iter().map(|e| e.trim()).filter(|e| !e.is_empty()) {
            self.add("email", &e.to_lowercase(), "config").await?;
        }
        for d in domains.iter().map(|d| d.trim()).filter(|d| !d.is_empty()) {
            self.add("domain", &d.to_lowercase(), "config").await?;
        }
        tracing::info!(
            emails = emails.len(),
            domains = domains.len(),
            "seeded edge_operators from config"
        );
        Ok(true)
    }

    pub async fn list(&self) -> Result<Vec<Operator>> {
        let conn = self.pool.conn(&self.tenant).await?;
        let rows = conn
            .query(
                "SELECT kind, value, added_by, added_at FROM edge_operators \
                 WHERE tenant_id = $1 ORDER BY kind, value",
                &[&self.tenant],
            )
            .await?;
        conn.commit().await?;
        Ok(rows.iter().map(Operator::from_row).collect())
    }

    /// One entry by its key, or `None`.
    pub async fn get(&self, kind: &str, value: &str) -> Result<Option<Operator>> {
        let conn = self.pool.conn(&self.tenant).await?;
        let row = conn
            .query_opt(
                "SELECT kind, value, added_by, added_at FROM edge_operators \
                 WHERE tenant_id = $1 AND kind = $2 AND value = $3",
                &[&self.tenant, &kind, &value],
            )
            .await?;
        conn.commit().await?;
        Ok(row.as_ref().map(Operator::from_row))
    }

    /// Add an entry; `false` when it was already there (the row, its
    /// `added_by` and `added_at` are left as they were).
    pub async fn add(&self, kind: &str, value: &str, added_by: &str) -> Result<bool> {
        let conn = self.pool.conn(&self.tenant).await?;
        let inserted = conn
            .execute(
                "INSERT INTO edge_operators (tenant_id, kind, value, added_by, added_at) \
                 VALUES ($1, $2, $3, $4, now()) ON CONFLICT (tenant_id, kind, value) DO NOTHING",
                &[&self.tenant, &kind, &value, &added_by],
            )
            .await
            .wrap_err("adding operator")?;
        conn.commit().await?;
        Ok(inserted > 0)
    }

    /// Number of allowlist entries.
    pub async fn count(&self) -> Result<i64> {
        let conn = self.pool.conn(&self.tenant).await?;
        let count: i64 = conn
            .query_one(
                "SELECT count(*) FROM edge_operators WHERE tenant_id = $1",
                &[&self.tenant],
            )
            .await?
            .get(0);
        conn.commit().await?;
        Ok(count)
    }

    pub async fn remove(&self, kind: &str, value: &str) -> Result<bool> {
        let conn = self.pool.conn(&self.tenant).await?;
        let n = conn
            .execute(
                "DELETE FROM edge_operators WHERE tenant_id = $1 AND kind = $2 AND value = $3",
                &[&self.tenant, &kind, &value],
            )
            .await?;
        conn.commit().await?;
        Ok(n > 0)
    }

    /// Whether `email` (already lowercased + IdP-verified by the caller) is on
    /// the allowlist — an exact email match, or its domain matches.
    pub async fn is_allowed(&self, email: &str) -> Result<bool> {
        let domain = email.rsplit_once('@').map(|(_, d)| d).unwrap_or("");
        // A malformed email with no domain must never match a (mis-seeded) blank
        // domain row.
        let domain = if domain.is_empty() {
            "\0no-domain\0"
        } else {
            domain
        };
        let conn = self.pool.conn(&self.tenant).await?;
        let row = conn
            .query_one(
                "SELECT EXISTS (SELECT 1 FROM edge_operators \
                 WHERE tenant_id = $1 \
                   AND ((kind = 'email' AND value = $2) \
                     OR (kind = 'domain' AND value = $3)))",
                &[&self.tenant, &email, &domain],
            )
            .await?;
        conn.commit().await?;
        Ok(row.get(0))
    }
}

/// The admin audit trail: one row per operator mutation (who / what / when),
/// append-only. Recording is best-effort — a failed audit write must never fail
/// the mutation it describes, so callers log and continue.
#[derive(Clone)]
pub struct EdgeAuditStore {
    pool: TenantPool,
    tenant: TenantId,
}

/// One audit entry.
#[derive(Debug, serde::Serialize)]
pub struct AuditEntry {
    pub id: i64,
    pub operator: String,
    pub action: String,
    pub target: String,
    pub detail: String,
    pub at: String,
}

impl EdgeAuditStore {
    pub fn new(pool: TenantPool, tenant: TenantId) -> Self {
        Self { pool, tenant }
    }

    /// Append one entry. Best-effort: logs and swallows errors so it can never
    /// fail the mutation being audited.
    pub async fn record(&self, operator: &str, action: &str, target: &str, detail: &str) {
        if let Err(e) = self.try_record(operator, action, target, detail).await {
            tracing::warn!(operator, action, target, error = %e, "audit write failed");
        }
    }

    /// Append one entry, or say why it could not be: for an action that
    /// must not happen unrecorded (a platform rescue), where the caller
    /// stops rather than proceeds on a failed write.
    pub async fn try_record(
        &self,
        operator: &str,
        action: &str,
        target: &str,
        detail: &str,
    ) -> Result<()> {
        let conn = self.pool.conn(&self.tenant).await?;
        conn.execute(
            "INSERT INTO edge_audit (tenant_id, operator, action, target, detail) \
             VALUES ($1, $2, $3, $4, $5)",
            &[&self.tenant, &operator, &action, &target, &detail],
        )
        .await
        .wrap_err("writing the audit entry")?;
        conn.commit().await
    }

    /// The most recent entries (newest first), capped.
    pub async fn recent(&self, limit: i64) -> Result<Vec<AuditEntry>> {
        let conn = self.pool.conn(&self.tenant).await?;
        let rows = conn
            .query(
                "SELECT id, operator, action, target, detail, at FROM edge_audit \
                 WHERE tenant_id = $1 ORDER BY id DESC LIMIT $2",
                &[&self.tenant, &limit],
            )
            .await?;
        conn.commit().await?;
        Ok(rows
            .iter()
            .map(|r| {
                let at: chrono::DateTime<chrono::Utc> = r.get(5);
                AuditEntry {
                    id: r.get(0),
                    operator: r.get(1),
                    action: r.get(2),
                    target: r.get(3),
                    detail: r.get(4),
                    at: at.to_rfc3339(),
                }
            })
            .collect())
    }
}

/// Revision history: a snapshot of each agent/playbook/knowledge item on every
/// save, so an operator can view past versions and restore one. Best-effort —
/// like the audit trail, a failed snapshot never fails the save it records.
#[derive(Clone)]
pub struct EdgeRevisionStore {
    pool: TenantPool,
    tenant: TenantId,
}

/// One revision's metadata (content fetched separately).
#[derive(Debug, serde::Serialize)]
pub struct RevisionMeta {
    pub revision: i32,
    pub edited_by: String,
    pub created_at: String,
}

impl EdgeRevisionStore {
    pub fn new(pool: TenantPool, tenant: TenantId) -> Self {
        Self { pool, tenant }
    }

    /// Append a snapshot as the next revision of `(kind, item_id)`. Best-effort:
    /// logs and swallows so a save is never failed by history-keeping.
    pub async fn record(&self, kind: &str, item_id: &str, content: &str, edited_by: &str) {
        let write = async {
            let conn = self.pool.conn(&self.tenant).await?;
            // Next revision number for this item (races are harmless — the PK
            // makes a collision a no-op the caller can ignore).
            let next: i32 = conn
                .query_one(
                    "SELECT COALESCE(max(revision), 0) + 1 FROM edge_revisions \
                     WHERE tenant_id = $1 AND kind = $2 AND item_id = $3",
                    &[&self.tenant, &kind, &item_id],
                )
                .await?
                .get(0);
            conn.execute(
                "INSERT INTO edge_revisions (tenant_id, kind, item_id, revision, content, edited_by) \
                 VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT DO NOTHING",
                &[&self.tenant, &kind, &item_id, &next, &content, &edited_by],
            )
            .await?;
            conn.commit().await
        };
        if let Err(e) = write.await {
            tracing::warn!(kind, item_id, error = %e, "revision snapshot failed");
        }
    }

    /// Revision metadata for one item, newest first.
    pub async fn list(&self, kind: &str, item_id: &str) -> Result<Vec<RevisionMeta>> {
        let conn = self.pool.conn(&self.tenant).await?;
        let rows = conn
            .query(
                "SELECT revision, edited_by, created_at FROM edge_revisions \
                 WHERE tenant_id = $1 AND kind = $2 AND item_id = $3 ORDER BY revision DESC",
                &[&self.tenant, &kind, &item_id],
            )
            .await?;
        conn.commit().await?;
        Ok(rows
            .iter()
            .map(|r| {
                let at: chrono::DateTime<chrono::Utc> = r.get(2);
                RevisionMeta {
                    revision: r.get(0),
                    edited_by: r.get(1),
                    created_at: at.to_rfc3339(),
                }
            })
            .collect())
    }

    /// The content of one specific revision.
    pub async fn get(&self, kind: &str, item_id: &str, revision: i32) -> Result<Option<String>> {
        let conn = self.pool.conn(&self.tenant).await?;
        let row = conn
            .query_opt(
                "SELECT content FROM edge_revisions \
                 WHERE tenant_id = $1 AND kind = $2 AND item_id = $3 AND revision = $4",
                &[&self.tenant, &kind, &item_id, &revision],
            )
            .await?;
        conn.commit().await?;
        Ok(row.map(|r| r.get(0)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tenant::{rls_tests::with_bound_pool, run_edge_migrations, TenantStore};
    use workforce_common::session_postgres::{run_session_migrations, PostgresSessionStore};
    use workforce_postgres::test_support::with_test_pool;
    use workforce_postgres::PostgresPool;

    fn agent(id: &str) -> EdgeAgent {
        EdgeAgent {
            id: id.to_string(),
            display_name: id.to_uppercase(),
            ..EdgeAgent::default()
        }
    }

    /// One tenant's agents and operators are out of another's store. The
    /// registry writes through `registry`; the stores query through `stores`,
    /// which is the same pool or one the policies bind.
    async fn assert_agents_and_operators_stay_in_their_tenant(
        registry: PostgresPool,
        stores: PostgresPool,
    ) {
        let registry = TenantStore::new(registry);
        let a = TenantId::parse("a").unwrap();
        let b = TenantId::parse("b").unwrap();
        registry.create(&a, "Tenant A").await.unwrap();
        registry.create(&b, "Tenant B").await.unwrap();
        let tenants = TenantPool::new(stores);

        let agents_a = EdgeConfigStore::new(tenants.clone(), a.clone(), None);
        let agents_b = EdgeConfigStore::new(tenants.clone(), b.clone(), None);
        agents_a.upsert(&agent("support")).await.unwrap();
        assert!(agents_a.exists("support").await.unwrap());
        assert!(agents_b.load_all().await.unwrap().is_empty());
        assert!(!agents_b.exists("support").await.unwrap());
        agents_b.upsert(&agent("support")).await.unwrap();
        assert!(agents_b.delete("support").await.unwrap());
        assert_eq!(
            agents_a
                .load_all()
                .await
                .unwrap()
                .iter()
                .map(|agent| agent.id.as_str())
                .collect::<Vec<_>>(),
            ["support"],
            "the same id in two tenants is two rows, and one's delete leaves the other's"
        );

        let operators_a = EdgeOperatorStore::new(tenants.clone(), a);
        let operators_b = EdgeOperatorStore::new(tenants, b);
        assert!(operators_a
            .add("email", "ada@a.example", "test")
            .await
            .unwrap());
        assert!(
            !operators_a
                .add("email", "ada@a.example", "again")
                .await
                .unwrap(),
            "a second add of the same entry is a no-op"
        );
        assert!(operators_a.is_allowed("ada@a.example").await.unwrap());
        assert_eq!(operators_a.list().await.unwrap().len(), 1);
        assert!(
            !operators_b.is_allowed("ada@a.example").await.unwrap(),
            "an operator of one tenant may not sign into another"
        );
        assert!(operators_b.list().await.unwrap().is_empty());
        assert_eq!(operators_b.count().await.unwrap(), 0);
    }

    /// The explicit tenant predicates alone keep one tenant's agents and
    /// operators out of another's store, before any row-level policy applies.
    #[tokio::test(flavor = "multi_thread")]
    async fn one_tenants_agents_and_operators_are_invisible_to_another() {
        with_test_pool(PostgresSessionStore::build_pool, |pool| async move {
            run_session_migrations(&pool).await.unwrap();
            run_edge_migrations(&pool).await.unwrap();
            assert_agents_and_operators_stay_in_their_tenant(pool.clone(), pool).await;
        })
        .await;
    }

    /// The same, with the stores on a pool whose role the policies bind: the
    /// predicates and the policies agree, and neither blanks a tenant's own
    /// rows.
    #[tokio::test(flavor = "multi_thread")]
    async fn one_tenants_agents_and_operators_are_invisible_to_another_under_rls() {
        with_bound_pool(|superuser, bound| async move {
            assert_agents_and_operators_stay_in_their_tenant(superuser, bound).await;
        })
        .await;
    }

    fn agent_with_credential(id: &str, credential: &str) -> EdgeAgent {
        EdgeAgent {
            embed: crate::agent::EmbedSettings {
                mcp: Some(crate::agent::McpSettings {
                    url: "https://mcp.example".into(),
                    discovery_credential: Some(credential.into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..agent(id)
        }
    }

    fn credential_of(agent: &EdgeAgent) -> Option<&str> {
        agent.embed.mcp.as_ref()?.discovery_credential.as_deref()
    }

    async fn raw_credential(pool: &PostgresPool, id: &str) -> serde_json::Value {
        pool.get()
            .await
            .unwrap()
            .query_one(
                "SELECT config -> 'embed' -> 'mcp' -> 'discovery_credential' \
                 FROM edge_agents WHERE id = $1",
                &[&id],
            )
            .await
            .unwrap()
            .get::<_, Option<serde_json::Value>>(0)
            .unwrap_or(serde_json::Value::Null)
    }

    /// With a master key the discovery credential is sealed inside the JSON
    /// and opened on load; a cleartext value from before the key reads as it
    /// is and is sealed by the next write; without a key nothing changes.
    #[tokio::test]
    async fn the_discovery_credential_is_sealed_in_the_json_under_a_master_key() {
        with_test_pool(PostgresSessionStore::build_pool, |pool| async move {
            run_session_migrations(&pool).await.unwrap();
            run_edge_migrations(&pool).await.unwrap();
            let tenants = TenantPool::new(pool.clone());
            let master = crate::settings::tests::test_master_key();
            let plain = EdgeConfigStore::new(tenants.clone(), TenantId::default_tenant(), None);
            let sealed = EdgeConfigStore::new(
                tenants.clone(),
                TenantId::default_tenant(),
                Some(Arc::clone(&master)),
            );

            sealed
                .upsert(&agent_with_credential("support", "svc-token-1"))
                .await
                .unwrap();
            let raw = raw_credential(&pool, "support").await;
            assert!(raw.get("sealed").is_some(), "sealed in place: {raw}");
            assert!(!raw.to_string().contains("svc-token-1"));
            let loaded = sealed.load_all().await.unwrap();
            assert_eq!(credential_of(&loaded[0]), Some("svc-token-1"));
            let err = plain.load_all().await.unwrap_err();
            assert!(
                format!("{err:#}").contains("EDGE_MASTER_KEY is not set"),
                "{err:#}"
            );
            let other = EdgeConfigStore::new(
                tenants.clone(),
                TenantId::default_tenant(),
                Some(Arc::new(MasterKey::from_bytes(&[9u8; 32]))),
            );
            assert!(
                other.load_all().await.is_err(),
                "another key does not open it"
            );

            plain
                .upsert(&agent_with_credential("legacy", "svc-token-2"))
                .await
                .unwrap();
            assert_eq!(raw_credential(&pool, "legacy").await, "svc-token-2");
            let legacy = sealed
                .load_all()
                .await
                .unwrap()
                .into_iter()
                .find(|a| a.id == "legacy")
                .unwrap();
            assert_eq!(
                credential_of(&legacy),
                Some("svc-token-2"),
                "cleartext from before the key is read as it is"
            );
            sealed.upsert(&legacy).await.unwrap();
            assert!(
                raw_credential(&pool, "legacy")
                    .await
                    .get("sealed")
                    .is_some(),
                "and sealed on the next write"
            );

            sealed.upsert(&agent("toolless")).await.unwrap();
            assert_eq!(
                raw_credential(&pool, "toolless").await,
                serde_json::Value::Null
            );
        })
        .await;
    }

    fn agent_with_servers(id: &str) -> EdgeAgent {
        let server = |id: &str| crate::agent::McpServerSettings {
            id: id.into(),
            connection: crate::agent::McpSettings {
                url: format!("https://{id}.example.test/mcp"),
                discovery_credential: Some(format!("{id}-secret")),
                ..Default::default()
            },
            allowed_tools: vec![format!("{id}.*")],
            ..Default::default()
        };
        let mut agent = agent_with_credential(id, "primary-secret");
        agent.embed.mcp_servers = vec![server("docs"), server("billing")];
        agent
    }

    async fn raw_config(pool: &PostgresPool, id: &str) -> serde_json::Value {
        pool.get()
            .await
            .unwrap()
            .query_one("SELECT config FROM edge_agents WHERE id = $1", &[&id])
            .await
            .unwrap()
            .get(0)
    }

    /// Every server's credential is sealed in place and opens on load; the
    /// primary's seal is bound exactly as before.
    #[tokio::test]
    async fn an_additional_servers_credential_is_sealed_and_opens_on_load() {
        with_test_pool(PostgresSessionStore::build_pool, |pool| async move {
            run_session_migrations(&pool).await.unwrap();
            run_edge_migrations(&pool).await.unwrap();
            let store = EdgeConfigStore::new(
                TenantPool::new(pool.clone()),
                TenantId::default_tenant(),
                Some(crate::settings::tests::test_master_key()),
            );
            store.upsert(&agent_with_servers("support")).await.unwrap();
            let raw = raw_config(&pool, "support").await;
            for pointer in [
                "/embed/mcp/discovery_credential",
                "/embed/mcp_servers/0/discovery_credential",
                "/embed/mcp_servers/1/discovery_credential",
            ] {
                assert!(
                    raw.pointer(pointer).unwrap().get("sealed").is_some(),
                    "{pointer}"
                );
            }
            assert!(!raw.to_string().contains("-secret"), "{raw}");
            let loaded = store.load_all().await.unwrap();
            let credentials: Vec<Option<&str>> = loaded[0]
                .embed
                .mcp_server_specs()
                .iter()
                .map(|s| s.connection.discovery_credential.as_deref())
                .collect();
            assert_eq!(
                credentials,
                [
                    Some("primary-secret"),
                    Some("docs-secret"),
                    Some("billing-secret")
                ]
            );
        })
        .await;
    }

    /// A seal opens only for the server it was made for: reordering servers
    /// keeps every credential, while a blob moved to another server fails to
    /// open and the error names that server.
    #[tokio::test]
    async fn a_sealed_credential_is_bound_to_its_server_id() {
        with_test_pool(PostgresSessionStore::build_pool, |pool| async move {
            run_session_migrations(&pool).await.unwrap();
            run_edge_migrations(&pool).await.unwrap();
            let store = EdgeConfigStore::new(
                TenantPool::new(pool.clone()),
                TenantId::default_tenant(),
                Some(crate::settings::tests::test_master_key()),
            );
            store.upsert(&agent_with_servers("support")).await.unwrap();
            let mut raw = raw_config(&pool, "support").await;
            let write = |raw: serde_json::Value| {
                let pool = pool.clone();
                async move {
                    pool.get()
                        .await
                        .unwrap()
                        .execute(
                            "UPDATE edge_agents SET config = $1 WHERE id = 'support'",
                            &[&raw],
                        )
                        .await
                        .unwrap();
                }
            };

            raw["embed"]["mcp_servers"]
                .as_array_mut()
                .unwrap()
                .reverse();
            write(raw.clone()).await;
            let loaded = store.load_all().await.unwrap();
            assert_eq!(loaded[0].embed.mcp_servers[0].id, "billing");
            assert_eq!(
                loaded[0].embed.mcp_servers[0]
                    .connection
                    .discovery_credential
                    .as_deref(),
                Some("billing-secret"),
                "reordering keeps every seal openable"
            );

            let servers = raw["embed"]["mcp_servers"].as_array_mut().unwrap();
            let moved = servers[0]["discovery_credential"].clone();
            servers[0]["discovery_credential"] = servers[1]["discovery_credential"].clone();
            servers[1]["discovery_credential"] = moved;
            write(raw).await;
            let err = format!("{:#}", store.load_all().await.unwrap_err());
            assert!(err.contains("server billing"), "{err}");
        })
        .await;
    }
}
