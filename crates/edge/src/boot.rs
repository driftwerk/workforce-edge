//! Fail-closed connection to the edge's own data plane.
//!
//! The edge runs against a dedicated Postgres (customer sessions) and a
//! dedicated vector store — Weaviate, or a vellev tenant — for agent memory
//! and the customer-facing training corpus. Both are mandatory: each is
//! dialed with a retry budget and, if it never comes up, boot aborts rather
//! than silently degrading to a lesser store.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use eyre::{Result, WrapErr};
use ipnetwork::IpNetwork;
use vellev_client::{Client as VellevClient, Options as VellevOptions};
use workforce_agent::artifacts::FsArtifactStore;
use workforce_common::backend::connect_with_retry;
use workforce_common::postgres::{PostgresConfig, PostgresPool, SslMode};
use workforce_common::session_postgres::{run_session_migrations, PostgresSessionStore};
use workforce_crypto::{MasterKey, Zeroizing};
use workforce_memory::admin::MemoryAdmin;
use workforce_memory::store::MemoryStore;
use workforce_memory::vellev::{VellevStore, DEFAULT_COLLECTION};
use workforce_memory::weaviate::WeaviateStore;

use crate::analytics::{run_edge_analytics_migrations, EdgeAnalyticsStore};
use crate::config::{ArtifactsConfig, EdgeConfig, MemoryBackend, VellevConfig};
use crate::ingress::parse_trusted_proxies;
use crate::outbound::OutboundPolicy;
use crate::settings::TenantSettingsStore;
use crate::tenant::{rls_enforced, run_edge_migrations, TenantId, TenantPool, TenantStore};

/// Seconds each individual connection attempt is allowed before it is treated
/// as a failure — also the pause between attempts.
const ATTEMPT_INTERVAL: Duration = Duration::from_secs(5);

/// The environment variable carrying the key every tenant's secrets are
/// sealed under: 32 bytes, as 64 hex characters or their base64. Absent, the
/// settings table is not used and the file governs every tenant.
pub const MASTER_KEY_ENV: &str = "EDGE_MASTER_KEY";

/// What the install's master key gives the edge: the key tenant secrets are
/// sealed under, and the root admin sessions are signed from when the file
/// sets no session secret of its own. Both come from the one environment
/// value, so an install has either both or neither. Both print redacted.
#[derive(Debug)]
pub struct InstallKeys {
    pub master: Arc<MasterKey>,
    pub session_root: Arc<SessionRoot>,
}

/// The install keys from the environment, or `None` when the master key is
/// not set. A value that is set but malformed fails boot: an install that
/// meant to seal its secrets must not run unsealed. The error never carries
/// the value.
pub fn master_key_from_env() -> Result<Option<InstallKeys>> {
    match std::env::var(MASTER_KEY_ENV).map(Zeroizing::new) {
        Ok(value) => parse_master_key(&value).map(Some),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => {
            Err(eyre::eyre!("{MASTER_KEY_ENV} is not valid UTF-8"))
        }
    }
}

/// Parse a master key: 64 hex characters, or the standard base64 (padded or
/// not) of 32 bytes. Every decoded form of the material is wiped when it
/// goes out of scope; only the key schedule inside the [`MasterKey`] and the
/// [`SessionRoot`] derived beside it live on.
pub fn parse_master_key(value: &str) -> Result<InstallKeys> {
    let value = value.trim();
    let malformed =
        || eyre::eyre!("{MASTER_KEY_ENV} must be 32 bytes, as 64 hex characters or their base64");
    let bytes = if value.len() == 2 * workforce_crypto::KEY_LEN {
        hex::decode(value).map_err(|_| malformed())?
    } else {
        base64::engine::general_purpose::STANDARD
            .decode(value)
            .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(value))
            .map_err(|_| malformed())?
    };
    let bytes = Zeroizing::new(bytes);
    let key = Zeroizing::new(
        <[u8; workforce_crypto::KEY_LEN]>::try_from(bytes.as_slice()).map_err(|_| malformed())?,
    );
    Ok(InstallKeys {
        master: Arc::new(MasterKey::from_bytes(&key)),
        session_root: Arc::new(SessionRoot::from_master_key_bytes(&key)),
    })
}

/// The install-wide root every admin session key is derived from when the
/// file sets no `[admin].session_secret` (or `[platform].session_secret`):
/// an HKDF subkey of the master key under its own label, taken once at
/// parse time so the master key's raw bytes never outlive it. A reload
/// derives the same per-tenant key from the same root, which is what keeps
/// an operator signed in across the rebuilds every settings save triggers.
pub struct SessionRoot(Zeroizing<[u8; workforce_crypto::KEY_LEN]>);

