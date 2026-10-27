//! One tenant's running state: its agent hosts, playbook catalog, stores,
//! limiters, admin auth and git-sync state — everything an install owned as
//! a whole before it served more than one tenant. A self-hosted install has
//! exactly one; a cloud deployment holds one per active tenant, swapped
//! whole on reload and dropped on suspend.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;

use axum::http::StatusCode;
use eyre::{Result, WrapErr};
use serde::Serialize;
use tokio::sync::Semaphore;
use tokio_util::sync::{CancellationToken, WaitForCancellationFuture};
use workforce_memory::store::MemoryStore;

use crate::admin::AdminAuth;
use crate::agent::EdgeAgent;
use crate::analytics::EdgeAnalyticsStore;
use crate::boot::{ArtifactStoreHandle, DataPlane, SharedInfra};
use crate::config::{AuditConfig, DemoConfig, GitConfig, GraderConfig, LlmConfig};
use crate::config_store::{
    EdgeAuditStore, EdgeConfigStore, EdgeOperatorStore, EdgePlaybookStore, EdgeRevisionStore,
};
use crate::host::AgentHost;
use crate::ingress::RateLimiter;
use crate::knowledge::KnowledgeStore;
use crate::outbound::OutboundPolicy;
use crate::playbook::PlaybookCatalog;
use crate::settings::{LlmStatusReporter, TenantSettingsStore};
use crate::tenant::{PlanUsage, TenantId, TenantSettings, Usage};
use crate::token_exchange::TokenExchanger;

/// A tenant as the process serves it. Agent hosts live behind a lock so the
/// admin API can rebuild one on an edit and swap it in; in-flight turns keep
/// the `Arc<AgentHost>` they cloned at start, so a swap never tears a running
/// turn.
pub struct TenantRuntime {
    pub id: TenantId,
    pub name: String,
    hosts: std::sync::RwLock<HashMap<String, Arc<AgentHost>>>,
    /// Per-IP flood guard, checked before any crypto.
    pub(crate) rate_limiter: RateLimiter,
    /// Per-user turn budget, checked once the caller is verified. The per-IP
    /// limiter above cannot do this job: a whole office shares one address.
    pub(crate) user_limiter: Arc<RateLimiter<String>>,
    /// A tight, dedicated per-IP budget for the admin login (separate from the
    /// general limiter) — a password endpoint gets far less headroom.
    pub(crate) login_limiter: RateLimiter,
    /// This tenant's ceiling on concurrently-running turns, under the
    /// process-wide one.
    pub(crate) turn_limit: Arc<Semaphore>,
    /// The built-in manual test page; disabled (404) unless configured on.
    pub(crate) demo: DemoConfig,
    /// Operator auth; `None` ⇒ the whole admin surface is 404.
    pub(crate) admin: Option<AdminAuth>,
    /// Runtime agent-config store — edits persist here and hot-apply.
    pub(crate) config_store: EdgeConfigStore,
    /// Runtime playbook store — the admin UI's flows persist here and hot-swap
    /// the catalog.
    pub(crate) playbook_store: EdgePlaybookStore,
    /// The training-data corpus store (authored docs + embedded chunks).
    pub(crate) knowledge: KnowledgeStore,
    /// The operator allowlist (who may sign into the admin surface).
    pub(crate) operators: EdgeOperatorStore,
    /// The admin audit trail (who changed what, when).
    pub(crate) audit: EdgeAuditStore,
    /// Revision history (a snapshot of each item on every save).
    pub(crate) revisions: EdgeRevisionStore,
    /// Everything needed to (re)build an [`AgentHost`] when an agent is edited.
    pub(crate) rebuild: RebuildDeps,
    /// Serializes agent edits so two concurrent PUTs can't interleave into a
    /// DB-vs-memory split (store one edit, swap the other).
    pub(crate) edit_lock: tokio::sync::Mutex<()>,
    /// True when `[git_config]` is set: agents/playbooks/training come from git,
    /// so their admin mutation routes are read-only (git is the source of truth).
    pub(crate) git_managed: bool,
    /// The settings this runtime was built from, so the admin API can show
    /// them and seed the tenant's settings row from them.
    pub(crate) settings: TenantSettings,
    /// The tenant's settings row, when the install keeps one (a master key is
    /// set); `None` leaves the file in charge and the settings API read-only.
    pub(crate) settings_store: Option<TenantSettingsStore>,
    /// The tenant's key state, shared by its hosts and the admin API.
    pub(crate) llm_status: Arc<LlmStatusReporter>,
    /// The tenant's plan beside its usage: what admits a turn under the
    /// monthly caps, shared with its hosts and knowledge store.
    pub(crate) plan_usage: Arc<PlanUsage>,
    /// Commit SHA last applied by a git sync, so the poll skips no-op re-applies.
    last_synced_sha: std::sync::Mutex<Option<String>>,
    /// Cancelled when this runtime is retired or dropped, so its background
    /// work (the git poll) and the sockets pinned to it end with it.
    tasks: CancellationToken,
    /// Why the runtime was retired, set before `tasks` is cancelled so a
    /// socket woken by the cancellation can tell its widget what happened.
    retired: OnceLock<Retirement>,
}

/// Why a runtime stopped serving: the socket layer closes each open socket
/// with a reason the widget acts on (stop, or reconnect to the replacement).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Retirement {
    /// The tenant is suspended; carries the message its callers are shown.
    Suspended(String),
    /// A rebuilt runtime took over; the widget should reconnect to it.
    Reloaded,
}

/// Outcome of a git sync, returned to the admin "Sync now" button.
#[derive(Debug, Serialize)]
pub struct SyncReport {
    /// False when the repo hadn't moved since the last apply (a no-op).
    pub changed: bool,
    pub commit: String,
    pub agents: usize,
    pub playbooks: usize,
    pub training: usize,
}

/// The inputs `AgentHost::build` needs, kept so the admin API can rebuild a host
/// after an edit without threading them through every call.
pub(crate) struct RebuildDeps {
    pub(crate) data_plane: Arc<DataPlane>,
    pub(crate) llm: LlmConfig,
    pub(crate) catalog: Arc<PlaybookCatalog>,
    /// Tenant-level RFC 8693 exchanger, shared across the tenant's hosts.
    pub(crate) token_exchange: Option<Arc<TokenExchanger>>,
    /// The git config, so a runtime sync can re-fetch the repo without a restart.
    pub(crate) git_config: Option<GitConfig>,
    /// The config-repo loader, reaching GitHub under the tenant's policy.
    pub(crate) github: crate::git_config::GitHub,
    /// Answer-grader config, so a rebuilt host keeps grading.
    pub(crate) grader: Option<GraderConfig>,
    /// Audit config, so a rebuilt host keeps (or keeps not) retaining evidence.
    pub(crate) audit: Option<AuditConfig>,
    /// The tenant's key state, which every host reports its turns to.
    pub(crate) llm_status: Arc<LlmStatusReporter>,
    /// How the tenant's MCP and permissions endpoints are reached.
    pub(crate) outbound: OutboundPolicy,
    /// The tenant's plan and usage, which every host caps its memory by.
    pub(crate) plan_usage: Arc<PlanUsage>,
}

impl RebuildDeps {
    /// Build the host for `agent` without mounting it — the admin edit, create
    /// and import paths build first and write only on success.
    pub(crate) async fn build_host(&self, agent: Arc<EdgeAgent>) -> Result<AgentHost> {
        AgentHost::build(
            agent,
            &self.data_plane,
            &self.llm,
            Arc::clone(&self.catalog),
            self.token_exchange.clone(),
            self.grader.as_ref(),
            self.audit.as_ref(),
            Arc::clone(&self.llm_status),
            self.outbound,
            Arc::clone(&self.plan_usage),
        )
        .await
    }
}