impl SessionRoot {
    const LABEL: &'static [u8] = b"workforce-edge/v1/admin-session";
    const PLATFORM_LABEL: &'static [u8] = b"workforce-edge/v1/platform-session";

    fn from_master_key_bytes(key: &[u8; workforce_crypto::KEY_LEN]) -> Self {
        Self(workforce_crypto::derive_subkey(key, Self::LABEL))
    }

    /// `tenant`'s session secret, hex-encoded as `[admin].session_secret`
    /// takes it: stable across reloads, distinct per tenant.
    pub fn tenant_session_secret(&self, tenant: &TenantId) -> String {
        self.derive(crate::tenant::admin_session_label(tenant).as_bytes())
    }

    /// The platform surface's own session secret, under a label no tenant
    /// can name.
    pub fn platform_session_secret(&self) -> String {
        self.derive(Self::PLATFORM_LABEL)
    }

    fn derive(&self, info: &[u8]) -> String {
        hex::encode(workforce_crypto::derive_subkey(self.0.as_slice(), info).as_slice())
    }
}

impl std::fmt::Debug for SessionRoot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SessionRoot(..)")
    }
}

/// One tenant's artifact store plus the TTL each generated file is stamped
/// with; both travel together into the export tool and the download route.
#[derive(Clone)]
pub struct ArtifactStoreHandle {
    pub store: Arc<FsArtifactStore>,
    pub ttl: Duration,
    /// The directory the store owns: `{[artifacts] root}/{tenant}`.
    pub root: PathBuf,
}

/// What every tenant of the process shares, proven reachable at boot: the
/// pool, the registry, the install's vector store, the artifact root and
/// the ingress trust settings.
pub struct SharedInfra {
    /// Shared pool for the edge's own Postgres; per-agent session stores are
    /// derived from it (schema `sessions`, already migrated).
    pub sessions_pool: PostgresPool,
    /// The same pool as the edge's stores reach it: every connection scoped
    /// to the tenant it serves.
    pub tenants_pool: TenantPool,
    /// The tenant registry.
    pub tenants: TenantStore,
    /// The install's own vector store — Weaviate, or the `[vellev]` tenant.
    /// It serves the `default` tenant; every other tenant gets a vellev
    /// tenant of its own (see [`Self::memory_for`]).
    memory: Arc<dyn MemoryStore>,
    /// The `[vellev]` section, from which other tenants' stores are built.
    vellev: Option<VellevConfig>,
    /// The node under the install's `[vellev] api_key`, which the platform
    /// API creates and drops tenants with. Only an admin key may; a key
    /// that reaches one tenant is refused, which the platform reports.
    vellev_admin: Option<VellevClient>,
    /// The `[artifacts]` section: the root every tenant's file store lives
    /// under, and the TTL its files carry. `None` leaves the feature off.
    artifacts: Option<ArtifactsConfig>,
    /// Proxy hops trusted to set the forwarded client address and host.
    pub trusted_proxies: Vec<IpNetwork>,
    /// Whether the pool's role is bound by the row-level-security policies —
    /// the precondition for serving more than one tenant.
    pub rls_enforced: bool,
    /// The key tenant secrets are sealed under, from [`MASTER_KEY_ENV`].
    /// `None` leaves the settings table unused: the file governs.
    pub master_key: Option<Arc<MasterKey>>,
    /// The root admin session keys are derived from, beside the master key.
    /// `None` leaves a tenant whose file sets no session secret on a random
    /// per-boot key, as a single-tenant install without a master key runs.
    pub session_root: Option<Arc<SessionRoot>>,
    /// Where config repos are fetched from — GitHub's API in production
    /// ([`crate::git_config::GitHub::API`]); a test points it at a mock.
    pub(crate) github_api: String,
}

impl SharedInfra {
    /// The settings store for `tenant`, or `None` when the install has no
    /// master key and so no settings rows.
    pub fn settings_store_for(&self, tenant: &TenantId) -> Option<TenantSettingsStore> {
        self.master_key.as_ref().map(|master| {
            TenantSettingsStore::new(
                self.tenants_pool.clone(),
                tenant.clone(),
                Arc::clone(master),
            )
        })
    }