impl TenantRuntime {
    /// Build one [`AgentHost`] per agent. Agent config comes from the DB store
    /// (seeded from the settings' agents on first boot); the seed is the seed
    /// only, so subsequent edits in the admin UI survive restarts.
    pub async fn build(settings: TenantSettings, infra: &SharedInfra) -> Result<Arc<Self>> {
        // Fail loudly on a duplicate agent id in the seed rather than silently
        // shadowing (the DB store is keyed by id, so a dup would collapse).
        let mut seen = HashSet::new();
        for agent in &settings.seed_agents {
            if !seen.insert(&agent.id) {
                eyre::bail!("duplicate agent id {:?} in edge config", agent.id);
            }
        }
        let tenant = settings.id.clone();
        // Install policy, not the tenant's: a provider outside the allowlist
        // is refused here however the tenant came to name it.
        settings
            .llm
            .provider_allowed()
            .wrap_err_with(|| format!("tenant {tenant}"))?;
        let memory = infra.memory_for(&tenant).await?;
        let artifacts = infra.artifacts_for(&tenant).await?;
        let data_plane = Arc::new(DataPlane::for_tenant(
            infra,
            tenant.clone(),
            memory,
            artifacts,
        ));
        let settings_store = infra.settings_store_for(&tenant);
        let llm_status = Arc::new(LlmStatusReporter::new(
            settings_store.clone(),
            settings.llm_status.clone(),
        ));
        let plan_usage = Arc::new(PlanUsage::new(
            settings.plan.clone(),
            tenant.clone(),
            data_plane.tenants.clone(),
            Arc::clone(&data_plane.analytics),
            Arc::clone(&data_plane.memory),
        ));
        // The agent store first — we need the last-good DB mirror to decide
        // whether git state is safe to trust below.
        let config_store = EdgeConfigStore::new(
            data_plane.tenants.clone(),
            tenant.clone(),
            infra.master_key.clone(),
        );

        // Git-backed config (opt-in). `git_managed` (admin read-only) is decided by
        // config alone, so a transient fetch failure never flips the install back
        // to editable. `git_tree` is Some ONLY when we have fresh, trustworthy git
        // content to MIRROR; when it's None on a git-managed install we serve the
        // DB's last-good mirror untouched. Two protections (both: never let a bad
        // fetch destroy a running config):
        //   - fetch FAILS (GitHub down, PAT expired, 5xx) → keep last-good DB (loud
        //     warn); abort only when there's nothing to fall back to (first boot).
        //   - fetch SUCCEEDS but returns ZERO agents while the DB holds some →
        //     refuse to wipe (a restructured/renamed repo or wrong ref parses to
        //     nothing); keep last-good.
        let git_managed = settings.git_config.is_some();
        let github = infra.github(settings.outbound);
        let db_has_agents = !config_store
            .load_all()
            .await
            .wrap_err("loading agents")?
            .is_empty();
        let git_tree = match &settings.git_config {
            Some(gc) => match github.fetch(gc).await {
                Ok(tree) if tree.agents.is_empty() && db_has_agents => {
                    tracing::error!(
                        tenant = %tenant, repo = %gc.repo, git_ref = %gc.git_ref,
                        "git config returned ZERO agents but the DB holds a last-good mirror — \
                         refusing to wipe; serving last-good. Check the repo layout/ref."
                    );
                    None
                }
                Ok(tree) => {
                    tracing::info!(
                        tenant = %tenant, repo = %gc.repo, git_ref = %gc.git_ref,
                        agents = tree.agents.len(), playbooks = tree.playbooks.len(),
                        training = tree.training.len(),
                        "git-backed config loaded; admin is read-only for these items"
                    );
                    Some(tree)
                }
                Err(e) if db_has_agents => {
                    tracing::error!(
                        tenant = %tenant, error = %e, repo = %gc.repo,
                        "git config fetch FAILED; serving last-good DB mirror (admin stays \
                         read-only). Fix the upstream/PAT; a restart will retry."
                    );
                    None
                }
                Err(e) => {
                    return Err(e).wrap_err(
                        "git config fetch failed and there is no last-good config to fall back to",
                    );
                }
            },
            None => None,
        };

        // Mirror only fresh, non-degenerate git content. On a git-managed install
        // with no fresh tree, keep the DB's last-good mirror as-is.
        match (&git_tree, git_managed) {
            (Some(tree), _) => {
                config_store
                    .replace_all(&tree.agents)
                    .await
                    .wrap_err("mirroring git agents")?;
            }
            (None, true) => {}
            (None, false) => {
                config_store
                    .seed_if_empty(&settings.seed_agents)
                    .await
                    .wrap_err("seeding edge_agents")?;
            }
        }
        let stored = config_store.load_all().await.wrap_err("loading agents")?;
        // Branding is part of the agent config: git's `agent.toml` on a managed
        // install (read-only in the admin), or the DB agent def otherwise (edited
        // via the normal agent update). There is no separate overlay to merge.
        let agents: Vec<Arc<EdgeAgent>> = stored.into_iter().map(Arc::new).collect();

        // Guided flows: seed the playbook store from `playbooks_dir` on first
        // boot, then the DB is authoritative and edits in the admin UI hot-swap
        // the catalog. A dangling agent→playbook ref only warns (both are
        // runtime-editable, so boot must stay resilient).
        let playbook_store = EdgePlaybookStore::new(data_plane.tenants.clone(), tenant.clone());
        // Git-managed with fresh content: mirror the repo's shared `playbooks/`
        // pool. Git-managed without fresh content (fetch fell back to last-good):
        // keep the DB playbooks untouched. Otherwise read `playbooks_dir` only
        // when the store is empty (first boot) — once seeded the DB is
        // authoritative, so skipping the dir avoids re-reading it and avoids a
        // later dup-id in the dir bricking boot.
        if let Some(tree) = &git_tree {
            playbook_store
                .replace_all(&tree.playbooks)
                .await
                .wrap_err("mirroring git playbooks")?;
        } else if git_managed {
            // Serving last-good DB mirror; leave playbooks as they are.
        } else if playbook_store.is_seeded().await? {
            if settings.playbooks_dir.is_some() {
                tracing::info!(
                    tenant = %tenant,
                    "edge_playbooks already seeded; playbooks_dir is ignored (DB is authoritative)"
                );
            }
        } else {
            let dir_sources = crate::playbook::dir_sources(settings.playbooks_dir.as_deref())
                .wrap_err("reading playbooks_dir")?;
            playbook_store
                .seed_if_empty(&dir_sources)
                .await
                .wrap_err("seeding edge_playbooks")?;
        }
        let defs = crate::playbook::parse_sources(
            playbook_store
                .load_all()
                .await
                .wrap_err("loading playbooks")?,
        );
        crate::playbook::warn_dangling_refs(&agents, &defs);
        let catalog = PlaybookCatalog::new(defs);
        // Training data: authored docs in Postgres, chunks in the vector store.
        // The store writes to the raw memory store, not the per-turn
        // `CappedMemory` the hosts get: a corpus is bounded by the plan's
        // `knowledge_chunks` cap, checked before a document is embedded, so
        // capping its chunks a second time under `memory_objects` would only
        // let the two caps refuse each other's writes.
        let knowledge = KnowledgeStore::new(
            data_plane.tenants.clone(),
            tenant.clone(),
            data_plane.memory.clone(),
        )
        .with_plan(Arc::clone(&plan_usage));
        // Git-managed: mirror each agent's `training/` folder into its corpus.
        // Best-effort — a training re-embed failure warns but never bricks boot
        // (unlike the structural agents/playbooks mirror above).
        if let Some(tree) = &git_tree {
            let mut by_agent: HashMap<&str, Vec<(String, String, String)>> = HashMap::new();
            for f in &tree.training {
                by_agent
                    .entry(f.agent_id.as_str())
                    .or_default()
                    .push(crate::git_config::training_doc(f));
            }
            for agent in &agents {
                let docs = by_agent.remove(agent.id.as_str()).unwrap_or_default();
                match knowledge.replace_agent_docs(&agent.id, &docs).await {
                    Ok((embedded, kept, removed)) => tracing::info!(
                        tenant = %tenant, agent = %agent.id, embedded, kept, removed,
                        "git training data synced"
                    ),
                    Err(e) => {
                        tracing::warn!(tenant = %tenant, agent = %agent.id, error = %e, "git training sync failed")
                    }
                }
            }
            for orphan in by_agent.keys() {
                tracing::warn!(tenant = %tenant, agent = %orphan, "training/ folder has no matching agent; ignored");
            }
            // Cascade-delete: an agent removed/renamed in git had its row pruned by
            // replace_all above, but its corpus was mirrored per-current-agent and
            // would otherwise linger. Prune it here (only reached with a fresh,
            // non-degenerate tree — the zero-agent case set git_tree to None).
            let keep: HashSet<&str> = agents.iter().map(|a| a.id.as_str()).collect();
            match knowledge.prune_agents_not_in(&keep).await {
                Ok(pruned) => {
                    for (id, removed) in pruned {
                        tracing::warn!(tenant = %tenant, agent = %id, docs_removed = removed,
                            "cascade-delete: pruned corpus of an agent no longer in git config");
                    }
                }
                Err(e) => {
                    tracing::warn!(tenant = %tenant, error = %e, "cascade-delete: knowledge prune failed")
                }
            }
        }
        // Operator allowlist: seed emails/domains from [admin] on first boot.
        let operators = EdgeOperatorStore::new(data_plane.tenants.clone(), tenant.clone());
        let audit = EdgeAuditStore::new(data_plane.tenants.clone(), tenant.clone());
        let revisions = EdgeRevisionStore::new(data_plane.tenants.clone(), tenant.clone());
        if let Some(admin_cfg) = &settings.admin {
            operators
                .seed_if_empty(&admin_cfg.allowed_emails, &admin_cfg.allowed_domains)
                .await
                .wrap_err("seeding edge_operators")?;
        }
        // Token exchange (opt-in): one exchanger shared by the tenant's hosts.
        // Configuring the block is explicit operator intent, so boot fails fast on
        // an empty field (an unset ${ENV} expands to "") rather than warning on
        // every turn against a half-configured exchange.
        let token_exchange = match settings.token_exchange.clone() {
            Some(cfg) => {
                if cfg.issuer.trim().is_empty()
                    || cfg.client_id.trim().is_empty()
                    || cfg.client_secret.trim().is_empty()
                {
                    eyre::bail!(
                        "[token_exchange] is configured but issuer/client_id/client_secret is \
                         empty — is the secret's environment variable set?"
                    );
                }
                tracing::info!(tenant = %tenant, issuer = %cfg.issuer, client_id = %cfg.client_id, "token exchange enabled: minting per-turn MCP tokens");
                Some(Arc::new(TokenExchanger::with_policy(
                    cfg,
                    settings.outbound,
                )))
            }
            None => None,
        };
        let rebuild = RebuildDeps {
            data_plane,
            llm: settings.llm.clone(),
            catalog,
            token_exchange,
            git_config: settings.git_config.clone(),
            github,
            grader: settings.grader.clone(),
            audit: settings.audit.clone(),
            llm_status: Arc::clone(&llm_status),
            outbound: settings.outbound,
            plan_usage: Arc::clone(&plan_usage),
        };
        let mut hosts = HashMap::new();
        for agent in &agents {
            let host = rebuild
                .build_host(Arc::clone(agent))
                .await
                .wrap_err_with(|| format!("building agent host {}", agent.id))?;
            hosts.insert(agent.id.clone(), Arc::new(host));
        }
        let admin = settings
            .admin_with_session_key(infra.session_root.as_deref())
            .map(|cfg| AdminAuth::new(&cfg, tenant.clone(), settings.outbound))
            .transpose()
            .wrap_err("building admin auth")?;
        let runtime = Arc::new(Self {
            id: tenant,
            name: settings.name.clone(),
            hosts: std::sync::RwLock::new(hosts),
            rate_limiter: RateLimiter::new(
                settings.limits.rate_limit_per_min,
                Duration::from_secs(60),
            ),
            user_limiter: Arc::new(RateLimiter::per_user(
                settings.limits.user_rate_limit_per_min,
                Duration::from_secs(60),
            )),
            login_limiter: RateLimiter::new(10, Duration::from_secs(60)),
            turn_limit: Arc::new(Semaphore::new(settings.limits.max_concurrent_turns.max(1))),
            demo: settings.demo.clone(),
            admin,
            config_store,
            playbook_store,
            knowledge,
            operators,
            audit,
            revisions,
            rebuild,
            edit_lock: tokio::sync::Mutex::new(()),
            git_managed,
            settings_store,
            llm_status,
            plan_usage,
            settings,
            last_synced_sha: std::sync::Mutex::new(None),
            tasks: CancellationToken::new(),
            retired: OnceLock::new(),
        });
        // Pin the commit boot actually applied (only when it loaded a fresh tree;
        // if it fell back to the last-good DB, leave it unset so the first poll
        // applies the now-reachable commit), then start the live-sync poll.
        if let Some(gc) = &runtime.settings.git_config {
            if git_tree.is_some() {
                if let Ok(sha) = runtime.rebuild.github.latest_commit_sha(gc).await {
                    *runtime.last_synced_sha.lock().expect("sync sha") = Some(sha);
                }
            }
            if gc.poll_secs > 0 {
                runtime.spawn_git_poll(Duration::from_secs(gc.poll_secs));
            }
        }
        Ok(runtime)
    }

    /// A runtime assembled from ready-made hosts and the test infra's lazy
    /// stores — no database, no vector store, no git poll. A `git_config` in
    /// the settings makes it git-managed, as [`Self::build`] decides it.
    #[cfg(test)]
    pub(crate) fn for_tests(
        settings: TenantSettings,
        hosts: HashMap<String, Arc<AgentHost>>,
        infra: &SharedInfra,
    ) -> Arc<Self> {
        // Points at a closed port so a count fails fast rather than reaching
        // a real vector store during tests.
        let memory: Arc<dyn MemoryStore> = Arc::new(
            workforce_memory::weaviate::WeaviateStore::new("http://127.0.0.1:1", None),
        );
        Self::for_tests_with_memory(settings, hosts, infra, memory)
    }

    /// [`Self::for_tests`] on a vector store of the test's choosing, for a
    /// plan whose memory counter must be real.
    #[cfg(test)]
    pub(crate) fn for_tests_with_memory(
        settings: TenantSettings,
        hosts: HashMap<String, Arc<AgentHost>>,
        infra: &SharedInfra,
        memory: Arc<dyn MemoryStore>,
    ) -> Arc<Self> {
        let tenant = settings.id.clone();
        let data_plane = Arc::new(DataPlane::for_tenant(
            infra,
            tenant.clone(),
            memory,
            infra.test_artifacts_for(&tenant),
        ));
        let pool = data_plane.tenants.clone();
        let settings_store = infra.settings_store_for(&tenant);
        let llm_status = Arc::new(LlmStatusReporter::new(
            settings_store.clone(),
            settings.llm_status.clone(),
        ));
        let plan_usage = Arc::new(PlanUsage::new(
            settings.plan.clone(),
            tenant.clone(),
            pool.clone(),
            Arc::clone(&data_plane.analytics),
            Arc::clone(&data_plane.memory),
        ));
        Arc::new(Self {
            id: tenant.clone(),
            name: settings.name.clone(),
            hosts: std::sync::RwLock::new(hosts),
            rate_limiter: RateLimiter::new(
                settings.limits.rate_limit_per_min,
                Duration::from_secs(60),
            ),
            user_limiter: Arc::new(RateLimiter::per_user(
                settings.limits.user_rate_limit_per_min,
                Duration::from_secs(60),
            )),
            login_limiter: RateLimiter::new(10, Duration::from_secs(60)),
            turn_limit: Arc::new(Semaphore::new(settings.limits.max_concurrent_turns.max(1))),
            demo: settings.demo.clone(),
            admin: settings
                .admin_with_session_key(infra.session_root.as_deref())
                .map(|cfg| {
                    AdminAuth::new(&cfg, tenant.clone(), settings.outbound)
                        .expect("a valid test [admin]")
                }),
            config_store: EdgeConfigStore::new(pool.clone(), tenant.clone(), None),
            playbook_store: EdgePlaybookStore::new(pool.clone(), tenant.clone()),
            knowledge: KnowledgeStore::new(pool.clone(), tenant.clone(), data_plane.memory.clone())
                .with_plan(Arc::clone(&plan_usage)),
            operators: EdgeOperatorStore::new(pool.clone(), tenant.clone()),
            audit: EdgeAuditStore::new(pool.clone(), tenant.clone()),
            revisions: EdgeRevisionStore::new(pool, tenant),
            rebuild: RebuildDeps {
                data_plane,
                llm: settings.llm.clone(),
                catalog: PlaybookCatalog::new(HashMap::new()),
                token_exchange: None,
                git_config: None,
                github: infra.github(settings.outbound),
                grader: None,
                audit: settings.audit.clone(),
                llm_status: Arc::clone(&llm_status),
                outbound: settings.outbound,
                plan_usage: Arc::clone(&plan_usage),
            },
            edit_lock: tokio::sync::Mutex::new(()),
            git_managed: settings.git_config.is_some(),
            settings_store,
            llm_status,
            plan_usage,
            settings,
            last_synced_sha: std::sync::Mutex::new(None),
            tasks: CancellationToken::new(),
            retired: OnceLock::new(),
        })
    }