    /// The artifact store `tenant`'s files live in — `{root}/{tenant}`, so
    /// one store can never read another tenant's files — or `None` when
    /// `[artifacts]` is unset. For `default` the install's pre-tenancy files
    /// are first adopted into its directory (see [`adopt_legacy_artifact_dirs`]).
    pub async fn artifacts_for(&self, tenant: &TenantId) -> Result<Option<ArtifactStoreHandle>> {
        let Some(cfg) = &self.artifacts else {
            return Ok(None);
        };
        if tenant.is_default() {
            let registered: HashSet<String> = self
                .tenants
                .list()
                .await
                .wrap_err("listing tenants to adopt legacy artifacts")?
                .into_iter()
                .map(|row| row.id.as_str().to_string())
                .collect();
            let moved = adopt_legacy_artifact_dirs(Path::new(&cfg.root), &registered)
                .wrap_err("adopting the install's pre-tenancy artifacts")?;
            if moved > 0 {
                tracing::info!(
                    agents = moved,
                    root = %cfg.root,
                    "moved the install's artifacts under the default tenant"
                );
            }
        }
        self.artifact_store_for(tenant).map(Some)
    }

    /// Open `tenant`'s store under the configured root, without the legacy
    /// adoption; `[artifacts]` must be set.
    fn artifact_store_for(&self, tenant: &TenantId) -> Result<ArtifactStoreHandle> {
        let cfg = self
            .artifacts
            .as_ref()
            .ok_or_else(|| eyre::eyre!("[artifacts] is not configured"))?;
        let root = Path::new(&cfg.root).join(tenant.as_str());
        let store = FsArtifactStore::new(root.clone())
            .wrap_err_with(|| format!("initialising the artifact store for tenant {tenant}"))?;
        Ok(ArtifactStoreHandle {
            store,
            ttl: Duration::from_secs(cfg.ttl_secs),
            root,
        })
    }

    /// The artifact store `tenant` would get, built synchronously and without
    /// the legacy adoption — for tests that assemble runtimes with no database.
    #[cfg(test)]
    pub(crate) fn test_artifacts_for(&self, tenant: &TenantId) -> Option<ArtifactStoreHandle> {
        self.artifacts.as_ref().map(|_| {
            self.artifact_store_for(tenant)
                .expect("a writable test root")
        })
    }

    /// Give the test infra an `[artifacts]` root (default TTL).
    #[cfg(test)]
    pub(crate) fn with_artifacts(mut self, root: &Path) -> Self {
        self.artifacts = Some(ArtifactsConfig {
            root: root.to_string_lossy().into_owned(),
            ttl_secs: 24 * 60 * 60,
        });
        self
    }
    /// The vector store `tenant`'s memory and corpus live in: the install's
    /// own for `default`, else a vellev tenant named after the edge tenant,
    /// reached with the key its settings row holds and dialled fail-closed
    /// like the install's.
    ///
    /// Only `default` ever runs on the install's key. Any other tenant
    /// presents the key the platform API issued it when it created the
    /// vellev tenant; without one it does not build, so a row added by hand
    /// is suspended with the reason rather than served on a key that
    /// reaches every tenant.
    pub async fn memory_for(&self, tenant: &TenantId) -> Result<Arc<dyn MemoryStore>> {
        if tenant.is_default() {
            return Ok(Arc::clone(&self.memory));
        }
        let vellev = self.vellev.as_ref().ok_or_else(|| {
            eyre::eyre!(
                "tenant {tenant} needs a vellev tenant of its own, but [vellev] is not configured"
            )
        })?;
        let key = match self.settings_store_for(tenant) {
            Some(store) => store.vellev_api_key().await?,
            None => None,
        }
        .ok_or_else(|| {
            eyre::eyre!(
                "tenant {tenant} has no vellev key of its own; create the tenant through the \
                 platform API, which provisions its vellev tenant and key"
            )
        })?;
        let store = VellevStore::new(&vellev.url, &key, tenant.as_str(), DEFAULT_COLLECTION)?
            .with_local_model(&vellev.model, vellev.dimensions, None);
        ensure_collection("edge-vellev", vellev.connect_budget_secs, store).await
    }

    /// Create `tenant`'s vellev tenant and issue it a key, returning the
    /// key. Creating a tenant that exists changes nothing, so a retry after
    /// a failed boot of the edge tenant is safe; a key whose issue failed
    /// leaves no tenant behind.
    pub async fn provision_vellev_tenant(&self, tenant: &TenantId) -> Result<String> {
        let node = self.vellev_admin.as_ref().ok_or_else(|| {
            eyre::eyre!("tenant {tenant} needs a vellev tenant, but [vellev] is not configured")
        })?;
        let id = vellev_tenant_id(tenant)?;
        let remote = node.tenant(&id);
        remote
            .create()
            .await
            .wrap_err_with(|| format!("creating vellev tenant {tenant}"))?;
        match remote.issue_key().await {
            Ok(issued) => Ok(issued.key),
            Err(e) => {
                if let Err(dropped) = remote.drop().await {
                    tracing::warn!(
                        tenant = %tenant, error = %dropped,
                        "vellev tenant left behind after its key could not be issued"
                    );
                }
                Err(e).wrap_err_with(|| format!("issuing a vellev key for tenant {tenant}"))
            }
        }
    }