    /// The tenant's vector store.
    pub fn memory(&self) -> &Arc<dyn MemoryStore> {
        &self.rebuild.data_plane.memory
    }

    /// The tenant's usage/cost analytics store. It rides the data plane, which
    /// fail-closed boot guarantees is present, so this is infallible — unlike
    /// the per-host [`AgentHost::analytics`] `Option` (a test host may lack
    /// one). The analytics admin endpoints are tenant-wide (no `{agent}` in
    /// the path), so they reach the store through here rather than a host.
    pub fn analytics(&self) -> &Arc<EdgeAnalyticsStore> {
        &self.rebuild.data_plane.analytics
    }

    /// What the tenant has used of its plan, counted from its own stores
    /// (cached for a minute, dropped by the writes that move a counter) and
    /// its mounted hosts.
    pub async fn usage(&self) -> Result<Usage> {
        self.plan_usage.usage(self.host_count()).await
    }

    /// The artifact store the tenant's files live in, when the install keeps
    /// files at all.
    pub fn artifacts(&self) -> Option<&ArtifactStoreHandle> {
        self.rebuild.data_plane.artifacts.as_ref()
    }

    /// The data plane the tenant's hosts are built from.
    pub(crate) fn data_plane(&self) -> &Arc<DataPlane> {
        &self.rebuild.data_plane
    }

    /// How this tenant's own URLs are reached — the policy every module that
    /// dials a tenant-supplied endpoint (issuer, admin IdP, MCP, permissions,
    /// token exchange) is handed.
    pub fn outbound(&self) -> OutboundPolicy {
        self.rebuild.outbound
    }

    /// How long this tenant keeps each answer's tool evidence, or `None`
    /// when its auditing is off — in which case there is nothing to sweep.
    pub(crate) fn audit_retention(&self) -> Option<Duration> {
        self.rebuild
            .audit
            .as_ref()
            .filter(|audit| audit.enabled)
            .map(|audit| Duration::from_secs(u64::from(audit.retention_days) * 24 * 60 * 60))
    }

    /// Take this runtime out of service: its background work stops and every
    /// socket pinned to it closes with `why`. The reason is recorded before
    /// the token fires, so a woken socket always finds it. A second call
    /// keeps the first reason. Dropping the runtime cancels too.
    pub(crate) fn retire(&self, why: Retirement) {
        let _ = self.retired.set(why);
        self.tasks.cancel();
    }

    /// Resolves once the runtime is retired — for a socket to race its idle
    /// wait against, so it does not keep serving a tenant that stopped.
    pub(crate) fn cancelled(&self) -> WaitForCancellationFuture<'_> {
        self.tasks.cancelled()
    }

    /// Whether the runtime has been retired (or dropped from service).
    pub(crate) fn is_retired(&self) -> bool {
        self.tasks.is_cancelled()
    }

    /// Why the runtime was retired, or `None` while it serves. A cancelled
    /// token with no reason is the runtime being dropped; the socket treats
    /// that as a reload, since reconnecting is the safe move either way.
    pub(crate) fn retirement(&self) -> Option<&Retirement> {
        self.retired.get()
    }

    /// Reject a config mutation on a git-managed install — agents, playbooks, and
    /// training are read-only here (edit them in the git repo). Returns the same
    /// 409 the admin UI shows as a "managed in git" banner.
    pub(crate) fn ensure_config_writable(&self) -> std::result::Result<(), (StatusCode, String)> {
        if self.git_managed {
            return Err((
                StatusCode::CONFLICT,
                "This install's agents, playbooks, and training data are managed in git and are \
                 read-only here. Edit them in the config repository."
                    .to_string(),
            ));
        }
        Ok(())
    }

    /// Re-fetch the git repo and hot-apply it WITHOUT a restart: re-mirror
    /// agents/playbooks/training and rebuild every host in place. Cheap when
    /// nothing changed (a HEAD-SHA check short-circuits). Same safety guards as
    /// boot — a fetch failure or a degenerate (zero-agent) tree keeps the current
    /// config rather than wiping it. Serialised with agent edits via `edit_lock`.
    /// In-flight turns keep their old `Arc<AgentHost>`; only new turns see the
    /// rebuilt hosts.
    pub(crate) async fn resync_from_git(&self) -> Result<SyncReport> {
        let Some(gc) = self.rebuild.git_config.as_ref() else {
            eyre::bail!("this install is not git-managed");
        };
        let _guard = self.edit_lock.lock().await;

        // Cheap change check: skip the whole apply when the ref hasn't moved.
        let sha = self
            .rebuild
            .github
            .latest_commit_sha(gc)
            .await
            .wrap_err("checking the config repo's latest commit")?;
        if self.last_synced_sha.lock().expect("sync sha").as_deref() == Some(sha.as_str()) {
            return Ok(SyncReport {
                changed: false,
                commit: sha,
                agents: 0,
                playbooks: 0,
                training: 0,
            });
        }

        let db_has_agents = !self.config_store.load_all().await?.is_empty();
        let tree = match self.rebuild.github.fetch(gc).await {
            Ok(t) if t.agents.is_empty() && db_has_agents => {
                eyre::bail!(
                    "config repo has zero agents but the running config has some — \
                             refusing to wipe; check the repo layout/ref"
                );
            }
            Ok(t) => t,
            Err(e) => return Err(e).wrap_err("git sync fetch failed (kept the running config)"),
        };

        // Mirror + ingest (same as boot).
        self.config_store
            .replace_all(&tree.agents)
            .await
            .wrap_err("mirroring git agents")?;
        self.playbook_store
            .replace_all(&tree.playbooks)
            .await
            .wrap_err("mirroring git playbooks")?;
        let mut by_agent: HashMap<&str, Vec<(String, String, String)>> = HashMap::new();
        for f in &tree.training {
            by_agent
                .entry(f.agent_id.as_str())
                .or_default()
                .push(crate::git_config::training_doc(f));
        }
        for agent in &tree.agents {
            let docs = by_agent.remove(agent.id.as_str()).unwrap_or_default();
            if let Err(e) = self.knowledge.replace_agent_docs(&agent.id, &docs).await {
                tracing::warn!(tenant = %self.id, agent = %agent.id, error = %e, "git sync training ingest failed");
            }
        }
        // Cascade-delete the corpus of any agent removed/renamed in this commit
        // (the zero-agent tree already bailed above, so `keep` is never empty here).
        let keep: HashSet<&str> = tree.agents.iter().map(|a| a.id.as_str()).collect();
        match self.knowledge.prune_agents_not_in(&keep).await {
            Ok(pruned) => {
                for (id, removed) in pruned {
                    tracing::warn!(tenant = %self.id, agent = %id, docs_removed = removed,
                        "cascade-delete: pruned corpus of an agent removed from git config");
                }
            }
            Err(e) => {
                tracing::warn!(tenant = %self.id, error = %e, "cascade-delete: knowledge prune failed on sync")
            }
        }

        // Hot-swap the shared playbook catalog (every host shares it).
        let defs = crate::playbook::parse_sources(self.playbook_store.load_all().await?);
        self.rebuild.catalog.set(defs.into_values().collect());

        // Rebuild every host from the fresh config (branding included in it).
        let stored = self.config_store.load_all().await?;
        let mut new_hosts = HashMap::with_capacity(stored.len());
        for a in stored {
            let id = a.id.clone();
            let host = self
                .rebuild
                .build_host(Arc::new(a))
                .await
                .wrap_err_with(|| format!("rebuilding host {id} on git sync"))?;
            new_hosts.insert(id, Arc::new(host));
        }
        let retired = std::mem::replace(&mut *self.hosts.write().expect("hosts lock"), new_hosts);
        retired.values().for_each(|host| host.retire());
        *self.last_synced_sha.lock().expect("sync sha") = Some(sha.clone());
        tracing::info!(
            tenant = %self.id, commit = %sha, agents = tree.agents.len(),
            playbooks = tree.playbooks.len(), training = tree.training.len(),
            "git config re-synced live"
        );
        Ok(SyncReport {
            changed: true,
            commit: sha,
            agents: tree.agents.len(),
            playbooks: tree.playbooks.len(),
            training: tree.training.len(),
        })
    }

    /// Background poll: on a git-managed tenant, re-check the config repo every
    /// `interval` and hot-apply when the ref has moved. The HEAD-SHA check makes
    /// an unchanged poll cheap (one small API call), so this is a config change
    /// landing live within one interval — no redeploy, no button. The task holds
    /// the runtime weakly and ends with its cancellation token, so a replaced or
    /// suspended runtime does not keep polling from the grave.
    fn spawn_git_poll(self: &Arc<Self>, interval: Duration) {
        let runtime: Weak<Self> = Arc::downgrade(self);
        let cancelled = self.tasks.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(interval);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = cancelled.cancelled() => return,
                    _ = tick.tick() => {}
                }
                let Some(runtime) = runtime.upgrade() else {
                    return;
                };
                match runtime.resync_from_git().await {
                    Ok(r) if r.changed => {
                        tracing::info!(tenant = %runtime.id, commit = %r.commit, "git poll applied a config change")
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!(tenant = %runtime.id, error = %e, "git poll sync failed; will retry")
                    }
                }
            }
        });
    }

    /// A live host by id (clones the `Arc` out from under the lock).
    pub fn host(&self, agent_id: &str) -> Option<Arc<AgentHost>> {
        self.hosts
            .read()
            .expect("hosts lock")
            .get(agent_id)
            .cloned()
    }

    /// Snapshot of all live hosts (for the admin list).
    pub(crate) fn all_hosts(&self) -> Vec<Arc<AgentHost>> {
        self.hosts
            .read()
            .expect("hosts lock")
            .values()
            .cloned()
            .collect()
    }

    /// Number of mounted agent hosts.
    pub(crate) fn host_count(&self) -> usize {
        self.hosts.read().expect("hosts lock").len()
    }

    /// Unmount an agent host. Caller must hold `edit_lock` to serialize against
    /// concurrent edits/creates.
    pub(crate) fn remove_host(&self, agent_id: &str) {
        let removed = self.hosts.write().expect("hosts lock").remove(agent_id);
        removed.iter().for_each(|host| host.retire());
    }

    /// Reload every playbook from the store and hot-swap the catalog — called
    /// after any admin playbook create/edit/delete so the next customer turn
    /// serves the new set.
    pub(crate) async fn reload_playbooks(&self) -> Result<()> {
        let defs = crate::playbook::parse_sources(self.playbook_store.load_all().await?);
        self.rebuild.catalog.set(defs.into_values().collect());
        Ok(())
    }

    /// Persist an edited agent and hot-swap its rebuilt host, serialized against
    /// other edits. Rebuild happens BEFORE the store write and swap, so a build
    /// failure (e.g. MCP down) leaves the running host and stored config alone.
    pub(crate) async fn apply_agent_edit(&self, agent: EdgeAgent) -> Result<()> {
        // One edit at a time, so build→upsert→swap stays atomic per agent.
        let _guard = self.edit_lock.lock().await;
        self.apply_agent_edit_inner(agent).await
    }

    /// The lock-free body of [`Self::apply_agent_edit`]. Callers that ALREADY
    /// hold `edit_lock` (e.g. restore) MUST use this — re-taking the
    /// non-reentrant lock would deadlock the whole edit plane.
    pub(crate) async fn apply_agent_edit_inner(&self, agent: EdgeAgent) -> Result<()> {
        let host = self
            .rebuild
            .build_host(Arc::new(agent.clone()))
            .await
            .wrap_err("rebuilding agent host")?;
        self.config_store.upsert(&agent).await?;
        let replaced = self
            .hosts
            .write()
            .expect("hosts lock")
            .insert(agent.id.clone(), Arc::new(host));
        replaced.iter().for_each(|host| host.retire());
        Ok(())
    }
}