    /// Drop `tenant`'s vellev tenant, its data, keys and backups with it.
    /// Returns whether there was one.
    pub async fn drop_vellev_tenant(&self, tenant: &TenantId) -> Result<bool> {
        let node = self.vellev_admin.as_ref().ok_or_else(|| {
            eyre::eyre!(
                "tenant {tenant} has a vellev tenant to drop, but [vellev] is not configured"
            )
        })?;
        node.tenant(&vellev_tenant_id(tenant)?)
            .drop()
            .await
            .wrap_err_with(|| format!("dropping vellev tenant {tenant}"))
    }

    /// Remove `tenant`'s artifact directory, files and all; nothing to do
    /// when `[artifacts]` is unset or the directory was never made.
    pub fn remove_artifacts_for(&self, tenant: &TenantId) -> Result<()> {
        let Some(cfg) = &self.artifacts else {
            return Ok(());
        };
        let root = Path::new(&cfg.root).join(tenant.as_str());
        match std::fs::remove_dir_all(&root) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).wrap_err_with(|| format!("removing {}", root.display())),
        }
    }

    /// The pool as a lazily-built placeholder, for tests that construct the
    /// runtime shapes without a database: nothing is dialled until a query.
    #[cfg(test)]
    pub(crate) fn for_tests() -> Self {
        let pool = PostgresPool::from_config(&PostgresConfig {
            enabled: true,
            url: Some("postgres://edge@127.0.0.1:1/edge".to_string()),
            sslmode: SslMode::Disable,
            ..PostgresConfig::default()
        })
        .expect("a lazily-connected pool");
        Self::for_tests_on(pool)
    }

    /// The infra over a real (test-harness) pool, so runtimes built on it
    /// reach a migrated database; the vector store stays a placeholder.
    #[cfg(test)]
    pub(crate) fn for_tests_on(pool: PostgresPool) -> Self {
        Self {
            sessions_pool: pool.clone(),
            tenants_pool: TenantPool::new(pool.clone()),
            tenants: TenantStore::new(pool),
            memory: Arc::new(WeaviateStore::new("http://127.0.0.1:1", None)),
            vellev: None,
            vellev_admin: None,
            artifacts: None,
            trusted_proxies: vec![],
            rls_enforced: false,
            master_key: None,
            session_root: None,
            github_api: crate::git_config::GitHub::API.to_string(),
        }
    }

    /// Give the test infra a master key, so runtimes built on it keep
    /// settings rows. The sealing key alone: admin sessions stay random per
    /// boot, as without a master key. [`Self::with_install_keys`] gives
    /// both, as the environment does.
    #[cfg(test)]
    pub(crate) fn with_master_key(mut self, key: Arc<MasterKey>) -> Self {
        self.master_key = Some(key);
        self
    }

    /// Give the test infra the master key and the session root parsed from
    /// one value, as [`connect`] takes them from the environment.
    #[cfg(test)]
    pub(crate) fn with_install_keys(mut self, keys: InstallKeys) -> Self {
        self.master_key = Some(keys.master);
        self.session_root = Some(keys.session_root);
        self
    }

    /// Point the test infra's config-repo loader at a mock GitHub.
    #[cfg(test)]
    pub(crate) fn with_github_api(mut self, api_base: &str) -> Self {
        self.github_api = api_base.to_string();
        self
    }

    /// Give the test infra a `[vellev]` node (a mock) under `api_key`, so
    /// tenants other than `default` get stores and the platform API can
    /// provision them. One attempt per call: a mock answers at once.
    #[cfg(test)]
    pub(crate) fn with_vellev(mut self, url: &str, api_key: &str) -> Self {
        let cfg = VellevConfig {
            url: url.to_string(),
            api_key: api_key.to_string(),
            tenant: TenantId::DEFAULT.to_string(),
            model: "Qdrant/all-MiniLM-L6-v2-onnx".to_string(),
            dimensions: 384,
            connect_budget_secs: 1,
        };
        self.vellev_admin = Some(vellev_admin_client(&cfg).expect("a mock node URL"));
        self.vellev = Some(cfg);
        self
    }

    /// The config-repo loader for a tenant reaching GitHub under `policy`.
    pub(crate) fn github(&self, policy: OutboundPolicy) -> crate::git_config::GitHub {
        crate::git_config::GitHub::at(&self.github_api, policy)
    }
}

/// One tenant's view of the data plane — what an [`crate::host::AgentHost`]
/// is built from.
pub struct DataPlane {
    /// The tenant every store here is bound to.
    pub tenant: TenantId,
    /// Shared pool for the edge's own Postgres; per-agent session stores are
    /// derived from it (schema `sessions`, already migrated).
    pub sessions_pool: PostgresPool,
    /// The same pool as the edge's stores reach it: every connection scoped
    /// to the tenant it serves.
    pub tenants: TenantPool,
    /// The tenant's vector store — agent memory and training corpus.
    pub memory: Arc<dyn MemoryStore>,
    /// Per-turn usage/cost accounting + subject name resolution, on the shared
    /// pool's own `edge_analytics` schema (already migrated).
    pub analytics: Arc<EdgeAnalyticsStore>,
    /// The tenant's own file-artifact store for agent-generated downloads
    /// (`export_xlsx`). `None` when `[artifacts]` is unconfigured, which
    /// leaves the feature off.
    pub artifacts: Option<ArtifactStoreHandle>,
}

impl DataPlane {
    /// The data plane as `tenant` sees it: its own memory store, analytics
    /// and artifact store, on the install's pool.
    pub fn for_tenant(
        infra: &SharedInfra,
        tenant: TenantId,
        memory: Arc<dyn MemoryStore>,
        artifacts: Option<ArtifactStoreHandle>,
    ) -> Self {
        Self {
            sessions_pool: infra.sessions_pool.clone(),
            tenants: infra.tenants_pool.clone(),
            memory,
            analytics: Arc::new(EdgeAnalyticsStore::new(
                infra.tenants_pool.clone(),
                tenant.clone(),
            )),
            artifacts,
            tenant,
        }
    }

    /// A per-agent session-store handle off the shared, already-migrated
    /// pool, confined to this tenant's rows.
    pub fn session_store(&self, agent_id: impl Into<String>) -> Arc<PostgresSessionStore> {
        PostgresSessionStore::with_shared_pool_in_tenant(
            self.sessions_pool.clone(),
            self.tenant.as_str(),
            agent_id,
        )
    }
}

/// Dial both backends fail-closed. Returns once both are reachable, or aborts
/// boot with the last connection error after the budget elapses.
pub async fn connect(config: &EdgeConfig) -> Result<SharedInfra> {
    let (master_key, session_root) = match master_key_from_env()? {
        Some(keys) => {
            tracing::info!(
                "edge master key present; tenant settings and secrets are held sealed in Postgres"
            );
            (Some(keys.master), Some(keys.session_root))
        }
        None => (None, None),
    };
    let (sessions_pool, rls_enforced) = connect_postgres(config).await?;
    let tenants_pool = TenantPool::new(sessions_pool.clone());
    let memory = connect_memory(config).await?;
    if let Some(cfg) = &config.artifacts {
        workforce_common::private_fs::ensure_private_dir(Path::new(&cfg.root))
            .wrap_err("initialising the artifact root")?;
    }
    let vellev_admin = config
        .vellev
        .as_ref()
        .map(vellev_admin_client)
        .transpose()
        .wrap_err("building the vellev admin client")?;
    Ok(SharedInfra {
        tenants: TenantStore::new(sessions_pool.clone()),
        sessions_pool,
        tenants_pool,
        memory,
        vellev: config.vellev.clone(),
        vellev_admin,
        artifacts: config.artifacts.clone(),
        trusted_proxies: parse_trusted_proxies(&config.server.trusted_proxies),
        rls_enforced,
        master_key,
        session_root,
        github_api: crate::git_config::GitHub::API.to_string(),
    })
}

/// The node under the install's key, for the tenant-management calls.
fn vellev_admin_client(cfg: &VellevConfig) -> Result<VellevClient> {
    VellevClient::connect(&[&cfg.url], &cfg.api_key, VellevOptions::default())
        .wrap_err("[vellev] url is not a node URL")
}

/// An edge tenant's id as vellev names its tenant: the same slug. Vellev's
/// grammar is a superset of the edge's, so this cannot fail for a parsed
/// [`TenantId`]; the error is kept rather than an `expect`.
fn vellev_tenant_id(tenant: &TenantId) -> Result<vellev_core::TenantId> {
    vellev_core::TenantId::new(tenant.as_str())
        .map_err(|e| eyre::eyre!("tenant {tenant} is not a vellev tenant id: {e}"))
}