impl Drop for TenantRuntime {
    fn drop(&mut self) {
        self.tasks.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_agent::artifacts::{
        ArtifactBody, ArtifactDescriptor, ArtifactStore, ContentType,
    };

    use crate::config::EdgeConfig;

    fn settings(id: &str) -> TenantSettings {
        let cfg: EdgeConfig = toml::from_str(
            r#"
            [server]
            bind = "0.0.0.0:8080"

            [postgres]
            url = "postgres://edge/edge"

            [vellev]
            url = "http://vellev:18080"
            api_key = "k"
            tenant = "harbor"

            [llm]
            api_key = "sk-test"
            "#,
        )
        .expect("a valid test config");
        TenantSettings {
            id: TenantId::parse(id).unwrap(),
            name: format!("{id} Ltd"),
            ..TenantSettings::from_edge_toml(&cfg)
        }
    }

    #[test]
    fn each_tenant_gets_its_own_store_under_the_configured_root() {
        let tmp = tempfile::tempdir().unwrap();
        let infra = SharedInfra::for_tests().with_artifacts(tmp.path());
        let acme = TenantRuntime::for_tests(settings("acme"), HashMap::new(), &infra);
        let contoso = TenantRuntime::for_tests(settings("contoso"), HashMap::new(), &infra);
        let acme_store = acme.artifacts().expect("acme has a store");
        let contoso_store = contoso.artifacts().expect("contoso has a store");
        assert_eq!(acme_store.root, tmp.path().join("acme"));
        assert_eq!(contoso_store.root, tmp.path().join("contoso"));

        let handle = acme_store
            .store
            .write(
                "support",
                ArtifactDescriptor::from_context("test", ContentType::ApplicationJson, "support"),
                ArtifactBody::Json(serde_json::json!({"rows": 1})),
            )
            .unwrap();
        assert!(
            tmp.path()
                .join("acme/support")
                .join(format!("{}.meta.json", handle.id.as_str()))
                .is_file(),
            "the layout is root/{{tenant}}/{{agent}}/{{id}}"
        );
        assert!(acme_store
            .store
            .metadata("support", &handle.id)
            .unwrap()
            .is_some());
        assert!(
            contoso_store
                .store
                .metadata("support", &handle.id)
                .unwrap()
                .is_none(),
            "a file staged under one tenant is invisible to another"
        );
        assert!(contoso_store.store.read("support", &handle.id).is_err());
    }

    #[test]
    fn no_artifacts_section_means_no_store() {
        let infra = SharedInfra::for_tests();
        let acme = TenantRuntime::for_tests(settings("acme"), HashMap::new(), &infra);
        assert!(acme.artifacts().is_none());
    }

    /// The tenant's outbound policy reaches MCP discovery through the host
    /// build: a `public` tenant whose agent names a loopback `http` MCP server
    /// is refused at build, before anything is dialled, while a `private`
    /// tenant (the self-hosted shape) builds and defers discovery to the
    /// first turn as before.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_public_tenant_refuses_a_private_mcp_server_at_build_and_a_private_one_builds() {
        use crate::agent::{EmbedSettings, McpSettings};
        use crate::analytics::run_edge_analytics_migrations;
        use crate::tenant::run_edge_migrations;
        use workforce_common::session_postgres::{run_session_migrations, PostgresSessionStore};
        use workforce_postgres::test_support::with_test_pool;

        with_test_pool(PostgresSessionStore::build_pool, |pool| async move {
            run_session_migrations(&pool).await.unwrap();
            run_edge_analytics_migrations(&pool).await.unwrap();
            run_edge_migrations(&pool).await.unwrap();
            let infra = SharedInfra::for_tests_on(pool);
            let agent = EdgeAgent {
                id: "support".into(),
                embed: EmbedSettings {
                    mcp: Some(McpSettings {
                        url: "http://127.0.0.1:1/mcp".into(),
                        discovery_credential: None,
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                ..Default::default()
            };
            let with_policy = |outbound: OutboundPolicy| TenantSettings {
                seed_agents: vec![agent.clone()],
                outbound,
                ..settings("default")
            };

            let Err(err) = TenantRuntime::build(with_policy(OutboundPolicy::Public), &infra).await
            else {
                panic!("a public tenant must refuse a loopback http MCP server");
            };
            let message = format!("{err:#}");
            assert!(
                message.contains("mcp.url is refused by the outbound policy"),
                "{message}"
            );
            assert!(message.contains("building agent host support"), "{message}");

            let runtime = TenantRuntime::build(with_policy(OutboundPolicy::Private), &infra)
                .await
                .expect("a private tenant defers discovery to the first turn");
            assert_eq!(runtime.outbound(), OutboundPolicy::Private);
            assert!(runtime.host("support").is_some());
        })
        .await;
    }

    /// An additional server a `public` tenant refuses as written, a loopback
    /// `http` one beside a public https primary, is left out of the agent when
    /// it is optional, and fails the build by field when it is required.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_public_tenant_excludes_an_optional_server_it_refuses_and_refuses_a_required_one() {
        use crate::agent::{EmbedSettings, McpServerSettings, McpSettings};
        use crate::analytics::run_edge_analytics_migrations;
        use crate::tenant::run_edge_migrations;
        use workforce_common::session_postgres::{run_session_migrations, PostgresSessionStore};
        use workforce_postgres::test_support::with_test_pool;

        for required in [None, Some(true)] {
            with_test_pool(PostgresSessionStore::build_pool, |pool| async move {
                run_session_migrations(&pool).await.unwrap();
                run_edge_analytics_migrations(&pool).await.unwrap();
                run_edge_migrations(&pool).await.unwrap();
                let infra = SharedInfra::for_tests_on(pool);
                let agent = EdgeAgent {
                    id: "northwind-help".into(),
                    embed: EmbedSettings {
                        mcp: Some(McpSettings {
                            url: "https://93.184.216.34/mcp".into(),
                            ..Default::default()
                        }),
                        mcp_servers: vec![McpServerSettings {
                            id: "docs".into(),
                            connection: McpSettings {
                                url: "http://127.0.0.1:2/mcp".into(),
                                required,
                                ..Default::default()
                            },
                            tool_prefix: Some("docs".into()),
                            allowed_tools: vec!["docs.*".into()],
                            tool_grants: vec![],
                        }],
                        ..Default::default()
                    },
                    ..Default::default()
                };
                let settings = TenantSettings {
                    seed_agents: vec![agent],
                    outbound: OutboundPolicy::Public,
                    ..settings("default")
                };
                match (required, TenantRuntime::build(settings, &infra).await) {
                    (None, Ok(runtime)) => {
                        let host = runtime.host("northwind-help").expect("the agent serves");
                        assert_eq!(crate::mcp::tests::excluded_ids(host.mcp()), ["docs"]);
                    }
                    (Some(true), Err(err)) => {
                        let message = format!("{err:#}");
                        assert!(
                            message.contains("building agent host northwind-help")
                                && message.contains(
                                    "mcp_servers[docs].url is refused by the outbound policy"
                                ),
                            "{message}"
                        );
                    }
                    (required, Ok(_)) => panic!("required = {required:?} must not build"),
                    (required, Err(err)) => panic!("required = {required:?} must build: {err:#}"),
                }
            })
            .await;
        }
    }

    /// The install's provider allowlist is checked before anything is
    /// dialled: a tenant on a provider outside it does not build, and the
    /// error names the field.
    #[tokio::test]
    async fn a_tenant_on_a_provider_outside_the_allowlist_is_refused() {
        let infra = SharedInfra::for_tests();
        let mut settings = settings("acme");
        settings.llm.provider = "openrouter".into();
        settings.llm.allowed_providers = vec!["anthropic".into(), "gemini".into()];
        let Err(err) = TenantRuntime::build(settings, &infra).await else {
            panic!("the build must refuse");
        };
        let message = format!("{err:#}");
        assert!(message.contains("allowed_providers"), "{message}");
        assert!(message.contains("openrouter"), "{message}");
        assert!(message.contains("tenant acme"), "{message}");
    }

    /// The cloud shape: no `[admin].session_secret` in the file, a master
    /// key in the environment. Two consecutive builds of the same tenant —
    /// what every settings save does — sign under the same key, so a
    /// session the first minted verifies on the second. With the sealing
    /// key alone (no session root) each build mints its own, which is the
    /// bug: every save signed the operator out.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_rebuilt_tenant_verifies_the_sessions_its_predecessor_minted_under_the_master_key() {
        use crate::admin::{AdminConfig, AdminOidc};
        use crate::analytics::run_edge_analytics_migrations;
        use crate::tenant::run_edge_migrations;
        use workforce_common::session_postgres::{run_session_migrations, PostgresSessionStore};
        use workforce_postgres::test_support::with_test_pool;

        with_test_pool(PostgresSessionStore::build_pool, |pool| async move {
            run_session_migrations(&pool).await.unwrap();
            run_edge_analytics_migrations(&pool).await.unwrap();
            run_edge_migrations(&pool).await.unwrap();
            let settings = || TenantSettings {
                admin: Some(AdminConfig {
                    oidc: Some(AdminOidc {
                        issuer: "https://accounts.google.com".into(),
                        audience: Some("edge-admin".into()),
                        client_id: Some("edge-admin".into()),
                        client_secret: None,
                        mode: None,
                    }),
                    password_hash: None,
                    allowed_emails: vec![],
                    allowed_domains: vec![],
                    session_secret: None,
                    public_url: None,
                    session_hours: 1,
                }),
                ..settings("default")
            };
            let session_on = |runtime: &Arc<TenantRuntime>| {
                runtime
                    .admin
                    .as_ref()
                    .expect("an admin surface")
                    .issue_session("ops@harbor.example")
                    .unwrap()
            };
            let verifies = |runtime: &Arc<TenantRuntime>, token: &str| {
                runtime
                    .admin
                    .as_ref()
                    .expect("an admin surface")
                    .verify_session(token)
                    .is_some()
            };

            let keyed = SharedInfra::for_tests_on(pool.clone())
                .with_install_keys(crate::boot::test_install_keys());
            let first = TenantRuntime::build(settings(), &keyed).await.unwrap();
            let keyed_token = session_on(&first);
            let second = TenantRuntime::build(settings(), &keyed).await.unwrap();
            assert!(
                verifies(&second, &keyed_token),
                "the rebuilt tenant signs under the same master-key-derived key"
            );
            assert!(verifies(&first, &session_on(&second)));

            let sealing_only = SharedInfra::for_tests_on(pool)
                .with_master_key(crate::settings::tests::test_master_key());
            let first = TenantRuntime::build(settings(), &sealing_only)
                .await
                .unwrap();
            let token = session_on(&first);
            let second = TenantRuntime::build(settings(), &sealing_only)
                .await
                .unwrap();
            assert!(
                !verifies(&second, &token),
                "without a session root each build keys itself: the reload sign-out"
            );
            assert!(
                !verifies(&second, &keyed_token),
                "and a root-keyed session is no good under a per-boot key"
            );
        })
        .await;
    }
}