/// Adopt an install's pre-tenancy artifacts into the `default` tenant's
/// directory. Returns how many agent directories moved.
///
/// Before tenancy the store's layout was `{root}/{agent}/{id}`; it is now
/// `{root}/{tenant}/{agent}/{id}`, and `default` lives at `{root}/default`
/// rather than the bare root so no tenant's files are ever a sibling of
/// another tenant's directory. An upgraded self-hosted install must not lose
/// the downloads its customers were handed a minute ago, so the first boot of
/// `default` moves every immediate subdirectory of `root` that is not a
/// registered tenant id (`default` included) under `{root}/default/`. The
/// rule is by registry, not by content: a directory named after a tenant is
/// that tenant's, whatever it holds. Idempotent — a moved directory is gone
/// from the root, and a directory already present at the destination is
/// merged entry by entry so a boot interrupted mid-move finishes next time.
fn adopt_legacy_artifact_dirs(root: &Path, registered: &HashSet<String>) -> Result<usize> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        // A root that does not exist yet has nothing to adopt; the store
        // creates it.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e).wrap_err_with(|| format!("reading {}", root.display())),
    };
    let default = TenantId::default_tenant();
    let default_dir = root.join(default.as_str());
    let mut moved = 0;
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if name == default.as_str() {
            continue;
        }
        if registered.contains(&name) {
            // By the rule above this directory is the tenant's, so a legacy
            // agent of the same name has files it will never see again.
            // Loud, because the loss is silent otherwise.
            tracing::warn!(
                tenant = %name,
                path = %entry.path().display(),
                "artifact directory named after a registered tenant is left as that tenant's; \
                 a pre-tenancy agent of the same name is NOT adopted into default"
            );
            continue;
        }
        let from = entry.path();
        let to = default_dir.join(&name);
        if to.is_dir() {
            let mut kept = 0;
            for file in std::fs::read_dir(&from)? {
                let file = file?;
                let dest = to.join(file.file_name());
                // The destination holds files written since the interrupted
                // move; an artifact id names one file, so a clash is a
                // same-named file whose newer copy must win. Keep it.
                if dest.exists() {
                    tracing::warn!(
                        agent = %name,
                        file = %file.file_name().to_string_lossy(),
                        "legacy artifact already present under default; the legacy copy is left \
                         in place, not overwritten"
                    );
                    kept += 1;
                    continue;
                }
                std::fs::rename(file.path(), dest)?;
            }
            if kept == 0 {
                std::fs::remove_dir(&from)?;
            } else {
                tracing::warn!(
                    agent = %name,
                    kept,
                    path = %from.display(),
                    "legacy artifact directory left behind with the files that were not adopted"
                );
            }
        } else {
            workforce_common::private_fs::ensure_private_dir(&default_dir)?;
            std::fs::rename(&from, &to)?;
        }
        tracing::info!(agent = %name, "adopted a pre-tenancy artifact directory into default");
        moved += 1;
    }
    Ok(moved)
}

/// The migrated pool, and whether its role is bound by the row-level-security
/// policies.
async fn connect_postgres(config: &EdgeConfig) -> Result<(PostgresPool, bool)> {
    let pg = postgres_config(&config.postgres);
    let budget = Duration::from_secs(config.postgres.connect_budget_secs);
    let pool = connect_with_retry("edge-postgres", budget, ATTEMPT_INTERVAL, || {
        let pg = pg.clone();
        async move {
            let pool = PostgresSessionStore::build_pool(&pg)?;
            pool.ping().await?;
            Ok(pool)
        }
    })
    .await?;
    run_session_migrations(&pool)
        .await
        .wrap_err("running edge session-schema migrations")?;
    run_edge_analytics_migrations(&pool)
        .await
        .wrap_err("running edge analytics-schema migrations")?;
    run_edge_migrations(&pool)
        .await
        .wrap_err("running edge table migrations")?;
    let tenants = TenantStore::new(pool.clone());
    tenants.ensure_default().await?;
    let enforced = rls_enforced(&pool).await?;
    if !enforced {
        // One tenant has nothing to be kept from; more than one does, and
        // only the policies keep a query that forgot its tenant honest.
        let served = tenants.list().await?.len();
        if served > 1 {
            return Err(eyre::eyre!(
                "the edge's database role bypasses row-level security (superuser or BYPASSRLS) \
                 while this deployment serves {served} tenants; run the edge as a plain role"
            ));
        }
        tracing::warn!(
            "the edge's database role bypasses row-level security; fine for one tenant, \
             refused once there are more"
        );
    }
    tracing::info!("edge Postgres ready (schemas `sessions` + `edge_analytics` migrated)");
    Ok((pool, enforced))
}

/// Dial whichever vector store the config names, until its collection is
/// ensured or the budget runs out.
async fn connect_memory(config: &EdgeConfig) -> Result<Arc<dyn MemoryStore>> {
    match config.memory_backend()? {
        MemoryBackend::Weaviate(weaviate) => {
            let store = WeaviateStore::new(weaviate.url.clone(), weaviate.api_key.as_deref());
            ensure_collection("edge-weaviate", weaviate.connect_budget_secs, store).await
        }
        MemoryBackend::Vellev(vellev) => {
            let store = VellevStore::new(
                &vellev.url,
                &vellev.api_key,
                &vellev.tenant,
                DEFAULT_COLLECTION,
            )?
            .with_local_model(&vellev.model, vellev.dimensions, None);
            ensure_collection("edge-vellev", vellev.connect_budget_secs, store).await
        }
    }
}

async fn ensure_collection<S: MemoryAdmin + MemoryStore + 'static>(
    name: &'static str,
    budget_secs: u64,
    store: S,
) -> Result<Arc<dyn MemoryStore>> {
    let store = Arc::new(store);
    let probe = Arc::clone(&store);
    connect_with_retry(
        name,
        Duration::from_secs(budget_secs),
        ATTEMPT_INTERVAL,
        || {
            let probe = Arc::clone(&probe);
            async move { probe.ensure_collection().await }
        },
    )
    .await?;
    tracing::info!(store = name, "edge vector store ready (collection ensured)");
    Ok(store)
}

/// Map the edge's own Postgres settings onto the shared pool config. The edge
/// always drives a real, enabled Postgres — there is no SQLite fallback.
fn postgres_config(cfg: &crate::config::PostgresConfig) -> PostgresConfig {
    let sslmode = match cfg.sslmode.to_ascii_lowercase().as_str() {
        "disable" => SslMode::Disable,
        _ => SslMode::Require,
    };
    PostgresConfig {
        enabled: true,
        url: Some(cfg.url.clone()),
        sslmode,
        connect_timeout_secs: cfg.connect_budget_secs,
        application_name: Some("workforce-edge".to_string()),
        ..PostgresConfig::default()
    }
}

/// The install keys of the test master key
/// ([`crate::settings::tests::test_master_key`]'s bytes), parsed as boot
/// parses the environment, so a test infra on them signs admin sessions as
/// a booted one does.
#[cfg(test)]
pub(crate) fn test_install_keys() -> InstallKeys {
    parse_master_key(&hex::encode([7u8; 32])).expect("a well-formed test key")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_edge_postgres_config_to_an_enabled_pool_config() {
        let edge = crate::config::PostgresConfig {
            url: "postgres://edge/edge".into(),
            sslmode: "require".into(),
            connect_budget_secs: 120,
        };
        let mapped = postgres_config(&edge);
        assert!(mapped.enabled, "edge always drives a real Postgres");
        assert_eq!(mapped.url.as_deref(), Some("postgres://edge/edge"));
        assert_eq!(mapped.sslmode, SslMode::Require);
        assert_eq!(mapped.connect_timeout_secs, 120);
        assert_eq!(mapped.application_name.as_deref(), Some("workforce-edge"));
    }

    #[test]
    fn a_master_key_is_64_hex_chars_or_base64_of_32_bytes_and_errors_never_echo_it() {
        let raw = [0x5au8; 32];
        let hex = hex::encode(raw);
        let b64 = base64::engine::general_purpose::STANDARD.encode(raw);
        let b64_nopad = base64::engine::general_purpose::STANDARD_NO_PAD.encode(raw);
        let sealed = parse_master_key(&hex)
            .unwrap()
            .master
            .seal(b"ctx", b"v")
            .unwrap();
        for form in [hex.as_str(), b64.as_str(), b64_nopad.as_str()] {
            let keys = parse_master_key(&format!(" {form}\n")).unwrap();
            assert_eq!(keys.master.open(b"ctx", &sealed).unwrap().as_slice(), b"v");
        }
        for bad in ["", "abc", &hex[..62], &b64[..10], "not base64 at all!!"] {
            let err = parse_master_key(bad).unwrap_err().to_string();
            assert!(err.contains(MASTER_KEY_ENV), "{err}");
            assert!(bad.is_empty() || !err.contains(bad), "{err}");
        }
    }

    /// The session root is a function of the master key alone: the same
    /// value parsed twice (in any form) derives the same per-tenant and
    /// platform secrets, a different key different ones, and no derived
    /// secret is the key, another tenant's, or the platform's.
    #[test]
    fn the_session_root_derives_stable_distinct_secrets_from_the_master_key() {
        let raw = [0x5au8; 32];
        let hex = hex::encode(raw);
        let b64 = base64::engine::general_purpose::STANDARD.encode(raw);
        let acme = TenantId::parse("acme").unwrap();
        let contoso = TenantId::parse("contoso").unwrap();
        let from_hex = parse_master_key(&hex).unwrap().session_root;
        let from_b64 = parse_master_key(&b64).unwrap().session_root;
        let other = parse_master_key(&hex::encode([0xa5u8; 32]))
            .unwrap()
            .session_root;

        let acme_secret = from_hex.tenant_session_secret(&acme);
        assert_eq!(
            acme_secret,
            from_b64.tenant_session_secret(&acme),
            "stable across parses, whatever the key's form"
        );
        assert_ne!(acme_secret, from_hex.tenant_session_secret(&contoso));
        assert_ne!(acme_secret, other.tenant_session_secret(&acme));
        assert_ne!(acme_secret, from_hex.platform_session_secret());
        assert_eq!(
            from_hex.platform_session_secret(),
            from_b64.platform_session_secret()
        );
        assert_eq!(acme_secret.len(), 64, "32 bytes, hex");
        assert!(acme_secret.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(acme_secret, hex, "never the master key itself");
        assert_eq!(format!("{from_hex:?}"), "SessionRoot(..)");
    }

    #[test]
    fn sslmode_disable_is_honoured_for_local_dev() {
        let edge = crate::config::PostgresConfig {
            url: "postgres://localhost/edge".into(),
            sslmode: "disable".into(),
            connect_budget_secs: 1,
        };
        assert_eq!(postgres_config(&edge).sslmode, SslMode::Disable);
    }

    fn registered(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|id| id.to_string()).collect()
    }

    #[test]
    fn legacy_agent_dirs_move_under_default_once_and_tenant_dirs_stay() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("support")).unwrap();
        std::fs::write(root.join("support/a1.meta.json"), "{}").unwrap();
        std::fs::write(root.join("support/a1.json"), "[]").unwrap();
        std::fs::create_dir_all(root.join("acme/support")).unwrap();
        std::fs::write(root.join("acme/support/b1.meta.json"), "{}").unwrap();
        // A stray file at the root is not an agent directory.
        std::fs::write(root.join(".keep"), "").unwrap();

        let moved = adopt_legacy_artifact_dirs(root, &registered(&["default", "acme"])).unwrap();
        assert_eq!(moved, 1, "only the agent directory moves");
        assert!(root.join("default/support/a1.meta.json").is_file());
        assert!(root.join("default/support/a1.json").is_file());
        assert!(
            !root.join("support").exists(),
            "the legacy directory is gone"
        );
        assert!(
            root.join("acme/support/b1.meta.json").is_file(),
            "a registered tenant's directory is left alone"
        );
        assert!(root.join(".keep").is_file());

        let again = adopt_legacy_artifact_dirs(root, &registered(&["default", "acme"])).unwrap();
        assert_eq!(again, 0, "a second boot finds nothing to adopt");
        assert!(root.join("default/support/a1.meta.json").is_file());
        assert!(
            !root.join("default/default").exists(),
            "default's own directory is never adopted into itself"
        );
    }

    #[test]
    fn a_partially_moved_agent_dir_is_merged_into_default() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("support")).unwrap();
        std::fs::write(root.join("support/late.json"), "[]").unwrap();
        std::fs::create_dir_all(root.join("default/support")).unwrap();
        std::fs::write(root.join("default/support/early.json"), "[]").unwrap();

        let moved = adopt_legacy_artifact_dirs(root, &registered(&["default"])).unwrap();
        assert_eq!(moved, 1);
        assert!(root.join("default/support/early.json").is_file());
        assert!(root.join("default/support/late.json").is_file());
        assert!(!root.join("support").exists());
    }

    #[test]
    fn a_missing_root_has_nothing_to_adopt() {
        let tmp = tempfile::tempdir().unwrap();
        let moved =
            adopt_legacy_artifact_dirs(&tmp.path().join("absent"), &registered(&["default"]))
                .unwrap();
        assert_eq!(moved, 0);
    }
}
