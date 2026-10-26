//! The public HTTP surface — the only thing the internet reaches.
//!
//! Every request is from an untrusted browser, so the whole trust story lives
//! here. [`authenticate`] gates each request in cheap-before-crypto order:
//! per-IP rate limit → agent exists → `Origin` allowed → valid Keycloak token.
//! The verified subject becomes the `ext:{realm}:{sub}` session owner, so a
//! customer can only ever reach their own conversations.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, FromRequestParts, Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use eyre::{Result, WrapErr};
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use ipnetwork::IpNetwork;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{mpsc, Semaphore};
use tokio_util::sync::CancellationToken;
use workforce_agent::artifacts::disposition::content_disposition_attachment;
use workforce_agent::artifacts::{ArtifactBody, ArtifactId, ArtifactStore, ContentType};
use workforce_agent::llm::provider::ProviderKind;

use crate::agent::{
    EdgeAgent, EmbedSettings, McpAuth, McpServerSettings, McpServerSpec, McpSettings, ToolGrant,
};
use crate::boot::SharedInfra;
use crate::config::{EdgeConfig, GitConfig};
use crate::git_config::{self, Expansion};
use crate::host::{AgentHost, CustomerEvent, TurnGate, TurnRequest};
use workforce_common::session::MessageRole;

use crate::ingress::{bearer_token, client_ip, external_owner, request_host, RateLimiter};
use crate::mcp::TokenCell;
use crate::outbound::OutboundPolicy;
use crate::runtime::{Retirement, TenantRuntime};
use crate::settings::{validate_llm_key, LlmStatus, StoredSettings};
use crate::tenant::{
    PlanLimit, Resolved, ServedTenant, Tenant, TenantId, TenantMode, TenantRejection,
    TenantSettings, TenantStatus,
};
use crate::wire::{ClientFrame, FormSubmission, ScreenContext, ServerFrame};

/// Shared state for the public surface: what every tenant shares, and the
/// tenants themselves. A request's host picks its [`TenantRuntime`]; every
/// handler works on that and never on the map.
pub struct EdgeState {
    infra: Arc<SharedInfra>,
    tenants: std::sync::RwLock<HashMap<TenantId, Arc<TenantRuntime>>>,
    /// Lowercase host → tenant, a cache of the registry's `tenant_hostnames`.
    hostnames: std::sync::RwLock<HashMap<String, TenantId>>,
    /// Tenants not being served, with the message their callers are shown.
    suspended: std::sync::RwLock<HashMap<TenantId, Suspension>>,
    /// Tenants retired for deletion ([`Self::retire`]) and not yet forgotten:
    /// no runtime is installed for one, so a reload racing the deletion
    /// cannot leave a runtime serving rows that are gone.
    deleting: std::sync::RwLock<HashSet<TenantId>>,
    mode: TenantMode,
    /// The file the tenants' settings are still built from (see
    /// [`TenantSettings::for_tenant`]), so a tenant can be rebuilt on reload.
    config: EdgeConfig,
    /// Process-wide ceiling on concurrently-running turns, over every tenant's
    /// own.
    global_turn_limit: Arc<Semaphore>,
    /// Per-IP budget for requests whose host names no served tenant. Those
    /// never reach a tenant's own limiter, and an unknown host or a suspended
    /// tenant answers without any check otherwise.
    unresolved_limiter: RateLimiter,
    /// Cancelled on SIGTERM/ctrl_c so sockets finish their current turn and
    /// stop accepting new ones, letting a deploy drain instead of hard-cutting.
    shutdown: CancellationToken,
    /// The platform surface, when `[platform]` is set; `None` leaves every
    /// `/platform` route 404.
    platform: Option<Arc<crate::platform::PlatformState>>,
    /// The `/metrics` route, when `[metrics]` is enabled; `None` leaves it
    /// unmounted.
    metrics: Option<Arc<crate::metrics::MetricsEndpoint>>,
}

/// Requests per minute one address may send to hosts that name no served
/// tenant. Enough for a widget's reconnect backoff, not for a scan.
pub(crate) const UNRESOLVED_PER_MIN: u32 = 30;

/// Why a tenant is not being served: what its callers are told, and — for
/// a tenant that failed to build — the failure itself, which stays in the
/// log and the platform's tenant view and never reaches a public response.
#[derive(Clone, Debug)]
struct Suspension {
    /// The public message: the operator's on a suspend, a fixed string on a
    /// build failure.
    message: String,
    /// The build failure, when that is the cause.
    detail: Option<String>,
}

impl EdgeState {
    /// Build the tenants this process serves: the one `default` tenant from
    /// the file, or — with `server.multi_tenant` — every tenant in the
    /// registry, each on its registered hostnames.
    pub async fn build(config: &EdgeConfig, infra: Arc<SharedInfra>) -> Result<Arc<Self>> {
        let platform = config
            .platform
            .as_ref()
            .map(|cfg| {
                crate::platform::PlatformState::new(
                    cfg,
                    config.server.default_outbound,
                    infra.session_root.as_deref(),
                )
            })
            .transpose()
            .wrap_err("building the platform surface")?
            .map(Arc::new);
        let metrics =
            crate::metrics::MetricsEndpoint::from_config(config.metrics.as_ref())?.map(Arc::new);
        if platform.is_some() && !config.server.multi_tenant {
            tracing::warn!(
                "[platform] is set without server.multi_tenant: the platform API can manage the \
                 registry, but only the default tenant is served"
            );
        }
        let state = Self {
            infra,
            tenants: std::sync::RwLock::new(HashMap::new()),
            hostnames: std::sync::RwLock::new(HashMap::new()),
            suspended: std::sync::RwLock::new(HashMap::new()),
            deleting: std::sync::RwLock::new(HashSet::new()),
            mode: if config.server.multi_tenant {
                TenantMode::Multi
            } else {
                TenantMode::Single(TenantId::default_tenant())
            },
            config: config.clone(),
            global_turn_limit: Arc::new(Semaphore::new(config.server.max_concurrent_turns.max(1))),
            unresolved_limiter: RateLimiter::new(UNRESOLVED_PER_MIN, Duration::from_secs(60)),
            shutdown: CancellationToken::new(),
            platform,
            metrics,
        };
        match &state.mode {
            TenantMode::Single(id) => {
                // Without a master key the file alone configures the one
                // tenant, as it always has; with one, its settings row joins
                // in, seeded from the file on first boot.
                let settings = if state.infra.master_key.is_some() {
                    let row = state
                        .infra
                        .tenants
                        .get(id)
                        .await?
                        .ok_or_else(|| eyre::eyre!("tenant {id} is not in the registry"))?;
                    state.settings_for(&row).await?
                } else {
                    TenantSettings::from_edge_toml(config)
                };
                let runtime = TenantRuntime::build(settings, &state.infra).await?;
                state.install(id.clone(), runtime)?;
            }
            TenantMode::Multi => {
                if config.weaviate.is_some() {
                    eyre::bail!(
                        "server.multi_tenant needs [vellev]: Weaviate has no tenant boundary, so \
                         one instance cannot keep tenants' memory apart"
                    );
                }
                if !state.infra.rls_enforced {
                    eyre::bail!(
                        "server.multi_tenant needs a database role the row-level-security \
                         policies bind; the configured role bypasses them"
                    );
                }
                if state.infra.master_key.is_none() {
                    eyre::bail!(
                        "server.multi_tenant needs {}: each tenant's secrets are held sealed in \
                         its settings row, and there is no file to hold them instead",
                        crate::boot::MASTER_KEY_ENV
                    );
                }
                if config.llm.allowed_providers.is_empty() {
                    eyre::bail!(
                        "server.multi_tenant needs [llm] allowed_providers: the providers tenants \
                         may bring a key for (for example [\"anthropic\", \"openai\", \"gemini\"])"
                    );
                }
                // A name that is no provider at all matches nothing, as it
                // never did; a known provider that takes no bearer key is
                // refused outright.
                if let Some(provider) = config.llm.allowed_providers.iter().find(|p| {
                    crate::settings::provider_takes_a_key(p)
                        == Err(crate::settings::KeyFailure::ProviderNotAllowed)
                }) {
                    eyre::bail!(
                        "server.multi_tenant cannot allow llm provider {provider:?}: it takes no \
                         API key (a URL in its place, the process's own credentials or a login), \
                         so it would run every tenant on the deployment's. [llm] \
                         allowed_providers must name bearer-key providers only"
                    );
                }
                if let Some(field) = config.shared_file_secret() {
                    eyre::bail!(
                        "server.multi_tenant cannot run with {field} in edge.toml: a secret in the \
                         file would be shared by every tenant. Each tenant's secrets are held \
                         sealed in its settings row; remove it from the file"
                    );
                }
                if let Some(admin) = &config.admin {
                    TenantSettings::admin_shareable(admin)?;
                }
                let rows = state.infra.tenants.list().await?;
                for row in &rows {
                    for host in &row.hostnames {
                        if state.platform.as_ref().is_some_and(|p| p.is_host(host)) {
                            // The registry cannot be stopped from holding it,
                            // but the platform routes answer on it first.
                            tracing::warn!(
                                tenant = %row.id, host = %host,
                                "tenant hostname is the platform's own; the tenant is not reachable on it"
                            );
                        }
                        state
                            .hostnames
                            .write()
                            .expect("hostnames lock")
                            .insert(host.to_ascii_lowercase(), row.id.clone());
                    }
                    match row.status {
                        TenantStatus::Active => {
                            let built = match state.settings_for(row).await {
                                Ok(settings) => TenantRuntime::build(settings, &state.infra).await,
                                Err(e) => Err(e),
                            };
                            state.install_or_suspend(row.id.clone(), built);
                        }
                        TenantStatus::Suspended => {
                            state.suspended.write().expect("suspended lock").insert(
                                row.id.clone(),
                                Suspension {
                                    message: SUSPENDED_MESSAGE.to_string(),
                                    detail: None,
                                },
                            );
                        }
                    }
                }
                tracing::info!(
                    tenants = rows.len(),
                    active = state.active().len(),
                    "edge serving the tenant registry by hostname"
                );
            }
        }
        Ok(Arc::new(state))
    }

    /// Put `runtime` in service as `id`, retiring whatever served that id
    /// before: its background work stops, its sockets are told to reconnect,
    /// and in-flight turns keep the hosts they already hold. Refused — the
    /// runtime retired unserved — while `id` is being deleted, so a build
    /// that began before [`Self::retire`] cannot land after it.
    fn install(&self, id: TenantId, runtime: Arc<TenantRuntime>) -> Result<()> {
        let mut tenants = self.tenants.write().expect("tenants lock");
        if self.is_deleting(&id) {
            runtime.retire(Retirement::Suspended(TENANT_DELETED_MESSAGE.to_string()));
            return Err(deleting_error(&id));
        }
        let previous = tenants.insert(id, runtime);
        drop(tenants);
        if let Some(previous) = previous {
            previous.retire(Retirement::Reloaded);
        }
        Ok(())
    }

    /// Whether `id` is retired for a deletion that has not finished.
    fn is_deleting(&self, id: &TenantId) -> bool {
        self.deleting.read().expect("deleting lock").contains(id)
    }

    /// Put a tenant's boot outcome in service: a built runtime serves; a
    /// failed build suspends the tenant, the failure logged and shown to the
    /// platform, so the others boot. Multi-tenant only — a self-hosted
    /// install has nothing else to serve, so its one failure fails boot.
    fn install_or_suspend(&self, id: TenantId, built: Result<Arc<TenantRuntime>>) {
        match built.and_then(|runtime| self.install(id.clone(), runtime)) {
            Ok(()) => {}
            Err(error) => self.suspend_failed(&id, &error),
        }
    }

    /// Suspend `id` over a build failure, retiring whatever served it. Its
    /// callers see a fixed message; the failure itself — which can name
    /// hosts, repos and the shape of the tenant's config — goes to the log
    /// and the platform's tenant view only.
    fn suspend_failed(&self, id: &TenantId, error: &eyre::Report) {
        let detail = format!("tenant failed to start: {error:#}");
        tracing::error!(tenant = %id, error = %format!("{error:#}"), "tenant failed to start; suspended");
        self.suspended.write().expect("suspended lock").insert(
            id.clone(),
            Suspension {
                message: UNAVAILABLE_MESSAGE.to_string(),
                detail: Some(detail),
            },
        );
        let retired = self.tenants.write().expect("tenants lock").remove(id);
        if let Some(retired) = retired {
            retired.retire(Retirement::Suspended(UNAVAILABLE_MESSAGE.to_string()));
        }
    }

    /// Serve `id` from its registry row as boot would — the platform API's
    /// path for a tenant it just created or is resuming. A runtime that
    /// builds replaces whatever served the tenant and lifts its suspension;
    /// one that fails to build suspends the tenant with the failure as its
    /// detail, which the error carries too. A tenant being deleted is not
    /// started.
    pub(crate) async fn start(&self, id: &TenantId) -> Result<()> {
        if self.is_deleting(id) {
            return Err(deleting_error(id));
        }
        let row = self
            .infra
            .tenants
            .get(id)
            .await?
            .ok_or_else(|| eyre::eyre!("tenant {id} is not in the registry"))?;
        let built = match self.settings_for(&row).await {
            Ok(settings) => TenantRuntime::build(settings, &self.infra).await,
            Err(e) => Err(e),
        };
        self.refresh_hostnames(&row);
        match built {
            Ok(runtime) => {
                self.install(id.clone(), runtime)?;
                self.suspended.write().expect("suspended lock").remove(id);
                Ok(())
            }
            Err(error) => {
                self.suspend_failed(id, &error);
                Err(error)
            }
        }
    }

    /// How `id` is being served, whatever its hostnames: the runtime, the
    /// suspension message, or nothing at all.
    pub(crate) fn serving(&self, id: &TenantId) -> Resolved {
        if let Some(suspension) = self.suspended.read().expect("suspended lock").get(id) {
            return Resolved::Suspended(suspension.message.clone());
        }
        match self.tenants.read().expect("tenants lock").get(id) {
            Some(runtime) => Resolved::Tenant(Arc::clone(runtime)),
            None => Resolved::Unknown,
        }
    }

    /// Why `id` is not being served, for the platform's tenant view: the
    /// build failure when there was one, else the message its callers see;
    /// `None` while it serves.
    pub(crate) fn unavailable(&self, id: &TenantId) -> Option<String> {
        self.suspended
            .read()
            .expect("suspended lock")
            .get(id)
            .map(|suspension| {
                suspension
                    .detail
                    .clone()
                    .unwrap_or_else(|| suspension.message.clone())
            })
    }

    /// The first step of deleting `id`: its runtime is retired and its
    /// hostnames answer that it is gone, while its rows are still there for
    /// the deletion to finish — or for a retry, if it does not. A tenant
    /// stays in this state, and no runtime is installed for it, until
    /// [`Self::forget`].
    pub(crate) fn retire(&self, id: &TenantId) {
        self.deleting
            .write()
            .expect("deleting lock")
            .insert(id.clone());
        self.suspended.write().expect("suspended lock").insert(
            id.clone(),
            Suspension {
                message: TENANT_DELETED_MESSAGE.to_string(),
                detail: None,
            },
        );
        let retired = self.tenants.write().expect("tenants lock").remove(id);
        if let Some(retired) = retired {
            retired.retire(Retirement::Suspended(TENANT_DELETED_MESSAGE.to_string()));
        }
        tracing::warn!(tenant = %id, "tenant retired for deletion");
    }

    /// The last step of deleting `id`, once its rows are gone: its hostnames
    /// leave the cache and answer 404 from here on, and any runtime still
    /// installed for it is retired.
    pub(crate) fn forget(&self, id: &TenantId) {
        let retired = self.tenants.write().expect("tenants lock").remove(id);
        if let Some(retired) = retired {
            retired.retire(Retirement::Suspended(TENANT_DELETED_MESSAGE.to_string()));
        }
        self.suspended.write().expect("suspended lock").remove(id);
        self.deleting.write().expect("deleting lock").remove(id);
        self.hostnames
            .write()
            .expect("hostnames lock")
            .retain(|_, tenant| tenant != id);
    }

    /// Reach `id` on `host` from now on, without a reload; the registry
    /// already holds the registration.
    pub(crate) fn hostname_added(&self, id: &TenantId, host: &str) {
        self.hostnames
            .write()
            .expect("hostnames lock")
            .insert(host.to_ascii_lowercase(), id.clone());
    }

    /// Stop reaching any tenant on `host` from now on.
    pub(crate) fn hostname_removed(&self, host: &str) {
        self.hostnames
            .write()
            .expect("hostnames lock")
            .remove(&host.to_ascii_lowercase());
    }

    /// Whether `host` is a registered hostname of a tenant being served —
    /// the TLS gate's question, answered from the cache alone. Single mode
    /// registers no hostnames, so it answers no.
    pub(crate) fn hostname_served(&self, host: &str) -> bool {
        let id = match self
            .hostnames
            .read()
            .expect("hostnames lock")
            .get(&host.to_ascii_lowercase())
        {
            Some(id) => id.clone(),
            None => return false,
        };
        !self
            .suspended
            .read()
            .expect("suspended lock")
            .contains_key(&id)
            && self.tenants.read().expect("tenants lock").contains_key(&id)
    }

    /// The platform surface, when configured.
    pub(crate) fn platform(&self) -> Option<&Arc<crate::platform::PlatformState>> {
        self.platform.as_ref()
    }

    /// What every tenant shares.
    pub(crate) fn infra(&self) -> &Arc<SharedInfra> {
        &self.infra
    }

    /// Whether `ip` may receive another answer for a host that names no
    /// served tenant.
    pub(crate) fn admit_unresolved(&self, ip: IpAddr) -> bool {
        self.unresolved_limiter.check(ip)
    }

    /// The tenant a request's host reaches. In single mode every host reaches
    /// the one tenant; in multi mode the host must be registered.
    pub fn resolve(&self, host: &str) -> Resolved {
        let id = match &self.mode {
            TenantMode::Single(id) => id.clone(),
            TenantMode::Multi => {
                let hostnames = self.hostnames.read().expect("hostnames lock");
                match hostnames.get(&host.to_ascii_lowercase()) {
                    Some(id) => id.clone(),
                    None => return Resolved::Unknown,
                }
            }
        };
        if let Some(suspension) = self.suspended.read().expect("suspended lock").get(&id) {
            return Resolved::Suspended(suspension.message.clone());
        }
        match self.tenants.read().expect("tenants lock").get(&id) {
            Some(runtime) => Resolved::Tenant(Arc::clone(runtime)),
            None => Resolved::Unknown,
        }
    }

    /// Rebuild `id` from its registry row and swap the new runtime in. The old
    /// one serves until the new one is ready and installed, and only then is
    /// retired — so a build failure leaves it in place, and its git poll
    /// never writes beside the new runtime's. A tenant being deleted is not
    /// rebuilt.
    pub async fn reload(&self, id: &TenantId) -> Result<()> {
        if self.is_deleting(id) {
            return Err(deleting_error(id));
        }
        let row = self
            .infra
            .tenants
            .get(id)
            .await?
            .ok_or_else(|| eyre::eyre!("tenant {id} is not in the registry"))?;
        let settings = self.settings_for(&row).await?;
        let runtime = TenantRuntime::build(settings, &self.infra)
            .await
            .wrap_err_with(|| format!("rebuilding tenant {id}"))?;
        self.refresh_hostnames(&row);
        self.install(id.clone(), runtime)
    }

    /// A tenant's settings: the file's sections, overlaid with its settings
    /// row when the install keeps one. The `default` tenant's row is seeded
    /// from the file on its first boot — the self-hosted path onto the table —
    /// and any other tenant's exists once the platform API creates it or its
    /// admin saves settings.
    pub(crate) async fn settings_for(&self, row: &Tenant) -> Result<TenantSettings> {
        let Some(store) = self.infra.settings_store_for(&row.id) else {
            return Ok(TenantSettings::for_tenant(row, &self.config, None));
        };
        let from_file = TenantSettings::for_tenant(row, &self.config, None);
        if row.id.is_default() {
            store.seed_if_empty(&from_file).await?;
        }
        let stored = store
            .load()
            .await
            .wrap_err_with(|| format!("loading the settings of tenant {}", row.id))?;
        Ok(TenantSettings::for_tenant(
            row,
            &self.config,
            stored.as_ref(),
        ))
    }

    /// Stop serving `id`: its runtime is dropped (background work with it),
    /// its open sockets are closed with `message`, and its routes answer
    /// with it until [`Self::resume`]. Its data stays.
    pub async fn suspend(&self, id: &TenantId, message: String) {
        self.suspended.write().expect("suspended lock").insert(
            id.clone(),
            Suspension {
                message: message.clone(),
                detail: None,
            },
        );
        let retired = self.tenants.write().expect("tenants lock").remove(id);
        if let Some(retired) = retired {
            retired.retire(Retirement::Suspended(message));
        }
        tracing::warn!(tenant = %id, "tenant suspended");
    }

    /// Serve `id` again, rebuilt from its registry row. A build failure
    /// leaves it suspended with the failure as its message.
    pub async fn resume(&self, id: &TenantId) -> Result<()> {
        self.start(id).await?;
        tracing::info!(tenant = %id, "tenant resumed");
        Ok(())
    }

    /// Every tenant currently being served.
    pub fn active(&self) -> Vec<Arc<TenantRuntime>> {
        self.tenants
            .read()
            .expect("tenants lock")
            .values()
            .cloned()
            .collect()
    }

    /// Re-cache a tenant's hostnames from its row: entries that moved away
    /// from it go, entries it gained arrive.
    fn refresh_hostnames(&self, row: &Tenant) {
        let mut hostnames = self.hostnames.write().expect("hostnames lock");
        hostnames.retain(|_, id| *id != row.id);
        for host in &row.hostnames {
            hostnames.insert(host.to_ascii_lowercase(), row.id.clone());
        }
    }

    /// Proxy hops trusted to set the forwarded client address and host.
    pub fn trusted_proxies(&self) -> &[IpNetwork] {
        &self.infra.trusted_proxies
    }

    /// The semaphore every turn in the process holds a permit from.
    fn turn_gate(&self, tenant: &TenantRuntime) -> TurnGate {
        TurnGate {
            tenant: Arc::clone(&tenant.turn_limit),
            global: Arc::clone(&self.global_turn_limit),
        }
    }

    /// A state over ready-made runtimes and no data plane, for router tests:
    /// `hostnames` maps each host to the tenant it reaches in multi mode.
    #[cfg(test)]
    pub(crate) fn for_tests(
        mode: TenantMode,
        tenants: Vec<(Arc<TenantRuntime>, Vec<&str>)>,
        infra: Arc<SharedInfra>,
        config: EdgeConfig,
        global_turn_limit: usize,
    ) -> Arc<Self> {
        let platform = config
            .platform
            .as_ref()
            .map(|cfg| {
                crate::platform::PlatformState::new(
                    cfg,
                    config.server.default_outbound,
                    infra.session_root.as_deref(),
                )
            })
            .transpose()
            .expect("a valid test [platform]")
            .map(Arc::new);
        let metrics = crate::metrics::MetricsEndpoint::from_config(config.metrics.as_ref())
            .expect("a valid test [metrics]")
            .map(Arc::new);
        let state = Self {
            infra,
            tenants: std::sync::RwLock::new(HashMap::new()),
            hostnames: std::sync::RwLock::new(HashMap::new()),
            suspended: std::sync::RwLock::new(HashMap::new()),
            deleting: std::sync::RwLock::new(HashSet::new()),
            mode,
            config,
            global_turn_limit: Arc::new(Semaphore::new(global_turn_limit)),
            unresolved_limiter: RateLimiter::new(UNRESOLVED_PER_MIN, Duration::from_secs(60)),
            shutdown: CancellationToken::new(),
            platform,
            metrics,
        };
        for (runtime, hostnames) in tenants {
            for host in hostnames {
                state
                    .hostnames
                    .write()
                    .expect("hostnames lock")
                    .insert(host.to_string(), runtime.id.clone());
            }
            state
                .install(runtime.id.clone(), runtime)
                .expect("nothing is being deleted in a fresh test state");
        }
        Arc::new(state)
    }
}

/// What a suspended tenant's callers are told when the registry, not an
/// operator, is the source of the suspension.
pub(crate) const SUSPENDED_MESSAGE: &str = "This assistant is temporarily unavailable.";

/// What the callers of a tenant that failed to build are told. Fixed: the
/// failure itself stays in the log and the platform's tenant view.
pub(crate) const UNAVAILABLE_MESSAGE: &str = "This assistant is unavailable.";

/// What a deleted tenant's open sockets are told as they close.
const TENANT_DELETED_MESSAGE: &str = "This assistant is no longer available.";

fn deleting_error(id: &TenantId) -> eyre::Report {
    eyre::eyre!("tenant {id} is being deleted")
}

/// How often to drop turn claims left by a process that died mid-turn.
const TURN_CLAIM_SWEEP_INTERVAL: Duration = Duration::from_secs(300);

/// Claims older than this are orphans: it is above the runtime's own per-turn
/// ceiling (`host::MAX_TURN`, 240s), so a legitimately-running turn is never
/// swept out from under itself.
const TURN_CLAIM_MAX_AGE: Duration = Duration::from_secs(300);

/// How often retained tool evidence is checked against its retention.
const EVIDENCE_SWEEP_INTERVAL: Duration = Duration::from_secs(3600);

/// Interval between artifact-store sweeps that purge expired downloads.
const ARTIFACT_SWEEP_INTERVAL: Duration = Duration::from_secs(600);

/// Start the process's housekeeping over every served tenant: the turn-claim
/// sweep, the tool-evidence retention sweep and the artifact purge. Each
/// tick walks [`EdgeState::active`] and works through that runtime's own
/// stores, so a sweep never crosses a tenant boundary and a suspended tenant
/// is left untouched until it is served again. The loops end with the
/// state's shutdown token.
pub fn spawn_tenant_loops(state: Arc<EdgeState>) {
    spawn_loop(Arc::clone(&state), TURN_CLAIM_SWEEP_INTERVAL, |state| {
        Box::pin(sweep_turn_claims(state, TURN_CLAIM_MAX_AGE))
    });
    spawn_loop(Arc::clone(&state), EVIDENCE_SWEEP_INTERVAL, |state| {
        Box::pin(sweep_tool_evidence(state))
    });
    spawn_loop(state, ARTIFACT_SWEEP_INTERVAL, |state| {
        Box::pin(purge_artifacts(state))
    });
}

/// Run `tick` every `interval` until shutdown. Missed ticks are skipped, so a
/// slow sweep is never followed by a burst of catch-up sweeps.
fn spawn_loop<F>(state: Arc<EdgeState>, interval: Duration, tick: F)
where
    F: for<'a> Fn(&'a EdgeState) -> futures::future::BoxFuture<'a, ()> + Send + 'static,
{
    tokio::spawn(async move {
        let shutdown = state.shutdown.clone();
        let mut timer = tokio::time::interval(interval);
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => return,
                _ = timer.tick() => {}
            }
            tick(&state).await;
        }
    });
}

/// Purge stale rows from each tenant's cross-process turn registry.
///
/// Readers already ignore claims older than the turn ceiling, so this is hygiene
/// rather than correctness — without it a replica killed mid-turn leaves a row
/// per lost turn forever.
async fn sweep_turn_claims(state: &EdgeState, max_age: Duration) {
    for tenant in state.active() {
        match tenant.analytics().sweep_stale_turns(max_age).await {
            Ok(n) if n > 0 => {
                tracing::info!(tenant = %tenant.id, swept = n, "swept stale turn claims")
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(
                tenant = %tenant.id, error = %format!("{e:#}"), "turn-claim sweep failed"
            ),
        }
    }
}

/// Drop each auditing tenant's tool evidence past its retention.
///
/// A tenant with auditing off stores nothing and is skipped. Retention is the
/// whole reason this store is acceptable — it holds customer data, and without
/// a sweeper it would accumulate indefinitely.
async fn sweep_tool_evidence(state: &EdgeState) {
    for tenant in state.active() {
        let Some(retention) = tenant.audit_retention() else {
            continue;
        };
        match tenant.analytics().sweep_tool_evidence(retention).await {
            Ok(n) if n > 0 => tracing::info!(
                tenant = %tenant.id,
                swept = n,
                days = retention.as_secs() / (24 * 60 * 60),
                "swept tool evidence past retention"
            ),
            Ok(_) => {}
            Err(e) => tracing::warn!(
                tenant = %tenant.id, error = %format!("{e:#}"), "tool-evidence sweep failed"
            ),
        }
    }
}

/// Purge each tenant's expired downloads from its own artifact store.
async fn purge_artifacts(state: &EdgeState) {
    for tenant in state.active() {
        let Some(artifacts) = tenant.artifacts() else {
            continue;
        };
        match artifacts.store.purge_expired() {
            Ok(n) if n > 0 => {
                tracing::info!(tenant = %tenant.id, purged = n, "swept expired artifacts")
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(tenant = %tenant.id, error = %e, "artifact sweep failed"),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct WsQuery {
    /// Non-browser fallback for the token (browsers pass it as a subprotocol).
    pub token: Option<String>,
    /// Conversation to resume; absent ⇒ the server mints a new one.
    pub conversation: Option<String>,
}

/// A verified customer-facing caller: the tenant and agent host, the owner
/// key its sessions are scoped to, and the token forwarded to the company MCP.
pub(crate) struct Caller {
    pub(crate) tenant: Arc<TenantRuntime>,
    pub(crate) host: Arc<AgentHost>,
    pub(crate) owner: String,
    token: String,
    /// The caller's display identity (name/email) from the token, carried into
    /// each turn for usage attribution. Owner-pinned, so it is captured once at
    /// authenticate and not refreshed by a same-owner token rotation.
    display_name: Option<String>,
    email: Option<String>,
}

/// Whether a request's `Origin` passes the embed gate. A **present** Origin
/// must be allow-listed. An **absent** Origin passes: browsers omit it on
/// same-origin GETs (the conversation API called from the demo page, whose
/// page and edge share an origin) and non-browser clients omit it too — in
/// both cases the bearer token is the real gate. WebSocket handshakes always
/// carry an Origin, so the socket path stays fully checked.
fn origin_permitted(embed: &crate::agent::EmbedSettings, origin: Option<&str>) -> bool {
    match origin {
        Some(o) => embed.origin_allowed(o),
        None => true,
    }
}

/// Gate + authenticate a public request against the tenant its host resolved
/// to. Returns a bare [`StatusCode`] on failure so no internal detail leaks to
/// the browser.
pub(crate) async fn authenticate(
    tenant: &Arc<TenantRuntime>,
    trusted: &[IpNetwork],
    agent_id: &str,
    headers: &HeaderMap,
    query_token: Option<&str>,
    peer: Option<SocketAddr>,
) -> Result<Caller, StatusCode> {
    // Per-IP limit FIRST — before the registry lookup and the Keycloak crypto —
    // so an unauthenticated flood can't burn signature-verifies or runs.
    let ip = client_ip(trusted, headers, peer.map(|p| p.ip()));
    if !tenant.rate_limiter.check(ip) {
        return Err(StatusCode::TOO_MANY_REQUESTS);
    }
    let host = tenant.host(agent_id).ok_or(StatusCode::NOT_FOUND)?;
    let embed = &host.agent().embed;
    let origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok());
    if !origin_permitted(embed, origin) {
        return Err(StatusCode::FORBIDDEN);
    }
    let token = bearer_token(headers, query_token).ok_or(StatusCode::UNAUTHORIZED)?;
    // The issuer is the tenant's own URL, reached under the tenant's policy.
    let claims = crate::keycloak::validator_for_policy(
        &embed.keycloak_issuer,
        embed.keycloak_audience.as_deref(),
        tenant.outbound(),
    )
    .verify(&token)
    .await
    .map_err(|_| StatusCode::UNAUTHORIZED)?;
    // Valid token but a holder outside the configured email allowlist — an
    // authenticated caller this agent does not serve.
    if !embed.email_allowed(claims.email.as_deref(), claims.email_verified) {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(Caller {
        tenant: Arc::clone(tenant),
        owner: external_owner(&embed.keycloak_issuer, &claims.sub),
        display_name: claims.display_name(),
        email: claims.email.clone(),
        token,
        host,
    })
}

/// `GET /embed/{agent}` — the widget loader the host page includes. Carries no
/// secret, so it needs only the agent to exist (rate-limited; 404 otherwise).
/// The customer's token is obtained later by the widget via the host-page
/// `postMessage` handshake. The loader mounts the SPA (served from `/widget/`)
/// in a sandboxed iframe. A suspended tenant gets a script that mounts nothing
/// and says why on the console: the host page's `<script>` tag must not break.
async fn public_widget(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path(agent_id): Path<String>,
    headers: HeaderMap,
    State(state): State<Arc<EdgeState>>,
    tenant: Result<ServedTenant, TenantRejection>,
) -> Response {
    let tenant = match tenant {
        Ok(tenant) => tenant,
        Err(TenantRejection::Suspended(message)) => return suspended_loader(&message),
        Err(unknown) => return unknown.into_response(),
    };
    let ip = client_ip(state.trusted_proxies(), &headers, Some(peer.ip()));
    if !tenant.rate_limiter.check(ip) {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    }
    let Some(host) = tenant.host(&agent_id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let agent = host.agent();
    match crate::assets::loader_script(
        &agent_id,
        &agent.embed,
        &agent.welcome,
        &nonce,
        host.artifacts().is_some(),
    ) {
        // Never cache: this script embeds a fresh per-load nonce and the current
        // theme, so a cached copy would reuse a stale nonce and stale branding.
        Some(body) => (
            [
                (
                    header::CONTENT_TYPE,
                    "application/javascript; charset=utf-8",
                ),
                (header::CACHE_CONTROL, "no-store"),
            ],
            body,
        )
            .into_response(),
        None => {
            tracing::error!(agent = %agent_id, "widget bundle not embedded in this build");
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
    }
}

/// The loader a suspended tenant's host pages receive: a 200 that renders
/// nothing and leaves the reason on the console.
fn suspended_loader(message: &str) -> Response {
    let warning = serde_json::to_string(&format!("workforce-edge: {message}"))
        .unwrap_or_else(|_| "\"workforce-edge: suspended\"".into());
    (
        [
            (
                header::CONTENT_TYPE,
                "application/javascript; charset=utf-8",
            ),
            (header::CACHE_CONTROL, "no-store"),
        ],
        format!("console.warn({warning});\n"),
    )
        .into_response()
}

/// `GET /widget/…` — the chat SPA's own static assets, loaded inside the
/// sandboxed iframe. Public and cacheable; carries no secret.
async fn public_widget_asset(path: Option<Path<String>>) -> Response {
    let path = path.map(|p| p.0).unwrap_or_default();
    match crate::assets::widget_asset(&path) {
        Some((content_type, bytes)) => {
            // Hashed asset filenames make immutable caching safe; index.html is
            // small and re-fetched, so keep it uncached.
            let cache = if path.starts_with("assets/") {
                "public, max-age=31536000, immutable"
            } else {
                "no-cache"
            };
            (
                [
                    (header::CONTENT_TYPE, content_type),
                    (header::CACHE_CONTROL, cache.to_string()),
                    (
                        header::CONTENT_SECURITY_POLICY,
                        // Harden the sandboxed iframe's own content (only the
                        // bundle + Google Identity Services scripts, no objects,
                        // no base hijack). Crucially NO `frame-ancestors`: the
                        // widget is EMBEDDED cross-origin by customer sites by
                        // design — framing is gated by `allowed_origins` + the
                        // token handshake, not CSP. Blocking framing here breaks
                        // every embed.
                        "script-src 'self' https://accounts.google.com; object-src 'none'; base-uri 'self'"
                            .to_string(),
                    ),
                ],
                bytes,
            )
                .into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// `GET /demo/{agent}` — the built-in manual test page (see [`crate::demo`]).
/// 404 unless `[demo] enabled` with a Google client id AND the agent exists, so
/// production installs expose nothing here. Carries no secret: the page holds
/// only the public Google client id; the tester's own sign-in produces the
/// token, which then flows through the exact same widget handshake and WS auth
/// as a real customer page.
async fn public_demo(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path(agent_id): Path<String>,
    headers: HeaderMap,
    State(state): State<Arc<EdgeState>>,
    ServedTenant(tenant): ServedTenant,
) -> Response {
    let ip = client_ip(state.trusted_proxies(), &headers, Some(peer.ip()));
    if !tenant.rate_limiter.check(ip) {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    }
    let enabled = tenant.demo.enabled && tenant.demo.google_client_id.is_some();
    if !enabled || tenant.host(&agent_id).is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    let client_id = tenant.demo.google_client_id.as_deref().unwrap_or_default();
    let body = crate::demo::demo_html(&agent_id, client_id);
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], body).into_response()
}

/// `GET /public/ws/{agent}` — the customer-facing chat socket. Authenticates at
/// upgrade, then streams one conversation. A new chat mints a server-side
/// conversation id; a resume passes `?conversation=`, owner-scoped so a guessed
/// id only ever resolves the caller's own session.
async fn public_chat_ws(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    ws: WebSocketUpgrade,
    Path(agent_id): Path<String>,
    Query(query): Query<WsQuery>,
    headers: HeaderMap,
    State(state): State<Arc<EdgeState>>,
    tenant: Result<ServedTenant, TenantRejection>,
) -> Response {
    // Browsers can't set `Authorization` on a WebSocket, so the token rides
    // `Sec-WebSocket-Protocol` (a JWT is all valid token-chars), keeping the
    // bearer out of proxy/access logs. `?token=` stays as a non-browser
    // fallback only.
    let header_proto = headers
        .get(header::SEC_WEBSOCKET_PROTOCOL)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    // Customer chat messages are small; cap frame/message size so a socket can't
    // buffer a huge body (tungstenite defaults to 64 MiB).
    let mut ws = ws
        .max_message_size(MAX_WS_MESSAGE_BYTES)
        .max_frame_size(MAX_WS_MESSAGE_BYTES);
    // Echo the subprotocol the browser offered (the token rides it): a client
    // that sent `Sec-WebSocket-Protocol` expects it confirmed in the 101, or
    // strict browsers fail the connection.
    if let Some(proto) = &header_proto {
        ws = ws.protocols([proto.clone()]);
    }
    let resumed = query.conversation.as_deref().is_some_and(|c| !c.is_empty());
    let conversation = query
        .conversation
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let tenant = match tenant {
        Ok(tenant) => tenant,
        // Complete the upgrade so the widget hears why, then close it for good:
        // a refused handshake would only make it retry.
        Err(TenantRejection::Suspended(message)) => {
            return ws.on_upgrade(move |socket| close_suspended(socket, message, conversation));
        }
        Err(unknown) => return unknown.into_response(),
    };
    let ws_token = header_proto.as_deref().or(query.token.as_deref());
    let caller = match authenticate(
        &tenant,
        state.trusted_proxies(),
        &agent_id,
        &headers,
        ws_token,
        Some(peer),
    )
    .await
    {
        Ok(caller) => caller,
        Err(code) => return code.into_response(),
    };
    let gate = state.turn_gate(&tenant);
    let shutdown = state.shutdown.clone();
    ws.on_upgrade(move |socket| run_socket(socket, caller, conversation, resumed, gate, shutdown))
}

/// Tell a socket its tenant is suspended, then close it with 1008 (policy
/// violation) so the widget stops rather than reconnects.
async fn close_suspended(socket: WebSocket, message: String, conversation: String) {
    let (mut sender, _receiver) = socket.split();
    close_retired(&mut sender, &Retirement::Suspended(message), &conversation).await;
}

/// What a widget is told when its runtime was replaced under it.
const RELOADED_MESSAGE: &str = "This assistant was updated; reconnecting.";

/// Tell a socket why its tenant stopped serving, then close it with the code
/// the widget acts on: 1008 (policy violation) for a suspension, so it stops;
/// 1012 (service restart) for a reload, so it reconnects to the replacement.
/// A runtime cancelled without a recorded reason is one being dropped, and
/// reconnecting is the safe answer to that too.
async fn close_retired(
    sender: &mut SplitSink<WebSocket, Message>,
    why: &Retirement,
    conversation: &str,
) {
    let (code, close, message) = match why {
        Retirement::Suspended(message) => (
            TenantRejection::SUSPENDED_CODE,
            axum::extract::ws::close_code::POLICY,
            message.as_str(),
        ),
        Retirement::Reloaded => (
            TenantRejection::RELOADED_CODE,
            axum::extract::ws::close_code::RESTART,
            RELOADED_MESSAGE,
        ),
    };
    let frame = ServerFrame::error(code, message.to_string(), conversation);
    if send_frame(sender, &frame).await.is_err() {
        return;
    }
    let _ = sender
        .send(Message::Close(Some(axum::extract::ws::CloseFrame {
            code: close,
            reason: message.to_string().into(),
        })))
        .await;
}

/// Max bytes for an inbound WebSocket message/frame.
const MAX_WS_MESSAGE_BYTES: usize = 64 * 1024;

/// App-level keepalive cadence. Cloudflare drops a proxied connection after
/// 100s without traffic (the ALB after 300s), and a turn's tool phase sends
/// nothing until its first answer token — so the socket layer pings well inside
/// the tightest proxy budget, idle and mid-turn alike. A failed ping is also
/// the fastest detector of a half-open socket.
const HEARTBEAT: Duration = Duration::from_secs(25);

/// Describe why a socket read ended, for the close log.
///
/// The close frame carries the only evidence of WHY a browser went away — 1001
/// (navigating/sleeping) and 1006 (abnormal, no close handshake) point at very
/// different causes, and discarding it as `Close(_)` is what left a real incident
/// unattributable. `None` and a transport error are folded in here so every exit
/// from a read loop logs a cause rather than a bare "closed".
fn close_cause(incoming: &Option<Result<Message, axum::Error>>) -> String {
    match incoming {
        Some(Ok(Message::Close(Some(frame)))) => {
            let reason = frame.reason.trim();
            if reason.is_empty() {
                format!("close code={}", frame.code)
            } else {
                format!("close code={} reason={reason}", frame.code)
            }
        }
        Some(Ok(Message::Close(None))) => "close without a frame".to_string(),
        // A transport error is the shape an abruptly-dropped TCP connection takes
        // — no close handshake ever arrives.
        Some(Err(e)) => format!("transport error: {e}"),
        None => "stream ended without a close frame".to_string(),
        Some(Ok(_)) => "non-close frame".to_string(),
    }
}

/// One customer message waiting to be answered: either the frame that just
/// interrupted a running turn, or the next one read off an idle socket.
///
/// A named struct rather than a tuple because these travel together through the
/// socket loop and `forward_events`, and a positional 5-tuple of mostly-optional
/// fields is easy to mis-order at a call site.
struct PendingMessage {
    content: String,
    context: Option<ScreenContext>,
    submission: Option<FormSubmission>,
    attachments: Vec<String>,
    client_ref: Option<String>,
}

/// Why [`forward_events`] stopped forwarding a turn to this socket.
enum ForwardEnd {
    /// The turn finished (its event stream closed).
    TurnEnded,
    /// The customer sent a new message mid-turn; it is queued in `pending`.
    Interrupted,
    /// The socket died. The turn, if any, keeps running detached. Carries why,
    /// so the caller logs a cause instead of a bare "closed".
    Disconnected(String),
}

/// Drive one customer chat socket: greet, then run one turn per `chat` frame.
/// Turns are serialized per conversation and bounded by the tenant's and the
/// process's turn ceilings (`gate`).
/// A turn belongs to its conversation, not this socket: a socket that dies
/// mid-turn detaches (the turn completes and saves), a reconnecting socket
/// re-attaches to the live stream, and a new message for a busy conversation
/// interrupts it — from any socket.
async fn run_socket(
    socket: WebSocket,
    caller: Caller,
    conversation: String,
    resumed: bool,
    gate: TurnGate,
    shutdown: CancellationToken,
) {
    let (mut sender, mut receiver) = socket.split();
    // Greet only a NEW conversation — a resumed one already renders its
    // transcript, and a fresh greeting under it would read as a new message.
    let welcome = caller.host.agent().welcome.clone();
    let greeting = ServerFrame::new(
        "connected",
        (!resumed && !welcome.is_empty()).then_some(welcome),
        &conversation,
    );
    if send_frame(&mut sender, &greeting).await.is_err() {
        return;
    }
    let agent_id = caller.host.agent().id.clone();
    // `replica` on every socket line: with more than one task serving the same
    // VIP, attributing a reconnect to a different process than the running turn
    // otherwise means reconstructing it from interleaved log prefixes.
    let replica = crate::build_info::replica_id();
    tracing::info!(agent = %agent_id, conversation = %conversation, replica = %replica, resumed, "edge ws connected");

    // The token used for the company MCP on each turn. Seeded from the connect
    // handshake, then refreshed in-place by `token` frames — the Keycloak token
    // expires (~5 min) but a chat can outlive it, so without this a long
    // conversation's turns would start failing MCP calls (401) until reload.
    let mcp_token = TokenCell::new(caller.token.clone());

    // A message the user sent to INTERRUPT the running turn — answered next,
    // paired with the (raw, untrusted) screen context and (raw, capped-on-ingest)
    // form submission from the same chat / form_submit frame.
    let mut pending: Option<PendingMessage> = None;

    // A reconnect can land while the conversation's previous turn still runs
    // (its socket died mid-turn): re-attach and stream it to this socket
    // before taking new input.
    if resumed {
        if let Some(rx) = caller.host.attach_turn(&caller.owner, &conversation) {
            tracing::info!(
                agent = %agent_id,
                conversation = %conversation,
                replica = %replica,
                "edge ws re-attached to a running turn"
            );
            match forward_events(
                &mut sender,
                &mut receiver,
                rx,
                &caller,
                &mcp_token,
                &conversation,
                &mut pending,
            )
            .await
            {
                ForwardEnd::Disconnected(cause) => {
                    tracing::info!(agent = %agent_id, conversation = %conversation, replica = %replica, mid_turn = true, cause = %cause, "edge ws closed");
                    return;
                }
                ForwardEnd::TurnEnded | ForwardEnd::Interrupted => {}
            }
        } else if let Some(elsewhere) = caller
            .host
            .turn_running_elsewhere(&caller.owner, &conversation)
            .await
        {
            // The split, caught in the act. Distinguished from a normal reopen
            // (nothing running at all, which logs nothing) by the shared turn
            // registry, so this fires only when a customer is genuinely about to
            // lose an answer. On a single-replica install it should never appear.
            tracing::warn!(
                agent = %agent_id,
                conversation = %conversation,
                replica = %replica,
                owning_replica = %elsewhere.replica,
                running_for_secs = (chrono::Utc::now() - elsewhere.started_at).num_seconds(),
                "edge ws resumed onto a replica that does not own this conversation's running \
                 turn: no replay can be streamed here, so the answer reaches the customer only \
                 if the widget's transcript recovery outlives the turn"
            );
        }
    }

    'socket: loop {
        // Draining for a deploy: any turn already running has finished by the
        // time we loop back here; start no new one. Tell the client to reconnect
        // to a healthy replica, then close so graceful shutdown can complete.
        if shutdown.is_cancelled() {
            let _ = send_frame(
                &mut sender,
                &ServerFrame::new("reconnect", None, &conversation),
            )
            .await;
            break 'socket;
        }
        // The same for a tenant that stopped serving mid-conversation: this
        // socket holds the retired runtime's hosts and turn ceiling, so it
        // must not start another turn on them. A turn that was running
        // finished on the hosts it held; the next one goes to whatever now
        // serves the tenant, or nowhere.
        if caller.tenant.is_retired() {
            let why = retirement(&caller);
            tracing::info!(agent = %agent_id, conversation = %conversation, replica = %replica, reason = ?why, "edge ws closed: tenant retired");
            close_retired(&mut sender, &why, &conversation).await;
            break 'socket;
        }
        // The next message to answer: a queued interrupt, else the next `chat` /
        // `form_submit` frame off the socket (token refreshes applied inline;
        // others ignored). The frame carries the customer message plus its
        // optional, UNTRUSTED screen context and, for a form_submit, the submitted
        // values.
        let PendingMessage {
            content,
            context: raw_context,
            submission: raw_submission,
            attachments: raw_attachments,
            client_ref,
        } = match pending.take() {
            Some(p) => p,
            None => {
                let mut heartbeat = tokio::time::interval(HEARTBEAT);
                heartbeat.reset();
                loop {
                    // Race the idle wait against shutdown so a parked socket
                    // closes promptly on a deploy instead of pinning graceful
                    // shutdown open, and against the keepalive so proxies don't
                    // reap a socket the user is merely reading behind.
                    let incoming = tokio::select! {
                        biased;
                        _ = shutdown.cancelled() => {
                            let _ = send_frame(
                                &mut sender,
                                &ServerFrame::new("reconnect", None, &conversation),
                            )
                            .await;
                            break 'socket;
                        }
                        // A parked socket learns of a suspension or reload at
                        // once, rather than on its next message.
                        _ = caller.tenant.cancelled() => {
                            let why = retirement(&caller);
                            tracing::info!(agent = %agent_id, conversation = %conversation, replica = %replica, reason = ?why, "edge ws closed: tenant retired");
                            close_retired(&mut sender, &why, &conversation).await;
                            break 'socket;
                        }
                        _ = heartbeat.tick() => {
                            if send_frame(&mut sender, &ServerFrame::new("ping", None, &conversation))
                                .await
                                .is_err()
                            {
                                tracing::info!(agent = %agent_id, conversation = %conversation, replica = %replica, mid_turn = false, cause = "keepalive ping failed; socket gone", "edge ws closed");
                                break 'socket;
                            }
                            continue;
                        }
                        msg = receiver.next() => msg,
                    };
                    match incoming {
                        Some(Ok(Message::Text(raw))) => {
                            let Ok(frame) = serde_json::from_str::<ClientFrame>(&raw) else {
                                continue;
                            };
                            if frame.kind == "token" {
                                apply_token_frame(&caller, frame.content, &mcp_token).await;
                                continue;
                            }
                            // A form_submit is a chat turn whose message is the widget-
                            // built human summary; its values ride alongside.
                            if frame.kind == "chat" || frame.kind == "form_submit" {
                                let c = frame.content.unwrap_or_default();
                                // A file dragged in with no typed message is a
                                // real turn — attachments alone carry it.
                                if !c.trim().is_empty() || !frame.attachments.is_empty() {
                                    break PendingMessage {
                                        content: c,
                                        context: frame.context,
                                        submission: frame.submission,
                                        attachments: frame.attachments,
                                        client_ref: frame.client_ref,
                                    };
                                }
                            }
                            continue;
                        }
                        other @ (Some(Ok(Message::Close(_))) | Some(Err(_)) | None) => {
                            tracing::info!(agent = %agent_id, conversation = %conversation, replica = %replica, mid_turn = false, cause = %close_cause(&other), "edge ws closed");
                            break 'socket;
                        }
                        Some(Ok(_)) => continue, // Ping/Pong/Binary — ignore
                    }
                }
            }
        };

        // Fair use, per person rather than per address. Refused as a turn-level
        // error so the customer is told, rather than dropped at the socket where
        // it would look like the assistant had died.
        if !caller.tenant.user_limiter.check(caller.owner.clone()) {
            tracing::warn!(
                agent = %agent_id,
                conversation = %conversation,
                replica = %replica,
                "edge turn refused: per-user rate limit reached"
            );
            let _ = send_frame(
                &mut sender,
                &ServerFrame::new(
                    "error",
                    Some("You've sent a lot of messages in a short time — give it a moment and try again.".into()),
                    &conversation,
                ),
            )
            .await;
            continue;
        }
        // The plan's monthly caps, refused the same way: the customer reads
        // why, nothing is recorded, and the admin's banner names the cap.
        if let Err(limit) = caller.tenant.plan_usage.admit_turn(&caller.owner).await {
            tracing::warn!(
                agent = %agent_id,
                conversation = %conversation,
                replica = %replica,
                limit = limit.field(),
                "edge turn refused: plan limit reached"
            );
            let _ = send_frame(
                &mut sender,
                &ServerFrame::error(PlanLimit::CODE, limit.message().into(), &conversation),
            )
            .await;
            continue;
        }
        let req = TurnRequest {
            owner: caller.owner.clone(),
            conversation: conversation.clone(),
            message: content,
            mcp_token: mcp_token.clone(),
            identity: crate::analytics::TurnIdentity {
                display_name: caller.display_name.clone(),
                email: caller.email.clone(),
            },
            // Cap the untrusted, host-supplied context at the boundary (drops to
            // `None` when empty) before it is ever threaded into the turn.
            context: raw_context.and_then(|c| c.sanitized()),
            // Already capped on ingest (`lenient_submission`); threaded through
            // as-is — it is the customer's own trusted action.
            submission: raw_submission,
            // Ids only. Ownership is re-checked in the turn before any is read.
            attachments: raw_attachments,
            client_ref,
        };
        // The turn is owned by the conversation registry, not this socket:
        // `begin_turn` interrupts any turn already running for it (cross-socket
        // type-to-interrupt) and the receiver closing marks the turn's end. The
        // concurrency permit is acquired INSIDE, after the interrupt frees the
        // prior turn's — so an interrupt preempts even under permit saturation.
        let rx = caller.host.begin_turn(req, gate.clone()).await;
        match forward_events(
            &mut sender,
            &mut receiver,
            rx,
            &caller,
            &mcp_token,
            &conversation,
            &mut pending,
        )
        .await
        {
            ForwardEnd::Disconnected(cause) => {
                tracing::info!(agent = %agent_id, conversation = %conversation, replica = %replica, mid_turn = true, cause = %cause, "edge ws closed; turn continues detached");
                return;
            }
            // An interrupt loops straight back around: `pending` holds the new
            // message and `begin_turn` stops the old turn before starting it.
            ForwardEnd::Interrupted | ForwardEnd::TurnEnded => {}
        }
    }
}

/// Forward one turn's events to the socket until the turn ends, the customer
/// interrupts with a new message, or the socket dies. Also applies mid-turn
/// token refreshes and keeps the wire warm with `ping` frames (see
/// [`HEARTBEAT`]). Dropping the returned-from receiver is what detaches this
/// socket from the turn — never cancels it.
async fn forward_events(
    sender: &mut SplitSink<WebSocket, Message>,
    receiver: &mut SplitStream<WebSocket>,
    mut rx: mpsc::Receiver<CustomerEvent>,
    caller: &Caller,
    mcp_token: &TokenCell,
    conversation: &str,
    pending: &mut Option<PendingMessage>,
) -> ForwardEnd {
    let mut heartbeat = tokio::time::interval(HEARTBEAT);
    heartbeat.reset();
    // Whether this socket saw the turn out. A stream that ends without one of
    // these was cut from under us (displaced by another socket, or detached for
    // failing to drain) while the turn itself runs on — see the `None` arm.
    let mut saw_terminal = false;
    loop {
        tokio::select! {
            biased;
            incoming = receiver.next() => match incoming {
                other @ (None | Some(Err(_)) | Some(Ok(Message::Close(_)))) => {
                    return ForwardEnd::Disconnected(close_cause(&other))
                }
                Some(Ok(Message::Text(raw))) => {
                    let Ok(frame) = serde_json::from_str::<ClientFrame>(&raw) else {
                        continue;
                    };
                    // A token refresh can land mid-turn; apply it (else a tick
                    // inside a turn is lost and the token expires ~3 min later).
                    if frame.kind == "token" {
                        apply_token_frame(caller, frame.content, mcp_token).await;
                        continue;
                    }
                    // A form_submit mid-turn interrupts exactly like a chat.
                    if frame.kind == "chat" || frame.kind == "form_submit" {
                        let c = frame.content.unwrap_or_default();
                        // Same rule as the idle path: a file with no typed text
                        // is still a turn, and still interrupts.
                        if !c.trim().is_empty() || !frame.attachments.is_empty() {
                            *pending = Some(PendingMessage {
                                content: c,
                                context: frame.context,
                                submission: frame.submission,
                                attachments: frame.attachments,
                                client_ref: frame.client_ref,
                            });
                            return ForwardEnd::Interrupted;
                        }
                    }
                    continue;
                }
                Some(Ok(_)) => continue, // Binary/Ping/Pong mid-turn — ignore
            },
            event = rx.recv() => match event {
                Some(event) => {
                    saw_terminal |= matches!(
                        event,
                        CustomerEvent::Done { .. } | CustomerEvent::Error { .. }
                    );
                    if send_frame(sender, &customer_frame(&event, conversation))
                        .await
                        .is_err()
                    {
                        return ForwardEnd::Disconnected("send failed; socket gone".to_string());
                    }
                }
                None if saw_terminal => return ForwardEnd::TurnEnded,
                // The turn is still running, but this socket is no longer its
                // subscriber. Returning `TurnEnded` here would park the widget on
                // the typing dots forever: its socket stays open, so it never
                // detects a drop, and no further frame is coming. Close instead —
                // the widget reconnects, re-attaches to the live turn, and falls
                // back to the transcript if it has already finished.
                None => {
                    tracing::info!(
                        conversation = %conversation,
                        "edge socket detached from a live turn; closing so the widget re-attaches"
                    );
                    return ForwardEnd::Disconnected(
                        "detached from a live turn by a newer socket".to_string(),
                    );
                }
            },
            _ = heartbeat.tick() => {
                if send_frame(sender, &ServerFrame::new("ping", None, conversation))
                    .await
                    .is_err()
                {
                    return ForwardEnd::Disconnected("keepalive ping failed; socket gone".to_string());
                }
            }
        }
    }
}

/// Why the caller's runtime was retired, once its token has fired. A runtime
/// cancelled with no reason recorded is one being dropped; a reload is what
/// the widget should assume, since reconnecting finds whatever serves now.
fn retirement(caller: &Caller) -> Retirement {
    caller
        .tenant
        .retirement()
        .cloned()
        .unwrap_or(Retirement::Reloaded)
}

/// Apply a `token` frame's payload to the socket's live MCP token cell: ignore an
/// empty one, then store it only if it validates as a fresh same-owner token so
/// in-flight and future turns pick it up on their next MCP request.
async fn apply_token_frame(caller: &Caller, content: Option<String>, cell: &TokenCell) {
    let Some(token) = content.filter(|t| !t.trim().is_empty()) else {
        return;
    };
    let current = cell.get();
    if let Some(valid) = accept_refreshed_token(caller, token, &current).await {
        cell.set(valid);
    }
}

/// Validate a candidate refreshed token from the widget and return it ONLY if it
/// verifies (signature/issuer/audience/expiry), passes the email allowlist, AND
/// resolves to the SAME owner as the socket — so a refresh can only ever rotate
/// this user's own token, never swap identity. `current` short-circuits the
/// crypto when the token is unchanged (the widget dedups, but this also bounds
/// repeated verifies). Rejections are logged (never the raw JWT).
async fn accept_refreshed_token(
    caller: &Caller,
    new_token: String,
    current: &str,
) -> Option<String> {
    if new_token == current {
        return None;
    }
    let embed = &caller.host.agent().embed;
    match crate::keycloak::validator_for_policy(
        &embed.keycloak_issuer,
        embed.keycloak_audience.as_deref(),
        caller.tenant.outbound(),
    )
    .verify(&new_token)
    .await
    {
        Ok(claims)
            if embed.email_allowed(claims.email.as_deref(), claims.email_verified)
                && external_owner(&embed.keycloak_issuer, &claims.sub) == caller.owner =>
        {
            Some(new_token)
        }
        Ok(_) => {
            tracing::warn!(owner = %caller.owner, "edge token refresh rejected: owner/allowlist mismatch");
            None
        }
        Err(_) => {
            tracing::debug!(owner = %caller.owner, "edge token refresh rejected: token did not verify");
            None
        }
    }
}

/// Map a [`CustomerEvent`] to a wire frame — text-only surface.
fn customer_frame(event: &CustomerEvent, session_id: &str) -> ServerFrame {
    match event {
        CustomerEvent::Typing => ServerFrame::new("typing", None, session_id),
        CustomerEvent::Delta { text } => ServerFrame::new("token", Some(text.clone()), session_id),
        CustomerEvent::Done { text, message_id } => {
            ServerFrame::new("done", Some(text.clone()), session_id)
                .with_message_id(message_id.clone())
        }
        CustomerEvent::Error { message } => {
            ServerFrame::new("error", Some(message.clone()), session_id)
        }
        CustomerEvent::Attachment { file } => ServerFrame::attachment(file.clone(), session_id),
        CustomerEvent::Progress { label } => {
            ServerFrame::new("progress", Some(label.clone()), session_id)
        }
        CustomerEvent::Resume { text } => {
            ServerFrame::new("resume", Some(text.clone()), session_id)
        }
        CustomerEvent::Rewind { text } => {
            ServerFrame::new("rewind", Some(text.clone()), session_id)
        }
    }
}

async fn send_frame(
    sender: &mut SplitSink<WebSocket, Message>,
    frame: &ServerFrame,
) -> Result<(), axum::Error> {
    let text = serde_json::to_string(frame).unwrap_or_default();
    sender.send(Message::Text(text.into())).await
}

/// The channel a conversation's session lives under. The owner is bound
/// separately by every store call, so this key alone never grants access.
fn conversation_channel(agent_id: &str, conversation: &str) -> String {
    format!("embed:{agent_id}:{conversation}")
}

/// The conversation id inside a session channel, when the channel belongs to
/// `agent_id`'s embed namespace.
fn conversation_id<'a>(channel: &'a str, agent_id: &str) -> Option<&'a str> {
    channel
        .strip_prefix("embed:")?
        .strip_prefix(agent_id)?
        .strip_prefix(':')
        .filter(|c| !c.is_empty())
}

/// One row of the owner's conversation list.
#[derive(Debug, serde::Serialize)]
struct ConversationSummary {
    conversation: String,
    title: String,
    updated_at: String,
    messages: usize,
}

/// One transcript message — customer-visible roles only.
#[derive(Debug, Clone, serde::Serialize)]
struct ConversationMessage {
    role: &'static str,
    content: String,
    at: String,
    /// The persisted message id of an assistant row, so a restored transcript
    /// can still rate the answer (the live path gets it from the socket's
    /// `done` frame). Absent on user rows.
    #[serde(skip_serializing_if = "Option::is_none")]
    message_id: Option<String>,
    /// Files the assistant produced on this turn, so a reloaded transcript can
    /// re-render their download buttons. Absent for turns with no attachment.
    #[serde(skip_serializing_if = "Option::is_none")]
    attachments: Option<Vec<crate::wire::AttachmentRef>>,
    /// The client-minted reference of the turn this answer belongs to, so a
    /// widget recovering a dropped turn matches its own answer exactly rather
    /// than by position. Absent on user rows and on answers saved before this.
    #[serde(skip_serializing_if = "Option::is_none")]
    reply_to: Option<String>,
}

/// A page of a conversation's transcript, newest-anchored. `start` is the index
/// of the first returned message within the full (filtered) transcript, so the
/// client asks for older messages with `?before=start`; `has_more` is true while
/// earlier messages remain. Index-from-start is stable under append.
#[derive(Debug, serde::Serialize)]
struct ConversationPage {
    messages: Vec<ConversationMessage>,
    start: usize,
    has_more: bool,
}

/// Transcript paging: `limit` messages ending just before index `before`
/// (default: the newest page).
#[derive(Debug, serde::Deserialize)]
struct TranscriptQuery {
    limit: Option<usize>,
    before: Option<usize>,
}

/// Default / maximum transcript page sizes.
const TRANSCRIPT_PAGE: usize = 30;
const TRANSCRIPT_PAGE_MAX: usize = 100;

/// The human title for a conversation. The store pre-fills a
/// `{channel}:{owner}` placeholder label on create; treat that (and empty) as
/// untitled so no internal id surfaces — a conversation is titled from its first
/// message on send.
fn display_title(session: &workforce_common::session::Session) -> String {
    let placeholder = format!("{}:{}", session.channel, session.owner);
    match session.label.as_deref() {
        Some(l) if !l.is_empty() && l != placeholder => l.to_string(),
        _ => "New chat".to_string(),
    }
}

fn summarize(
    session: &workforce_common::session::Session,
    agent_id: &str,
) -> Option<ConversationSummary> {
    let conversation = conversation_id(&session.channel, agent_id)?;
    Some(ConversationSummary {
        conversation: conversation.to_string(),
        title: display_title(session),
        updated_at: session.updated_at.to_rfc3339(),
        messages: session.message_count(),
    })
}

fn transcript(session: &workforce_common::session::Session) -> Vec<ConversationMessage> {
    session
        .messages
        .iter()
        .filter_map(|m| {
            let role = match m.role {
                MessageRole::User => "user",
                MessageRole::Assistant => "assistant",
                // Internal turns never reach the customer surface.
                MessageRole::System | MessageRole::Tool => return None,
            };
            // Rehydrate any download references the turn stashed on its assistant
            // message (namespaced key, written by the edge; see `persist_attachments`).
            let attachments = m
                .tool_data
                .as_ref()
                .and_then(|td| td.get("edge_attachments"))
                .and_then(|v| {
                    serde_json::from_value::<Vec<crate::wire::AttachmentRef>>(v.clone()).ok()
                })
                .filter(|files| !files.is_empty());
            // The turn reference this answer belongs to, stamped alongside the
            // attachments — what lets a recovering widget claim its OWN answer.
            let reply_to = m
                .tool_data
                .as_ref()
                .and_then(|td| td.get("edge_reply_to"))
                .and_then(|v| v.as_str())
                .map(str::to_string);
            Some(ConversationMessage {
                role,
                content: m.content.clone(),
                at: m.timestamp.to_rfc3339(),
                message_id: (role == "assistant").then(|| m.id.clone()),
                attachments,
                reply_to,
            })
        })
        .collect()
}

/// Cap on the conversation list.
const MAX_CONVERSATIONS_LISTED: usize = 50;

/// `GET /public/conversations/{agent}` — the caller's own conversations,
/// newest first. Same gate chain as the socket (rate → agent → Origin →
/// token → email allowlist); the store query is bound to the verified owner,
/// so no id enumeration can cross customers.
async fn list_conversations(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path(agent_id): Path<String>,
    headers: HeaderMap,
    State(state): State<Arc<EdgeState>>,
    ServedTenant(tenant): ServedTenant,
) -> Result<impl IntoResponse, StatusCode> {
    let caller = authenticate(
        &tenant,
        state.trusted_proxies(),
        &agent_id,
        &headers,
        None,
        Some(peer),
    )
    .await?;
    let sessions = caller
        .host
        .sessions()
        .list_for_owner(&caller.owner, MAX_CONVERSATIONS_LISTED)
        .map_err(|e| {
            tracing::warn!(agent = %agent_id, error = %e, "conversation list failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    let list: Vec<ConversationSummary> = sessions
        .iter()
        .filter_map(|s| summarize(s, &agent_id))
        .collect();
    // Per-user and mutates as the customer chats — never let a browser/proxy
    // cache it (a stale list hides just-created conversations).
    Ok(([(header::CACHE_CONTROL, "no-store")], axum::Json(list)))
}

/// `GET /public/files/{agent}/{id}` — download one of this customer's files:
/// either something the agent generated for them (an `export_xlsx` spreadsheet)
/// or one of their own uploads, replayed into a restored transcript.
/// Authenticated and owner-scoped: only the caller the artifact belongs to may
/// fetch it. Fails closed — an unconfigured store, an unknown id, an owner
/// mismatch, or a missing owner is a 404, so no id enumeration can cross
/// customers.
async fn download_file(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path((agent_id, artifact_id)): Path<(String, String)>,
    headers: HeaderMap,
    State(state): State<Arc<EdgeState>>,
    ServedTenant(tenant): ServedTenant,
) -> Result<Response, StatusCode> {
    let caller = authenticate(
        &tenant,
        state.trusted_proxies(),
        &agent_id,
        &headers,
        None,
        Some(peer),
    )
    .await?;
    let artifacts = caller.host.artifacts().ok_or(StatusCode::NOT_FOUND)?;
    let id = ArtifactId::parse(&artifact_id).map_err(|_| StatusCode::NOT_FOUND)?;
    serve_owned_artifact(artifacts.store.as_ref(), &agent_id, &id, &caller.owner)
}

/// Serve an artifact's bytes to its owner as an attachment download, or a
/// `StatusCode` that fails closed. Split from the handler so the owner-scoping
/// and serving is testable without the auth stack. Only the caller whose
/// `owner` produced the file is served; any mismatch (including an artifact with
/// no recorded owner) is a 404.
fn serve_owned_artifact(
    store: &dyn ArtifactStore,
    agent_id: &str,
    id: &ArtifactId,
    owner: &str,
) -> Result<Response, StatusCode> {
    let descriptor = store
        .metadata(agent_id, id)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    if descriptor.user_id.as_deref() != Some(owner) {
        return Err(StatusCode::NOT_FOUND);
    }
    let bytes = store
        .read(agent_id, id)
        .map_err(|_| StatusCode::NOT_FOUND)?
        .into_bytes()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let content_type = descriptor
        .media_type
        .as_deref()
        .unwrap_or_else(|| descriptor.mime.as_str());
    let content_type =
        HeaderValue::from_str(content_type).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let disposition = descriptor
        .filename
        .as_deref()
        .map(content_disposition_attachment)
        .map(|d| HeaderValue::from_str(&d))
        .transpose()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let mut response = bytes.into_response();
    let out = response.headers_mut();
    out.insert(header::CONTENT_TYPE, content_type);
    if let Some(disposition) = disposition {
        out.insert(header::CONTENT_DISPOSITION, disposition);
    }
    out.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

/// Largest staged table the widget will page over; beyond this the model should
/// deliver a spreadsheet export instead of an in-widget browse.
const MAX_TABLE_ROWS: usize = 50_000;
/// Page-size ceiling and default for a table page request.
const MAX_TABLE_PAGE: usize = 200;
const DEFAULT_TABLE_PAGE: usize = 25;

/// Query for one page of a staged table. `filters` is a URL-encoded JSON array
/// `[{"col","op","value"}]` with `op ∈ contains|gte|lte`.
#[derive(Debug, Default, Deserialize)]
struct TablePageQuery {
    #[serde(default)]
    offset: usize,
    limit: Option<usize>,
    sort: Option<String>,
    dir: Option<String>,
    q: Option<String>,
    filters: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawFilter {
    col: String,
    op: String,
    value: Value,
}

/// `GET /public/tables/{agent}/{id}` — one filtered/sorted page of a staged
/// tabular artifact, so the widget browses a large result server-side instead of
/// inlining it. Authenticated and owner-scoped exactly like [`download_file`].
async fn table_page(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path((agent_id, artifact_id)): Path<(String, String)>,
    Query(params): Query<TablePageQuery>,
    headers: HeaderMap,
    State(state): State<Arc<EdgeState>>,
    ServedTenant(tenant): ServedTenant,
) -> Result<Response, StatusCode> {
    let caller = authenticate(
        &tenant,
        state.trusted_proxies(),
        &agent_id,
        &headers,
        None,
        Some(peer),
    )
    .await?;
    let artifacts = caller.host.artifacts().ok_or(StatusCode::NOT_FOUND)?;
    let id = ArtifactId::parse(&artifact_id).map_err(|_| StatusCode::NOT_FOUND)?;
    serve_table_page(
        artifacts.store.as_ref(),
        &agent_id,
        &id,
        &caller.owner,
        &params,
    )
}

/// Resolve a staged tabular artifact for its owner and answer one page, or a
/// `StatusCode` that fails closed. Split from the handler so owner-scoping and
/// querying are testable without the auth stack. Fails closed to 404 for an owner
/// mismatch, a missing owner, a non-tabular artifact, or a non-array body; 413
/// when the table is too large to browse.
fn serve_table_page(
    store: &dyn ArtifactStore,
    agent_id: &str,
    id: &ArtifactId,
    owner: &str,
    params: &TablePageQuery,
) -> Result<Response, StatusCode> {
    let descriptor = store
        .metadata(agent_id, id)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    if descriptor.user_id.as_deref() != Some(owner) {
        return Err(StatusCode::NOT_FOUND);
    }
    let Some(schema) = descriptor
        .schema
        .filter(|_| descriptor.mime == ContentType::ApplicationJson)
    else {
        return Err(StatusCode::NOT_FOUND);
    };
    if descriptor.row_count.is_some_and(|n| n > MAX_TABLE_ROWS) {
        return Err(StatusCode::PAYLOAD_TOO_LARGE);
    }
    let rows = match store
        .read(agent_id, id)
        .map_err(|_| StatusCode::NOT_FOUND)?
    {
        ArtifactBody::Json(Value::Array(rows)) => rows,
        _ => return Err(StatusCode::NOT_FOUND),
    };
    if rows.len() > MAX_TABLE_ROWS {
        return Err(StatusCode::PAYLOAD_TOO_LARGE);
    }

    let types = crate::table_query::infer_types(&schema, &rows);
    let query = resolve_table_query(&schema, &types, params);
    let page = crate::table_query::query(&schema, &types, &rows, &query);
    let columns: Vec<Value> = schema
        .iter()
        .zip(&types)
        .map(|(key, ty)| serde_json::json!({ "key": key, "label": key, "type": ty.as_str() }))
        .collect();

    let body = axum::Json(serde_json::json!({
        "columns": columns,
        "rows": page.rows,
        "total": page.total,
        "offset": query.offset,
        "limit": query.limit,
    }));
    let mut response = body.into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

/// Validate a raw page query against the artifact's schema/types: clamp the page,
/// resolve the sort + filter columns by name (unknown ones dropped), and parse
/// filter bounds per column type. A malformed `filters` param yields no filters.
fn resolve_table_query(
    schema: &[String],
    types: &[crate::table_query::ColumnType],
    params: &TablePageQuery,
) -> crate::table_query::TableQuery {
    use crate::table_query::{ColumnType, Filter};
    let column = |name: &str| schema.iter().position(|c| c == name);
    let limit = params
        .limit
        .unwrap_or(DEFAULT_TABLE_PAGE)
        .clamp(1, MAX_TABLE_PAGE);
    let sort = params
        .sort
        .as_deref()
        .and_then(column)
        .map(|col| (col, params.dir.as_deref() == Some("desc")));
    let search = params
        .q
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase);
    let filters = params
        .filters
        .as_deref()
        .and_then(|raw| serde_json::from_str::<Vec<RawFilter>>(raw).ok())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|f| {
            let col = column(&f.col)?;
            match f.op.as_str() {
                "contains" if types[col] == ColumnType::Text => {
                    let needle = filter_text(&f.value)?.to_lowercase();
                    (!needle.is_empty()).then_some(Filter::Contains { col, needle })
                }
                "gte" if types[col] != ColumnType::Text => Some(Filter::Min {
                    col,
                    bound: filter_bound(&f.value, types[col])?,
                }),
                "lte" if types[col] != ColumnType::Text => Some(Filter::Max {
                    col,
                    bound: filter_bound(&f.value, types[col])?,
                }),
                _ => None,
            }
        })
        .collect();
    crate::table_query::TableQuery {
        offset: params.offset,
        limit,
        sort,
        search,
        filters,
    }
}

/// A filter value as text (JSON string or number).
fn filter_text(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// A numeric filter bound under the column type: a number, a numeric string, or
/// an ISO date string (as epoch millis).
fn filter_bound(value: &Value, ty: crate::table_query::ColumnType) -> Option<f64> {
    use crate::table_query::ColumnType;
    match ty {
        ColumnType::Number => match value {
            Value::Number(n) => n.as_f64(),
            Value::String(s) => s.trim().parse::<f64>().ok().filter(|f| f.is_finite()),
            _ => None,
        },
        // The widget sends date bounds as epoch millis; an ISO string also works.
        ColumnType::Date => match value {
            Value::Number(n) => n.as_f64(),
            Value::String(s) => crate::table_query::parse_date_millis(s.trim()),
            _ => None,
        },
        ColumnType::Text => None,
    }
}

/// `GET /public/conversations/{agent}/{conversation}` — one conversation's
/// transcript. Resolved via `find(channel, owner)`, so a guessed id only ever
/// reaches the caller's own session; anything else is a plain 404.
async fn get_conversation(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path((agent_id, conversation)): Path<(String, String)>,
    Query(page): Query<TranscriptQuery>,
    headers: HeaderMap,
    State(state): State<Arc<EdgeState>>,
    ServedTenant(tenant): ServedTenant,
) -> Result<impl IntoResponse, StatusCode> {
    let caller = authenticate(
        &tenant,
        state.trusted_proxies(),
        &agent_id,
        &headers,
        None,
        Some(peer),
    )
    .await?;
    let channel = conversation_channel(&agent_id, &conversation);
    let sessions = caller.host.sessions();
    // `find` is owner-bound but returns a *shell* (no messages); re-load the
    // resolved id in full so the transcript actually contains the turns. Both
    // steps 404 on a miss, so a guessed id never crosses to another customer.
    let shell = sessions
        .find(&channel, &caller.owner)
        .map_err(|e| {
            tracing::warn!(agent = %agent_id, error = %e, "conversation read failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::NOT_FOUND)?;
    let session = sessions
        .get(&shell.id)
        .map_err(|e| {
            tracing::warn!(agent = %agent_id, error = %e, "conversation load failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::NOT_FOUND)?;

    // Newest-anchored window over the filtered transcript. `before` defaults to
    // the end (latest page); older pages walk backwards from there.
    let all = transcript(&session);
    let limit = page
        .limit
        .unwrap_or(TRANSCRIPT_PAGE)
        .clamp(1, TRANSCRIPT_PAGE_MAX);
    let end = page.before.unwrap_or(all.len()).min(all.len());
    let start = end.saturating_sub(limit);
    Ok((
        [(header::CACHE_CONTROL, "no-store")],
        axum::Json(ConversationPage {
            messages: all[start..end].to_vec(),
            start,
            has_more: start > 0,
        }),
    ))
}

/// `DELETE /public/conversations/{agent}/{conversation}` — remove one of the
/// caller's own conversations. Owner-bound resolve first, so deleting someone
/// else's id is indistinguishable from a missing one (404).
async fn delete_conversation(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path((agent_id, conversation)): Path<(String, String)>,
    headers: HeaderMap,
    State(state): State<Arc<EdgeState>>,
    ServedTenant(tenant): ServedTenant,
) -> Result<StatusCode, StatusCode> {
    let caller = authenticate(
        &tenant,
        state.trusted_proxies(),
        &agent_id,
        &headers,
        None,
        Some(peer),
    )
    .await?;
    let channel = conversation_channel(&agent_id, &conversation);
    let session = caller
        .host
        .sessions()
        .find(&channel, &caller.owner)
        .map_err(|e| {
            tracing::warn!(agent = %agent_id, error = %e, "conversation resolve failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::NOT_FOUND)?;
    caller.host.sessions().delete(&session.id).map_err(|e| {
        tracing::warn!(agent = %agent_id, error = %e, "conversation delete failed");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(StatusCode::NO_CONTENT)
}

/// The maximum length of a feedback comment; longer is rejected (400) rather
/// than silently truncated. A backstop for the widget's own client-side cap.
const FEEDBACK_COMMENT_MAX: usize = 2000;
/// A sane upper bound on the rated `message_id` (a UUID today), so a hostile
/// body can't push an unbounded value into the row.
const FEEDBACK_MESSAGE_ID_MAX: usize = 256;

/// A customer's rating of one assistant answer. `rating` is `+1`/`-1`; `comment`
/// is optional free text (capped). `message_id` is the id the `done` frame
/// surfaced for the rated answer.
#[derive(Debug, Deserialize)]
struct FeedbackRequest {
    message_id: String,
    rating: i16,
    #[serde(default)]
    comment: Option<String>,
}

/// `POST /public/conversations/{agent}/{conversation}/feedback` — the caller
/// rates one assistant answer (thumbs ±1 + optional comment). Owner-scoped
/// exactly like `delete_conversation`: `find(channel, owner)` is owner-bound, so
/// rating a conversation the caller doesn't own is indistinguishable from a
/// missing one (404) — a customer can never rate another customer's messages.
/// The upsert writes the verified `owner` (token-derived, never client-supplied),
/// so the `(owner, message_id)` row is always partitioned to this caller.
async fn submit_feedback(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path((agent_id, conversation)): Path<(String, String)>,
    headers: HeaderMap,
    State(state): State<Arc<EdgeState>>,
    ServedTenant(tenant): ServedTenant,
    axum::Json(body): axum::Json<FeedbackRequest>,
) -> Result<StatusCode, StatusCode> {
    let caller = authenticate(
        &tenant,
        state.trusted_proxies(),
        &agent_id,
        &headers,
        None,
        Some(peer),
    )
    .await?;
    // Validate the body first — reject junk (400) before touching the DB.
    // rating is exactly ±1; message_id is non-empty and bounded; comment is
    // trimmed, dropped when empty, and rejected when over the cap.
    if body.rating != 1 && body.rating != -1 {
        return Err(StatusCode::BAD_REQUEST);
    }
    let message_id = body.message_id.trim();
    if message_id.is_empty() || message_id.len() > FEEDBACK_MESSAGE_ID_MAX {
        return Err(StatusCode::BAD_REQUEST);
    }
    let comment = match body.comment.as_deref().map(str::trim) {
        Some(c) if c.chars().count() > FEEDBACK_COMMENT_MAX => return Err(StatusCode::BAD_REQUEST),
        Some(c) if !c.is_empty() => Some(c),
        _ => None,
    };

    // Owner-scope the write: resolve the conversation bound to the caller's own
    // owner. A miss (someone else's or a bogus id) is a plain 404.
    let channel = conversation_channel(&agent_id, &conversation);
    caller
        .host
        .sessions()
        .find(&channel, &caller.owner)
        .map_err(|e| {
            tracing::warn!(agent = %agent_id, error = %e, "feedback conversation resolve failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::NOT_FOUND)?;

    caller
        .tenant
        .analytics()
        .record_feedback(
            &agent_id,
            &caller.owner,
            &conversation,
            message_id,
            body.rating,
            comment,
        )
        .await
        .map_err(|e| {
            tracing::warn!(agent = %agent_id, error = %e, "recording feedback failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    // Improvement-loop signal: a negative rating is a candidate quality gap for
    // the lessons/curator pipeline. Emit a structured event (not a new
    // subsystem) so that pipeline can pick it up.
    if body.rating < 0 {
        tracing::info!(
            event = "edge_negative_feedback",
            agent = %agent_id,
            conversation = %conversation,
            owner = %caller.owner,
            message_id = %message_id,
            has_comment = comment.is_some(),
            "customer rated an assistant answer down"
        );
    }
    Ok(StatusCode::NO_CONTENT)
}

/// The verified operator on a guarded request (put in the request extension by
/// `admin_guard`), for audit attribution.
#[derive(Clone)]
struct Operator(String);

/// Mint the session cookie for a signed-in `operator` and 200.
fn login_success(auth: &crate::admin::AdminAuth, operator: &str) -> Response {
    match auth.issue_session(operator) {
        Ok(token) => (
            [(header::SET_COOKIE, auth.session_cookie(&token))],
            axum::Json(serde_json::json!({"ok": true, "operator": operator})),
        )
            .into_response(),
        Err(e) => {
            tracing::error!(error = %e, "admin session issue failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// `GET /admin/api/login/config` — which sign-in methods the login page should
/// offer (public; carries no secret). 404 when admin is disabled.
async fn admin_login_config(ServedTenant(tenant): ServedTenant) -> Response {
    let Some(auth) = tenant.admin.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let oidc_mode = auth.oidc_mode().map(|m| match m {
        crate::admin::OidcMode::Google => "google",
        crate::admin::OidcMode::Redirect => "redirect",
    });
    axum::Json(serde_json::json!({
        "password": auth.has_password(),
        "oidc_mode": oidc_mode,
        // Only used to render the Google button (google mode).
        "google_client_id": auth.oidc_client_id().filter(|_| oidc_mode == Some("google")),
    }))
    .into_response()
}

/// `POST /admin/api/login` — break-glass password login. Rate-limited by IP
/// before the argon2 verify; wrong password answers 401 with no detail.
async fn admin_login(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(state): State<Arc<EdgeState>>,
    ServedTenant(tenant): ServedTenant,
    headers: HeaderMap,
    body: axum::Json<serde_json::Value>,
) -> Response {
    if tenant.admin.is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    let ip = client_ip(state.trusted_proxies(), &headers, Some(peer.ip()));
    if !tenant.login_limiter.check(ip) {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    }
    let password = body
        .get("password")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    // argon2 verification is CPU + ~19 MiB per attempt: run it off the async
    // worker so concurrent logins can't stall customer chat turns.
    let auth_tenant = Arc::clone(&tenant);
    let verified = tokio::task::spawn_blocking(move || {
        auth_tenant
            .admin
            .as_ref()
            .is_some_and(|a| a.verify_password(&password))
    })
    .await
    .unwrap_or(false);
    if !verified {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    tenant
        .audit
        .record(
            crate::admin::BREAK_GLASS_OPERATOR,
            "login",
            "break-glass",
            "",
        )
        .await;
    login_success(
        tenant.admin.as_ref().expect("admin present"),
        crate::admin::BREAK_GLASS_OPERATOR,
    )
}

/// `POST /admin/api/login/oidc` — sign in with an OIDC token (Google id_token).
/// Verify the token, require a verified email that is on the operator allowlist.
async fn admin_login_oidc(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(state): State<Arc<EdgeState>>,
    ServedTenant(tenant): ServedTenant,
    headers: HeaderMap,
    body: axum::Json<serde_json::Value>,
) -> Response {
    let Some(auth) = tenant.admin.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let ip = client_ip(state.trusted_proxies(), &headers, Some(peer.ip()));
    if !tenant.login_limiter.check(ip) {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    }
    let token = body.get("token").and_then(|v| v.as_str()).unwrap_or("");
    let email = match auth.verify_oidc_email(token).await {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(error = %e, "admin OIDC verify failed");
            return StatusCode::UNAUTHORIZED.into_response();
        }
    };
    match tenant.operators.is_allowed(&email).await {
        Ok(true) => {
            tenant.audit.record(&email, "login", "oidc", "").await;
            login_success(auth, &email)
        }
        Ok(false) => {
            tracing::warn!(email = %email, "admin OIDC login for non-allowlisted operator");
            (
                StatusCode::FORBIDDEN,
                axum::Json(
                    serde_json::json!({"error": "your account is not on the operator allowlist"}),
                ),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "operator allowlist check failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// Name of the short-lived cookie carrying the Authorization Code flow's
/// state/nonce/PKCE between `/start` and `/callback`.
const OIDC_FLOW_COOKIE: &str = "wf_admin_oidc_flow";

/// The `redirect_uri` the IdP will call back — from configured `public_url`, else
/// derived from the forwarded host. Must EXACTLY match what the IdP registered.
fn oidc_redirect_uri(auth: &crate::admin::AdminAuth, headers: &HeaderMap) -> String {
    let base = auth.public_url().map(str::to_string).unwrap_or_else(|| {
        let proto = headers
            .get("x-forwarded-proto")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("https");
        let host = headers
            .get(header::HOST)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        format!("{proto}://{host}")
    });
    format!(
        "{}/admin/api/login/oidc/callback",
        base.trim_end_matches('/')
    )
}

/// Read one cookie's value from the `Cookie` header.
pub(crate) fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.to_string())
}

fn flow_cookie(token: &str) -> String {
    // SameSite=Lax (NOT Strict) so the cookie survives the top-level redirect
    // back from the IdP; scoped to the login path; short-lived.
    format!("{OIDC_FLOW_COOKIE}={token}; HttpOnly; Secure; SameSite=Lax; Path=/admin/api/login; Max-Age=600")
}
fn clear_flow_cookie() -> String {
    format!("{OIDC_FLOW_COOKIE}=; HttpOnly; Secure; SameSite=Lax; Path=/admin/api/login; Max-Age=0")
}

/// Bounce back to the SPA login page with a human error (also clears the flow
/// cookie). Used for every callback failure so nothing dead-ends on a blank page.
fn oidc_login_error(msg: &str) -> Response {
    let loc = format!(
        "/admin/?login_error={}",
        url::form_urlencoded::byte_serialize(msg.as_bytes()).collect::<String>()
    );
    (
        StatusCode::FOUND,
        axum::response::AppendHeaders([
            (header::SET_COOKIE, clear_flow_cookie()),
            (header::LOCATION, loc),
        ]),
    )
        .into_response()
}

/// `GET /admin/api/login/oidc/start` — begin the Authorization Code + PKCE flow:
/// mint state/nonce/PKCE, stash them in a signed short-lived cookie, and 302 to
/// the IdP's authorization endpoint. Redirect mode only.
async fn admin_login_oidc_start(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(state): State<Arc<EdgeState>>,
    ServedTenant(tenant): ServedTenant,
    headers: HeaderMap,
) -> Response {
    let Some(auth) = tenant.admin.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if auth.oidc_mode() != Some(crate::admin::OidcMode::Redirect) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let ip = client_ip(state.trusted_proxies(), &headers, Some(peer.ip()));
    if !tenant.login_limiter.check(ip) {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    }
    let (Some(issuer), Some(client_id)) = (auth.oidc_issuer(), auth.oidc_client_id()) else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    // The admin issuer is the tenant's too: reached under its policy.
    let endpoints = match crate::keycloak::discover_endpoints_with(issuer, tenant.outbound()).await
    {
        Ok(e) => e,
        Err(e) => {
            tracing::error!(error = %e, "OIDC discovery failed");
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };
    let pkce = crate::oidc::pkce();
    let st = crate::oidc::random_token();
    let nonce = crate::oidc::random_token();
    let redirect_uri = oidc_redirect_uri(auth, &headers);
    let flow = crate::oidc::FlowState {
        state: st.clone(),
        verifier: pkce.verifier,
        nonce: nonce.clone(),
        exp: (chrono::Utc::now().timestamp() + 600) as usize,
    };
    let cookie = match auth.issue_flow(&flow) {
        Ok(t) => flow_cookie(&t),
        Err(e) => {
            tracing::error!(error = %e, "signing OIDC flow state failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let url = match crate::oidc::authorize_url(
        &endpoints.authorization_endpoint,
        client_id,
        &redirect_uri,
        &st,
        &nonce,
        &pkce.challenge,
    ) {
        Ok(u) => u,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    (
        StatusCode::FOUND,
        axum::response::AppendHeaders([(header::SET_COOKIE, cookie), (header::LOCATION, url)]),
    )
        .into_response()
}

/// `GET /admin/api/login/oidc/callback?code&state` — finish the flow: match
/// state, exchange the code (PKCE), verify the id_token + nonce, allowlist, and
/// mint the session. Every failure bounces to the login page with a message.
async fn admin_login_oidc_callback(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(state): State<Arc<EdgeState>>,
    ServedTenant(tenant): ServedTenant,
    headers: HeaderMap,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let Some(auth) = tenant.admin.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if auth.oidc_mode() != Some(crate::admin::OidcMode::Redirect) {
        return StatusCode::NOT_FOUND.into_response();
    }
    // Recover + verify the signed flow state (state/nonce/PKCE verifier). This is
    // free (no network) and fails a cookie-less probe cheaply.
    let Some(flow) = cookie_value(&headers, OIDC_FLOW_COOKIE).and_then(|t| auth.verify_flow(&t))
    else {
        return oidc_login_error("Your sign-in link expired — please try again.");
    };
    // Only THEN spend rate-limit budget: a holder of a valid (signed) flow cookie
    // could otherwise replay the callback to drive unbounded outbound IdP calls
    // (discovery + code exchange) for the cookie's 10-minute life.
    let ip = client_ip(state.trusted_proxies(), &headers, Some(peer.ip()));
    if !tenant.login_limiter.check(ip) {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    }
    if params.contains_key("error") {
        tracing::warn!(error = ?params.get("error"), "IdP returned an error");
        return oidc_login_error("Sign-in was cancelled or failed at the identity provider.");
    }
    let (Some(code), Some(st)) = (params.get("code"), params.get("state")) else {
        return oidc_login_error("Malformed sign-in response.");
    };
    // CSRF: the returned state must match the one we signed into the cookie.
    if st != &flow.state {
        tracing::warn!("OIDC state mismatch");
        return oidc_login_error("Sign-in verification failed — please try again.");
    }
    let (Some(issuer), Some(client_id)) = (auth.oidc_issuer(), auth.oidc_client_id()) else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let redirect_uri = oidc_redirect_uri(auth, &headers);
    let endpoints = match crate::keycloak::discover_endpoints_with(issuer, tenant.outbound()).await
    {
        Ok(e) => e,
        Err(e) => {
            tracing::error!(error = %e, "OIDC discovery failed");
            return oidc_login_error("Couldn't reach the identity provider.");
        }
    };
    let id_token = match crate::oidc::exchange_code_with(
        tenant.outbound(),
        &endpoints.token_endpoint,
        client_id,
        auth.oidc_client_secret(),
        &redirect_uri,
        code,
        &flow.verifier,
    )
    .await
    {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(error = %e, "OIDC code exchange failed");
            return oidc_login_error("Sign-in could not be completed.");
        }
    };
    let verified = match auth.verify_oidc_claims(&id_token).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "OIDC id_token verification failed");
            return oidc_login_error("Sign-in token could not be verified.");
        }
    };
    // Replay defence: the id_token's nonce must match the one we generated.
    if verified.nonce.as_deref() != Some(flow.nonce.as_str()) {
        tracing::warn!("OIDC nonce mismatch");
        return oidc_login_error("Sign-in verification failed — please try again.");
    }
    match tenant.operators.is_allowed(&verified.email).await {
        Ok(true) => {
            tenant
                .audit
                .record(&verified.email, "login", "oidc", "")
                .await;
            let token = match auth.issue_session(&verified.email) {
                Ok(t) => t,
                Err(e) => {
                    tracing::error!(error = %e, "admin session issue failed");
                    return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                }
            };
            (
                StatusCode::FOUND,
                axum::response::AppendHeaders([
                    (header::SET_COOKIE, auth.session_cookie(&token)),
                    (header::SET_COOKIE, clear_flow_cookie()),
                    (header::LOCATION, "/admin/".to_string()),
                ]),
            )
                .into_response()
        }
        Ok(false) => {
            tracing::warn!(email = %verified.email, "admin OIDC login for non-allowlisted operator");
            oidc_login_error("Your account is not on the operator allowlist.")
        }
        Err(e) => {
            tracing::error!(error = %e, "operator allowlist check failed");
            oidc_login_error("Sign-in failed — please try again.")
        }
    }
}

/// `POST /admin/api/logout` — clear the session cookie.
async fn admin_logout(ServedTenant(tenant): ServedTenant) -> Response {
    let Some(auth) = tenant.admin.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    (
        [(header::SET_COOKIE, auth.clear_cookie())],
        axum::Json(serde_json::json!({"ok": true})),
    )
        .into_response()
}

/// Middleware fronting the guarded `/admin/api/*` subtree: default-deny, so a
/// new admin handler is protected by construction rather than by remembering to
/// call the guard. The tenant is resolved first (an unknown host is 404, a
/// suspended tenant 503), then its session checked: 404 when its admin is
/// disabled, 401 without a valid session for THIS tenant. The tenant and the
/// verified operator are placed in the request extensions for the handlers.
async fn admin_guard(
    State(state): State<Arc<EdgeState>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let (mut parts, body) = request.into_parts();
    let tenant = match ServedTenant::from_request_parts(&mut parts, &state).await {
        Ok(tenant) => tenant,
        Err(rejection) => return rejection.into_response(),
    };
    match crate::admin::require_admin(tenant.admin.as_ref(), &parts.headers) {
        Ok(operator) => {
            // A write must come from the tenant's own admin page: the cookie
            // is host-only, but every tenant's host is same-site with the
            // others' and the platform's, so a page on one could POST to
            // another with the cookie attached. See `csrf_allows`.
            let peer = parts
                .extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map(|c| c.0.ip());
            let host = request_host(state.trusted_proxies(), &parts.headers, peer);
            let own_origin = |origin: &str| {
                host.as_deref()
                    .is_some_and(|host| crate::admin::origin_is_host(origin, host))
            };
            if !crate::admin::csrf_allows(&parts.method, &parts.headers, own_origin) {
                return StatusCode::FORBIDDEN.into_response();
            }
            parts.extensions.insert(Operator(operator));
            parts.extensions.insert(tenant);
            next.run(axum::extract::Request::from_parts(parts, body))
                .await
        }
        Err(code) => code.into_response(),
    }
}

/// `GET /admin/api/me` — session probe for the SPA (reaches here only past the
/// guard, so a 200 IS the "signed in" answer). Returns the operator and the
/// tenant they are signed into.
async fn admin_me(
    ServedTenant(tenant): ServedTenant,
    operator: axum::Extension<Operator>,
) -> Response {
    // `git_managed` tells the UI to render agents/playbooks/training (branding
    // included) read-only — the config repo is the source of truth.
    // `llm_status` is the key banner: a tenant whose key is rejected learns
    // it here, not from a dead widget. `plan`, `usage` and `over` are the
    // plan banner and the usage card; a count that fails leaves `usage`
    // null and `over` empty, and says so in the log.
    let usage = match tenant.usage().await {
        Ok(usage) => Some(usage),
        Err(e) => {
            tracing::error!(tenant = %tenant.id, error = %e, "counting plan usage failed");
            None
        }
    };
    let plan = tenant.plan_usage.plan();
    let over: Vec<&str> = usage.as_ref().map(|u| plan.over(u)).unwrap_or_default();
    axum::Json(serde_json::json!({
        "operator": operator.0 .0,
        "git_managed": tenant.git_managed,
        "tenant": { "id": tenant.id, "name": tenant.name },
        "llm_status": tenant.llm_status.current(),
        "plan": plan,
        "usage": usage,
        "over": over,
    }))
    .into_response()
}

/// What the settings page shows: every section without its secrets, whether
/// each secret is set, and the key's state. `editable` is false without a
/// master key, when the file governs and the settings API only reads.
#[derive(Serialize)]
struct SettingsView<'a> {
    llm: &'a crate::config::LlmConfig,
    grader: &'a Option<crate::config::GraderConfig>,
    token_exchange: &'a Option<crate::config::TokenExchangeConfig>,
    admin: &'a Option<crate::admin::AdminConfig>,
    limits: crate::tenant::TenantLimits,
    audit: &'a Option<crate::config::AuditConfig>,
    outbound: OutboundPolicy,
    llm_status: LlmStatus,
    has_llm_key: bool,
    has_token_exchange_secret: bool,
    has_admin_client_secret: bool,
    /// Whether the tenant has a settings row; without one the values shown
    /// are the file's.
    stored: bool,
    editable: bool,
}

fn settings_error(status: StatusCode, message: impl Into<String>) -> Response {
    (
        status,
        axum::Json(serde_json::json!({"error": message.into()})),
    )
        .into_response()
}

/// The 409 a settings write gets on an install with no master key, whose
/// settings are the file's.
fn no_settings_store() -> Response {
    settings_error(
        StatusCode::CONFLICT,
        format!(
            "{} is not set: this install's settings are read from edge.toml",
            crate::boot::MASTER_KEY_ENV
        ),
    )
}

/// `GET /admin/api/settings` — the tenant's settings, secrets omitted.
async fn admin_settings(ServedTenant(tenant): ServedTenant) -> Response {
    let stored = match &tenant.settings_store {
        Some(store) => match store.load().await {
            Ok(stored) => stored,
            Err(e) => {
                tracing::error!(tenant = %tenant.id, error = %e, "loading settings failed");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        },
        None => None,
    };
    let is_stored = stored.is_some();
    let settings = stored.unwrap_or_else(|| StoredSettings::from_settings(&tenant.settings));
    axum::Json(SettingsView {
        llm: &settings.llm,
        grader: &settings.grader,
        token_exchange: &settings.token_exchange,
        admin: &settings.admin,
        limits: settings.limits,
        audit: &settings.audit,
        outbound: settings.outbound,
        llm_status: tenant.llm_status.current(),
        has_llm_key: settings.has_llm_key(),
        has_token_exchange_secret: settings.has_token_exchange_secret(),
        has_admin_client_secret: settings.has_admin_client_secret(),
        stored: is_stored,
        editable: tenant.settings_store.is_some(),
    })
    .into_response()
}

/// The non-secret sections a settings save carries: the shape `GET` returns,
/// without the derived fields and without `outbound`, which is the
/// platform's to set (`PATCH /platform/api/tenants/{id}`) and never the
/// tenant's own — a tenant that could loosen its policy to `private` could
/// reach the deployment's private addresses.
#[derive(Deserialize)]
struct SettingsUpdate {
    /// The `llm` section as a partial object: a field it carries replaces
    /// the stored one, a field it omits is kept, so a client editing the
    /// provider never resets the failover chain or the retry policy.
    llm: serde_json::Map<String, Value>,
    #[serde(default)]
    grader: Option<crate::config::GraderConfig>,
    #[serde(default)]
    token_exchange: Option<crate::config::TokenExchangeConfig>,
    #[serde(default)]
    admin: Option<crate::admin::AdminConfig>,
    limits: crate::tenant::TenantLimits,
    #[serde(default)]
    audit: Option<crate::config::AuditConfig>,
}

/// `previous` with the fields `update` names replaced: the merge is on the
/// JSON the row holds (the key is a sealed column, never in it), so an
/// absent field keeps its stored value and a `null` clears an optional one.
fn merge_llm(
    previous: &crate::config::LlmConfig,
    update: serde_json::Map<String, Value>,
) -> std::result::Result<crate::config::LlmConfig, serde_json::Error> {
    let mut merged = match serde_json::to_value(previous)? {
        Value::Object(fields) => fields,
        _ => unreachable!("a struct serialises to an object"),
    };
    merged.extend(update);
    serde_json::from_value(Value::Object(merged))
}

/// The first secret field a settings body names, if any. Secrets travel
/// through their own write-only endpoints; a body carrying one is refused
/// rather than quietly stripped, so a client never believes it stored a key.
/// The platform's tenant PATCH, which carries the admin section too, runs
/// the same check.
pub(crate) fn secret_in_settings_body(body: &Value) -> Option<&'static str> {
    const SECRETS: [(&[&str], &str); 6] = [
        (&["llm", "api_key"], "llm.api_key"),
        (&["grader", "api_key"], "grader.api_key"),
        (
            &["token_exchange", "client_secret"],
            "token_exchange.client_secret",
        ),
        (&["admin", "password_hash"], "admin.password_hash"),
        (&["admin", "session_secret"], "admin.session_secret"),
        (
            &["admin", "oidc", "client_secret"],
            "admin.oidc.client_secret",
        ),
    ];
    SECRETS.iter().find_map(|(path, name)| {
        path.iter()
            .try_fold(body, |node, key| node.get(key))
            .filter(|value| !value.is_null())
            .map(|_| *name)
    })
}

/// `PUT /admin/api/settings` — save the non-secret sections and rebuild the
/// tenant on them. The row is written first and the tenant rebuilt from it;
/// a rebuild that fails (an admin block with no sign-in method, a token
/// exchange with no secret) puts the previous row back and reports why, so
/// the running tenant and its next boot never see settings it cannot start on.
/// The limits are held under the plan's ceilings, and `outbound` is refused:
/// it is the platform's to set.
async fn admin_settings_put(
    State(state): State<Arc<EdgeState>>,
    ServedTenant(tenant): ServedTenant,
    operator: axum::Extension<Operator>,
    body: axum::Json<Value>,
) -> Response {
    let Some(store) = &tenant.settings_store else {
        return no_settings_store();
    };
    if let Some(field) = secret_in_settings_body(&body) {
        return settings_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("{field} is set through its own endpoint, never in a settings save"),
        );
    }
    if body.get("outbound").is_some_and(|v| !v.is_null()) {
        return settings_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "outbound is set by the platform, never in a settings save",
        );
    }
    let update: SettingsUpdate = match serde_json::from_value(body.0) {
        Ok(update) => update,
        Err(e) => return settings_error(StatusCode::UNPROCESSABLE_ENTITY, e.to_string()),
    };
    let previous = match store.load().await {
        Ok(previous) => previous,
        Err(e) => {
            tracing::error!(tenant = %tenant.id, error = %e, "loading settings failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    // What the save edits: the row, or — before the first save — the
    // settings the tenant runs on, which is what `GET` showed.
    let base = previous
        .clone()
        .unwrap_or_else(|| StoredSettings::from_settings(&tenant.settings));
    let mut llm = match merge_llm(&base.llm, update.llm) {
        Ok(llm) => llm,
        Err(e) => return settings_error(StatusCode::UNPROCESSABLE_ENTITY, e.to_string()),
    };
    llm.allowed_providers = state.config.llm.allowed_providers.clone();
    if ProviderKind::from_str_loose(&llm.provider).is_none() {
        return settings_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("unknown llm provider {:?}", llm.provider),
        );
    }
    if let Err(e) = llm.provider_allowed() {
        return settings_error(StatusCode::UNPROCESSABLE_ENTITY, e.to_string());
    }
    if update.limits.rate_limit_per_min == 0
        || update.limits.user_rate_limit_per_min == 0
        || update.limits.max_concurrent_turns == 0
    {
        return settings_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "limits must each be at least 1",
        );
    }
    let plan = tenant.plan_usage.plan();
    if let Some(field) = plan.limit_over_ceiling(&update.limits) {
        return settings_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("{field} is above this workspace's plan"),
        );
    }
    let next = StoredSettings {
        llm,
        llm_status: None,
        grader: update.grader,
        token_exchange: update.token_exchange,
        admin: update.admin,
        limits: update.limits,
        audit: update.audit,
        outbound: base.outbound,
    };
    if let Err(e) = store.save(&next).await {
        tracing::error!(tenant = %tenant.id, error = %e, "saving settings failed");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    if let Err(e) = state.reload(&tenant.id).await {
        let restored = match &previous {
            Some(previous) => store.save(previous).await,
            None => store.delete().await.map(|_| ()),
        };
        if let Err(restore) = restored {
            tracing::error!(tenant = %tenant.id, error = %restore, "restoring settings after a failed rebuild");
        }
        tracing::warn!(tenant = %tenant.id, error = %format!("{e:#}"), "settings rejected: the tenant would not rebuild on them");
        return settings_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("the tenant would not start on these settings: {e:#}"),
        );
    }
    tenant
        .audit
        .record(&operator.0 .0, "settings.update", "settings", "")
        .await;
    axum::Json(serde_json::json!({"ok": true})).into_response()
}

#[derive(Deserialize)]
struct KeyBody {
    key: String,
}

/// `PUT /admin/api/settings/llm-key` — validate a key with one probe against
/// the tenant's provider and model, store it sealed when it passes, and
/// rebuild the tenant's hosts on it. The outcome is recorded as the key's
/// state either way; a failing key is not stored, and the response carries
/// the failure's fixed message, never the provider's text.
async fn admin_llm_key_put(
    State(state): State<Arc<EdgeState>>,
    ServedTenant(tenant): ServedTenant,
    operator: axum::Extension<Operator>,
    body: axum::Json<KeyBody>,
) -> Response {
    let Some(store) = &tenant.settings_store else {
        return no_settings_store();
    };
    let key = body.key.trim();
    if key.is_empty() {
        return settings_error(StatusCode::UNPROCESSABLE_ENTITY, "key must not be empty");
    }
    if let Err(e) = store.seed_if_empty(&tenant.settings).await {
        tracing::error!(tenant = %tenant.id, error = %e, "creating the settings row failed");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let llm = match store.load().await {
        Ok(Some(stored)) => {
            let mut llm = stored.llm;
            llm.allowed_providers = state.config.llm.allowed_providers.clone();
            llm
        }
        Ok(None) => tenant.settings.llm.clone(),
        Err(e) => {
            tracing::error!(tenant = %tenant.id, error = %e, "loading settings failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    if let Err(e) = llm.provider_allowed() {
        return settings_error(StatusCode::UNPROCESSABLE_ENTITY, e.to_string());
    }
    if let Err(failure) = validate_llm_key(&llm, key).await {
        tracing::warn!(tenant = %tenant.id, class = ?failure, "llm key validation failed");
        let status = LlmStatus::failed(failure);
        tenant.llm_status.set(status.clone()).await;
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            axum::Json(serde_json::json!({
                "error": failure.message(),
                "llm_status": status,
            })),
        )
            .into_response();
    }
    if let Err(e) = store.set_llm_key(key).await {
        tracing::error!(tenant = %tenant.id, error = %e, "storing the llm key failed");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let status = LlmStatus::ok();
    tenant.llm_status.set(status.clone()).await;
    tenant
        .audit
        .record(&operator.0 .0, "settings.llm_key.set", "settings", "")
        .await;
    if let Err(e) = state.reload(&tenant.id).await {
        tracing::error!(tenant = %tenant.id, error = %format!("{e:#}"), "rebuilding the tenant on its new key failed");
        return settings_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("the key was stored, but the tenant failed to rebuild on it: {e:#}"),
        );
    }
    axum::Json(serde_json::json!({"ok": true, "llm_status": status})).into_response()
}

/// `DELETE /admin/api/settings/llm-key` — remove the stored key; the tenant
/// runs on the file's key if there is one, else on none.
async fn admin_llm_key_delete(
    State(state): State<Arc<EdgeState>>,
    ServedTenant(tenant): ServedTenant,
    operator: axum::Extension<Operator>,
) -> Response {
    let Some(store) = &tenant.settings_store else {
        return no_settings_store();
    };
    match store.load().await {
        Ok(Some(_)) => {}
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::error!(tenant = %tenant.id, error = %e, "loading settings failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    }
    if let Err(e) = store.clear_llm_key().await {
        tracing::error!(tenant = %tenant.id, error = %e, "clearing the llm key failed");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    tenant.llm_status.set(LlmStatus::unset()).await;
    tenant
        .audit
        .record(&operator.0 .0, "settings.llm_key.clear", "settings", "")
        .await;
    if let Err(e) = state.reload(&tenant.id).await {
        tracing::error!(tenant = %tenant.id, error = %format!("{e:#}"), "rebuilding the tenant without its key failed");
        return settings_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("the key was removed, but the tenant failed to rebuild: {e:#}"),
        );
    }
    axum::Json(serde_json::json!({"ok": true})).into_response()
}

#[derive(Deserialize)]
struct SecretBody {
    secret: String,
}

/// Which sealed secret a `/admin/api/settings/{secret}` route addresses.
/// The LLM key has its own routes, since it is validated before it is kept.
#[derive(Clone, Copy)]
enum StoredSecret {
    TokenExchange,
    AdminClient,
}

impl StoredSecret {
    fn from_path(name: &str) -> Option<Self> {
        match name {
            "token-exchange-secret" => Some(Self::TokenExchange),
            "admin-client-secret" => Some(Self::AdminClient),
            _ => None,
        }
    }

    fn action(self) -> &'static str {
        match self {
            Self::TokenExchange => "settings.token_exchange_secret",
            Self::AdminClient => "settings.admin_client_secret",
        }
    }
}

/// Store or clear one sealed secret and rebuild the tenant on it.
async fn set_stored_secret(
    state: &EdgeState,
    tenant: &TenantRuntime,
    operator: &str,
    which: StoredSecret,
    secret: Option<&str>,
) -> Response {
    let Some(store) = &tenant.settings_store else {
        return no_settings_store();
    };
    if let Err(e) = store.seed_if_empty(&tenant.settings).await {
        tracing::error!(tenant = %tenant.id, error = %e, "creating the settings row failed");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let written = match which {
        StoredSecret::TokenExchange => store.set_token_exchange_secret(secret).await,
        StoredSecret::AdminClient => store.set_admin_client_secret(secret).await,
    };
    if let Err(e) = written {
        tracing::error!(tenant = %tenant.id, error = %e, action = which.action(), "storing a secret failed");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let verb = if secret.is_some() { "set" } else { "clear" };
    tenant
        .audit
        .record(
            operator,
            &format!("{}.{verb}", which.action()),
            "settings",
            "",
        )
        .await;
    if let Err(e) = state.reload(&tenant.id).await {
        tracing::error!(tenant = %tenant.id, error = %format!("{e:#}"), "rebuilding the tenant on a changed secret failed");
        return settings_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("the secret was stored, but the tenant failed to rebuild on it: {e:#}"),
        );
    }
    axum::Json(serde_json::json!({"ok": true})).into_response()
}

/// `PUT /admin/api/settings/{secret}` — store a sealed secret (write-only).
async fn admin_secret_put(
    State(state): State<Arc<EdgeState>>,
    ServedTenant(tenant): ServedTenant,
    operator: axum::Extension<Operator>,
    Path(name): Path<String>,
    body: axum::Json<SecretBody>,
) -> Response {
    let Some(which) = StoredSecret::from_path(&name) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let secret = body.secret.trim();
    if secret.is_empty() {
        return settings_error(StatusCode::UNPROCESSABLE_ENTITY, "secret must not be empty");
    }
    set_stored_secret(&state, &tenant, &operator.0 .0, which, Some(secret)).await
}

/// `DELETE /admin/api/settings/{secret}` — clear a sealed secret.
async fn admin_secret_delete(
    State(state): State<Arc<EdgeState>>,
    ServedTenant(tenant): ServedTenant,
    operator: axum::Extension<Operator>,
    Path(name): Path<String>,
) -> Response {
    let Some(which) = StoredSecret::from_path(&name) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    set_stored_secret(&state, &tenant, &operator.0 .0, which, None).await
}

/// What the first-run wizard needs: which of its steps are already done.
/// Every flag is read from the live state — the key from the settings (the
/// row when there is one, else the file), the rest from the mounted agents
/// — so a step finished anywhere (the agent editor, the settings page, a
/// git import) reads as finished here, and the wizard resumes rather than
/// repeats.
#[derive(Serialize)]
struct SetupReport {
    /// A key is stored (or the file supplies one).
    llm_key: bool,
    llm_status: LlmStatus,
    /// Whether the settings API takes writes (a master key is set).
    editable: bool,
    /// Mounted agents, and the first by id — the one the wizard resumes on.
    agents: usize,
    agent: Option<String>,
    /// The plan's cap on `agents`; `None` when the plan leaves it unbounded.
    agent_cap: Option<u32>,
    /// Any agent names an MCP server / an allowed origin / an issuer.
    mcp: bool,
    origins: bool,
    oidc: bool,
    /// `/demo/{agent}` answers on this tenant, so the wizard can preview.
    demo: bool,
    /// Every step is done.
    complete: bool,
}

/// `GET /admin/api/setup` — the setup report. Guarded.
async fn admin_setup(ServedTenant(tenant): ServedTenant) -> Response {
    let stored = match &tenant.settings_store {
        Some(store) => match store.load().await {
            Ok(stored) => stored,
            Err(e) => {
                tracing::error!(tenant = %tenant.id, error = %e, "loading settings failed");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        },
        None => None,
    };
    let llm_key = stored
        .unwrap_or_else(|| StoredSettings::from_settings(&tenant.settings))
        .has_llm_key();
    let hosts = tenant.all_hosts();
    let agent = hosts
        .iter()
        .map(|h| h.agent().id.as_str())
        .min()
        .map(str::to_string);
    let any = |pred: fn(&EmbedSettings) -> bool| hosts.iter().any(|h| pred(&h.agent().embed));
    let mcp = any(|e| e.mcp.is_some());
    let origins = any(|e| !e.allowed_origins.is_empty());
    let oidc = any(|e| !e.keycloak_issuer.trim().is_empty());
    axum::Json(SetupReport {
        llm_key,
        llm_status: tenant.llm_status.current(),
        editable: tenant.settings_store.is_some(),
        agents: hosts.len(),
        agent,
        agent_cap: tenant.plan_usage.plan().agents,
        mcp,
        origins,
        oidc,
        demo: tenant.demo.enabled && tenant.demo.google_client_id.is_some(),
        complete: llm_key && !hosts.is_empty() && mcp && origins && oidc,
    })
    .into_response()
}

/// The MCP block as the wizard probes it: the URL and the bearer the agent
/// would keep as its `discovery_credential`. Blank or absent ⇒ anonymous.
#[derive(Deserialize)]
struct McpDiscoverBody {
    url: String,
    #[serde(default)]
    discovery_credential: Option<String>,
}

/// Manual `Debug` so a logged body can never print the credential.
impl std::fmt::Debug for McpDiscoverBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpDiscoverBody")
            .field("url", &self.url)
            .field(
                "discovery_credential",
                &self.discovery_credential.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

/// `POST /admin/api/mcp/discover` — connect to an MCP server, initialise
/// and list its tools, under the tenant's outbound policy and the same
/// deadlines a turn's discovery runs under. A read: nothing is stored, so
/// nothing is audited; the probe is logged with its target. 422 names a
/// URL the policy refuses, 502 a server that did not answer. Guarded.
async fn admin_mcp_discover(
    ServedTenant(tenant): ServedTenant,
    operator: axum::Extension<Operator>,
    body: axum::Json<McpDiscoverBody>,
) -> Response {
    let url = body.url.trim();
    if url.is_empty() {
        return settings_error(StatusCode::UNPROCESSABLE_ENTITY, "url must not be empty");
    }
    tracing::info!(tenant = %tenant.id, operator = %operator.0 .0, url, "surveying an MCP server");
    match crate::mcp::survey(url, body.discovery_credential.as_deref(), tenant.outbound()).await {
        Ok(survey) => axum::Json(serde_json::json!({"ok": true, "survey": survey})).into_response(),
        Err(crate::mcp::SurveyError::Url(reason)) => {
            settings_error(StatusCode::UNPROCESSABLE_ENTITY, reason)
        }
        Err(crate::mcp::SurveyError::Server(reason)) => {
            tracing::warn!(tenant = %tenant.id, url, error = %reason, "MCP survey failed");
            settings_error(StatusCode::BAD_GATEWAY, reason)
        }
    }
}

#[derive(Debug, Deserialize)]
struct OidcCheckBody {
    issuer: String,
    #[serde(default)]
    audience: Option<String>,
    /// A customer token to verify the way the widget's socket would.
    #[serde(default)]
    token: Option<String>,
}

/// `POST /admin/api/oidc/check` — fetch an issuer's discovery document and
/// JWKS under the tenant's outbound policy and report what they say; with
/// a `token`, verify it through the very validator the public socket uses
/// for this issuer and audience. A read, logged not audited. 422 names an
/// issuer that could not be read. Guarded.
async fn admin_oidc_check(
    ServedTenant(tenant): ServedTenant,
    operator: axum::Extension<Operator>,
    body: axum::Json<OidcCheckBody>,
) -> Response {
    let issuer = body.issuer.trim();
    if issuer.is_empty() {
        return settings_error(StatusCode::UNPROCESSABLE_ENTITY, "issuer must not be empty");
    }
    let audience = body
        .audience
        .as_deref()
        .map(str::trim)
        .filter(|a| !a.is_empty());
    tracing::info!(tenant = %tenant.id, operator = %operator.0 .0, issuer, "checking an OIDC issuer");
    let report = match crate::keycloak::inspect_issuer(issuer, tenant.outbound()).await {
        Ok(report) => report,
        Err(e) => {
            tracing::warn!(tenant = %tenant.id, issuer, error = %format!("{e:#}"), "OIDC issuer check failed");
            return settings_error(StatusCode::UNPROCESSABLE_ENTITY, format!("{e:#}"));
        }
    };
    let token = match body
        .token
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        Some(token) => {
            let validator =
                crate::keycloak::validator_for_policy(issuer, audience, tenant.outbound());
            Some(match validator.verify(token).await {
                Ok(claims) => serde_json::json!({
                    "ok": true,
                    "sub": claims.sub,
                    "email": claims.email,
                    "email_verified": claims.email_verified,
                    "name": claims.display_name(),
                }),
                Err(e) => serde_json::json!({"ok": false, "error": e.to_string()}),
            })
        }
        None => None,
    };
    // A trailing slash is the one difference that is not a mismatch: the
    // validator pins `iss` to the configured string, so the wizard should
    // bind the agent to the document's spelling.
    let canonical = |s: &str| s.trim_end_matches('/').to_string();
    let issuer_matches = report.issuer.as_deref().map(canonical) == Some(canonical(issuer));
    axum::Json(serde_json::json!({
        "ok": true,
        "issuer_matches": issuer_matches,
        "report": report,
        "token": token,
    }))
    .into_response()
}

/// `GET /admin/api/operators` — the operator allowlist. Guarded.
async fn admin_operators(ServedTenant(tenant): ServedTenant) -> Response {
    match tenant.operators.list().await {
        Ok(ops) => axum::Json(serde_json::json!({ "operators": ops })).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "listing operators failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[derive(Debug, Deserialize)]
struct OperatorBody {
    kind: String,
    value: String,
}

/// `POST /admin/api/operators` — add an allowlist entry (email or domain).
async fn admin_operator_add(
    ServedTenant(tenant): ServedTenant,
    operator: axum::Extension<Operator>,
    body: axum::Json<OperatorBody>,
) -> Response {
    let kind = body.kind.as_str();
    if kind != "email" && kind != "domain" {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            axum::Json(serde_json::json!({"error": "kind must be 'email' or 'domain'"})),
        )
            .into_response();
    }
    let value = body.value.trim().to_lowercase();
    if value.is_empty() || value.len() > 320 {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            axum::Json(serde_json::json!({"error": "value must be 1..320 chars"})),
        )
            .into_response();
    }
    match tenant.operators.add(kind, &value, &operator.0 .0).await {
        Ok(_) => {
            tenant
                .audit
                .record(
                    &operator.0 .0,
                    "operator.add",
                    &format!("{kind}:{value}"),
                    "",
                )
                .await;
            axum::Json(serde_json::json!({"ok": true})).into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "adding operator failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// `DELETE /admin/api/operators/{kind}/{value}` — remove an allowlist entry.
async fn admin_operator_delete(
    ServedTenant(tenant): ServedTenant,
    operator: axum::Extension<Operator>,
    Path((kind, value)): Path<(String, String)>,
) -> Response {
    let value = value.to_lowercase();
    // Refuse to remove the last allowlist entry on an OIDC-only install — that
    // would lock every operator out. Break-glass installs are exempt.
    let has_break_glass = tenant.admin.as_ref().is_some_and(|a| a.has_password());
    if !has_break_glass && tenant.operators.count().await.unwrap_or(0) <= 1 {
        return (
            StatusCode::CONFLICT,
            axum::Json(serde_json::json!({
                "error": "can't remove the last operator with no break-glass password — you'd be locked out"
            })),
        )
            .into_response();
    }
    match tenant.operators.remove(&kind, &value).await {
        Ok(true) => {
            tenant
                .audit
                .record(
                    &operator.0 .0,
                    "operator.remove",
                    &format!("{kind}:{value}"),
                    "",
                )
                .await;
            axum::Json(serde_json::json!({"ok": true})).into_response()
        }
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::error!(error = %e, "removing operator failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// `GET /admin/api/agents` — the editable agent config. Returns everything the
/// edit form needs (incl. system_prompt + branding) but NEVER an MCP
/// `discovery_credential`: a secret that must not reach the browser, shown
/// only as `has_discovery_credential`. Guarded by `admin_guard`.
async fn admin_agents(ServedTenant(tenant): ServedTenant) -> Response {
    let mut agents: Vec<serde_json::Value> = tenant
        .all_hosts()
        .iter()
        .map(|h| {
            let a = h.agent();
            let servers: Vec<serde_json::Value> =
                a.embed.mcp_server_specs().iter().map(server_view).collect();
            let (mcp_settings, mcp_servers) = match servers.split_first() {
                Some((primary, additional)) => (Some(primary.clone()), additional.to_vec()),
                None => (None, Vec::new()),
            };
            serde_json::json!({
                "id": a.id,
                "display_name": a.display_name,
                "identity": display_identity(a.as_ref()),
                "model": a.model,
                "welcome": a.welcome,
                "playbooks": a.playbooks,
                "auto_playbook": a.playbook,
                "allowed_origins": a.embed.allowed_origins,
                "allowed_emails": a.embed.allowed_emails,
                "keycloak_issuer": a.embed.keycloak_issuer,
                "keycloak_audience": a.embed.keycloak_audience,
                "allowed_tools": a.embed.allowed_tools,
                "branding": a.embed.branding,
                "data_tables": a.embed.data_tables_enabled(),
                "charts": a.embed.charts_enabled(),
                "forms": a.embed.forms_enabled(),
                "maps": a.embed.maps_enabled(),
                "diagrams": a.embed.diagrams_enabled(),
                "excalidraw_export": a.embed.excalidraw_export_enabled(),
                "inline_images": a.embed.inline_images_enabled(),
                "images": a.embed.images_enabled(),
                "uploads": a.embed.uploads,
                "upload_max_mb": a.embed.upload_max_mb,
                "drawing": a.embed.drawing == Some(true),
                "has_mcp": a.embed.mcp.is_some(),
                "mcp_url": a.embed.mcp.as_ref().map(|m| m.url.clone()),
                "has_discovery_credential": a
                    .embed
                    .mcp
                    .as_ref()
                    .is_some_and(McpSettings::has_discovery_credential),
                "mcp_settings": mcp_settings,
                "mcp_servers": mcp_servers,
                "permissions": a.embed.permissions,
                "tool_grants": a.embed.tool_grants,
                "policy": a.embed.policy,
                "image_hosts": a.embed.image_hosts,
                "spill_skip_tools": a.embed.spill_skip_tools,
                "passive_memory": a.embed.passive_memory_enabled(),
            })
        })
        .collect();
    agents.sort_by(|a, b| {
        a["id"]
            .as_str()
            .unwrap_or("")
            .cmp(b["id"].as_str().unwrap_or(""))
    });
    axum::Json(serde_json::json!({ "agents": agents })).into_response()
}

/// One MCP server as the editor shows it, with its effective defaults and never
/// its credential.
fn server_view(spec: &McpServerSpec<'_>) -> serde_json::Value {
    serde_json::json!({
        "id": spec.id,
        "url": spec.connection.url,
        "auth": spec.connection.auth,
        "audience": spec.connection.audience,
        "required": spec.required,
        "unavailable_skip_secs": spec.unavailable_skip().as_secs(),
        "has_discovery_credential": spec.connection.has_discovery_credential(),
        "tool_prefix": spec.tool_prefix,
        "allowed_tools": spec.tools.allowed_tools,
        "tool_grants": spec.tools.tool_grants,
    })
}

/// Snapshot the live agent as a revision, with every MCP `discovery_credential`
/// stripped — a secret must never land in a revision (which the GET-revision
/// endpoint hands to the browser). Restore re-attaches the live credentials,
/// so fidelity isn't lost. Best-effort.
async fn snapshot_agent(tenant: &TenantRuntime, id: &str, operator: &str) {
    let Some(host) = tenant.host(id) else { return };
    let mut snap = host.agent().as_ref().clone();
    snap.embed.strip_credentials();
    if let Ok(json) = serde_json::to_string(&snap) {
        tenant.revisions.record("agent", id, &json, operator).await;
    }
}

/// The operator-editable slice of an agent's config. All fields optional —
/// only what's sent is changed. The MCP `discovery_credential` is write-only:
/// it can be set or cleared here but is never read back (the GET says only
/// whether one is set), so a secret never round-trips through the browser.
#[derive(Debug, Deserialize)]
struct AgentEdit {
    display_name: Option<String>,
    /// The identity files (SOUL/IDENTITY/AGENTS/TOOLS). When present, they become
    /// authoritative and the legacy `system_prompt` is cleared.
    identity: Option<crate::agent::IdentityFiles>,
    welcome: Option<String>,
    #[serde(default, deserialize_with = "double_option")]
    model: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    auto_playbook: Option<Option<String>>,
    playbooks: Option<Vec<String>>,
    allowed_origins: Option<Vec<String>>,
    allowed_emails: Option<Vec<String>>,
    allowed_tools: Option<Vec<String>>,
    keycloak_issuer: Option<String>,
    #[serde(default, deserialize_with = "double_option")]
    keycloak_audience: Option<Option<String>>,
    branding: Option<crate::agent::Branding>,
    data_tables: Option<bool>,
    charts: Option<bool>,
    forms: Option<bool>,
    maps: Option<bool>,
    diagrams: Option<bool>,
    excalidraw_export: Option<bool>,
    inline_images: Option<bool>,
    images: Option<bool>,
    uploads: Option<Vec<crate::agent::UploadKind>>,
    upload_max_mb: Option<u32>,
    drawing: Option<bool>,
    /// The primary company MCP server. Absent ⇒ unchanged; `null` or a blank
    /// `url` ⇒ no MCP (a pure-chat agent).
    #[serde(default, deserialize_with = "double_option")]
    mcp: Option<Option<McpEdit>>,
    /// The additional MCP servers, as the whole list. Absent ⇒ unchanged, so a
    /// client that predates them never wipes them; `[]` removes them all.
    mcp_servers: Option<Vec<McpServerEdit>>,
    /// Where `backend_permissions` grants are resolved. `null` removes it.
    #[serde(default, deserialize_with = "double_option")]
    permissions: Option<Option<crate::agent::PermissionsConfig>>,
    tool_grants: Option<Vec<crate::agent::ToolGrant>>,
    /// `null` removes the policy block.
    #[serde(default, deserialize_with = "double_option")]
    policy: Option<Option<crate::agent::EmbedPolicy>>,
    image_hosts: Option<Vec<String>>,
    spill_skip_tools: Option<Vec<String>>,
    passive_memory: Option<bool>,
}

/// An MCP server's connection as an edit or create body carries it, under
/// [`edited`]'s rule: absent keeps the stored value, `null` resets it to the
/// default, anything else replaces it. A blank `url` is that default: no
/// primary server, and a refused additional one. The credential is
/// write-only, so `null` keeps it as absence does and the empty string clears
/// it.
#[derive(Deserialize)]
struct McpEdit {
    #[serde(default, deserialize_with = "double_option")]
    url: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    discovery_credential: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    auth: Option<Option<McpAuth>>,
    #[serde(default, deserialize_with = "double_option")]
    audience: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    required: Option<Option<bool>>,
    #[serde(default, deserialize_with = "double_option")]
    unavailable_skip_secs: Option<Option<u64>>,
}

/// Manual `Debug` so a logged edit body can never print the credential.
impl std::fmt::Debug for McpEdit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpEdit")
            .field("url", &self.url)
            .field(
                "discovery_credential",
                &self
                    .discovery_credential
                    .as_ref()
                    .map(|c| c.as_ref().map(|_| "<redacted>")),
            )
            .field("auth", &self.auth)
            .field("audience", &self.audience)
            .field("required", &self.required)
            .field("unavailable_skip_secs", &self.unavailable_skip_secs)
            .finish()
    }
}

impl McpEdit {
    /// The primary server this edit yields over `current` (the agent's MCP
    /// before the edit): [`Self::over`] it, `None` when that leaves no URL.
    fn resolve(self, current: Option<McpSettings>) -> Option<McpSettings> {
        Some(self.over(current.unwrap_or_default())).filter(|mcp| !mcp.url.is_empty())
    }

    /// The connection this edit yields over `current`.
    fn over(self, current: McpSettings) -> McpSettings {
        McpSettings {
            url: edited(self.url, current.url).trim().to_string(),
            discovery_credential: resolve_credential(
                self.discovery_credential,
                current.discovery_credential,
            ),
            auth: edited(self.auth, current.auth),
            audience: self.audience.unwrap_or(current.audience),
            required: self.required.unwrap_or(current.required),
            unavailable_skip_secs: self
                .unavailable_skip_secs
                .unwrap_or(current.unavailable_skip_secs),
        }
    }
}

/// The edit rule every field of an MCP server follows: absent keeps `kept`,
/// `null` resets to the default, a value replaces it. An `Option` field needs
/// no helper: its default is `None`, so `edit.unwrap_or(kept)` is the rule.
fn edited<T: Default>(edit: Option<Option<T>>, kept: T) -> T {
    edit.map_or(kept, Option::unwrap_or_default)
}

/// The write-only credential rule: absent or `null` keeps `kept`, the empty
/// string clears it, anything else replaces it.
fn resolve_credential(edit: Option<Option<String>>, kept: Option<String>) -> Option<String> {
    match edit {
        None | Some(None) => kept,
        Some(Some(c)) => Some(c.trim().to_string()).filter(|c| !c.is_empty()),
    }
}

/// An additional MCP server as an edit or create body carries it: its
/// connection plus its own tool lists, every field under [`edited`]'s rule.
#[derive(Debug, Deserialize)]
struct McpServerEdit {
    #[serde(default)]
    id: String,
    #[serde(flatten)]
    connection: McpEdit,
    #[serde(default, deserialize_with = "double_option")]
    tool_prefix: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    allowed_tools: Option<Option<Vec<String>>>,
    #[serde(default, deserialize_with = "double_option")]
    tool_grants: Option<Option<Vec<ToolGrant>>>,
}

impl McpServerEdit {
    /// The server this edit yields over `current` (the agent's servers before
    /// the edit). What a field keeps comes from the server with the same id,
    /// so a new or renamed server starts from the defaults: no URL (refused
    /// by validation), no credential, no tools.
    fn resolve(self, current: &[McpServerSettings]) -> McpServerSettings {
        let id = self.id.trim().to_string();
        let kept = current
            .iter()
            .find(|s| s.id == id)
            .cloned()
            .unwrap_or_default();
        McpServerSettings {
            connection: self.connection.over(kept.connection),
            id,
            tool_prefix: self.tool_prefix.unwrap_or(kept.tool_prefix),
            allowed_tools: edited(self.allowed_tools, kept.allowed_tools),
            tool_grants: edited(self.tool_grants, kept.tool_grants),
        }
    }
}

/// Normalise the integration slice of an embed the way the editor means it:
/// trimmed URLs and names, blank entries dropped, image hosts lowercased (the
/// matcher is case-insensitive, so the stored form may as well be canonical).
fn tidy_embed(embed: &mut EmbedSettings) {
    if let Some(mcp) = embed.mcp.as_mut() {
        tidy_connection(mcp, true);
    }
    for server in &mut embed.mcp_servers {
        server.id = server.id.trim().to_string();
        tidy_connection(&mut server.connection, false);
        server.tool_prefix = server
            .tool_prefix
            .take()
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty());
        server.allowed_tools = tidy_names(std::mem::take(&mut server.allowed_tools));
        tidy_grants(&mut server.tool_grants);
    }
    if let Some(p) = embed.permissions.as_mut() {
        p.url = p.url.trim().to_string();
    }
    tidy_grants(&mut embed.tool_grants);
    embed.image_hosts = tidy_names(std::mem::take(&mut embed.image_hosts))
        .into_iter()
        .map(|h| h.to_ascii_lowercase())
        .collect();
    embed.spill_skip_tools = tidy_names(std::mem::take(&mut embed.spill_skip_tools));
}

/// Trim a connection and drop every option set to its default, so a save that
/// changes nothing stores exactly what was there.
fn tidy_connection(connection: &mut McpSettings, required_by_default: bool) {
    connection.url = connection.url.trim().to_string();
    connection.audience = connection
        .audience
        .take()
        .map(|a| a.trim().to_string())
        .filter(|a| !a.is_empty());
    connection.required = connection.required.filter(|r| *r != required_by_default);
    connection.unavailable_skip_secs = connection
        .unavailable_skip_secs
        .filter(|secs| *secs != crate::agent::DEFAULT_UNAVAILABLE_SKIP_SECS);
}

fn tidy_grants(grants: &mut [ToolGrant]) {
    for grant in grants {
        grant.requires = grant.requires.trim().to_string();
        grant.tools = tidy_names(std::mem::take(&mut grant.tools));
    }
}

fn tidy_names(names: Vec<String>) -> Vec<String> {
    names
        .into_iter()
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
        .collect()
}

/// The integration fields an edit may touch, so a PUT validates only what it
/// changed: an agent seeded from a file before these checks existed keeps
/// taking edits to its other fields.
#[derive(Clone, Copy, Default)]
struct EmbedChecks {
    mcp: bool,
    /// The servers' static rules: additional servers' URLs, and everything
    /// [`EmbedSettings::check_mcp_servers`] checks — which the primary, the
    /// allowlist and the grants all feed.
    mcp_servers: bool,
    permissions: bool,
    tool_grants: bool,
    image_hosts: bool,
    /// Coverage of `spill_skip_tools` depends on the allowlist and the
    /// grants, so a change to any of the three re-checks it.
    spill_skip_tools: bool,
}

impl EmbedChecks {
    const ALL: Self = Self {
        mcp: true,
        mcp_servers: true,
        permissions: true,
        tool_grants: true,
        image_hosts: true,
        spill_skip_tools: true,
    };

    fn for_edit(edit: &AgentEdit) -> Self {
        Self {
            mcp: edit.mcp.is_some(),
            mcp_servers: edit.mcp_servers.is_some()
                || edit.mcp.is_some()
                || edit.allowed_tools.is_some()
                || edit.tool_grants.is_some(),
            permissions: edit.permissions.is_some(),
            tool_grants: edit.tool_grants.is_some(),
            image_hosts: edit.image_hosts.is_some(),
            spill_skip_tools: edit.spill_skip_tools.is_some()
                || edit.allowed_tools.is_some()
                || edit.tool_grants.is_some()
                || edit.mcp_servers.is_some(),
        }
    }
}

/// Shape checks on the integration slice of an embed, shared by create, edit
/// and import. Each error names the field, so the 422 says what to fix.
/// Reachability is not checked here: the host build does that under the
/// tenant's outbound policy.
fn validate_embed(embed: &EmbedSettings, checks: EmbedChecks) -> std::result::Result<(), String> {
    if checks.mcp {
        if let Some(mcp) = &embed.mcp {
            check_mcp_url("mcp.url", &mcp.url)?;
        }
    }
    if checks.mcp_servers {
        embed.mcp_servers.iter().try_for_each(|s| {
            check_mcp_url(&format!("mcp_servers[{}].url", s.id), &s.connection.url)
        })?;
        embed.check_mcp_servers().map_err(|e| {
            crate::metrics::config_refused(e.reason);
            e.message
        })?;
    }
    if checks.permissions {
        if let Some(p) = &embed.permissions {
            if !p.url.contains("{user}") {
                return Err(
                    "permissions.url must contain the {user} placeholder for the signed-in user"
                        .to_string(),
                );
            }
            url::Url::parse(&p.url.replace("{user}", "user"))
                .map_err(|e| format!("permissions.url is not a URL: {e}"))?;
        }
    }
    if checks.tool_grants {
        crate::agent::check_tool_grants("tool_grants", &embed.tool_grants)?;
    }
    if checks.image_hosts {
        if let Some(host) = embed.image_hosts.iter().find(|h| !valid_image_host(h)) {
            return Err(format!(
                "image_hosts: {host:?} is not a hostname (lowercase, no scheme or path; a leading dot allows subdomains)"
            ));
        }
    }
    if checks.spill_skip_tools {
        if let Some(tool) = embed
            .spill_skip_tools
            .iter()
            .find(|t| !spill_entry_covered(embed, t))
        {
            return Err(format!(
                "spill_skip_tools: {tool:?} is not among the agent's allowed or granted tools"
            ));
        }
    }
    Ok(())
}

/// An MCP URL's shape: present, parseable, and http(s).
fn check_mcp_url(field: &str, url: &str) -> std::result::Result<(), String> {
    if url.is_empty() {
        return Err(format!("{field} is required"));
    }
    let parsed = url::Url::parse(url).map_err(|e| format!("{field} is not a URL: {e}"))?;
    if matches!(parsed.scheme(), "http" | "https") {
        Ok(())
    } else {
        Err(format!("{field} must be an http(s) URL"))
    }
}

/// A bare hostname as `image_hosts` holds it: lowercase DNS labels, optionally
/// with the leading dot that admits subdomains. No scheme, port or path.
fn valid_image_host(host: &str) -> bool {
    let bare = host.strip_prefix('.').unwrap_or(host);
    !bare.is_empty()
        && bare.len() <= 253
        && bare.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        })
}

/// Whether a `spill_skip_tools` entry names something the agent can call: a
/// tool some server's allowlist admits (exactly or by glob), or a granted tool
/// it names exactly or as the prefix the spill matcher accepts.
fn spill_entry_covered(embed: &EmbedSettings, entry: &str) -> bool {
    embed.any_server_admits(entry)
        || embed.all_tool_grants().flat_map(|g| &g.tools).any(|t| {
            t == entry || t.starts_with(&format!("{entry}.")) || t.starts_with(&format!("{entry}_"))
        })
}

/// The identity files to show in the editor. Migrates a legacy `system_prompt`
/// (no identity files yet) into IDENTITY.md so the operator sees + edits the
/// current persona; the migration is made authoritative on the next save.
fn display_identity(agent: &EdgeAgent) -> crate::agent::IdentityFiles {
    if agent.identity.is_empty() && !agent.system_prompt.trim().is_empty() {
        crate::agent::IdentityFiles {
            identity: agent.system_prompt.clone(),
            ..Default::default()
        }
    } else {
        agent.identity.clone()
    }
}

/// Distinguish "field absent" from "field present and null" so an edit can both
/// leave `model` unchanged (absent) and clear it to the install default (null).
fn double_option<'de, D, T>(de: D) -> std::result::Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Some(Option::deserialize(de)?))
}

/// Merge an [`AgentEdit`] onto an agent in place. Absent fields are left as-is;
/// `model`/`auto_playbook` present-and-null clear to the default; blank strings
/// there also clear. The MCP credential is kept unless the edit names it.
fn apply_edit(agent: &mut EdgeAgent, edit: AgentEdit) {
    if let Some(v) = edit.display_name {
        agent.display_name = v;
    }
    if let Some(v) = edit.identity {
        // Only retire the legacy prompt when the incoming identity actually has
        // content — an all-blank `identity` must not silently wipe the persona.
        if !v.is_empty() {
            agent.system_prompt = String::new();
        }
        agent.identity = v;
    }
    if let Some(v) = edit.welcome {
        agent.welcome = v;
    }
    if let Some(v) = edit.model {
        agent.model = v.filter(|s| !s.trim().is_empty());
    }
    if let Some(v) = edit.auto_playbook {
        agent.playbook = v.filter(|s| !s.trim().is_empty());
    }
    if let Some(v) = edit.playbooks {
        agent.playbooks = v;
    }
    if let Some(v) = edit.allowed_origins {
        agent.embed.allowed_origins = v;
    }
    if let Some(v) = edit.allowed_emails {
        agent.embed.allowed_emails = v;
    }
    if let Some(v) = edit.allowed_tools {
        agent.embed.allowed_tools = v;
    }
    if let Some(v) = edit.keycloak_issuer {
        if !v.trim().is_empty() {
            agent.embed.keycloak_issuer = v.trim().to_string();
        }
    }
    if let Some(v) = edit.keycloak_audience {
        agent.embed.keycloak_audience = v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    }
    if let Some(v) = edit.branding {
        agent.embed.branding = v;
    }
    if let Some(v) = edit.data_tables {
        agent.embed.data_tables = Some(v);
    }
    if let Some(v) = edit.charts {
        agent.embed.charts = Some(v);
    }
    if let Some(v) = edit.forms {
        agent.embed.forms = Some(v);
    }
    if let Some(v) = edit.maps {
        agent.embed.maps = Some(v);
    }
    if let Some(v) = edit.diagrams {
        agent.embed.diagrams = Some(v);
    }
    if let Some(v) = edit.excalidraw_export {
        agent.embed.excalidraw_export = Some(v);
    }
    if let Some(v) = edit.inline_images {
        agent.embed.inline_images = Some(v);
    }
    if let Some(v) = edit.images {
        agent.embed.images = Some(v);
    }
    if let Some(v) = edit.uploads {
        agent.embed.uploads = v;
    }
    if let Some(v) = edit.upload_max_mb {
        agent.embed.upload_max_mb = Some(v);
    }
    if let Some(v) = edit.drawing {
        agent.embed.drawing = Some(v);
    }
    if let Some(v) = edit.mcp {
        let current = agent.embed.mcp.take();
        agent.embed.mcp = v.and_then(|m| m.resolve(current));
    }
    if let Some(v) = edit.mcp_servers {
        let current = std::mem::take(&mut agent.embed.mcp_servers);
        agent.embed.mcp_servers = v.into_iter().map(|m| m.resolve(&current)).collect();
    }
    if let Some(v) = edit.permissions {
        agent.embed.permissions = v;
    }
    if let Some(v) = edit.tool_grants {
        agent.embed.tool_grants = v;
    }
    if let Some(v) = edit.policy {
        agent.embed.policy = v;
    }
    if let Some(v) = edit.image_hosts {
        agent.embed.image_hosts = v;
    }
    if let Some(v) = edit.spill_skip_tools {
        agent.embed.spill_skip_tools = v;
    }
    if let Some(v) = edit.passive_memory {
        agent.embed.passive_memory = Some(v);
    }
    tidy_embed(&mut agent.embed);
}

/// `PUT /admin/api/agents/{id}` — apply an operator edit: patch the stored
/// agent, rebuild its host, and hot-swap it. Guarded by `admin_guard`.
async fn admin_agent_update(
    ServedTenant(tenant): ServedTenant,
    operator: axum::Extension<Operator>,
    Path(id): Path<String>,
    body: axum::Json<AgentEdit>,
) -> Response {
    if let Err(e) = tenant.ensure_config_writable() {
        return e.into_response();
    }
    // Start from the live agent so unspecified fields (and the untouched MCP
    // block) are preserved.
    let Some(host) = tenant.host(&id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mut agent = host.agent().as_ref().clone();
    let checks = EmbedChecks::for_edit(&body.0);
    apply_edit(&mut agent, body.0);
    if let Err(e) = validate_embed(&agent.embed, checks) {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            axum::Json(serde_json::json!({"error": e})),
        )
            .into_response();
    }
    // Referenced playbooks must resolve (local or synced) — a dangling id would
    // silently serve without the flow.
    for pb in agent.playbook.iter().chain(agent.playbooks.iter()) {
        if tenant.rebuild.catalog.resolve(pb).is_none() {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                axum::Json(serde_json::json!({"error": format!("unknown playbook {pb:?}")})),
            )
                .into_response();
        }
    }
    match tenant.apply_agent_edit(agent).await {
        Ok(()) => {
            snapshot_agent(&tenant, &id, &operator.0 .0).await;
            tenant
                .audit
                .record(&operator.0 .0, "agent.update", &id, "")
                .await;
            axum::Json(serde_json::json!({"ok": true})).into_response()
        }
        Err(e) => {
            tracing::error!(agent = %id, error = %e, "agent edit failed");
            (
                StatusCode::BAD_GATEWAY,
                axum::Json(
                    serde_json::json!({"error": "could not apply the change (rebuild failed)"}),
                ),
            )
                .into_response()
        }
    }
}

/// `POST /admin/api/git/sync` — re-fetch the config repo and hot-apply it, no
/// restart. Allowed on a git-managed install (it's a sync, not a config edit);
/// 409 on a non-git install.
async fn admin_git_sync(
    ServedTenant(tenant): ServedTenant,
    operator: axum::Extension<Operator>,
) -> Response {
    if !tenant.git_managed {
        return (
            StatusCode::CONFLICT,
            axum::Json(serde_json::json!({"error": "this install is not git-managed"})),
        )
            .into_response();
    }
    match tenant.resync_from_git().await {
        Ok(report) => {
            if report.changed {
                tenant
                    .audit
                    .record(&operator.0 .0, "git.sync", &report.commit, "")
                    .await;
            }
            axum::Json(report).into_response()
        }
        Err(e) => {
            tracing::warn!(error = %e, "manual git sync failed");
            (
                StatusCode::BAD_GATEWAY,
                axum::Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response()
        }
    }
}

/// `POST /admin/api/import/git` body: the repo to read once. The token is
/// used for the fetch and dropped — never stored, never logged — so this has
/// no `Debug`.
#[derive(Deserialize)]
struct GitImportRequest {
    /// `owner/name` on GitHub.
    repo: String,
    /// Branch, tag or commit; `main` when absent.
    #[serde(default)]
    git_ref: Option<String>,
    /// A token with `contents: read` on the repo.
    token: String,
    /// Bindings for the `${NAME}` placeholders in the repo's `agent.toml`
    /// files. The import never reads this process's environment.
    #[serde(default)]
    env: BTreeMap<String, String>,
}

/// What an import wrote, for the response and the audit row.
#[derive(Serialize)]
struct GitImportReport {
    agents: usize,
    playbooks: usize,
    training_docs: usize,
    /// Always empty on success; a 422 carries the names instead.
    unresolved: Vec<String>,
}

/// `owner/name`, as GitHub spells a repository.
fn valid_repo(repo: &str) -> bool {
    let Some((owner, name)) = repo.split_once('/') else {
        return false;
    };
    let part_ok = |p: &str| {
        !p.is_empty()
            && p.len() <= 100
            && p.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    part_ok(owner) && part_ok(name) && name != "." && name != ".."
}

/// `POST /admin/api/import/git` — read a config repo once and write its
/// agents, playbooks and training into this tenant, which stays editable
/// afterwards (git mode is the other way to use a repo, and a git-managed
/// tenant is refused here: its repo already owns these items). Placeholders
/// in the repo's `agent.toml` files expand from the body's `env` and nothing
/// else; any left unresolved refuse the import before a byte is written.
/// Every agent is validated and built before the first write, so a repo the
/// tenant could not start on changes nothing. Existing items with the same
/// id are replaced, each agent and playbook is snapshotted as a revision, and
/// the tenant is rebuilt on the result. Guarded.
async fn admin_import_git(
    State(state): State<Arc<EdgeState>>,
    ServedTenant(tenant): ServedTenant,
    operator: axum::Extension<Operator>,
    body: axum::Json<GitImportRequest>,
) -> Response {
    if let Err(e) = tenant.ensure_config_writable() {
        return e.into_response();
    }
    let fail = |status: StatusCode, msg: String| {
        (status, axum::Json(serde_json::json!({ "error": msg }))).into_response()
    };
    let unprocessable = |msg: String| fail(StatusCode::UNPROCESSABLE_ENTITY, msg);
    let req = body.0;
    let repo = req.repo.trim();
    if !valid_repo(repo) {
        return unprocessable("repo must be the GitHub owner/name".to_string());
    }
    let git_ref = req
        .git_ref
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .unwrap_or("main");
    if git_ref.chars().any(char::is_whitespace) || git_ref.contains("..") {
        return unprocessable("git_ref must be a branch, tag or commit".to_string());
    }
    let token = req.token.trim();
    if token.is_empty() {
        return unprocessable("a token with read access to the repo is required".to_string());
    }
    let who = &operator.0 .0;
    let source = GitConfig {
        repo: repo.to_string(),
        git_ref: git_ref.to_string(),
        token: token.to_string(),
        poll_secs: 0,
    };
    let files = match state
        .infra
        .github(tenant.outbound())
        .fetch_files(&source)
        .await
    {
        Ok(files) => files,
        Err(e) => {
            tracing::warn!(tenant = %tenant.id, repo, git_ref, error = %format!("{e:#}"), "git import fetch failed");
            return fail(
                StatusCode::BAD_GATEWAY,
                format!("could not fetch the repo: {e:#}"),
            );
        }
    };
    // The token's work is done; nothing below can see it.
    drop(source);
    let parsed = match git_config::parse_tree_with(files, Expansion::Map(&req.env)) {
        Ok(parsed) => parsed,
        Err(e) => return unprocessable(format!("{e:#}")),
    };
    if !parsed.unresolved.is_empty() {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            axum::Json(serde_json::json!({
                "error": format!(
                    "the repo's agent config names {} placeholder(s) the request did not bind; supply them in `env`",
                    parsed.unresolved.len()
                ),
                "unresolved": parsed.unresolved,
            })),
        )
            .into_response();
    }
    let tree = parsed.tree;
    let mut agents = tree.agents;
    let playbook_ids: std::collections::HashSet<&str> =
        tree.playbooks.iter().map(|(id, _)| id.as_str()).collect();
    for agent in &mut agents {
        if !crate::agent::valid_id(&agent.id) {
            return unprocessable(format!(
                "agent {:?}: id must be 1–64 chars of lowercase letters, digits, or dashes",
                agent.id
            ));
        }
        if agent.display_name.trim().is_empty() {
            return unprocessable(format!("agent {}: display_name is required", agent.id));
        }
        if agent.embed.keycloak_issuer.trim().is_empty() {
            return unprocessable(format!(
                "agent {}: embed.keycloak_issuer is required",
                agent.id
            ));
        }
        tidy_embed(&mut agent.embed);
        if let Err(e) = validate_embed(&agent.embed, EmbedChecks::ALL) {
            return unprocessable(format!("agent {}: {e}", agent.id));
        }
        // A referenced playbook must come with the repo or already exist.
        for pb in agent.playbook.iter().chain(agent.playbooks.iter()) {
            if !playbook_ids.contains(pb.as_str()) && tenant.rebuild.catalog.resolve(pb).is_none() {
                return unprocessable(format!("agent {}: unknown playbook {pb:?}", agent.id));
            }
        }
    }
    for (id, source) in &tree.playbooks {
        match workforce_playbook::PlaybookDefinition::from_toml(source) {
            Ok(def) if def.playbook.id == *id => {}
            Ok(def) => {
                return unprocessable(format!(
                    "playbooks/{id}.toml: its id ({:?}) must match the file name",
                    def.playbook.id
                ))
            }
            Err(e) => return unprocessable(format!("playbooks/{id}.toml: {e}")),
        }
    }
    // Build every host before writing anything: a repo this tenant cannot
    // start on (an MCP URL its outbound policy refuses, say) must not reach
    // the store, where the next boot would trip over it.
    for agent in &agents {
        if let Err(e) = tenant.rebuild.build_host(Arc::new(agent.clone())).await {
            tracing::warn!(tenant = %tenant.id, agent = %agent.id, error = %format!("{e:#}"), "imported agent would not build");
            return unprocessable(format!("agent {} would not start: {e:#}", agent.id));
        }
    }

    let mut by_agent: HashMap<&str, Vec<(String, String, String)>> = HashMap::new();
    for f in &tree.training {
        by_agent
            .entry(f.agent_id.as_str())
            .or_default()
            .push(git_config::training_doc(f));
    }
    {
        // Serialised with every other config write, like an edit.
        let _guard = tenant.edit_lock.lock().await;
        // The agent cap counts the ids the tenant would hold, not the writes:
        // an agent the repo replaces in place is not a new one. Checked under
        // the lock so a create racing the import cannot slip past it, and
        // before the first write so a refused import leaves nothing behind.
        let mounted: HashSet<String> = tenant
            .all_hosts()
            .iter()
            .map(|h| h.agent().id.clone())
            .collect();
        let added = agents.iter().filter(|a| !mounted.contains(&a.id)).count();
        if let Err(message) = tenant.plan_usage.plan().admit_agents(mounted.len() + added) {
            tracing::info!(tenant = %tenant.id, repo, git_ref, agents = agents.len(), "git import refused: plan limit reached");
            return agent_cap_response(message);
        }
        for agent in &agents {
            if let Err(e) = tenant.config_store.upsert(agent).await {
                tracing::error!(tenant = %tenant.id, agent = %agent.id, error = %e, "storing an imported agent failed");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
            let mut snapshot = agent.clone();
            snapshot.embed.strip_credentials();
            if let Ok(json) = serde_json::to_string(&snapshot) {
                tenant
                    .revisions
                    .record("agent", &agent.id, &json, who)
                    .await;
            }
        }
        for (id, source) in &tree.playbooks {
            if let Err(e) = tenant.playbook_store.upsert(id, source).await {
                tracing::error!(tenant = %tenant.id, playbook = %id, error = %e, "storing an imported playbook failed");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
            tenant.revisions.record("playbook", id, source, who).await;
        }
        for agent in &agents {
            let docs = by_agent.remove(agent.id.as_str()).unwrap_or_default();
            match tenant.knowledge.replace_agent_docs(&agent.id, &docs).await {
                Ok((embedded, kept, removed)) => tracing::info!(
                    tenant = %tenant.id, agent = %agent.id, embedded, kept, removed,
                    "imported training data"
                ),
                Err(e) => {
                    tracing::warn!(tenant = %tenant.id, agent = %agent.id, error = %e, "importing training data failed")
                }
            }
        }
        for orphan in by_agent.keys() {
            tracing::warn!(tenant = %tenant.id, agent = %orphan, "training/ folder has no matching agent in the repo; ignored");
        }
    }
    let report = GitImportReport {
        agents: agents.len(),
        playbooks: tree.playbooks.len(),
        training_docs: tree.training.len(),
        unresolved: Vec::new(),
    };
    let detail = format!(
        "{} agent(s), {} playbook(s), {} training doc(s)",
        report.agents, report.playbooks, report.training_docs
    );
    tenant
        .audit
        .record(who, "import.git", &format!("{repo}@{git_ref}"), &detail)
        .await;
    if let Err(e) = state.reload(&tenant.id).await {
        tracing::error!(tenant = %tenant.id, error = %format!("{e:#}"), "rebuilding the tenant on an import failed");
        return fail(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("the repo was imported, but the tenant failed to rebuild on it: {e:#}"),
        );
    }
    tracing::info!(
        tenant = %tenant.id, repo, git_ref, operator = %who,
        agents = report.agents, playbooks = report.playbooks, training = report.training_docs,
        "config repo imported"
    );
    axum::Json(report).into_response()
}

/// The full spec to stand up a NEW agent. The MCP block's credential is
/// write-only, as in an edit.
#[derive(Debug, Deserialize)]
struct AgentCreate {
    id: String,
    display_name: String,
    #[serde(default)]
    identity: crate::agent::IdentityFiles,
    #[serde(default)]
    welcome: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    allowed_origins: Vec<String>,
    #[serde(default)]
    allowed_emails: Vec<String>,
    keycloak_issuer: String,
    #[serde(default)]
    keycloak_audience: Option<String>,
    #[serde(default)]
    allowed_tools: Vec<String>,
    #[serde(default)]
    mcp: Option<McpEdit>,
    #[serde(default)]
    mcp_servers: Vec<McpServerEdit>,
    #[serde(default)]
    branding: crate::agent::Branding,
    #[serde(default)]
    data_tables: Option<bool>,
    #[serde(default)]
    charts: Option<bool>,
    #[serde(default)]
    forms: Option<bool>,
    #[serde(default)]
    maps: Option<bool>,
    #[serde(default)]
    diagrams: Option<bool>,
    #[serde(default)]
    excalidraw_export: Option<bool>,
    #[serde(default)]
    inline_images: Option<bool>,
    #[serde(default)]
    images: Option<bool>,
    #[serde(default)]
    uploads: Option<Vec<crate::agent::UploadKind>>,
    #[serde(default)]
    upload_max_mb: Option<u32>,
    #[serde(default)]
    drawing: Option<bool>,
    #[serde(default)]
    permissions: Option<crate::agent::PermissionsConfig>,
    #[serde(default)]
    tool_grants: Vec<crate::agent::ToolGrant>,
    #[serde(default)]
    policy: Option<crate::agent::EmbedPolicy>,
    #[serde(default)]
    image_hosts: Vec<String>,
    #[serde(default)]
    spill_skip_tools: Vec<String>,
    #[serde(default)]
    passive_memory: Option<bool>,
}

/// `POST /admin/api/agents` — create a new agent. Validates the id + required
/// fields, builds and hot-mounts its host, then persists. Guarded.
async fn admin_agent_create(
    ServedTenant(tenant): ServedTenant,
    operator: axum::Extension<Operator>,
    body: axum::Json<AgentCreate>,
) -> Response {
    if let Err(e) = tenant.ensure_config_writable() {
        return e.into_response();
    }
    let c = body.0;
    let fail = |msg: &str| {
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            axum::Json(serde_json::json!({ "error": msg })),
        )
            .into_response()
    };
    if !crate::agent::valid_id(&c.id) {
        return fail("id must be 1–64 chars of lowercase letters, digits, or dashes");
    }
    if c.display_name.trim().is_empty() {
        return fail("a display name is required");
    }
    if c.keycloak_issuer.trim().is_empty() {
        return fail("an identity provider (issuer) is required");
    }
    if c.identity.is_empty() {
        return fail("give the agent a persona — fill in at least one identity file");
    }
    let _guard = tenant.edit_lock.lock().await;
    // Reject a duplicate id (a live host or a stored row) — creating over an
    // existing agent would silently replace it.
    if tenant.host(&c.id).is_some() || tenant.config_store.exists(&c.id).await.unwrap_or(false) {
        return (
            StatusCode::CONFLICT,
            axum::Json(serde_json::json!({"error": format!("an agent '{}' already exists", c.id)})),
        )
            .into_response();
    }
    // Under the lock, so two creates racing at the cap cannot both pass.
    if let Err(message) = tenant
        .plan_usage
        .plan()
        .admit_agents(tenant.host_count() + 1)
    {
        tracing::info!(tenant = %tenant.id, agent = %c.id, "agent create refused: plan limit reached");
        return agent_cap_response(message);
    }
    let agent = EdgeAgent {
        id: c.id.clone(),
        display_name: c.display_name,
        system_prompt: String::new(),
        identity: c.identity,
        welcome: c.welcome,
        model: c.model.filter(|s| !s.trim().is_empty()),
        playbook: None,
        playbooks: vec![],
        progress_labels: vec![],
        derived_figures: Default::default(),
        embed: crate::agent::EmbedSettings {
            allowed_origins: c.allowed_origins,
            allowed_tools: c.allowed_tools,
            keycloak_issuer: c.keycloak_issuer.trim().to_string(),
            keycloak_audience: c
                .keycloak_audience
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            allowed_emails: c.allowed_emails,
            mcp: c.mcp.and_then(|m| m.resolve(None)),
            mcp_servers: c.mcp_servers.into_iter().map(|m| m.resolve(&[])).collect(),
            branding: c.branding,
            passive_memory: c.passive_memory,
            data_tables: c.data_tables,
            charts: c.charts,
            forms: c.forms,
            maps: c.maps,
            diagrams: c.diagrams,
            excalidraw_export: c.excalidraw_export,
            inline_images: c.inline_images,
            images: c.images,
            image_hosts: c.image_hosts,
            spill_skip_tools: c.spill_skip_tools,
            uploads: c.uploads.unwrap_or_default(),
            upload_max_mb: c.upload_max_mb,
            drawing: c.drawing,
            tool_grants: c.tool_grants,
            permissions: c.permissions,
            policy: c.policy,
        },
    };
    let mut agent = agent;
    tidy_embed(&mut agent.embed);
    if let Err(e) = validate_embed(&agent.embed, EmbedChecks::ALL) {
        return fail(&e);
    }
    match tenant.apply_agent_edit_inner(agent).await {
        Ok(()) => {
            snapshot_agent(&tenant, &c.id, &operator.0 .0).await;
            tenant
                .audit
                .record(&operator.0 .0, "agent.create", &c.id, "")
                .await;
            axum::Json(serde_json::json!({"ok": true})).into_response()
        }
        Err(e) => {
            tracing::error!(agent = %c.id, error = %e, "agent create failed");
            (
                StatusCode::BAD_GATEWAY,
                axum::Json(serde_json::json!({"error": "could not build the agent (check the MCP URL / issuer)"})),
            )
                .into_response()
        }
    }
}

/// `DELETE /admin/api/agents/{id}` — remove an agent: unmount its host, drop its
/// stored config, and best-effort purge its training corpus. Refuses to remove
/// the last agent (an install needs at least one). Guarded.
async fn admin_agent_delete(
    ServedTenant(tenant): ServedTenant,
    operator: axum::Extension<Operator>,
    Path(id): Path<String>,
) -> Response {
    if let Err(e) = tenant.ensure_config_writable() {
        return e.into_response();
    }
    let _guard = tenant.edit_lock.lock().await;
    if tenant.host(&id).is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    if tenant.host_count() <= 1 {
        return (
            StatusCode::CONFLICT,
            axum::Json(serde_json::json!({"error": "can't remove the last agent — an install needs at least one"})),
        )
            .into_response();
    }
    // Purge ALL of the agent's derived Weaviate data first so it can't outlive
    // the agent — critically, so a later agent created under the SAME id can't
    // inherit the old one's training data, learned lessons, or conversation
    // memory. Customer memory goes first and must succeed: it is personal
    // data, and an agent whose memory could not be erased is kept, untouched,
    // so the delete can be retried. Knowledge and lessons follow, best-effort.
    let scope = workforce_core::types::AgentId::new(&id);
    if let Some(host) = tenant.host(&id) {
        match crate::memory::forget_all_customer_memory(host.memory().as_ref(), &scope).await {
            Ok(n) => tracing::info!(agent = %id, memories = n, "purged agent memory"),
            Err(e) => {
                tracing::error!(agent = %id, error = %format!("{e:#}"), "purging memory failed");
                return (
                    StatusCode::BAD_GATEWAY,
                    axum::Json(serde_json::json!({"error": "could not erase what the agent remembers about its customers; the agent was kept, try again"})),
                )
                    .into_response();
            }
        }
        match crate::lessons::forget_all_lessons(host.memory().as_ref(), &scope).await {
            Ok(n) => tracing::info!(agent = %id, lessons = n, "purged agent lessons"),
            Err(e) => tracing::warn!(agent = %id, error = %e, "purging lessons failed"),
        }
    }
    if let Ok(docs) = tenant.knowledge.list(&id).await {
        for d in docs {
            let _ = tenant.knowledge.delete(&id, &d.id).await;
        }
    }
    if let Err(e) = tenant.config_store.delete(&id).await {
        tracing::error!(agent = %id, error = %e, "deleting agent config failed");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    tenant.remove_host(&id);
    tenant
        .audit
        .record(&operator.0 .0, "agent.delete", &id, "")
        .await;
    axum::Json(serde_json::json!({"ok": true})).into_response()
}

/// `GET /admin/api/playbooks` — the guided flows, each as its editable TOML
/// source plus a summary. Guarded by `admin_guard`.
async fn admin_playbooks(ServedTenant(tenant): ServedTenant) -> Response {
    let sources = match tenant.playbook_store.load_all().await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, "listing playbooks failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let playbooks: Vec<serde_json::Value> = sources
        .into_iter()
        .map(|(id, source)| {
            // Parse for a summary; a stored def always parsed once, but stay
            // resilient to schema drift.
            let (name, steps) = workforce_playbook::PlaybookDefinition::from_toml(&source)
                .map(|d| (d.playbook.name, d.steps.len()))
                .unwrap_or_else(|_| (id.clone(), 0));
            serde_json::json!({"id": id, "name": name, "steps": steps, "source": source})
        })
        .collect();
    axum::Json(serde_json::json!({ "playbooks": playbooks })).into_response()
}

#[derive(Debug, Deserialize)]
struct PlaybookBody {
    source: String,
}

/// `PUT /admin/api/playbooks/{id}` — create or replace a playbook from TOML.
/// The parsed id must equal the path id. Validates before persisting, then
/// hot-swaps the catalog. Guarded by `admin_guard`.
async fn admin_playbook_put(
    ServedTenant(tenant): ServedTenant,
    operator: axum::Extension<Operator>,
    Path(id): Path<String>,
    body: axum::Json<PlaybookBody>,
) -> Response {
    if let Err(e) = tenant.ensure_config_writable() {
        return e.into_response();
    }
    let def = match workforce_playbook::PlaybookDefinition::from_toml(&body.source) {
        Ok(d) => d,
        Err(e) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                axum::Json(serde_json::json!({"error": format!("invalid playbook: {e}")})),
            )
                .into_response()
        }
    };
    if id.len() > 128 {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            axum::Json(serde_json::json!({"error": "playbook id too long (max 128 chars)"})),
        )
            .into_response();
    }
    if def.playbook.id != id {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            axum::Json(serde_json::json!({
                "error": format!("playbook id in the TOML ({:?}) must match the URL ({id:?})", def.playbook.id)
            })),
        )
            .into_response();
    }
    // Serialize with agent edits so a concurrent change can't race the reload.
    let _guard = tenant.edit_lock.lock().await;
    if let Err(e) = tenant.playbook_store.upsert(&id, &body.source).await {
        tracing::error!(playbook = %id, error = %e, "saving playbook failed");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    if let Err(e) = tenant.reload_playbooks().await {
        tracing::error!(error = %e, "reloading playbooks failed");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    tenant
        .revisions
        .record("playbook", &id, &body.source, &operator.0 .0)
        .await;
    tenant
        .audit
        .record(&operator.0 .0, "playbook.save", &id, "")
        .await;
    axum::Json(serde_json::json!({"ok": true})).into_response()
}

/// `DELETE /admin/api/playbooks/{id}` — remove a playbook. Blocked (409) while
/// an agent still references it, listing the agents so the operator can detach
/// it first. Guarded by `admin_guard`.
async fn admin_playbook_delete(
    ServedTenant(tenant): ServedTenant,
    operator: axum::Extension<Operator>,
    Path(id): Path<String>,
) -> Response {
    if let Err(e) = tenant.ensure_config_writable() {
        return e.into_response();
    }
    // Hold the edit lock across the reference scan + delete so a concurrent
    // agent edit can't attach this playbook between the check and the removal.
    let _guard = tenant.edit_lock.lock().await;
    let referencing: Vec<String> = tenant
        .all_hosts()
        .iter()
        .filter(|h| {
            let a = h.agent();
            a.playbook.as_deref() == Some(id.as_str()) || a.playbooks.contains(&id)
        })
        .map(|h| h.agent().id.clone())
        .collect();
    if !referencing.is_empty() {
        return (
            StatusCode::CONFLICT,
            axum::Json(serde_json::json!({
                "error": "playbook is still attached to agents; detach it first",
                "agents": referencing,
            })),
        )
            .into_response();
    }
    match tenant.playbook_store.delete(&id).await {
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Ok(true) => {
            if let Err(e) = tenant.reload_playbooks().await {
                tracing::error!(error = %e, "reloading playbooks failed");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
            tenant
                .audit
                .record(&operator.0 .0, "playbook.delete", &id, "")
                .await;
            axum::Json(serde_json::json!({"ok": true})).into_response()
        }
        Err(e) => {
            tracing::error!(playbook = %id, error = %e, "deleting playbook failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// Cap on the operator conversation list per agent.
const ADMIN_CONVERSATIONS_MAX: usize = 200;

/// One row of the operator-wide conversation list (across ALL customers of the
/// agent — unlike the public API, which is owner-scoped).
#[derive(Debug, serde::Serialize)]
struct AdminConversation {
    session: String,
    /// The public conversation id (the channel's embed segment), so a
    /// feedback/turn row — which stores that id, not the internal session id —
    /// can deep-link straight to this exact transcript. `None` for a session
    /// whose channel isn't in this agent's embed namespace.
    conversation: Option<String>,
    owner: String,
    /// The customer's captured name, when known — the UI shows it in place of
    /// the raw `owner` key. `None` when the subject hasn't been resolved yet.
    display_name: Option<String>,
    title: String,
    updated_at: String,
    messages: usize,
}

/// Resolve session owners to captured identities for the admin views, tolerating
/// an absent store or a lookup failure — names are a display nicety, never
/// load-bearing, so a failure degrades to raw owner keys rather than a 500.
async fn resolve_owner_names(
    host: &AgentHost,
    owners: &[String],
) -> HashMap<String, crate::analytics::Identity> {
    let Some(analytics) = host.analytics() else {
        return HashMap::new();
    };
    match analytics.resolve_identities(owners).await {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(error = %e, "resolving conversation owner names failed");
            HashMap::new()
        }
    }
}

/// `GET /admin/api/agents/{agent}/conversations` — every conversation this agent
/// has had, newest first, across all customers. Guarded by `admin_guard`.
async fn admin_conversations(
    ServedTenant(tenant): ServedTenant,
    Path(agent_id): Path<String>,
) -> Response {
    let Some(host) = tenant.host(&agent_id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let sessions = match host.sessions().list(None, ADMIN_CONVERSATIONS_MAX) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(agent = %agent_id, error = %e, "listing conversations failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let owners: Vec<String> = sessions.iter().map(|s| s.owner.clone()).collect();
    let names = resolve_owner_names(&host, &owners).await;
    let conversations: Vec<AdminConversation> = sessions
        .iter()
        .map(|s| AdminConversation {
            session: s.id.to_string(),
            conversation: conversation_id(&s.channel, &agent_id).map(str::to_string),
            display_name: names.get(&s.owner).and_then(|i| i.display_name.clone()),
            owner: s.owner.clone(),
            title: display_title(s),
            updated_at: s.updated_at.to_rfc3339(),
            messages: s.message_count(),
        })
        .collect();
    axum::Json(serde_json::json!({ "conversations": conversations })).into_response()
}

/// `GET /admin/api/agents/{agent}/conversations/{session}` — one conversation's
/// full transcript (user/assistant turns). Guarded by `admin_guard`.
async fn admin_conversation_transcript(
    ServedTenant(tenant): ServedTenant,
    Path((agent_id, session_id)): Path<(String, String)>,
) -> Response {
    let Some(host) = tenant.host(&agent_id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let session = match host.sessions().get(&session_id) {
        Ok(Some(s)) => s,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::error!(agent = %agent_id, error = %e, "loading conversation failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let names = resolve_owner_names(&host, std::slice::from_ref(&session.owner)).await;
    let display_name = names
        .get(&session.owner)
        .and_then(|i| i.display_name.clone());
    axum::Json(serde_json::json!({
        "owner": session.owner,
        "display_name": display_name,
        "title": display_title(&session),
        "messages": transcript(&session),
    }))
    .into_response()
}

/// Default window (days) for the analytics endpoints when `from`/`to` are
/// absent.
const ANALYTICS_DEFAULT_WINDOW_DAYS: i64 = 30;
/// Default and hard cap on the turns drill-down page size.
const ANALYTICS_TURNS_DEFAULT_LIMIT: i64 = 50;
const ANALYTICS_TURNS_MAX_LIMIT: i64 = 200;
/// Upper bound on generated timeseries buckets — a wide window at a fine bucket
/// (e.g. `hour` over years) is rejected rather than asking Postgres for millions
/// of rows. ~5k buckets = 5k hours (~208 days) or 5k days (~13 years).
const ANALYTICS_MAX_BUCKETS: i64 = 5000;

/// The shared query string for every analytics endpoint. Flat (no
/// `#[serde(flatten)]` — `serde_urlencoded`, which backs `axum::Query`, does not
/// support it); endpoints ignore the params they don't use.
#[derive(Debug, Deserialize)]
struct AnalyticsFilter {
    from: Option<String>,
    to: Option<String>,
    agent: Option<String>,
    owner: Option<String>,
    /// `timeseries` only: `day` (default) or `hour`.
    bucket: Option<String>,
    /// `turns` only: opaque page cursor.
    cursor: Option<String>,
    /// `turns` only: page size (default 50, capped at 200).
    limit: Option<i64>,
}

/// A `400 Bad Request` with a short reason in the body.
fn bad_request(msg: &str) -> Response {
    (StatusCode::BAD_REQUEST, msg.to_string()).into_response()
}

/// Resolve the shared filter into an [`AnalyticsQuery`]: RFC3339 `from`/`to`
/// (defaulting to the last 30 days), and empty `agent`/`owner` treated as
/// absent so a blank filter never matches nothing. A malformed timestamp is an
/// `Err` with a reason the caller turns into a 400 — not a silent default.
fn analytics_window(
    filter: &AnalyticsFilter,
) -> Result<crate::analytics::AnalyticsQuery, &'static str> {
    let parse =
        |s: &str| chrono::DateTime::parse_from_rfc3339(s).map(|dt| dt.with_timezone(&chrono::Utc));
    let to = match &filter.to {
        Some(s) => parse(s).map_err(|_| "invalid `to` (expected RFC3339)")?,
        None => chrono::Utc::now(),
    };
    let from = match &filter.from {
        Some(s) => parse(s).map_err(|_| "invalid `from` (expected RFC3339)")?,
        None => to - chrono::Duration::days(ANALYTICS_DEFAULT_WINDOW_DAYS),
    };
    Ok(crate::analytics::AnalyticsQuery {
        from,
        to,
        agent: filter.agent.clone().filter(|s| !s.is_empty()),
        owner: filter.owner.clone().filter(|s| !s.is_empty()),
    })
}

/// Whether a timeseries request would generate more than `ANALYTICS_MAX_BUCKETS`
/// buckets — the guard that keeps a wide window at a fine bucket from asking
/// Postgres for millions of rows.
fn timeseries_window_too_wide(
    q: &crate::analytics::AnalyticsQuery,
    bucket: crate::analytics::TimeBucket,
) -> bool {
    (q.to - q.from).num_seconds() / bucket.step().num_seconds().max(1) > ANALYTICS_MAX_BUCKETS
}

/// Encode a turns cursor as URL-safe base64 of `"{rfc3339}|{id}"`.
fn encode_turns_cursor(c: &crate::analytics::TurnsCursor) -> String {
    use base64::Engine;
    let raw = format!("{}|{}", c.ts.to_rfc3339(), c.id);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw)
}

/// Decode a turns cursor; `None` for any malformed value (→ 400 at the handler).
fn decode_turns_cursor(s: &str) -> Option<crate::analytics::TurnsCursor> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s)
        .ok()?;
    let text = String::from_utf8(bytes).ok()?;
    let (ts, id) = text.split_once('|')?;
    let ts = chrono::DateTime::parse_from_rfc3339(ts)
        .ok()?
        .with_timezone(&chrono::Utc);
    Some(crate::analytics::TurnsCursor {
        ts,
        id: id.parse().ok()?,
    })
}

/// `GET /admin/api/analytics/summary` — KPIs over the filtered window. Guarded
/// by `admin_guard`.
async fn admin_analytics_summary(
    ServedTenant(tenant): ServedTenant,
    Query(filter): Query<AnalyticsFilter>,
) -> Response {
    let query = match analytics_window(&filter) {
        Ok(q) => q,
        Err(msg) => return bad_request(msg),
    };
    match tenant.analytics().summary(&query).await {
        Ok(summary) => axum::Json(summary).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "analytics summary query failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// `GET /admin/api/analytics/timeseries?bucket=day|hour` — zero-filled per-bucket
/// totals. Guarded by `admin_guard`.
async fn admin_analytics_timeseries(
    ServedTenant(tenant): ServedTenant,
    Query(filter): Query<AnalyticsFilter>,
) -> Response {
    let query = match analytics_window(&filter) {
        Ok(q) => q,
        Err(msg) => return bad_request(msg),
    };
    let bucket = match &filter.bucket {
        None => crate::analytics::TimeBucket::Day,
        Some(s) => match crate::analytics::TimeBucket::parse(s) {
            Some(b) => b,
            None => return bad_request("invalid `bucket` (expected day|hour)"),
        },
    };
    if timeseries_window_too_wide(&query, bucket) {
        return bad_request(
            "window too wide for this bucket; narrow the range or use a coarser bucket",
        );
    }
    match tenant.analytics().timeseries(&query, bucket).await {
        Ok(buckets) => axum::Json(serde_json::json!({ "buckets": buckets })).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "analytics timeseries query failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// `GET /admin/api/analytics/by/{dimension}` — cost-ranked rows grouped by
/// `user|agent|model|outcome`. Guarded by `admin_guard`.
async fn admin_analytics_by_dimension(
    ServedTenant(tenant): ServedTenant,
    Path(dimension): Path<String>,
    Query(filter): Query<AnalyticsFilter>,
) -> Response {
    let query = match analytics_window(&filter) {
        Ok(q) => q,
        Err(msg) => return bad_request(msg),
    };
    let Some(dim) = crate::analytics::Dimension::parse(&dimension) else {
        return bad_request("invalid dimension (expected user|agent|model|outcome)");
    };
    match tenant.analytics().by_dimension(&query, dim).await {
        Ok(rows) => axum::Json(serde_json::json!({ "rows": rows })).into_response(),
        Err(e) => {
            tracing::error!(error = %e, dimension = %dimension, "analytics by-dimension query failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// `GET /admin/api/analytics/turns` — cursor-paginated per-turn drill-down.
/// Guarded by `admin_guard`.
async fn admin_analytics_turns(
    ServedTenant(tenant): ServedTenant,
    Query(filter): Query<AnalyticsFilter>,
) -> Response {
    let query = match analytics_window(&filter) {
        Ok(q) => q,
        Err(msg) => return bad_request(msg),
    };
    let limit = filter
        .limit
        .unwrap_or(ANALYTICS_TURNS_DEFAULT_LIMIT)
        .clamp(1, ANALYTICS_TURNS_MAX_LIMIT);
    let cursor = match &filter.cursor {
        Some(c) => match decode_turns_cursor(c) {
            Some(cur) => Some(cur),
            None => return bad_request("invalid cursor"),
        },
        None => None,
    };
    match tenant.analytics().turns(&query, cursor, limit).await {
        Ok(result) => {
            let next_cursor = result.next.as_ref().map(encode_turns_cursor);
            axum::Json(serde_json::json!({
                "turns": result.turns,
                "next_cursor": next_cursor,
            }))
            .into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "analytics turns query failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// `GET /admin/api/analytics/feedback` — cursor-paginated customer ratings over
/// the filtered window. Mirrors `admin_analytics_turns` (shared `AnalyticsFilter`
/// + turns cursor codec). Guarded by `admin_guard`.
async fn admin_analytics_feedback(
    ServedTenant(tenant): ServedTenant,
    Query(filter): Query<AnalyticsFilter>,
) -> Response {
    let query = match analytics_window(&filter) {
        Ok(q) => q,
        Err(msg) => return bad_request(msg),
    };
    let limit = filter
        .limit
        .unwrap_or(ANALYTICS_TURNS_DEFAULT_LIMIT)
        .clamp(1, ANALYTICS_TURNS_MAX_LIMIT);
    let cursor = match &filter.cursor {
        Some(c) => match decode_turns_cursor(c) {
            Some(cur) => Some(cur),
            None => return bad_request("invalid cursor"),
        },
        None => None,
    };
    match tenant.analytics().feedback(&query, cursor, limit).await {
        Ok(result) => {
            let next_cursor = result.next.as_ref().map(encode_turns_cursor);
            axum::Json(serde_json::json!({
                "feedback": result.feedback,
                "next_cursor": next_cursor,
            }))
            .into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "analytics feedback query failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// `GET /admin/api/analytics/grades/summary` — answer-quality distribution over
/// the filtered window. Guarded by `admin_guard`.
async fn admin_analytics_grades_summary(
    ServedTenant(tenant): ServedTenant,
    Query(filter): Query<AnalyticsFilter>,
) -> Response {
    let query = match analytics_window(&filter) {
        Ok(q) => q,
        Err(msg) => return bad_request(msg),
    };
    match tenant.analytics().grades_summary(&query).await {
        Ok(summary) => axum::Json(summary).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "analytics grades-summary query failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// `GET /admin/api/analytics/gaps` — non-`good` grades clustered by topic, most
/// frequent first. Guarded by `admin_guard`.
async fn admin_analytics_gaps(
    ServedTenant(tenant): ServedTenant,
    Query(filter): Query<AnalyticsFilter>,
) -> Response {
    let query = match analytics_window(&filter) {
        Ok(q) => q,
        Err(msg) => return bad_request(msg),
    };
    match tenant.analytics().gaps(&query).await {
        Ok(rows) => axum::Json(serde_json::json!({ "rows": rows })).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "analytics gaps query failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// `GET /admin/api/analytics/grades` — cursor-paginated per-grade drill-down.
/// Mirrors `admin_analytics_feedback`. Guarded by `admin_guard`.
async fn admin_analytics_grades(
    ServedTenant(tenant): ServedTenant,
    Query(filter): Query<AnalyticsFilter>,
) -> Response {
    let query = match analytics_window(&filter) {
        Ok(q) => q,
        Err(msg) => return bad_request(msg),
    };
    let limit = filter
        .limit
        .unwrap_or(ANALYTICS_TURNS_DEFAULT_LIMIT)
        .clamp(1, ANALYTICS_TURNS_MAX_LIMIT);
    let cursor = match &filter.cursor {
        Some(c) => match decode_turns_cursor(c) {
            Some(cur) => Some(cur),
            None => return bad_request("invalid cursor"),
        },
        None => None,
    };
    match tenant.analytics().grades(&query, cursor, limit).await {
        Ok(result) => {
            let next_cursor = result.next.as_ref().map(encode_turns_cursor);
            axum::Json(serde_json::json!({
                "grades": result.grades,
                "next_cursor": next_cursor,
            }))
            .into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "analytics grades query failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// `GET /admin/api/agents/{agent}/lessons` — the durable lessons this agent has
/// learned from its conversations. Guarded by `admin_guard`.
async fn admin_lessons(
    ServedTenant(tenant): ServedTenant,
    Path(agent_id): Path<String>,
) -> Response {
    let Some(host) = tenant.host(&agent_id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let agent = workforce_core::types::AgentId::new(&agent_id);
    let stored = match crate::lessons::list_lessons(host.memory().as_ref(), &agent).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(agent = %agent_id, error = %e, "listing lessons failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let lessons: Vec<serde_json::Value> = stored
        .into_iter()
        .map(|m| {
            let triggers: Vec<&str> = m
                .tags
                .iter()
                .filter_map(|t| t.strip_prefix("trigger:"))
                .collect();
            serde_json::json!({
                "id": m.id.0,
                "content": m.content,
                "importance": m.importance,
                "triggers": triggers,
                "created_at": m.created_at.to_rfc3339(),
            })
        })
        .collect();
    axum::Json(serde_json::json!({ "lessons": lessons })).into_response()
}

/// `DELETE /admin/api/agents/{agent}/lessons/{id}` — prune a bad lesson. Scoped
/// to the agent's shared lessons, so a customer's private memory can't be
/// deleted by id. Guarded by `admin_guard`.
async fn admin_lesson_delete(
    ServedTenant(tenant): ServedTenant,
    operator: axum::Extension<Operator>,
    Path((agent_id, lesson_id)): Path<(String, String)>,
) -> Response {
    let Some(host) = tenant.host(&agent_id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let agent = workforce_core::types::AgentId::new(&agent_id);
    match crate::lessons::forget_lesson(host.memory().as_ref(), &agent, &lesson_id).await {
        Ok(true) => {
            tenant
                .audit
                .record(
                    &operator.0 .0,
                    "lesson.delete",
                    &format!("{agent_id}/{lesson_id}"),
                    "",
                )
                .await;
            axum::Json(serde_json::json!({"ok": true})).into_response()
        }
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::error!(agent = %agent_id, error = %e, "deleting lesson failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[derive(Debug, Deserialize)]
struct ForgetCustomerBody {
    owner: String,
}

/// `POST /admin/api/customers/forget` — erase everything the edge holds about
/// one customer, given their owner key: memories, conversations, the content
/// of their analytics and their files, on every agent including removed ones.
/// A key that is not a customer's is a 400, and a customer active within
/// [`crate::erasure::SETTLE`] a 409. The body reports each part; any part that
/// failed makes it a 500, so an erasure is never reported as done when it was
/// not. The audit records that a customer was forgotten, never whom. Guarded
/// by `admin_guard`.
async fn admin_customer_forget(
    ServedTenant(tenant): ServedTenant,
    operator: axum::Extension<Operator>,
    axum::Json(body): axum::Json<ForgetCustomerBody>,
) -> Response {
    let plane = tenant.data_plane();
    let sessions = |agent: &str| -> Arc<dyn workforce_common::session::SessionStore> {
        plane.session_store(agent)
    };
    let hosted: Vec<String> = tenant
        .all_hosts()
        .iter()
        .map(|host| host.agent().id.clone())
        .collect();
    let stores = crate::erasure::Stores {
        memory: plane.memory.as_ref(),
        sessions: &sessions,
        analytics: &plane.analytics,
        files: plane.artifacts.as_ref().map(|a| a.store.as_ref()),
    };
    let refused = |status: StatusCode, error: &str| {
        (status, axum::Json(serde_json::json!({ "error": error }))).into_response()
    };
    let erasure = match crate::erasure::forget_customer(&body.owner, &hosted, &stores).await {
        Ok(erasure) => erasure,
        Err(crate::erasure::Refusal::NotACustomer) => {
            return refused(StatusCode::BAD_REQUEST, "not a customer's key")
        }
        Err(crate::erasure::Refusal::Active) => {
            return refused(
                StatusCode::CONFLICT,
                &format!(
                    "this customer used an agent in the last {} minutes; forget them once they have been idle that long",
                    crate::erasure::SETTLE.as_secs() / 60
                ),
            )
        }
        Err(crate::erasure::Refusal::Unchecked(e)) => {
            tracing::error!(error = %e, "could not tell whether a customer is active");
            return refused(
                StatusCode::SERVICE_UNAVAILABLE,
                "could not tell whether the customer is active; nothing was erased",
            );
        }
    };
    let detail = format!(
        "memories={} conversations={} analytics={} files={} failures={}",
        erasure.memories.erased,
        erasure.conversations.erased,
        erasure.analytics.erased,
        erasure.files.erased,
        erasure.failures(),
    );
    tenant
        .audit
        .record(&operator.0 .0, "customer.forget", "", &detail)
        .await;
    let status = if erasure.complete() {
        StatusCode::OK
    } else {
        tracing::error!(
            failures = erasure.failures(),
            "forgetting a customer left data behind"
        );
        StatusCode::INTERNAL_SERVER_ERROR
    };
    (status, axum::Json(erasure)).into_response()
}

/// `GET /admin/api/agents/{agent}/knowledge` — the agent's training documents.
/// Guarded by `admin_guard`.
async fn admin_knowledge(
    ServedTenant(tenant): ServedTenant,
    Path(agent_id): Path<String>,
) -> Response {
    if tenant.host(&agent_id).is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    match tenant.knowledge.list(&agent_id).await {
        Ok(docs) => axum::Json(serde_json::json!({ "documents": docs })).into_response(),
        Err(e) => {
            tracing::error!(agent = %agent_id, error = %e, "listing knowledge failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[derive(Debug, Deserialize)]
struct KnowledgeBody {
    title: String,
    content: String,
}

/// `PUT /admin/api/agents/{agent}/knowledge/{id}` — add or replace a training
/// document; re-chunks and re-embeds it. Guarded by `admin_guard`.
async fn admin_knowledge_put(
    ServedTenant(tenant): ServedTenant,
    operator: axum::Extension<Operator>,
    Path((agent_id, doc_id)): Path<(String, String)>,
    body: axum::Json<KnowledgeBody>,
) -> Response {
    if let Err(e) = tenant.ensure_config_writable() {
        return e.into_response();
    }
    if tenant.host(&agent_id).is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    if doc_id.len() > 128 {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            axum::Json(serde_json::json!({"error": "document id too long (max 128 chars)"})),
        )
            .into_response();
    }
    if body.content.trim().is_empty() {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            axum::Json(serde_json::json!({"error": "document content is empty"})),
        )
            .into_response();
    }
    // Serialize with other config mutations (embedding is a few round trips).
    let _guard = tenant.edit_lock.lock().await;
    match tenant
        .knowledge
        .upsert(&agent_id, &doc_id, &body.title, &body.content)
        .await
    {
        Ok(chunks) => {
            let item = format!("{agent_id}/{doc_id}");
            if let Ok(json) = serde_json::to_string(
                &serde_json::json!({"title": body.title, "content": body.content}),
            ) {
                tenant
                    .revisions
                    .record("knowledge", &item, &json, &operator.0 .0)
                    .await;
            }
            let detail: String = body.title.chars().take(120).collect();
            tenant
                .audit
                .record(&operator.0 .0, "knowledge.save", &item, &detail)
                .await;
            axum::Json(serde_json::json!({"ok": true, "chunks": chunks})).into_response()
        }
        Err(e) => match e.downcast_ref::<PlanLimit>() {
            Some(limit) => {
                tracing::info!(agent = %agent_id, doc = %doc_id, "knowledge save refused: plan limit reached");
                plan_limit_response(*limit)
            }
            None => {
                tracing::error!(agent = %agent_id, doc = %doc_id, error = %e, "saving knowledge failed");
                (
                    StatusCode::BAD_GATEWAY,
                    axum::Json(serde_json::json!({"error": "could not index the document"})),
                )
                    .into_response()
            }
        },
    }
}

/// The 422 a write refused by the plan gets: the code every surface uses
/// and the sentence the operator reads.
fn plan_limit_response(limit: PlanLimit) -> Response {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        axum::Json(serde_json::json!({
            "error": PlanLimit::CODE,
            "message": limit.message(),
        })),
    )
        .into_response()
}

/// The 422 an agent create or import refused by the plan's agent cap gets:
/// the code of [`plan_limit_response`], with the numbered sentence from
/// [`crate::tenant::Plan::admit_agents`] in place of the customer-facing
/// one — the operator is the one who can act on the cap.
fn agent_cap_response(message: String) -> Response {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        axum::Json(serde_json::json!({
            "error": PlanLimit::CODE,
            "message": message,
        })),
    )
        .into_response()
}

/// `DELETE /admin/api/agents/{agent}/knowledge/{id}` — remove a document and its
/// chunks. Guarded by `admin_guard`.
async fn admin_knowledge_delete(
    ServedTenant(tenant): ServedTenant,
    operator: axum::Extension<Operator>,
    Path((agent_id, doc_id)): Path<(String, String)>,
) -> Response {
    if let Err(e) = tenant.ensure_config_writable() {
        return e.into_response();
    }
    if tenant.host(&agent_id).is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    let _guard = tenant.edit_lock.lock().await;
    match tenant.knowledge.delete(&agent_id, &doc_id).await {
        Ok(true) => {
            tenant
                .audit
                .record(
                    &operator.0 .0,
                    "knowledge.delete",
                    &format!("{agent_id}/{doc_id}"),
                    "",
                )
                .await;
            axum::Json(serde_json::json!({"ok": true})).into_response()
        }
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::error!(agent = %agent_id, doc = %doc_id, error = %e, "deleting knowledge failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// A slug id from a filename/title: lowercase alnum + dashes, ≤128 chars.
fn slug(s: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    let s = out.trim_matches('-');
    let s: String = s.chars().take(128).collect();
    if s.is_empty() {
        "doc".into()
    } else {
        s
    }
}

/// One file's outcome in an upload batch.
#[derive(serde::Serialize)]
struct UploadResult {
    name: String,
    ok: bool,
    chunks: usize,
    error: Option<String>,
}

/// Cap on files accepted in one upload request.
const MAX_UPLOAD_FILES: usize = 50;

/// `POST /admin/api/agents/{agent}/knowledge/upload` — multipart upload of one or
/// more training files. Each file is text-extracted (txt/md/csv/html/pdf/docx…),
/// chunked, and embedded, keyed by a slug of its filename (re-uploading a name
/// replaces it). Returns a per-file result so one bad file can't sink the batch.
async fn admin_knowledge_upload(
    ServedTenant(tenant): ServedTenant,
    operator: axum::Extension<Operator>,
    Path(agent_id): Path<String>,
    mut multipart: axum::extract::Multipart,
) -> Response {
    if let Err(e) = tenant.ensure_config_writable() {
        return e.into_response();
    }
    if tenant.host(&agent_id).is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    // NB: no lock is held across the multipart body read or extraction — a slow
    // client (slowloris) or a heavy PDF must not wedge the whole admin edit
    // plane. `edit_lock` is taken per file, only around the store write.
    let mut results: Vec<UploadResult> = Vec::new();
    loop {
        if results.len() >= MAX_UPLOAD_FILES {
            break;
        }
        let field = match multipart.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => {
                tracing::warn!(error = %e, "multipart read failed");
                return (
                    StatusCode::BAD_REQUEST,
                    axum::Json(serde_json::json!({"error": "malformed upload"})),
                )
                    .into_response();
            }
        };
        let name = field.file_name().map(str::to_string).unwrap_or_default();
        if name.trim().is_empty() {
            continue;
        }
        let bytes = match field.bytes().await {
            Ok(b) => b,
            Err(_) => {
                results.push(UploadResult {
                    name,
                    ok: false,
                    chunks: 0,
                    error: Some("file too large or truncated".into()),
                });
                continue;
            }
        };
        // Extraction is CPU-bound and can panic on exotic files — isolate it on a
        // blocking thread, and time-box it so one pathological PDF can't pin a
        // thread forever (the thread may run on, but the request + batch move on).
        let (fname, data) = (name.clone(), bytes.to_vec());
        let extracted = match tokio::time::timeout(
            std::time::Duration::from_secs(30),
            tokio::task::spawn_blocking(move || crate::extract::extract(&fname, &data)),
        )
        .await
        {
            Ok(join) => join.unwrap_or_else(|_| Err("extraction crashed on this file".into())),
            Err(_) => Err("file took too long to process".into()),
        };
        let text = match extracted {
            Ok(t) => t,
            Err(e) => {
                results.push(UploadResult {
                    name,
                    ok: false,
                    chunks: 0,
                    error: Some(e),
                });
                continue;
            }
        };
        let id = slug(&name);
        // Serialize just the store write with other config mutations.
        let _guard = tenant.edit_lock.lock().await;
        match tenant.knowledge.upsert(&agent_id, &id, &name, &text).await {
            Ok(chunks) => {
                let item = format!("{agent_id}/{id}");
                if let Ok(json) =
                    serde_json::to_string(&serde_json::json!({"title": name, "content": text}))
                {
                    tenant
                        .revisions
                        .record("knowledge", &item, &json, &operator.0 .0)
                        .await;
                }
                let detail: String = name.chars().take(120).collect();
                tenant
                    .audit
                    .record(&operator.0 .0, "knowledge.upload", &item, &detail)
                    .await;
                results.push(UploadResult {
                    name,
                    ok: true,
                    chunks,
                    error: None,
                });
            }
            Err(e) => {
                // A file the plan refuses is one result in the batch, like
                // any other file that was not indexed; the message names
                // the cap so the operator knows it was not the file.
                let error = match e.downcast_ref::<PlanLimit>() {
                    Some(limit) => {
                        tracing::info!(agent = %agent_id, file = %name, "knowledge upload refused: plan limit reached");
                        limit.message().to_string()
                    }
                    None => {
                        tracing::error!(agent = %agent_id, file = %name, error = %e, "knowledge upload embed failed");
                        "indexing failed".to_string()
                    }
                };
                results.push(UploadResult {
                    name,
                    ok: false,
                    chunks: 0,
                    error: Some(error),
                });
            }
        }
    }
    axum::Json(serde_json::json!({ "results": results })).into_response()
}

/// `GET /admin/api/revisions/{kind}/{item}` — a config item's version history.
/// `kind` ∈ agent|playbook|knowledge; `item` is the item id (for knowledge,
/// `agent/doc`). Guarded.
async fn admin_revisions(
    ServedTenant(tenant): ServedTenant,
    Path((kind, item)): Path<(String, String)>,
) -> Response {
    match tenant.revisions.list(&kind, &item).await {
        Ok(revs) => axum::Json(serde_json::json!({ "revisions": revs })).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "listing revisions failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// `GET /admin/api/revisions/{kind}/{item}/{revision}` — one revision's content.
async fn admin_revision_content(
    ServedTenant(tenant): ServedTenant,
    Path((kind, item, revision)): Path<(String, String, i32)>,
) -> Response {
    match tenant.revisions.get(&kind, &item, revision).await {
        Ok(Some(content)) => axum::Json(serde_json::json!({ "content": content })).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::error!(error = %e, "loading revision failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// `POST /admin/api/revisions/{kind}/{item}/{revision}/restore` — re-apply a past
/// revision. Runs the same validated save path as a normal edit (so a restore is
/// itself snapshotted + audited), and hot-applies. Guarded.
async fn admin_revision_restore(
    ServedTenant(tenant): ServedTenant,
    operator: axum::Extension<Operator>,
    Path((kind, item, revision)): Path<(String, String, i32)>,
) -> Response {
    if let Err(e) = tenant.ensure_config_writable() {
        return e.into_response();
    }
    let content = match tenant.revisions.get(&kind, &item, revision).await {
        Ok(Some(c)) => c,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::error!(error = %e, "loading revision failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let who = &operator.0 .0;
    let _guard = tenant.edit_lock.lock().await;
    let result: Result<(), Response> = match kind.as_str() {
        "agent" => match serde_json::from_str::<crate::agent::EdgeAgent>(&content) {
            // Require a live host (real agents always have one). Revisions
            // carry no credential: keep the running one when the snapshot
            // names the same server, so a restore neither wipes the secret
            // nor sends it to a server it was not set for.
            Ok(mut agent) if agent.id == item => match tenant.host(&item) {
                Some(h) => {
                    agent.embed.adopt_credentials(&h.agent().embed);
                    // We already hold edit_lock — use the lock-free inner or we
                    // would deadlock re-taking the non-reentrant lock.
                    tenant.apply_agent_edit_inner(agent).await.map_err(|e| {
                        tracing::error!(error = %e, "restore agent failed");
                        StatusCode::BAD_GATEWAY.into_response()
                    })
                }
                None => Err(StatusCode::NOT_FOUND.into_response()),
            },
            _ => Err(StatusCode::UNPROCESSABLE_ENTITY.into_response()),
        },
        "playbook" => match workforce_playbook::PlaybookDefinition::from_toml(&content) {
            Ok(def) if def.playbook.id == item => {
                let r = tenant.playbook_store.upsert(&item, &content).await;
                match r {
                    Ok(()) => tenant
                        .reload_playbooks()
                        .await
                        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
                    Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR.into_response()),
                }
            }
            _ => Err(StatusCode::UNPROCESSABLE_ENTITY.into_response()),
        },
        "knowledge" => {
            let (agent_id, doc_id) = match item.split_once('/') {
                Some(p) => p,
                None => return StatusCode::UNPROCESSABLE_ENTITY.into_response(),
            };
            let doc: serde_json::Value = serde_json::from_str(&content).unwrap_or_default();
            let title = doc.get("title").and_then(|v| v.as_str()).unwrap_or("");
            let body = doc.get("content").and_then(|v| v.as_str()).unwrap_or("");
            tenant
                .knowledge
                .upsert(agent_id, doc_id, title, body)
                .await
                .map(|_| ())
                .map_err(|_| StatusCode::BAD_GATEWAY.into_response())
        }
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    match result {
        Ok(()) => {
            // The restore IS a new save — snapshot it and audit it.
            tenant.revisions.record(&kind, &item, &content, who).await;
            tenant
                .audit
                .record(
                    who,
                    &format!("{kind}.restore"),
                    &item,
                    &format!("to rev {revision}"),
                )
                .await;
            axum::Json(serde_json::json!({"ok": true})).into_response()
        }
        Err(resp) => resp,
    }
}

/// `GET /admin/api/audit` — the admin audit trail, newest first. Guarded.
async fn admin_audit(ServedTenant(tenant): ServedTenant) -> Response {
    match tenant.audit.recent(500).await {
        Ok(entries) => axum::Json(serde_json::json!({ "entries": entries })).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "listing audit failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// `GET /admin` + `/admin/{*path}` — the operator SPA. The shell itself is
/// public (it only renders a login form until `/admin/api/me` succeeds); all
/// data lives behind the guarded `/admin/api/*` routes. 404 when admin is
/// disabled, so a locked-down install exposes nothing.
async fn admin_spa(tenant: &TenantRuntime, path: &str) -> Response {
    if tenant.admin.is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    match crate::assets::admin_asset(path) {
        Some((content_type, bytes)) => {
            let cache = if path.contains("/assets/") {
                "public, max-age=31536000, immutable"
            } else {
                "no-cache"
            };
            (
                [
                    (header::CONTENT_TYPE, content_type),
                    (header::CACHE_CONTROL, cache.to_string()),
                    (
                        header::CONTENT_SECURITY_POLICY,
                        // The admin console: block injected/inline scripts (only
                        // the bundle + Google Identity Services), objects, base
                        // hijacking, and — unlike the widget — being framed
                        // (clickjacking). img/style/connect/frame left to GSI.
                        "script-src 'self' https://accounts.google.com; object-src 'none'; base-uri 'self'; frame-ancestors 'none'"
                            .to_string(),
                    ),
                ],
                bytes,
            )
                .into_response()
        }
        None => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

async fn admin_root(ServedTenant(tenant): ServedTenant) -> Response {
    admin_spa(&tenant, "").await
}

async fn admin_path(ServedTenant(tenant): ServedTenant, Path(path): Path<String>) -> Response {
    admin_spa(&tenant, &path).await
}

/// `GET /health` — unauthenticated liveness for the load balancer and the
/// container health check. 200 once the process is serving; never rate-limited.
async fn health() -> &'static str {
    "ok"
}

/// `GET /version` — the build this edge is running, as JSON. Unauthenticated
/// like `/health`: it describes our own deployment, never a customer's data,
/// and it is what makes "did the deploy actually take?" answerable from a
/// browser instead of from an SSM session on the host.
async fn version() -> Response {
    axum::Json(crate::build_info::as_json()).into_response()
}

/// The agent segment of a conversation-API path, for the CORS gate.
fn agent_from_conversations_path(path: &str) -> Option<&str> {
    let rest = path.strip_prefix("/public/conversations/")?;
    let agent = rest.split('/').next().unwrap_or("");
    (!agent.is_empty()).then_some(agent)
}

/// The public router.
pub fn router(state: Arc<EdgeState>) -> Router {
    // The conversation API is called with `fetch` from the embedding page, so
    // (unlike the socket) it needs CORS. Origins are reflected only when the
    // target agent's own allowlist admits them — the browser-side counterpart
    // of the `Origin` check `authenticate` still enforces server-side.
    let cors_state = Arc::clone(&state);
    let cors = tower_http::cors::CorsLayer::new()
        .allow_methods([
            axum::http::Method::GET,
            axum::http::Method::POST,
            axum::http::Method::DELETE,
        ])
        // The feedback POST sends a JSON body, so the browser preflights it and
        // needs both POST above and `Content-Type` here — without them the widget
        // POST fails CORS in-browser while every server-side test still passes.
        .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE])
        .allow_origin(tower_http::cors::AllowOrigin::predicate(
            move |origin, parts| {
                // The tenant comes from the host, as it does for the handler
                // behind the preflight; a host that names no tenant admits
                // no origin.
                let peer = parts
                    .extensions
                    .get::<ConnectInfo<SocketAddr>>()
                    .map(|c| c.0.ip());
                let host = request_host(cors_state.trusted_proxies(), &parts.headers, peer);
                let Resolved::Tenant(tenant) =
                    cors_state.resolve(host.as_deref().unwrap_or_default())
                else {
                    return false;
                };
                agent_from_conversations_path(parts.uri.path())
                    .and_then(|id| tenant.host(id))
                    .zip(origin.to_str().ok())
                    .is_some_and(|(host, origin)| host.agent().embed.origin_allowed(origin))
            },
        ));
    let conversations = Router::new()
        .route("/public/conversations/{agent}", get(list_conversations))
        .route(
            "/public/conversations/{agent}/{conversation}",
            get(get_conversation).delete(delete_conversation),
        )
        .route(
            "/public/conversations/{agent}/{conversation}/feedback",
            axum::routing::post(submit_feedback),
        )
        .route(
            "/public/upload/{agent}",
            axum::routing::post(crate::upload::public_upload)
                // Customer uploads carry files; the per-file cap is enforced
                // while streaming, this is only the outer backstop.
                .layer(axum::extract::DefaultBodyLimit::max(
                    crate::agent::MAX_UPLOAD_MAX_MB as usize * crate::upload::MAX_UPLOAD_FILES * 1024 * 1024,
                )),
        )
        .route("/public/files/{agent}/{id}", get(download_file))
        .route("/public/tables/{agent}/{id}", get(table_page))
        .layer(cors);
    // Guarded admin API: default-deny via `admin_guard`. login/logout stay
    // outside the guard (login is how you get a session; logout just clears the
    // cookie). New admin data routes are added HERE and protected for free.
    let admin_api = Router::new()
        .route("/admin/api/me", get(admin_me))
        .route(
            "/admin/api/settings",
            get(admin_settings).put(admin_settings_put),
        )
        .route(
            "/admin/api/settings/llm-key",
            axum::routing::put(admin_llm_key_put).delete(admin_llm_key_delete),
        )
        .route(
            "/admin/api/settings/{secret}",
            axum::routing::put(admin_secret_put).delete(admin_secret_delete),
        )
        .route("/admin/api/setup", get(admin_setup))
        .route(
            "/admin/api/mcp/discover",
            axum::routing::post(admin_mcp_discover),
        )
        .route(
            "/admin/api/oidc/check",
            axum::routing::post(admin_oidc_check),
        )
        .route("/admin/api/audit", get(admin_audit))
        .route("/admin/api/revisions/{kind}/{item}", get(admin_revisions))
        .route(
            "/admin/api/revisions/{kind}/{item}/{revision}",
            get(admin_revision_content),
        )
        .route(
            "/admin/api/revisions/{kind}/{item}/{revision}/restore",
            axum::routing::post(admin_revision_restore),
        )
        .route(
            "/admin/api/operators",
            get(admin_operators).post(admin_operator_add),
        )
        .route(
            "/admin/api/operators/{kind}/{value}",
            axum::routing::delete(admin_operator_delete),
        )
        .route(
            "/admin/api/agents",
            get(admin_agents).post(admin_agent_create),
        )
        .route(
            "/admin/api/agents/{id}",
            axum::routing::put(admin_agent_update).delete(admin_agent_delete),
        )
        .route("/admin/api/git/sync", axum::routing::post(admin_git_sync))
        .route(
            "/admin/api/import/git",
            axum::routing::post(admin_import_git),
        )
        .route("/admin/api/playbooks", get(admin_playbooks))
        .route(
            "/admin/api/playbooks/{id}",
            axum::routing::put(admin_playbook_put).delete(admin_playbook_delete),
        )
        .route(
            "/admin/api/agents/{agent}/conversations",
            get(admin_conversations),
        )
        .route(
            "/admin/api/agents/{agent}/conversations/{session}",
            get(admin_conversation_transcript),
        )
        .route("/admin/api/analytics/summary", get(admin_analytics_summary))
        .route(
            "/admin/api/analytics/timeseries",
            get(admin_analytics_timeseries),
        )
        .route(
            "/admin/api/analytics/by/{dimension}",
            get(admin_analytics_by_dimension),
        )
        .route("/admin/api/analytics/turns", get(admin_analytics_turns))
        .route(
            "/admin/api/analytics/feedback",
            get(admin_analytics_feedback),
        )
        .route(
            "/admin/api/analytics/grades/summary",
            get(admin_analytics_grades_summary),
        )
        .route("/admin/api/analytics/gaps", get(admin_analytics_gaps))
        .route("/admin/api/analytics/grades", get(admin_analytics_grades))
        .route("/admin/api/agents/{agent}/lessons", get(admin_lessons))
        .route(
            "/admin/api/agents/{agent}/lessons/{id}",
            axum::routing::delete(admin_lesson_delete),
        )
        .route(
            "/admin/api/customers/forget",
            axum::routing::post(admin_customer_forget),
        )
        .route("/admin/api/agents/{agent}/knowledge", get(admin_knowledge))
        .route(
            "/admin/api/agents/{agent}/knowledge/upload",
            axum::routing::post(admin_knowledge_upload)
                // Uploads carry files — lift the default 2 MiB body cap to 32 MiB.
                .layer(axum::extract::DefaultBodyLimit::max(32 * 1024 * 1024)),
        )
        .route(
            "/admin/api/agents/{agent}/knowledge/{id}",
            axum::routing::put(admin_knowledge_put).delete(admin_knowledge_delete),
        )
        .layer(axum::middleware::from_fn_with_state(
            Arc::clone(&state),
            admin_guard,
        ));
    let metrics = state
        .metrics
        .clone()
        .map_or_else(Router::new, crate::metrics::router);
    Router::new()
        .route("/health", get(health))
        .route("/version", get(version))
        .merge(metrics)
        .route("/admin/api/login", axum::routing::post(admin_login))
        .route(
            "/admin/api/login/oidc",
            axum::routing::post(admin_login_oidc),
        )
        .route("/admin/api/login/oidc/start", get(admin_login_oidc_start))
        .route(
            "/admin/api/login/oidc/callback",
            get(admin_login_oidc_callback),
        )
        .route("/admin/api/login/config", get(admin_login_config))
        .route("/admin/api/logout", axum::routing::post(admin_logout))
        .merge(admin_api)
        .merge(crate::platform::router(Arc::clone(&state)))
        .route("/admin", get(admin_root))
        .route("/admin/", get(admin_root))
        .route("/admin/{*path}", get(admin_path))
        .route("/embed/{agent}", get(public_widget))
        .route("/widget/", get(public_widget_asset))
        .route("/widget/{*path}", get(public_widget_asset))
        .route("/demo/{agent}", get(public_demo))
        .route("/public/ws/{agent}", get(public_chat_ws))
        .merge(conversations)
        .with_state(state)
}

/// Bind and serve the public surface.
pub async fn serve(config: &EdgeConfig, state: Arc<EdgeState>) -> Result<()> {
    let addr: SocketAddr = config
        .server
        .bind
        .parse()
        .wrap_err_with(|| format!("invalid server.bind {:?}", config.server.bind))?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .wrap_err_with(|| format!("binding {addr}"))?;
    tracing::info!(%addr, "workforce-edge listening");
    // On SIGTERM (swarm stop) or ctrl_c, cancel the shared token: parked sockets
    // close, in-flight turns finish and then close, axum stops accepting new
    // connections, and the process exits once every socket is done — or swarm's
    // stop-grace-period elapses and it is killed.
    let shutdown = state.shutdown.clone();
    // Every turn of every tenant holds a global permit, so draining the global
    // semaphore means every tenant's turns are done.
    let turn_limit = Arc::clone(&state.global_turn_limit);
    let max_turns = config.server.max_concurrent_turns.max(1) as u32;
    tokio::spawn(await_shutdown_signal(shutdown.clone()));
    spawn_tenant_loops(Arc::clone(&state));
    axum::serve(
        listener,
        router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move { shutdown.cancelled().await })
    .await
    .wrap_err("edge server terminated")?;
    // The listener is closed and `run_socket` starts no new turns; wait (bounded)
    // for turns already running to release their permits, so a deploy delivers
    // their answers instead of the process exiting from under them. Bounded well
    // under the swarm stop-grace-period so a wedged turn can't hold exit forever.
    drain_turns(&turn_limit, max_turns).await;
    Ok(())
}

/// Longest [`serve`] waits for in-flight turns to finish after the listener
/// closes. Sized ABOVE the per-turn ceiling (`MAX_TURN`, 240s): any turn still
/// legally running at deploy time gets to finish, save its answer and leave a
/// terminal note — at 150s a turn 150-240s old was hard-killed with nothing
/// saved, so the widget burned its full recovery poll for an answer that was
/// never coming. The deployment's stop grace must exceed this (compose
/// `stop_grace_period` / ECS `stopTimeout` ≥ 270s) or the platform's SIGKILL
/// re-creates the same hole from outside.
const DRAIN_DEADLINE: Duration = Duration::from_secs(250);

/// Acquire every turn permit — which only succeeds once all in-flight turns have
/// released theirs — or give up at the deadline so exit is never blocked forever.
async fn drain_turns(turn_limit: &Semaphore, max_turns: u32) {
    match tokio::time::timeout(DRAIN_DEADLINE, turn_limit.acquire_many(max_turns)).await {
        Ok(Ok(_permits)) => tracing::info!("in-flight turns drained; exiting"),
        Ok(Err(_)) => {} // semaphore closed — nothing left to wait for
        Err(_) => tracing::warn!(
            secs = DRAIN_DEADLINE.as_secs(),
            "drain deadline reached with turns still running; exiting anyway"
        ),
    }
}

/// Resolve on the first SIGTERM (swarm stop) or ctrl_c (local dev), then cancel
/// `token` so [`serve`] drains and exits gracefully.
async fn await_shutdown_signal(token: CancellationToken) {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(e) => tracing::warn!(error = %e, "cannot listen for SIGTERM; ctrl_c only"),
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
    tracing::info!("shutdown signal received — draining in-flight turns before exit");
    token.cancel();
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_common::session::{Session, SessionStore};

    fn edit_from(json: serde_json::Value) -> AgentEdit {
        serde_json::from_value(json).unwrap()
    }

    #[tokio::test]
    async fn drain_turns_returns_immediately_when_no_turn_is_running() {
        let sem = Semaphore::new(4);
        // All permits free ⇒ acquire_many succeeds at once, well under the deadline.
        tokio::time::timeout(Duration::from_secs(1), drain_turns(&sem, 4))
            .await
            .expect("drain must return promptly when no turns are in flight");
    }

    #[tokio::test]
    async fn drain_turns_waits_for_an_in_flight_turn_then_completes() {
        let sem = Arc::new(Semaphore::new(2));
        // Simulate one running turn holding a permit.
        let held = Arc::clone(&sem).acquire_owned().await.unwrap();
        // Drain cannot finish while the turn holds its permit.
        let blocked = tokio::time::timeout(Duration::from_millis(150), drain_turns(&sem, 2)).await;
        assert!(
            blocked.is_err(),
            "drain must wait while a turn is still running"
        );
        // The turn finishes; drain now completes.
        drop(held);
        tokio::time::timeout(Duration::from_secs(1), drain_turns(&sem, 2))
            .await
            .expect("drain must complete once the turn releases its permit");
    }

    #[test]
    fn agent_edit_patches_only_supplied_fields_and_keeps_an_unnamed_mcp() {
        let mut agent = EdgeAgent {
            id: "a".into(),
            display_name: "Old".into(),
            system_prompt: "old prompt".into(),
            identity: Default::default(),
            welcome: "old".into(),
            model: Some("standard".into()),
            playbook: Some("tour".into()),
            playbooks: vec!["tour".into()],
            progress_labels: vec![],
            derived_figures: Default::default(),
            embed: crate::agent::EmbedSettings {
                mcp: Some(crate::agent::McpSettings {
                    url: "http://mcp".into(),
                    discovery_credential: Some("SECRET".into()),
                    ..Default::default()
                }),
                allowed_origins: vec!["https://old".into()],
                ..Default::default()
            },
        };
        // Only identity + welcome supplied: everything else untouched, and
        // saving identity files retires the legacy prompt.
        apply_edit(
            &mut agent,
            edit_from(
                serde_json::json!({"identity": {"identity": "new persona"}, "welcome": "hi"}),
            ),
        );
        assert_eq!(agent.identity.identity, "new persona");
        assert_eq!(
            agent.system_prompt, "",
            "identity save clears legacy prompt"
        );
        assert_eq!(agent.welcome, "hi");
        assert_eq!(agent.display_name, "Old", "absent field untouched");
        assert_eq!(agent.model.as_deref(), Some("standard"), "absent = keep");
        // An edit that says nothing about the MCP block leaves it, secret included.
        assert_eq!(
            agent
                .embed
                .mcp
                .as_ref()
                .unwrap()
                .discovery_credential
                .as_deref(),
            Some("SECRET")
        );

        // model present-and-null clears to the install default; blank also clears.
        apply_edit(&mut agent, edit_from(serde_json::json!({"model": null})));
        assert_eq!(agent.model, None, "explicit null clears");
        agent.model = Some("standard".into());
        apply_edit(&mut agent, edit_from(serde_json::json!({"model": "   "})));
        assert_eq!(agent.model, None, "blank clears");

        // Lists and allowlists replace wholesale.
        apply_edit(
            &mut agent,
            edit_from(serde_json::json!({"allowed_origins": ["https://new"], "playbooks": []})),
        );
        assert_eq!(agent.embed.allowed_origins, vec!["https://new".to_string()]);
        assert!(agent.playbooks.is_empty());
    }

    #[test]
    fn agent_edit_sets_and_flips_rich_output_flags() {
        let mut agent = EdgeAgent::default();
        assert!(!agent.embed.data_tables_enabled());
        assert!(!agent.embed.charts_enabled());
        assert!(!agent.embed.forms_enabled());
        assert!(!agent.embed.maps_enabled());
        assert!(!agent.embed.diagrams_enabled());
        assert!(!agent.embed.excalidraw_export_enabled());

        apply_edit(
            &mut agent,
            edit_from(
                serde_json::json!({"data_tables": true, "charts": true, "forms": true, "maps": true, "diagrams": true, "excalidraw_export": true}),
            ),
        );
        assert!(agent.embed.data_tables_enabled());
        assert!(agent.embed.charts_enabled());
        assert!(agent.embed.forms_enabled());
        assert!(agent.embed.maps_enabled());
        assert!(agent.embed.diagrams_enabled());
        assert!(agent.embed.excalidraw_export_enabled());

        // Absent fields leave the flags untouched.
        apply_edit(&mut agent, edit_from(serde_json::json!({"welcome": "hi"})));
        assert!(agent.embed.data_tables_enabled());
        assert!(agent.embed.charts_enabled());
        assert!(agent.embed.forms_enabled());
        assert!(agent.embed.maps_enabled());
        assert!(agent.embed.diagrams_enabled());

        // An explicit false switches a feature back off.
        apply_edit(
            &mut agent,
            edit_from(
                serde_json::json!({"charts": false, "forms": false, "maps": false, "diagrams": false, "excalidraw_export": false}),
            ),
        );
        assert!(agent.embed.data_tables_enabled());
        assert!(!agent.embed.charts_enabled());
        assert!(!agent.embed.forms_enabled());
        assert!(!agent.embed.maps_enabled());
        assert!(!agent.embed.diagrams_enabled());
        assert!(!agent.embed.excalidraw_export_enabled());
    }

    #[test]
    fn conversation_channel_and_id_round_trip() {
        let ch = conversation_channel("assistant", "c-123");
        assert_eq!(ch, "embed:assistant:c-123");
        assert_eq!(conversation_id(&ch, "assistant"), Some("c-123"));
    }

    #[test]
    fn conversation_id_rejects_other_namespaces() {
        // Another agent's channel, non-embed channels, and prefix-tricks all
        // fall out of the listing rather than leaking across agents.
        assert_eq!(conversation_id("embed:other:c-1", "assistant"), None);
        assert_eq!(conversation_id("slack:C123", "assistant"), None);
        assert_eq!(conversation_id("embed:assistant:", "assistant"), None);
        assert_eq!(
            conversation_id("embed:assistant2:c-1", "assistant"),
            None,
            "agent id must match a whole segment, not a prefix"
        );
    }

    #[test]
    fn summarize_uses_label_and_falls_back() {
        let mut s = Session::new("embed:assistant:c-9", "ext:i:sub", "assistant");
        let sum = summarize(&s, "assistant").unwrap();
        assert_eq!(sum.conversation, "c-9");
        assert_eq!(sum.title, "New chat");
        s.label = Some("Shift question".into());
        assert_eq!(summarize(&s, "assistant").unwrap().title, "Shift question");
        // The store's `{channel}:{owner}` placeholder is hidden, not shown as id.
        s.label = Some(format!("{}:{}", s.channel, s.owner));
        assert_eq!(summarize(&s, "assistant").unwrap().title, "New chat");
        // A session from another namespace never appears in the list.
        let other = Session::new("slack:C1", "ext:i:sub", "assistant");
        assert!(summarize(&other, "assistant").is_none());
    }

    #[test]
    fn transcript_exposes_only_user_and_assistant_turns() {
        let mut s = Session::new("embed:assistant:c-1", "ext:i:sub", "assistant");
        s.add_user_message("hello", None);
        s.add_assistant_message("hi there", 2);
        s.set_system_message("internal persona");
        let t = transcript(&s);
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].role, "user");
        assert_eq!(t[0].content, "hello");
        assert_eq!(t[1].role, "assistant");
        assert!(t.iter().all(|m| m.content != "internal persona"));
    }

    #[test]
    fn transcript_rehydrates_persisted_attachments() {
        let mut s = Session::new("embed:assistant:c-1", "ext:i:sub", "assistant");
        s.add_user_message("export it", None);
        s.add_assistant_message("done — here's your file", 4);
        // The edge stamps produced files onto the assistant message under a
        // namespaced key (see host::persist_attachments); transcript surfaces them.
        s.messages.last_mut().unwrap().tool_data = Some(serde_json::json!({
            "edge_attachments": [{
                "artifact_id": "art_00000000000000000000000000000001",
                "filename": "report.xlsx",
                "media_type": "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            }]
        }));
        let t = transcript(&s);
        assert_eq!(t.len(), 2);
        assert!(
            t[0].attachments.is_none(),
            "this user turn attached nothing"
        );
        let files = t[1]
            .attachments
            .as_ref()
            .expect("the assistant turn carries its file");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].filename, "report.xlsx");
        assert_eq!(files[0].artifact_id, "art_00000000000000000000000000000001");
    }

    #[test]
    fn transcript_carries_the_turn_reference_an_answer_belongs_to() {
        let mut s = Session::new("embed:assistant:c-1", "ext:i:sub", "assistant");
        s.add_user_message("closest deals to exchange?", None);
        s.add_assistant_message("the milestone analysis", 4);
        s.messages.last_mut().unwrap().tool_data =
            Some(serde_json::json!({ "edge_reply_to": "ref-a" }));

        let t = transcript(&s);
        assert_eq!(t[0].reply_to, None, "user rows carry no reference");
        assert_eq!(
            t[1].reply_to.as_deref(),
            Some("ref-a"),
            "the answer names the turn it belongs to, so recovery need not guess"
        );
    }

    #[test]
    fn an_answer_saved_before_the_reference_existed_still_reads_cleanly() {
        let mut s = Session::new("embed:assistant:c-1", "ext:i:sub", "assistant");
        s.add_user_message("count?", None);
        s.add_assistant_message("42", 1);
        assert_eq!(
            transcript(&s)[1].reply_to,
            None,
            "absent metadata must not break the transcript; the client falls back to position"
        );
    }

    #[test]
    fn transcript_rehydrates_the_customers_own_uploads() {
        // Same key, other role: a reload has to show the customer the photo THEY
        // sent, not just the files the agent produced (see
        // host::persist_user_attachments).
        let mut s = Session::new("embed:assistant:c-1", "ext:i:sub", "assistant");
        s.add_user_message("what is this?", None);
        s.messages.last_mut().unwrap().tool_data = Some(serde_json::json!({
            "edge_attachments": [{
                "artifact_id": "art_00000000000000000000000000000002",
                "filename": "drawing.png",
                "media_type": "image/png",
            }]
        }));
        s.add_assistant_message("a sketch of a cat", 4);
        let t = transcript(&s);
        let files = t[0]
            .attachments
            .as_ref()
            .expect("the user turn carries what they attached");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].filename, "drawing.png");
        assert_eq!(files[0].media_type, "image/png");
        assert!(
            t[1].attachments.is_none(),
            "the reply produced no file of its own"
        );
    }

    #[test]
    fn list_for_owner_is_owner_scoped_and_bounded() {
        use workforce_common::session::InMemorySessionStore;
        let store = InMemorySessionStore::new();
        for i in 0..3 {
            let s = store
                .get_or_create(
                    &format!("embed:assistant:a-{i}"),
                    "ext:i:alice",
                    "assistant",
                )
                .unwrap();
            store.save(&s).unwrap();
        }
        let s = store
            .get_or_create("embed:assistant:b-0", "ext:i:bob", "assistant")
            .unwrap();
        store.save(&s).unwrap();

        let alice = store.list_for_owner("ext:i:alice", 10).unwrap();
        assert_eq!(alice.len(), 3);
        assert!(alice.iter().all(|s| s.owner == "ext:i:alice"));
        let bob = store.list_for_owner("ext:i:bob", 10).unwrap();
        assert_eq!(bob.len(), 1);
        // The cap holds.
        assert_eq!(store.list_for_owner("ext:i:alice", 2).unwrap().len(), 2);
        // An unknown owner sees nothing, not an error.
        assert!(store
            .list_for_owner("ext:i:mallory", 10)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn origin_gate_allows_absent_but_checks_present() {
        use crate::agent::{Branding, EmbedSettings, McpSettings};
        let embed = EmbedSettings {
            allowed_origins: vec!["https://app.example".into()],
            allowed_tools: vec![],
            allowed_emails: vec![],
            keycloak_issuer: "https://kc".into(),
            keycloak_audience: None,
            mcp: None::<McpSettings>,
            mcp_servers: vec![],
            branding: Branding::default(),
            passive_memory: None,
            data_tables: None,
            charts: None,
            diagrams: None,
            excalidraw_export: None,
            uploads: vec![],
            upload_max_mb: None,
            drawing: None,
            inline_images: None,
            images: None,
            image_hosts: vec![],
            spill_skip_tools: vec![],
            forms: None,
            maps: None,
            tool_grants: vec![],
            permissions: None,
            policy: None,
        };
        // Absent (same-origin GET / non-browser) passes — token is the gate.
        assert!(origin_permitted(&embed, None));
        // Present must be allow-listed.
        assert!(origin_permitted(&embed, Some("https://app.example")));
        assert!(!origin_permitted(&embed, Some("https://evil.example")));
    }

    #[test]
    fn cors_gate_extracts_the_agent_segment() {
        assert_eq!(
            agent_from_conversations_path("/public/conversations/assistant"),
            Some("assistant")
        );
        assert_eq!(
            agent_from_conversations_path("/public/conversations/assistant/c-1"),
            Some("assistant")
        );
        assert_eq!(agent_from_conversations_path("/public/ws/assistant"), None);
        assert_eq!(
            agent_from_conversations_path("/public/conversations/"),
            None
        );
    }

    #[test]
    fn customer_frames_map_to_the_minimal_surface() {
        assert_eq!(customer_frame(&CustomerEvent::Typing, "s").kind, "typing");
        assert_eq!(
            customer_frame(&CustomerEvent::Delta { text: "hi".into() }, "s").kind,
            "token"
        );
        let done = customer_frame(
            &CustomerEvent::Done {
                text: "answer".into(),
                message_id: Some("msg-1".into()),
            },
            "s",
        );
        assert_eq!(done.kind, "done");
        assert_eq!(done.content.as_deref(), Some("answer"));
        // The done frame surfaces the assistant message id for feedback rating.
        assert_eq!(done.message_id.as_deref(), Some("msg-1"));

        let attachment = customer_frame(
            &CustomerEvent::Attachment {
                file: crate::wire::AttachmentRef {
                    artifact_id: "art_x".into(),
                    filename: "export.xlsx".into(),
                    media_type: "application/octet-stream".into(),
                },
            },
            "s",
        );
        assert_eq!(attachment.kind, "attachment");
        let file = attachment
            .attachment
            .expect("attachment frame carries the file");
        assert_eq!(file.artifact_id, "art_x");
        assert_eq!(file.filename, "export.xlsx");

        let progress = customer_frame(
            &CustomerEvent::Progress {
                label: "Invoke query".into(),
            },
            "s",
        );
        assert_eq!(progress.kind, "progress");
        assert_eq!(progress.content.as_deref(), Some("Invoke query"));

        let resume = customer_frame(
            &CustomerEvent::Resume {
                text: "so far".into(),
            },
            "s",
        );
        assert_eq!(resume.kind, "resume");
        assert_eq!(resume.content.as_deref(), Some("so far"));
    }

    #[test]
    fn a_rewind_frame_carries_the_corrected_text() {
        let rewind = customer_frame(
            &CustomerEvent::Rewind {
                text: "as it stands".into(),
            },
            "s",
        );
        assert_eq!(rewind.kind, "rewind");
        assert_eq!(rewind.content.as_deref(), Some("as it stands"));
    }

    #[test]
    fn serve_owned_artifact_is_owner_scoped_and_fails_closed() {
        use workforce_agent::artifacts::{
            ArtifactBody, ArtifactDescriptor, ContentType, FsArtifactStore,
        };
        let dir = tempfile::TempDir::new().unwrap();
        let store = FsArtifactStore::new(dir.path().to_path_buf()).unwrap();
        let write = |owner: Option<&str>| {
            store
                .write(
                    "agent-a",
                    ArtifactDescriptor {
                        producer: "export_xlsx".into(),
                        mime: ContentType::OctetStream,
                        schema: None,
                        row_count: None,
                        ttl_seconds: 3600,
                        agent_id: "agent-a".into(),
                        session_id: None,
                        user_id: owner.map(str::to_string),
                        filename: Some("f.xlsx".into()),
                        media_type: Some("application/octet-stream".into()),
                    },
                    ArtifactBody::Bytes(b"PKxlsx".to_vec()),
                )
                .unwrap()
                .id
        };

        let owned = write(Some("owner-1"));
        assert_eq!(
            serve_owned_artifact(store.as_ref(), "agent-a", &owned, "owner-1")
                .unwrap()
                .status(),
            StatusCode::OK,
        );
        assert_eq!(
            serve_owned_artifact(store.as_ref(), "agent-a", &owned, "owner-2").unwrap_err(),
            StatusCode::NOT_FOUND,
            "a different owner cannot read it",
        );

        let ownerless = write(None);
        assert_eq!(
            serve_owned_artifact(store.as_ref(), "agent-a", &ownerless, "owner-1").unwrap_err(),
            StatusCode::NOT_FOUND,
            "an artifact with no recorded owner fails closed",
        );

        let missing = ArtifactId::fresh();
        assert_eq!(
            serve_owned_artifact(store.as_ref(), "agent-a", &missing, "owner-1").unwrap_err(),
            StatusCode::NOT_FOUND,
        );
    }

    #[test]
    fn serve_table_page_is_owner_scoped_and_tabular_only() {
        use workforce_agent::artifacts::{ArtifactBody, ArtifactDescriptor, FsArtifactStore};
        let dir = tempfile::TempDir::new().unwrap();
        let store = FsArtifactStore::new(dir.path().to_path_buf()).unwrap();
        let tabular = |owner: Option<&str>, rows: usize| {
            let descriptor = ArtifactDescriptor {
                schema: Some(vec!["name".into(), "amount".into()]),
                row_count: Some(rows),
                user_id: owner.map(str::to_string),
                ..ArtifactDescriptor::from_context(
                    "data.query",
                    ContentType::ApplicationJson,
                    "agent-a",
                )
            };
            store
                .write(
                    "agent-a",
                    descriptor,
                    ArtifactBody::Json(serde_json::json!([
                        {"name": "Acme", "amount": 100},
                        {"name": "Birch", "amount": 40},
                    ])),
                )
                .unwrap()
                .id
        };
        let params = TablePageQuery::default();

        let owned = tabular(Some("owner-1"), 2);
        assert_eq!(
            serve_table_page(store.as_ref(), "agent-a", &owned, "owner-1", &params)
                .unwrap()
                .status(),
            StatusCode::OK,
        );
        assert_eq!(
            serve_table_page(store.as_ref(), "agent-a", &owned, "owner-2", &params).unwrap_err(),
            StatusCode::NOT_FOUND,
            "a different owner cannot browse it",
        );

        // A non-tabular artifact (an xlsx download) is not browsable.
        let xlsx = store
            .write(
                "agent-a",
                ArtifactDescriptor {
                    user_id: Some("owner-1".into()),
                    ..ArtifactDescriptor::from_context(
                        "export_xlsx",
                        ContentType::OctetStream,
                        "agent-a",
                    )
                },
                ArtifactBody::Bytes(b"PKxlsx".to_vec()),
            )
            .unwrap()
            .id;
        assert_eq!(
            serve_table_page(store.as_ref(), "agent-a", &xlsx, "owner-1", &params).unwrap_err(),
            StatusCode::NOT_FOUND,
        );

        // Too large to browse in-widget.
        let huge = tabular(Some("owner-1"), MAX_TABLE_ROWS + 1);
        assert_eq!(
            serve_table_page(store.as_ref(), "agent-a", &huge, "owner-1", &params).unwrap_err(),
            StatusCode::PAYLOAD_TOO_LARGE,
        );

        assert_eq!(
            serve_table_page(
                store.as_ref(),
                "agent-a",
                &ArtifactId::fresh(),
                "owner-1",
                &params
            )
            .unwrap_err(),
            StatusCode::NOT_FOUND,
        );
    }

    #[tokio::test]
    async fn serve_table_page_body_carries_typed_columns_and_a_sorted_page() {
        use workforce_agent::artifacts::{ArtifactBody, ArtifactDescriptor, FsArtifactStore};
        let dir = tempfile::TempDir::new().unwrap();
        let store = FsArtifactStore::new(dir.path().to_path_buf()).unwrap();
        let id = store
            .write(
                "agent-a",
                ArtifactDescriptor {
                    schema: Some(vec!["name".into(), "amount".into()]),
                    row_count: Some(3),
                    user_id: Some("owner-1".into()),
                    ..ArtifactDescriptor::from_context(
                        "data.query",
                        ContentType::ApplicationJson,
                        "agent-a",
                    )
                },
                ArtifactBody::Json(serde_json::json!([
                    {"name": "Acme", "amount": 100},
                    {"name": "Birch", "amount": 40},
                    {"name": "Cedar", "amount": 250},
                ])),
            )
            .unwrap()
            .id;

        let params = TablePageQuery {
            limit: Some(2),
            sort: Some("amount".into()),
            dir: Some("desc".into()),
            ..Default::default()
        };
        let response =
            serve_table_page(store.as_ref(), "agent-a", &id, "owner-1", &params).unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();

        assert_eq!(
            body["columns"],
            serde_json::json!([
                {"key": "name", "label": "name", "type": "text"},
                {"key": "amount", "label": "amount", "type": "number"},
            ]),
        );
        assert_eq!(body["total"], 3);
        assert_eq!(body["limit"], 2);
        // amount desc, first page of 2: Cedar (250), Acme (100).
        assert_eq!(
            body["rows"],
            serde_json::json!([["Cedar", 250], ["Acme", 100]])
        );
    }

    #[test]
    fn resolve_table_query_clamps_and_validates_against_schema() {
        use crate::table_query::{ColumnType, Filter};
        let schema: Vec<String> = ["name", "amount", "due"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let types = vec![ColumnType::Text, ColumnType::Number, ColumnType::Date];
        let q = |raw: TablePageQuery| resolve_table_query(&schema, &types, &raw);

        // Page size clamps; sort resolves by name with direction.
        let r = q(TablePageQuery {
            limit: Some(9999),
            sort: Some("amount".into()),
            dir: Some("desc".into()),
            ..Default::default()
        });
        assert_eq!(r.limit, MAX_TABLE_PAGE);
        assert_eq!(r.sort, Some((1, true)));

        // Unknown sort column is dropped; default page size applies.
        let r = q(TablePageQuery {
            sort: Some("nope".into()),
            ..Default::default()
        });
        assert_eq!(r.sort, None);
        assert_eq!(r.limit, DEFAULT_TABLE_PAGE);

        // Filters: text→contains, number→range; unknown column and wrong-type op dropped.
        let r = q(TablePageQuery {
            filters: Some(
                r#"[{"col":"name","op":"contains","value":"AC"},
                    {"col":"amount","op":"gte","value":"100"},
                    {"col":"due","op":"lte","value":"2026-07-10"},
                    {"col":"amount","op":"contains","value":"x"},
                    {"col":"ghost","op":"gte","value":1}]"#
                    .into(),
            ),
            ..Default::default()
        });
        assert_eq!(r.filters.len(), 3);
        assert!(matches!(r.filters[0], Filter::Contains { col: 0, ref needle } if needle == "ac"));
        assert!(matches!(r.filters[1], Filter::Min { col: 1, bound } if bound == 100.0));
        assert!(matches!(r.filters[2], Filter::Max { col: 2, .. }));

        // Malformed filters JSON degrades to none, never an error.
        let r = q(TablePageQuery {
            filters: Some("{not json".into()),
            ..Default::default()
        });
        assert!(r.filters.is_empty());
    }

    #[test]
    fn turns_cursor_round_trips_and_rejects_garbage() {
        let cursor = crate::analytics::TurnsCursor {
            ts: chrono::DateTime::parse_from_rfc3339("2026-07-01T12:34:56Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
            id: 4242,
        };
        let encoded = encode_turns_cursor(&cursor);
        let decoded = decode_turns_cursor(&encoded).expect("round-trips");
        assert_eq!(decoded.ts, cursor.ts);
        assert_eq!(decoded.id, cursor.id);
        // Malformed cursors decode to None (→ a 400, never a panic).
        assert!(decode_turns_cursor("not base64!!").is_none());
        assert!(decode_turns_cursor("").is_none());
    }

    #[test]
    fn analytics_window_defaults_to_30_days_and_rejects_bad_timestamps() {
        // Absent bounds → a ~30-day window ending about now.
        let q = analytics_window(&AnalyticsFilter {
            from: None,
            to: None,
            agent: None,
            owner: None,
            bucket: None,
            cursor: None,
            limit: None,
        })
        .expect("defaults");
        let span = q.to - q.from;
        assert_eq!(span.num_days(), ANALYTICS_DEFAULT_WINDOW_DAYS);
        // Blank agent/owner are treated as absent (never a match-nothing filter).
        let q = analytics_window(&AnalyticsFilter {
            from: Some("2026-07-01T00:00:00Z".into()),
            to: Some("2026-07-02T00:00:00Z".into()),
            agent: Some(String::new()),
            owner: Some("ext:iss\u{1f}a".into()),
            bucket: None,
            cursor: None,
            limit: None,
        })
        .expect("explicit bounds");
        assert!(q.agent.is_none());
        assert_eq!(q.owner.as_deref(), Some("ext:iss\u{1f}a"));
        // A malformed timestamp is an error the handler turns into a 400.
        assert!(analytics_window(&AnalyticsFilter {
            from: Some("yesterday".into()),
            to: None,
            agent: None,
            owner: None,
            bucket: None,
            cursor: None,
            limit: None,
        })
        .is_err());
    }

    #[test]
    fn timeseries_window_cap_rejects_only_oversized_windows() {
        use crate::analytics::TimeBucket;
        let base = chrono::DateTime::parse_from_rfc3339("2026-07-01T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let q = |days: i64| crate::analytics::AnalyticsQuery {
            from: base - chrono::Duration::days(days),
            to: base,
            agent: None,
            owner: None,
        };
        // 30 days at either bucket is well under the 5000 cap.
        assert!(!timeseries_window_too_wide(&q(30), TimeBucket::Day));
        assert!(!timeseries_window_too_wide(&q(30), TimeBucket::Hour));
        // ~2 years of hourly buckets (~17.5k) exceeds the cap; the same window by
        // day (730) does not.
        assert!(timeseries_window_too_wide(&q(730), TimeBucket::Hour));
        assert!(!timeseries_window_too_wide(&q(730), TimeBucket::Day));
        // ~20 years of daily buckets (~7.3k) exceeds the cap.
        assert!(timeseries_window_too_wide(&q(7300), TimeBucket::Day));
    }
}

#[cfg(test)]
mod tenant_tests {
    //! The router serving two tenants by hostname, driven in-process.
    use std::collections::HashMap;
    use std::net::{IpAddr, SocketAddr};
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{header, Method, Request, StatusCode};
    use tower::ServiceExt;
    use workforce_common::session::InMemorySessionStore;

    use super::*;
    use crate::admin::{AdminConfig, AdminOidc};
    use crate::agent::EdgeAgent;
    use crate::boot::SharedInfra;
    use crate::config::DemoConfig;
    use crate::host::tests::BlockingProvider;

    /// A self-hosted, Harbor-shaped file: one tenant, `[vellev]`, `[admin]`,
    /// no `multi_tenant`.
    fn edge_toml() -> EdgeConfig {
        toml::from_str(
            r#"
            [server]
            bind = "0.0.0.0:8080"
            rate_limit_per_min = 2

            [postgres]
            url = "postgres://edge/edge"

            [vellev]
            url = "http://vellev:18080"
            api_key = "k"
            tenant = "harbor"

            [llm]
            api_key = "sk-test"

            [admin]
            session_secret = "0123456789abcdef0123456789abcdef"
            allowed_emails = ["ops@harbor.example"]
            [admin.oidc]
            issuer = "https://accounts.google.com"
            client_id = "edge-admin"
            "#,
        )
        .expect("a valid test config")
    }

    fn admin_config() -> AdminConfig {
        AdminConfig {
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
            // Shared across tenants on purpose: the tenant claim, not the key,
            // must keep their sessions apart.
            session_secret: Some("0123456789abcdef0123456789abcdef".into()),
            public_url: None,
            session_hours: 1,
        }
    }

    /// A tenant serving agent `support`, whose embed allows `origin` and
    /// whose demo page carries `client_id` — the per-tenant "branding".
    fn tenant(id: &str, origin: &str, client_id: &str, infra: &SharedInfra) -> Arc<TenantRuntime> {
        tenant_on(id, origin, client_id, "https://idp.test", infra)
    }

    /// [`tenant`], with its agent's customers verified against `issuer`.
    fn tenant_on(
        id: &str,
        origin: &str,
        client_id: &str,
        issuer: &str,
        infra: &SharedInfra,
    ) -> Arc<TenantRuntime> {
        let agent = Arc::new(EdgeAgent {
            id: "support".into(),
            display_name: format!("{id} support"),
            embed: crate::agent::EmbedSettings {
                allowed_origins: vec![origin.into()],
                keycloak_issuer: issuer.into(),
                ..Default::default()
            },
            ..Default::default()
        });
        let host = AgentHost::for_test(
            agent,
            Arc::new(BlockingProvider),
            Arc::new(InMemorySessionStore::new()),
        );
        let settings = TenantSettings {
            id: TenantId::parse(id).unwrap(),
            name: format!("{id} Ltd"),
            hostnames: vec![format!("{id}.test")],
            admin: Some(admin_config()),
            demo: DemoConfig {
                enabled: true,
                google_client_id: Some(client_id.into()),
            },
            ..TenantSettings::from_edge_toml(&edge_toml())
        };
        TenantRuntime::for_tests(
            settings,
            HashMap::from([("support".to_string(), Arc::new(host))]),
            infra,
        )
    }

    fn two_tenants() -> (Arc<EdgeState>, Arc<TenantRuntime>, Arc<TenantRuntime>) {
        let infra = Arc::new(SharedInfra::for_tests());
        let acme = tenant("acme", "https://app.acme.test", "acme-client", &infra);
        let contoso = tenant(
            "contoso",
            "https://app.contoso.test",
            "contoso-client",
            &infra,
        );
        let state = EdgeState::for_tests(
            TenantMode::Multi,
            vec![
                (Arc::clone(&acme), vec!["acme.test"]),
                (Arc::clone(&contoso), vec!["contoso.test"]),
            ],
            infra,
            edge_toml(),
            1,
        );
        (state, acme, contoso)
    }

    fn peer(ip: &str) -> ConnectInfo<SocketAddr> {
        ConnectInfo(SocketAddr::new(ip.parse::<IpAddr>().unwrap(), 40000))
    }

    fn get(host: Option<&str>, path: &str, ip: &str) -> Request<Body> {
        let mut req = Request::builder().method(Method::GET).uri(path);
        if let Some(host) = host {
            req = req.header(header::HOST, host);
        }
        req.extension(peer(ip)).body(Body::empty()).unwrap()
    }

    async fn send(state: &Arc<EdgeState>, req: Request<Body>) -> (StatusCode, String) {
        let response = router(Arc::clone(state)).oneshot(req).await.unwrap();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    /// `/metrics` is mounted only when `[metrics]` is enabled, and then
    /// answers only a scraper presenting the configured bearer token.
    #[tokio::test]
    async fn the_metrics_endpoint_is_bearer_guarded_and_absent_when_disabled() {
        const TOKEN: &str = "metrics-scrape-token-0123456789abcdef";
        let state_with = |metrics: Option<crate::metrics::MetricsConfig>| {
            let mut cfg = edge_toml();
            cfg.metrics = metrics;
            EdgeState::for_tests(
                TenantMode::Single(TenantId::default_tenant()),
                vec![],
                Arc::new(SharedInfra::for_tests()),
                cfg,
                1,
            )
        };
        let scrape = |token: Option<&str>| {
            let mut req = Request::builder().method(Method::GET).uri("/metrics");
            if let Some(token) = token {
                req = req.header(header::AUTHORIZATION, format!("Bearer {token}"));
            }
            req.extension(peer("203.0.113.1"))
                .body(Body::empty())
                .unwrap()
        };

        let enabled = state_with(Some(crate::metrics::MetricsConfig {
            enabled: true,
            bearer_token: Some(TOKEN.into()),
        }));
        for refused in [None, Some("wrong-token-wrong-token-wrong-token")] {
            let (status, body) = send(&enabled, scrape(refused)).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{refused:?}");
            assert!(body.is_empty(), "{body}");
        }
        let (status, _) = send(&enabled, scrape(Some(TOKEN))).await;
        assert_eq!(status, StatusCode::OK);

        for disabled in [
            None,
            Some(crate::metrics::MetricsConfig {
                enabled: false,
                bearer_token: Some(TOKEN.into()),
            }),
        ] {
            let (status, _) = send(&state_with(disabled), scrape(Some(TOKEN))).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "not routed when disabled");
        }
    }

    #[tokio::test]
    async fn the_host_picks_the_tenant_and_an_unknown_host_is_404() {
        let (state, _, _) = two_tenants();
        let (status, body) = send(
            &state,
            get(Some("acme.test"), "/demo/support", "203.0.113.1"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.contains("acme-client"),
            "acme's demo page carries acme's client id"
        );
        let (status, body) = send(
            &state,
            get(Some("CONTOSO.test:443"), "/demo/support", "203.0.113.1"),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "hosts match case-insensitively, port ignored"
        );
        assert!(body.contains("contoso-client"));
        let (status, _) = send(
            &state,
            get(Some("acme.test"), "/embed/support", "203.0.113.1"),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "the same agent id on another tenant's host"
        );
        let (status, _) = send(
            &state,
            get(Some("other.test"), "/embed/support", "203.0.113.1"),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = send(&state, get(None, "/embed/support", "203.0.113.1")).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "no host names no tenant");
        let (status, _) = send(
            &state,
            get(Some("other.test"), "/admin/api/me", "203.0.113.1"),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "the admin surface is per tenant too"
        );
    }

    #[tokio::test]
    async fn a_suspended_tenant_answers_503_and_a_harmless_loader() {
        let (state, acme, _) = two_tenants();
        state.suspend(&acme.id, "Account on hold.".into()).await;
        let (status, body) = send(
            &state,
            get(
                Some("acme.test"),
                "/public/conversations/support",
                "203.0.113.1",
            ),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        let body: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["error"], "tenant_suspended");
        assert_eq!(body["message"], "Account on hold.");
        let (status, body) = send(
            &state,
            get(Some("acme.test"), "/embed/support", "203.0.113.1"),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "the host page's script tag must not break"
        );
        assert!(
            body.contains("console.warn") && body.contains("Account on hold."),
            "{body}"
        );
        assert!(!body.contains("__WF_EDGE__"), "nothing mounts");
        let (status, _) = send(
            &state,
            get(Some("contoso.test"), "/embed/support", "203.0.113.1"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "the other tenant is unaffected");
        assert!(matches!(state.resolve("acme.test"), Resolved::Suspended(_)));
        assert_eq!(state.active().len(), 1, "the suspended runtime is dropped");
    }

    #[tokio::test]
    async fn an_admin_session_is_bound_to_the_tenant_that_minted_it() {
        let (state, acme, _) = two_tenants();
        let token = acme
            .admin
            .as_ref()
            .unwrap()
            .issue_session("alice@acme.test")
            .unwrap();
        let cookie = format!("{}={token}", crate::admin::SESSION_COOKIE);
        let me = |host: &str| {
            let mut req = get(Some(host), "/admin/api/me", "203.0.113.1");
            req.headers_mut()
                .insert(header::COOKIE, cookie.parse().unwrap());
            req
        };
        let (status, body) = send(&state, me("acme.test")).await;
        assert_eq!(status, StatusCode::OK);
        let body: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["operator"], "alice@acme.test");
        assert_eq!(body["tenant"]["id"], "acme");
        assert_eq!(body["tenant"]["name"], "acme Ltd");
        let (status, _) = send(&state, me("contoso.test")).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "same key, other tenant: refused"
        );
    }

    #[tokio::test]
    async fn each_tenant_has_its_own_ip_budget() {
        let (state, _, _) = two_tenants();
        let ip = "203.0.113.7";
        for _ in 0..2 {
            let (status, _) = send(&state, get(Some("acme.test"), "/embed/support", ip)).await;
            assert_eq!(status, StatusCode::OK);
        }
        let (status, _) = send(&state, get(Some("acme.test"), "/embed/support", ip)).await;
        assert_eq!(
            status,
            StatusCode::TOO_MANY_REQUESTS,
            "acme's budget of 2 is spent"
        );
        let (status, _) = send(&state, get(Some("contoso.test"), "/embed/support", ip)).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "the same address is fresh to contoso"
        );
    }

    #[tokio::test]
    async fn cors_reflects_an_origin_only_for_the_tenant_whose_agent_allows_it() {
        let (state, _, _) = two_tenants();
        let preflight = |host: &str| {
            Request::builder()
                .method(Method::OPTIONS)
                .uri("/public/conversations/support")
                .header(header::HOST, host)
                .header(header::ORIGIN, "https://app.acme.test")
                .header(header::ACCESS_CONTROL_REQUEST_METHOD, "GET")
                .extension(peer("203.0.113.1"))
                .body(Body::empty())
                .unwrap()
        };
        let response = router(Arc::clone(&state))
            .oneshot(preflight("acme.test"))
            .await
            .unwrap();
        assert_eq!(
            response
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .and_then(|v| v.to_str().ok()),
            Some("https://app.acme.test")
        );
        let response = router(Arc::clone(&state))
            .oneshot(preflight("contoso.test"))
            .await
            .unwrap();
        assert!(
            response
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .is_none(),
            "acme's origin is not contoso's"
        );
    }

    #[tokio::test]
    async fn a_self_hosted_install_serves_the_default_tenant_on_any_host() {
        let cfg = edge_toml();
        assert!(!cfg.server.multi_tenant, "absent ⇒ single-tenant");
        let settings = TenantSettings::from_edge_toml(&cfg);
        assert!(settings.id.is_default());
        assert!(settings.hostnames.is_empty());
        assert!(
            settings.admin.is_some(),
            "[admin] still comes from the file"
        );
        assert_eq!(settings.limits.rate_limit_per_min, 2);

        let infra = Arc::new(SharedInfra::for_tests());
        let agent = Arc::new(EdgeAgent {
            id: "support".into(),
            ..Default::default()
        });
        let host = AgentHost::for_test(
            agent,
            Arc::new(BlockingProvider),
            Arc::new(InMemorySessionStore::new()),
        );
        let runtime = TenantRuntime::for_tests(
            settings,
            HashMap::from([("support".to_string(), Arc::new(host))]),
            &infra,
        );
        let state = EdgeState::for_tests(
            TenantMode::Single(TenantId::default_tenant()),
            vec![(runtime, vec![])],
            infra,
            cfg,
            8,
        );
        for host in ["anything.example", "localhost:8080", ""] {
            let Resolved::Tenant(found) = state.resolve(host) else {
                panic!("{host:?} must reach the default tenant");
            };
            assert!(found.id.is_default());
        }
        let (status, _) = send(
            &state,
            get(Some("edge.harbor.example"), "/embed/support", "203.0.113.1"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = send(&state, get(None, "/embed/support", "203.0.113.1")).await;
        assert_eq!(status, StatusCode::OK, "even a host-less request is served");
    }

    /// The housekeeping loops walk the active tenants through their own
    /// stores: a suspended tenant's rows and files are not touched.
    #[tokio::test(flavor = "multi_thread")]
    async fn tenant_loops_sweep_active_tenants_only() {
        use workforce_agent::artifacts::ArtifactDescriptor;
        use workforce_common::session_postgres::{run_session_migrations, PostgresSessionStore};
        use workforce_postgres::test_support::with_test_pool;

        use crate::analytics::{run_edge_analytics_migrations, TurnClaim};
        use crate::tenant::run_edge_migrations;

        with_test_pool(PostgresSessionStore::build_pool, |pool| async move {
            run_session_migrations(&pool).await.unwrap();
            run_edge_analytics_migrations(&pool).await.unwrap();
            run_edge_migrations(&pool).await.unwrap();
            let tmp = tempfile::tempdir().unwrap();
            let infra = Arc::new(SharedInfra::for_tests_on(pool).with_artifacts(tmp.path()));
            let acme = tenant("acme", "https://app.acme.test", "acme-client", &infra);
            let contoso = tenant(
                "contoso",
                "https://app.contoso.test",
                "contoso-client",
                &infra,
            );
            let state = EdgeState::for_tests(
                TenantMode::Multi,
                vec![
                    (Arc::clone(&acme), vec!["acme.test"]),
                    (Arc::clone(&contoso), vec!["contoso.test"]),
                ],
                Arc::clone(&infra),
                edge_toml(),
                1,
            );
            // Each tenant holds a turn claim and an already-expired file.
            let mut expired = Vec::new();
            for runtime in [&acme, &contoso] {
                runtime
                    .analytics()
                    .claim_turn(&TurnClaim {
                        turn_key: "k1",
                        conversation: "c1",
                        owner: "owner-1",
                        agent_id: "support",
                        replica: "replica-a",
                    })
                    .await
                    .unwrap();
                let store = &runtime.artifacts().expect("a store per tenant").store;
                let handle = store
                    .write(
                        "support",
                        ArtifactDescriptor::from_context(
                            "test",
                            ContentType::ApplicationJson,
                            "support",
                        )
                        .with_ttl(Duration::ZERO),
                        ArtifactBody::Json(serde_json::json!({})),
                    )
                    .unwrap();
                expired.push(handle.id);
            }
            state.suspend(&contoso.id, "paused".into()).await;
            assert_eq!(state.active().len(), 1);

            sweep_turn_claims(&state, Duration::ZERO).await;
            sweep_tool_evidence(&state).await;
            purge_artifacts(&state).await;

            let live = Duration::from_secs(240);
            assert!(
                acme.analytics()
                    .turn_owner("k1", live)
                    .await
                    .unwrap()
                    .is_none(),
                "the active tenant's stale claim is swept"
            );
            assert!(
                contoso
                    .analytics()
                    .turn_owner("k1", live)
                    .await
                    .unwrap()
                    .is_some(),
                "the suspended tenant's rows are not touched"
            );
            // An expired artifact already reads as absent, so the purge is
            // visible only on disk: the sidecar goes, or stays.
            let sidecar = |runtime: &TenantRuntime, id: &ArtifactId| {
                runtime
                    .artifacts()
                    .unwrap()
                    .root
                    .join("support")
                    .join(format!("{}.meta.json", id.as_str()))
            };
            assert!(
                !sidecar(&acme, &expired[0]).exists(),
                "the active tenant's expired file is purged"
            );
            assert!(
                sidecar(&contoso, &expired[1]).is_file(),
                "the suspended tenant's files are not touched"
            );
        })
        .await;
    }

    /// The router on a real listener, for the socket tests: a `oneshot`
    /// cannot carry a websocket upgrade through.
    async fn serve(state: Arc<EdgeState>) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                router(state).into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });
        addr
    }

    type ClientSocket = tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >;

    /// Open a widget socket to `path` as a page on `host` would: the host
    /// header picks the tenant, the token rides the query (the non-browser
    /// fallback — the subprotocol would be echoed but changes nothing here).
    async fn connect(addr: SocketAddr, host: &str, path: &str) -> ClientSocket {
        use tokio_tungstenite::tungstenite::handshake::client::generate_key;
        let request = tokio_tungstenite::tungstenite::http::Request::builder()
            .uri(format!("ws://{addr}{path}"))
            .header("Host", host)
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .header("Sec-WebSocket-Version", "13")
            .header("Sec-WebSocket-Key", generate_key())
            .body(())
            .unwrap();
        let (socket, _) = tokio_tungstenite::connect_async(request)
            .await
            .expect("the upgrade completes");
        socket
    }

    /// The next text frame, parsed.
    async fn next_frame(socket: &mut ClientSocket) -> serde_json::Value {
        use tokio_tungstenite::tungstenite::Message;
        match socket.next().await {
            Some(Ok(Message::Text(text))) => serde_json::from_str(text.as_str()).unwrap(),
            other => panic!("expected a text frame, got {other:?}"),
        }
    }

    /// The close frame's code and reason, then the end of the stream.
    async fn next_close(socket: &mut ClientSocket) -> (u16, String) {
        use tokio_tungstenite::tungstenite::Message;
        let close = match socket.next().await {
            Some(Ok(Message::Close(Some(frame)))) => {
                (u16::from(frame.code), frame.reason.to_string())
            }
            other => panic!("expected a close frame, got {other:?}"),
        };
        assert!(
            matches!(socket.next().await, None | Some(Err(_))),
            "nothing follows the close"
        );
        close
    }

    /// The public modulus of `keycloak::tests::PRIV_PEM`, as a JWK.
    const TEST_KEY_N: &str = "mZA1ivd3HPWJm-HT-pZAdIfEPrlFdX3ygMdc16QUFLUDC1P5h3lKav5ZweBrP4Sqx2cuhlEARAAvx_eqPuwI3_DGAi6Dh0sBnUWgvhr6zGvXLUmzVvJ_O2yFWjyFFkejLg7hp0Wezhq39JSkNGhRW1Y6f9IIGODE-L5NRKAneFvzee56BnMHY9wDF2dQ8yPR8SUmVOTy_ss_F3QeebLknEDcCxubJIreRqYisLaz5EXB3GoW5-tJjlpQbQIZ2KojXMyeN5sqWwZZhbY6EcG7BN8Z7ilJ9x6e7aTOtFWGhRXtuJDhfroLRx0kpEq4WIS_EATF0_8AYlxsNkrcGSBG7w";

    /// An identity provider whose discovery and JWKS verify tokens signed
    /// with the keycloak test key. Returns the server (kept alive by the
    /// caller) and the issuer it answers for. The realm is unique per call:
    /// the validator and endpoint caches are process-wide and keyed by
    /// issuer, and a port one test's server gave back is another's to get.
    async fn mock_idp() -> (wiremock::MockServer, String) {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        let realm = format!("realms/{}", uuid::Uuid::new_v4().simple());
        let issuer = format!("{}/{realm}", server.uri());
        Mock::given(method("GET"))
            .and(path(format!("/{realm}/.well-known/openid-configuration")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "jwks_uri": format!("{}/jwks", server.uri()),
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/jwks"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "keys": [{
                    "kty": "RSA", "kid": "test-key", "use": "sig", "alg": "RS256",
                    "n": TEST_KEY_N, "e": "AQAB",
                }],
            })))
            .mount(&server)
            .await;
        (server, issuer)
    }

    /// A customer token `issuer` would have minted.
    fn customer_token(issuer: &str) -> String {
        crate::keycloak::tests::sign(
            jsonwebtoken::Algorithm::RS256,
            serde_json::json!({
                "iss": issuer,
                "sub": "cust-1",
                "exp": 4_070_908_800u64,
                "email": "person@example.com",
            }),
        )
    }

    /// [`tenant_on`] with the tenant's outbound policy set: how its issuer,
    /// admin IdP and MCP are reached.
    fn tenant_reaching(
        id: &str,
        issuer: &str,
        admin: AdminConfig,
        outbound: OutboundPolicy,
        infra: &SharedInfra,
    ) -> Arc<TenantRuntime> {
        let agent = Arc::new(EdgeAgent {
            id: "support".into(),
            display_name: format!("{id} support"),
            embed: crate::agent::EmbedSettings {
                allowed_origins: vec![format!("https://app.{id}.test")],
                keycloak_issuer: issuer.into(),
                ..Default::default()
            },
            ..Default::default()
        });
        let host = AgentHost::for_test(
            agent,
            Arc::new(BlockingProvider),
            Arc::new(InMemorySessionStore::new()),
        );
        let settings = TenantSettings {
            id: TenantId::parse(id).unwrap(),
            name: format!("{id} Ltd"),
            hostnames: vec![format!("{id}.test")],
            admin: Some(admin),
            outbound,
            ..TenantSettings::from_edge_toml(&edge_toml())
        };
        TenantRuntime::for_tests(
            settings,
            HashMap::from([("support".to_string(), Arc::new(host))]),
            infra,
        )
    }

    /// The tenant's policy reaches end-customer token validation: a `public`
    /// tenant refuses a loopback `http` issuer before a byte is sent to it,
    /// so its customers are 401; a `private` tenant (self-hosted) verifies
    /// against the same issuer as before.
    #[tokio::test]
    async fn a_public_tenant_refuses_a_loopback_issuer_at_authenticate_and_a_private_one_accepts_it(
    ) {
        let (idp, issuer) = mock_idp().await;
        let infra = SharedInfra::for_tests();
        let token = customer_token(&issuer);
        let headers = HeaderMap::new();

        let public = tenant_reaching(
            "acme",
            &issuer,
            admin_config(),
            OutboundPolicy::Public,
            &infra,
        );
        let refused = authenticate(&public, &[], "support", &headers, Some(&token), None)
            .await
            .err()
            .expect("a loopback http issuer is refused under public");
        assert_eq!(refused, StatusCode::UNAUTHORIZED);
        assert!(
            idp.received_requests().await.unwrap().is_empty(),
            "nothing may reach the issuer"
        );

        let private = tenant_reaching(
            "contoso",
            &issuer,
            admin_config(),
            OutboundPolicy::Private,
            &infra,
        );
        let caller = authenticate(&private, &[], "support", &headers, Some(&token), None)
            .await
            .expect("verified against the in-cluster issuer");
        assert_eq!(caller.owner, external_owner(&issuer, "cust-1"));
        assert!(
            !idp.received_requests().await.unwrap().is_empty(),
            "the private tenant fetched the issuer's keys"
        );
    }

    /// An identity provider for the admin redirect flow: a discovery document
    /// naming its endpoints, and a token endpoint that answers any code.
    async fn redirect_idp() -> (wiremock::MockServer, String) {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        let issuer = format!("{}/realms/admin", server.uri());
        Mock::given(method("GET"))
            .and(path("/realms/admin/.well-known/openid-configuration"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "authorization_endpoint": format!("{}/auth", server.uri()),
                "token_endpoint": format!("{}/token", server.uri()),
                "jwks_uri": format!("{}/jwks", server.uri()),
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id_token": "not.a.jwt",
                "token_type": "Bearer",
            })))
            .mount(&server)
            .await;
        (server, issuer)
    }

    /// The tenant's policy reaches the admin sign-in: its redirect-mode OIDC
    /// discovery and code exchange go to a loopback `http` IdP under
    /// `private`, and are refused before any request under `public` — the
    /// callback bounces to the login page either way, but only the private
    /// tenant's IdP sees the code.
    #[tokio::test]
    async fn the_admin_code_exchange_follows_the_tenant_policy() {
        let (idp, issuer) = redirect_idp().await;
        let infra = Arc::new(SharedInfra::for_tests());
        let redirect_admin = |id: &str| AdminConfig {
            oidc: Some(AdminOidc {
                issuer: issuer.clone(),
                audience: Some("edge-admin".into()),
                client_id: Some("edge-admin".into()),
                client_secret: Some("s3cret".into()),
                mode: Some("redirect".into()),
            }),
            public_url: Some(format!("https://{id}.test")),
            ..admin_config()
        };
        let public = tenant_reaching(
            "acme",
            &issuer,
            redirect_admin("acme"),
            OutboundPolicy::Public,
            &infra,
        );
        let private = tenant_reaching(
            "contoso",
            &issuer,
            redirect_admin("contoso"),
            OutboundPolicy::Private,
            &infra,
        );
        let state = EdgeState::for_tests(
            TenantMode::Multi,
            vec![
                (Arc::clone(&public), vec!["acme.test"]),
                (Arc::clone(&private), vec!["contoso.test"]),
            ],
            infra,
            edge_toml(),
            1,
        );
        let callback = |runtime: &TenantRuntime, host: &str| {
            let flow = crate::oidc::FlowState {
                state: "st8".into(),
                verifier: "v3rifier".into(),
                nonce: "n0nce".into(),
                exp: (chrono::Utc::now().timestamp() + 600) as usize,
            };
            let cookie = runtime
                .admin
                .as_ref()
                .expect("admin configured")
                .issue_flow(&flow)
                .unwrap();
            Request::builder()
                .method(Method::GET)
                .uri("/admin/api/login/oidc/callback?code=c0de&state=st8")
                .header(header::HOST, host)
                .header(header::COOKIE, format!("{OIDC_FLOW_COOKIE}={cookie}"))
                .extension(peer("203.0.113.1"))
                .body(Body::empty())
                .unwrap()
        };
        let bounce = |response: axum::response::Response| {
            assert_eq!(response.status(), StatusCode::FOUND);
            response
                .headers()
                .get(header::LOCATION)
                .unwrap()
                .to_str()
                .unwrap()
                .to_string()
        };

        let response = router(Arc::clone(&state))
            .oneshot(callback(&public, "acme.test"))
            .await
            .unwrap();
        let location = bounce(response);
        assert!(
            location.contains("Couldn%27t+reach+the+identity+provider."),
            "{location}"
        );
        assert!(
            idp.received_requests().await.unwrap().is_empty(),
            "a public tenant's loopback IdP is never dialled"
        );

        let response = router(state)
            .oneshot(callback(&private, "contoso.test"))
            .await
            .unwrap();
        let location = bounce(response);
        assert!(
            location.contains("Sign-in+token+could+not+be+verified."),
            "discovery and the exchange ran; only the fake id_token failed: {location}"
        );
        let paths: Vec<String> = idp
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| r.url.path().to_string())
            .collect();
        assert!(
            paths.contains(&"/token".to_string()),
            "the private tenant exchanged its code: {paths:?}"
        );
    }

    /// One tenant on `acme.test` whose customers `issuer` verifies, served
    /// on a listener.
    async fn served_acme(issuer: &str) -> (Arc<EdgeState>, Arc<TenantRuntime>, SocketAddr) {
        let infra = Arc::new(SharedInfra::for_tests());
        let acme = tenant_on(
            "acme",
            "https://app.acme.test",
            "acme-client",
            issuer,
            &infra,
        );
        let state = EdgeState::for_tests(
            TenantMode::Multi,
            vec![(Arc::clone(&acme), vec!["acme.test"])],
            infra,
            edge_toml(),
            1,
        );
        let addr = serve(Arc::clone(&state)).await;
        (state, acme, addr)
    }

    #[tokio::test]
    async fn a_suspension_closes_an_open_socket_with_1008() {
        let (_idp, issuer) = mock_idp().await;
        let (state, acme, addr) = served_acme(&issuer).await;
        let path = format!("/public/ws/support?token={}", customer_token(&issuer));
        let mut socket = connect(addr, "acme.test", &path).await;
        assert_eq!(next_frame(&mut socket).await["type"], "connected");

        state.suspend(&acme.id, "Account on hold.".into()).await;

        let frame = next_frame(&mut socket).await;
        assert_eq!(frame["type"], "error");
        assert_eq!(frame["code"], "tenant_suspended");
        assert_eq!(frame["content"], "Account on hold.");
        let (code, reason) = next_close(&mut socket).await;
        assert_eq!(code, 1008, "policy violation: the widget stops");
        assert_eq!(reason, "Account on hold.");
    }

    #[tokio::test]
    async fn a_reload_tells_an_open_socket_to_reconnect_with_1012() {
        let (_idp, issuer) = mock_idp().await;
        let (state, acme, addr) = served_acme(&issuer).await;
        let path = format!("/public/ws/support?token={}", customer_token(&issuer));
        let mut socket = connect(addr, "acme.test", &path).await;
        assert_eq!(next_frame(&mut socket).await["type"], "connected");

        // What `reload` does once the rebuilt runtime is ready: install it,
        // which retires the one this socket is pinned to.
        let infra = SharedInfra::for_tests();
        let rebuilt = tenant_on(
            "acme",
            "https://app.acme.test",
            "acme-client",
            &issuer,
            &infra,
        );
        state.install(acme.id.clone(), rebuilt).unwrap();
        assert!(acme.is_retired());

        let frame = next_frame(&mut socket).await;
        assert_eq!(frame["type"], "error");
        assert_eq!(frame["code"], "tenant_reloaded");
        let (code, _) = next_close(&mut socket).await;
        assert_eq!(code, 1012, "service restart: the widget reconnects");

        let mut again = connect(addr, "acme.test", &path).await;
        assert_eq!(
            next_frame(&mut again).await["type"],
            "connected",
            "the replacement serves the reconnect"
        );
    }

    /// The pre-auth path: a suspended tenant's socket is upgraded so the
    /// widget can be told, then closed for good — and it costs no crypto.
    #[tokio::test]
    async fn a_suspended_tenants_socket_is_told_and_closed_before_any_auth() {
        let (state, acme, _) = two_tenants();
        state.suspend(&acme.id, "Account on hold.".into()).await;
        let addr = serve(Arc::clone(&state)).await;

        let mut socket = connect(addr, "acme.test", "/public/ws/support").await;
        let frame = next_frame(&mut socket).await;
        assert_eq!(frame["type"], "error");
        assert_eq!(frame["code"], "tenant_suspended");
        assert_eq!(frame["content"], "Account on hold.");
        let (code, reason) = next_close(&mut socket).await;
        assert_eq!(code, 1008);
        assert_eq!(reason, "Account on hold.");

        // A client that goes away before reading anything leaves the server
        // with a failed send and nothing else: the next socket is served.
        drop(connect(addr, "acme.test", "/public/ws/support").await);
        let mut socket = connect(addr, "acme.test", "/public/ws/support").await;
        assert_eq!(next_frame(&mut socket).await["code"], "tenant_suspended");
    }

    /// Hosts that name no served tenant share one per-address budget: they
    /// reach no tenant limiter, so without it a suspended tenant or a made-up
    /// host is a free endpoint.
    #[tokio::test]
    async fn hosts_naming_no_served_tenant_share_one_address_budget() {
        let (state, acme, _) = two_tenants();
        state.suspend(&acme.id, "Account on hold.".into()).await;
        let ip = "203.0.113.9";
        for _ in 0..UNRESOLVED_PER_MIN {
            let (status, _) = send(&state, get(Some("acme.test"), "/embed/support", ip)).await;
            assert_eq!(
                status,
                StatusCode::OK,
                "the harmless loader, while in budget"
            );
        }
        let (status, _) = send(&state, get(Some("acme.test"), "/embed/support", ip)).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        let (status, _) = send(&state, get(Some("other.test"), "/embed/support", ip)).await;
        assert_eq!(
            status,
            StatusCode::TOO_MANY_REQUESTS,
            "an unknown host draws on the same budget"
        );
        let (status, _) = send(
            &state,
            get(Some("acme.test"), "/embed/support", "203.0.113.10"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "another address has its own");
        let (status, _) = send(&state, get(Some("contoso.test"), "/embed/support", ip)).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "a served tenant has its own limiter"
        );
    }

    /// In multi-tenant mode one tenant's build failure suspends that tenant
    /// with the failure as its message; the others serve.
    #[tokio::test(flavor = "multi_thread")]
    async fn multi_tenant_boot_suspends_a_tenant_that_fails_to_build_and_serves_the_rest() {
        use workforce_common::session_postgres::{run_session_migrations, PostgresSessionStore};
        use workforce_postgres::test_support::with_test_pool;

        use crate::analytics::run_edge_analytics_migrations;
        use crate::tenant::{run_edge_migrations, TenantStore};

        with_test_pool(PostgresSessionStore::build_pool, |pool| async move {
            run_session_migrations(&pool).await.unwrap();
            run_edge_analytics_migrations(&pool).await.unwrap();
            run_edge_migrations(&pool).await.unwrap();
            let registry = TenantStore::new(pool.clone());
            registry.ensure_default().await.unwrap();
            registry
                .add_hostname(&TenantId::default_tenant(), "default.test")
                .await
                .unwrap();
            let contoso = TenantId::parse("contoso").unwrap();
            registry.create(&contoso, "Contoso").await.unwrap();
            registry
                .add_hostname(&contoso, "contoso.test")
                .await
                .unwrap();

            // The test infra has no `[vellev]`, so `default` (on the install's
            // own store) builds and any other tenant cannot get a store.
            let mut infra = SharedInfra::for_tests_on(pool)
                .with_master_key(crate::settings::tests::test_master_key());
            infra.rls_enforced = true;
            let mut cfg = edge_toml();
            cfg.server.multi_tenant = true;
            cfg.llm.allowed_providers = vec!["anthropic".into()];
            cfg.llm.api_key = None;
            let state = EdgeState::build(&cfg, Arc::new(infra))
                .await
                .expect("boot continues past one tenant's failure");

            assert!(matches!(state.resolve("default.test"), Resolved::Tenant(_)));
            let Resolved::Suspended(message) = state.resolve("contoso.test") else {
                panic!("the failed tenant is suspended");
            };
            assert_eq!(
                message, UNAVAILABLE_MESSAGE,
                "callers get a fixed message, never the failure"
            );
            let detail = state.unavailable(&contoso).expect("the failure is kept");
            assert!(
                detail.starts_with("tenant failed to start: ") && detail.contains("vellev"),
                "{detail}"
            );
            let (status, loader) = send(
                &state,
                get(Some("contoso.test"), "/embed/support", "203.0.113.1"),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "the loader still answers");
            assert!(
                loader.contains("console.warn") && loader.contains(UNAVAILABLE_MESSAGE),
                "{loader}"
            );
            assert!(
                !loader.contains("vellev") && !loader.contains("failed to start"),
                "the public loader carries no failure detail: {loader}"
            );
            let (status, body) = send(
                &state,
                get(
                    Some("contoso.test"),
                    "/public/conversations/support",
                    "203.0.113.1",
                ),
            )
            .await;
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
            let body: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(body["error"], "tenant_suspended");
            assert_eq!(body["message"], message);
        })
        .await;
    }

    fn registry_row(id: &str) -> Tenant {
        let now = chrono::Utc::now();
        Tenant {
            id: TenantId::parse(id).unwrap(),
            display_name: format!("{id} Ltd"),
            status: TenantStatus::Active,
            hostnames: vec![],
            plan: crate::tenant::Plan::default(),
            created_at: now,
            updated_at: now,
        }
    }

    /// Each tenant signs admin sessions with a key derived from the install
    /// secret and its id, and only `default` inherits the file's operators.
    #[test]
    fn a_session_minted_for_one_tenant_does_not_verify_under_anothers_key() {
        use crate::admin::AdminAuth;
        let cfg = edge_toml();
        let auth = |id: &str| {
            let settings = TenantSettings::for_tenant(&registry_row(id), &cfg, None);
            AdminAuth::new(
                settings.admin.as_ref().unwrap(),
                settings.id,
                settings.outbound,
            )
            .unwrap()
        };
        let acme = auth("acme");
        let token = acme.issue_session("alice@acme.test").unwrap();
        assert!(acme.verify_session(&token).is_some());
        assert!(
            auth("acme").verify_session(&token).is_some(),
            "the derivation is stable across rebuilds"
        );
        assert!(
            auth("contoso").verify_session(&token).is_none(),
            "another tenant's key does not verify it, before any claim is read"
        );

        let acme = TenantSettings::for_tenant(&registry_row("acme"), &cfg, None);
        let default = TenantSettings::from_edge_toml(&cfg);
        assert_ne!(
            acme.admin.as_ref().unwrap().session_secret,
            cfg.admin.as_ref().unwrap().session_secret,
            "no tenant signs with the raw install secret"
        );
        assert!(
            acme.admin.as_ref().unwrap().allowed_emails.is_empty(),
            "a registered tenant starts with no operators"
        );
        assert_eq!(
            default.admin.as_ref().unwrap().allowed_emails,
            ["ops@harbor.example"],
            "the self-hosted tenant keeps the file's"
        );
    }

    /// Until admin settings are per tenant, the file's `[admin]` is shared:
    /// the Google button mode can be, a break-glass password or a redirect
    /// client cannot.
    #[test]
    fn multi_tenant_boot_accepts_a_google_button_admin_and_refuses_redirect_or_password() {
        TenantSettings::admin_shareable(&admin_config()).expect("the Google button is shareable");

        let mut with_password = admin_config();
        with_password.password_hash = Some(
            "$argon2id$v=19$m=19456,t=2,p=1$c2FsdHNhbHRzYWx0$aGFzaGhhc2hoYXNoaGFzaGhhc2hoYXNoaGFzaGhhc2g"
                .into(),
        );
        let refused = TenantSettings::admin_shareable(&with_password).unwrap_err();
        assert!(refused.to_string().contains("password_hash"), "{refused}");

        let mut redirect = admin_config();
        redirect.oidc = Some(AdminOidc {
            issuer: "https://auth.harbor.example/realms/harbor".into(),
            audience: Some("edge-admin".into()),
            client_id: Some("edge-admin".into()),
            client_secret: None,
            mode: None,
        });
        redirect.public_url = Some("https://edge.harbor.example".into());
        let refused = TenantSettings::admin_shareable(&redirect).unwrap_err();
        assert!(refused.to_string().contains("redirect"), "{refused}");
    }

    /// A cloud deployment holds every tenant's secrets sealed and pins the
    /// providers tenants may bring a key for, so it refuses to boot without
    /// the key or the pin — before it touches the registry.
    #[tokio::test]
    async fn multi_tenant_boot_refuses_a_missing_master_key_and_an_empty_allowlist() {
        let mut cfg = edge_toml();
        cfg.server.multi_tenant = true;

        let mut infra = SharedInfra::for_tests();
        infra.rls_enforced = true;
        let Err(err) = EdgeState::build(&cfg, Arc::new(infra)).await else {
            panic!("boot must refuse");
        };
        assert!(err.to_string().contains("EDGE_MASTER_KEY"), "{err}");

        let mut infra =
            SharedInfra::for_tests().with_master_key(crate::settings::tests::test_master_key());
        infra.rls_enforced = true;
        let Err(err) = EdgeState::build(&cfg, Arc::new(infra)).await else {
            panic!("boot must refuse");
        };
        assert!(err.to_string().contains("[llm] allowed_providers"), "{err}");
    }

    /// A cloud deployment's file configures no tenant of its own, so a
    /// secret in it — the LLM key, the token-exchange secret, the admin
    /// OIDC client secret — or an allowed provider that takes no bearer
    /// key refuses boot, naming the field, before the registry is touched.
    #[tokio::test]
    async fn multi_tenant_boot_refuses_file_secrets_and_keyless_providers() {
        let refusal = |cfg: EdgeConfig| async move {
            let mut infra =
                SharedInfra::for_tests().with_master_key(crate::settings::tests::test_master_key());
            infra.rls_enforced = true;
            match EdgeState::build(&cfg, Arc::new(infra)).await {
                Err(err) => err.to_string(),
                Ok(_) => panic!("boot must refuse"),
            }
        };
        let mut base = edge_toml();
        base.server.multi_tenant = true;
        base.llm.allowed_providers = vec!["anthropic".into()];

        let with_llm_key = base.clone();
        assert!(
            with_llm_key.llm.api_key.is_some(),
            "the fixture carries one"
        );
        let err = refusal(with_llm_key).await;
        assert!(err.contains("[llm] api_key"), "{err}");
        assert!(!err.contains("sk-test"), "never the value: {err}");

        let mut with_exchange = base.clone();
        with_exchange.llm.api_key = None;
        with_exchange.token_exchange = Some(crate::config::TokenExchangeConfig {
            issuer: "https://kc.example/realms/x".into(),
            client_id: "edge".into(),
            client_secret: "te-secret".into(),
            audience: None,
            scope: None,
        });
        let err = refusal(with_exchange).await;
        assert!(err.contains("[token_exchange] client_secret"), "{err}");
        assert!(!err.contains("te-secret"), "{err}");

        let mut with_oidc_secret = base.clone();
        with_oidc_secret.llm.api_key = None;
        with_oidc_secret
            .admin
            .as_mut()
            .unwrap()
            .oidc
            .as_mut()
            .unwrap()
            .client_secret = Some("oidc-secret".into());
        let err = refusal(with_oidc_secret).await;
        assert!(err.contains("[admin.oidc] client_secret"), "{err}");
        assert!(!err.contains("oidc-secret"), "{err}");

        let mut with_vllm = base;
        with_vllm.llm.api_key = None;
        with_vllm.llm.allowed_providers = vec!["anthropic".into(), "vllm".into()];
        let err = refusal(with_vllm).await;
        assert!(
            err.contains("vllm") && err.contains("allowed_providers"),
            "{err}"
        );
    }

    /// An admin request as the SPA sends it: with the session cookie and the
    /// browser's own same-origin word, which a write needs.
    fn request(method: Method, path: &str, cookie: &str, body: Option<Value>) -> Request<Body> {
        let mut req = Request::builder()
            .method(method)
            .uri(path)
            .header(header::COOKIE, cookie)
            .header("sec-fetch-site", "same-origin");
        let body = match body {
            Some(json) => {
                req = req.header(header::CONTENT_TYPE, "application/json");
                Body::from(json.to_string())
            }
            None => Body::empty(),
        };
        req.extension(peer("203.0.113.1")).body(body).unwrap()
    }

    fn json(body: &str) -> Value {
        serde_json::from_str(body).unwrap_or_else(|e| panic!("{e}: {body}"))
    }

    /// A self-hosted state over a migrated database whose infra holds a
    /// master key: `default` on the `vllm` provider (a URL-keyed provider,
    /// which the key endpoint refuses — the probe itself is covered in
    /// `settings::tests`), with one agent host, an operator session and an
    /// empty plan.
    async fn settings_fixture(
        pool: workforce_postgres::PostgresPool,
    ) -> (Arc<EdgeState>, crate::settings::TenantSettingsStore, String) {
        settings_fixture_on(pool, crate::tenant::Plan::default()).await
    }

    /// [`settings_fixture`] with `default` on `plan`.
    async fn settings_fixture_on(
        pool: workforce_postgres::PostgresPool,
        plan: crate::tenant::Plan,
    ) -> (Arc<EdgeState>, crate::settings::TenantSettingsStore, String) {
        use workforce_common::session_postgres::run_session_migrations;

        use crate::analytics::run_edge_analytics_migrations;
        use crate::tenant::{run_edge_migrations, TenantStore};

        run_session_migrations(&pool).await.unwrap();
        run_edge_analytics_migrations(&pool).await.unwrap();
        run_edge_migrations(&pool).await.unwrap();
        let registry = TenantStore::new(pool.clone());
        registry.ensure_default().await.unwrap();
        registry
            .set_plan(&TenantId::default_tenant(), &plan)
            .await
            .unwrap();
        let infra = Arc::new(
            SharedInfra::for_tests_on(pool)
                .with_master_key(crate::settings::tests::test_master_key()),
        );
        let mut cfg = edge_toml();
        cfg.llm.provider = "vllm".into();
        cfg.llm.api_key = None;
        cfg.llm.default_model = Some("m".into());
        // The file's [admin], derived for `default` as a reload derives it, so
        // the session minted here stays valid across the rebuilds the
        // settings routes trigger.
        let settings = TenantSettings {
            limits: plan.cap_limits(TenantSettings::from_edge_toml(&cfg).limits),
            plan,
            ..TenantSettings::from_edge_toml(&cfg)
        };
        let host = AgentHost::for_test(
            Arc::new(EdgeAgent {
                id: "support".into(),
                ..Default::default()
            }),
            Arc::new(BlockingProvider),
            Arc::new(InMemorySessionStore::new()),
        );
        let runtime = TenantRuntime::for_tests(
            settings,
            HashMap::from([("support".to_string(), Arc::new(host))]),
            &infra,
        );
        let token = runtime
            .admin
            .as_ref()
            .unwrap()
            .issue_session("ops@harbor.example")
            .unwrap();
        let cookie = format!("{}={token}", crate::admin::SESSION_COOKIE);
        let store = infra
            .settings_store_for(&TenantId::default_tenant())
            .expect("the infra has a key");
        let state = EdgeState::for_tests(
            TenantMode::Single(TenantId::default_tenant()),
            vec![(runtime, vec![])],
            infra,
            cfg,
            1,
        );
        (state, store, cookie)
    }

    /// The key endpoint refuses a provider that takes no bearer key before
    /// any probe — a URL-keyed provider would let the key point the edge at
    /// any address — and stores nothing; the settings and session probes
    /// carry the key's state, and the key itself never appears in any
    /// response. (The probe against a live provider is covered in
    /// `settings::tests`, below the provider rule.)
    #[tokio::test(flavor = "multi_thread")]
    async fn an_llm_key_for_a_keyless_provider_is_refused_and_a_stored_keys_state_is_visible() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        use workforce_common::session_postgres::PostgresSessionStore;
        use workforce_postgres::test_support::with_test_pool;

        with_test_pool(PostgresSessionStore::build_pool, |pool| async move {
            let (state, store, cookie) = settings_fixture(pool).await;
            let accepting = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v1/chat/completions"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{"message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1},
                })))
                .expect(0)
                .mount(&accepting)
                .await;

            let (status, body) = send(
                &state,
                request(
                    Method::PUT,
                    "/admin/api/settings/llm-key",
                    &cookie,
                    Some(serde_json::json!({"key": accepting.uri()})),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
            let body = json(&body);
            assert_eq!(
                body["error"],
                crate::settings::KeyFailure::ProviderNotAllowed.message()
            );
            assert!(body["error"].as_str().unwrap().starts_with("provider not allowed"));
            assert_eq!(body["llm_status"]["state"], "failed");
            assert!(!body.to_string().contains(&accepting.uri()), "{body}");
            assert!(
                !store.load().await.unwrap().unwrap().has_llm_key(),
                "nothing is stored"
            );

            // A key the row holds (as a bearer-key tenant's would): its state
            // reaches every probe, its value none of them.
            store.set_llm_key("sk-ant-stored-123456").await.unwrap();
            store
                .set_llm_status(&crate::settings::LlmStatus::ok())
                .await
                .unwrap();
            state.reload(&TenantId::default_tenant()).await.unwrap();
            let (status, body) =
                send(&state, request(Method::GET, "/admin/api/settings", &cookie, None)).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let view = json(&body);
            assert_eq!(view["llm"]["provider"], "vllm");
            assert!(view["llm"].get("api_key").is_none(), "{view}");
            assert_eq!(view["has_llm_key"], true);
            assert_eq!(view["stored"], true);
            assert_eq!(view["editable"], true);
            assert_eq!(view["llm_status"]["state"], "ok");
            assert!(!body.contains("sk-ant-stored"), "the key never leaves the server");
            let (status, body) =
                send(&state, request(Method::GET, "/admin/api/me", &cookie, None)).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(json(&body)["llm_status"]["state"], "ok");

            let (status, body) = send(
                &state,
                request(Method::DELETE, "/admin/api/settings/llm-key", &cookie, None),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert!(!store.load().await.unwrap().unwrap().has_llm_key());
            let (_, body) =
                send(&state, request(Method::GET, "/admin/api/settings", &cookie, None)).await;
            assert_eq!(json(&body)["llm_status"]["state"], "unset");
        })
        .await;
    }

    /// The cloud shape at the router (production, 2026-10-09): the file has
    /// no `[admin]` at all, the tenant's sign-in is on its settings row as
    /// the platform set it, and the infra holds the master key. The admin
    /// session minted by the runtime a settings route built is still good
    /// after the rebuilds the key routes trigger: `/admin/api/me` is 200
    /// before `PUT /admin/api/settings/llm-key`, after it, and after the
    /// `DELETE` that rebuilds the tenant. (No bearer-key provider can be
    /// pointed at a mock, so the `PUT` is refused at the provider rule
    /// before its probe and rebuilds nothing; the `DELETE` and an explicit
    /// reload are the rebuilds the session must outlive.)
    #[tokio::test(flavor = "multi_thread")]
    async fn an_admin_session_outlives_the_rebuilds_settings_routes_trigger_without_a_file_admin() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        use workforce_common::session_postgres::{run_session_migrations, PostgresSessionStore};
        use workforce_postgres::test_support::with_test_pool;

        use crate::analytics::run_edge_analytics_migrations;
        use crate::tenant::{run_edge_migrations, TenantStore};

        with_test_pool(PostgresSessionStore::build_pool, |pool| async move {
            run_session_migrations(&pool).await.unwrap();
            run_edge_analytics_migrations(&pool).await.unwrap();
            run_edge_migrations(&pool).await.unwrap();
            TenantStore::new(pool.clone()).ensure_default().await.unwrap();
            let infra = Arc::new(
                SharedInfra::for_tests_on(pool).with_install_keys(crate::boot::test_install_keys()),
            );
            let mut cfg = edge_toml();
            cfg.admin = None;
            cfg.llm.provider = "vllm".into();
            cfg.llm.api_key = None;
            cfg.llm.default_model = Some("m".into());
            let default = TenantId::default_tenant();
            let store = infra
                .settings_store_for(&default)
                .expect("the infra has a key");
            store
                .seed_if_empty(&TenantSettings::from_edge_toml(&cfg))
                .await
                .unwrap();
            store
                .set_admin(&AdminConfig {
                    allowed_emails: vec!["ops@harbor.example".into()],
                    session_secret: None,
                    ..admin_config()
                })
                .await
                .unwrap();
            let state = EdgeState::for_tests(TenantMode::Single(default.clone()), vec![], infra, cfg, 1);
            state.reload(&default).await.unwrap();
            let Resolved::Tenant(built) = state.serving(&default) else {
                panic!("the tenant is served");
            };
            let token = built
                .admin
                .as_ref()
                .expect("the row's admin section")
                .issue_session("ops@harbor.example")
                .unwrap();
            let cookie = format!("{}={token}", crate::admin::SESSION_COOKIE);
            let me = |state: &Arc<EdgeState>| {
                let state = Arc::clone(state);
                let cookie = cookie.clone();
                async move { send(&state, request(Method::GET, "/admin/api/me", &cookie, None)).await }
            };
            let (status, body) = me(&state).await;
            assert_eq!(status, StatusCode::OK, "{body}");

            let provider = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/v1/chat/completions"))
                .respond_with(ResponseTemplate::new(200))
                .expect(0)
                .mount(&provider)
                .await;
            let (status, body) = send(
                &state,
                request(
                    Method::PUT,
                    "/admin/api/settings/llm-key",
                    &cookie,
                    Some(serde_json::json!({"key": provider.uri()})),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
            let (status, body) = me(&state).await;
            assert_eq!(status, StatusCode::OK, "{body}");

            store.set_llm_key("sk-ant-stored-123456").await.unwrap();
            let (status, body) = send(
                &state,
                request(Method::DELETE, "/admin/api/settings/llm-key", &cookie, None),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let Resolved::Tenant(rebuilt) = state.serving(&default) else {
                panic!("the tenant is served");
            };
            assert!(!Arc::ptr_eq(&built, &rebuilt), "the route rebuilt the tenant");
            let (status, body) = me(&state).await;
            assert_eq!(status, StatusCode::OK, "{body}");

            state.reload(&default).await.unwrap();
            let (status, body) = me(&state).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(json(&body)["operator"], "ops@harbor.example");
        })
        .await;
    }

    /// A tenant admin's write must come from the tenant's own page: without
    /// the browser's same-origin word or a matching `Origin` it is 403, with
    /// a foreign `Origin` it is 403, and a read passes regardless.
    #[tokio::test]
    async fn admin_writes_need_a_same_origin_request() {
        let (state, acme, _) = two_tenants();
        let token = acme
            .admin
            .as_ref()
            .unwrap()
            .issue_session("alice@acme.test")
            .unwrap();
        let cookie = format!("{}={token}", crate::admin::SESSION_COOKIE);
        let on_acme = |method: Method, headers: &[(&str, &str)]| {
            let mut req = Request::builder()
                .method(method)
                .uri("/admin/api/settings")
                .header(header::HOST, "acme.test")
                .header(header::COOKIE, &cookie)
                .header(header::CONTENT_TYPE, "application/json");
            for (name, value) in headers {
                req = req.header(*name, *value);
            }
            req.extension(peer("203.0.113.1"))
                .body(Body::from("{}"))
                .unwrap()
        };
        let (status, _) = send(&state, on_acme(Method::GET, &[])).await;
        assert_eq!(status, StatusCode::OK, "a read needs no proof");
        let (status, _) = send(&state, on_acme(Method::PUT, &[])).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "no header, no write");
        let (status, _) = send(
            &state,
            on_acme(Method::PUT, &[("origin", "https://contoso.test")]),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "another tenant's page");
        let (status, _) = send(
            &state,
            on_acme(Method::PUT, &[("sec-fetch-site", "same-site")]),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "a sibling subdomain");
        let (status, body) = send(
            &state,
            on_acme(Method::PUT, &[("origin", "https://acme.test")]),
        )
        .await;
        assert_ne!(
            status,
            StatusCode::FORBIDDEN,
            "the tenant's own page passes the guard: {body}"
        );
        let (status, _) = send(
            &state,
            on_acme(Method::PUT, &[("sec-fetch-site", "same-origin")]),
        )
        .await;
        assert_ne!(status, StatusCode::FORBIDDEN);
    }

    /// A partial `llm` section edits the fields it names and keeps the rest:
    /// a save that changes the model never resets the failover chain or the
    /// retry policy, and a `null` clears an optional field.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_partial_llm_section_keeps_the_fields_it_omits() {
        use workforce_common::session_postgres::PostgresSessionStore;
        use workforce_postgres::test_support::with_test_pool;

        with_test_pool(PostgresSessionStore::build_pool, |pool| async move {
            let (state, store, cookie) = settings_fixture(pool).await;
            let (_, body) = send(
                &state,
                request(Method::GET, "/admin/api/settings", &cookie, None),
            )
            .await;
            let mut doc = json(&body);
            for derived in [
                "llm_status",
                "has_llm_key",
                "has_token_exchange_secret",
                "has_admin_client_secret",
                "stored",
                "editable",
                "outbound",
            ] {
                doc.as_object_mut().unwrap().remove(derived);
            }
            doc["llm"]["failover"] = serde_json::json!(["gemini:standard"]);
            doc["llm"]["retry"]["max_retries"] = Value::from(7);
            doc["llm"]["tier_key"] = Value::from("custom");
            let (status, body) = send(
                &state,
                request(
                    Method::PUT,
                    "/admin/api/settings",
                    &cookie,
                    Some(doc.clone()),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");

            let mut partial = doc.clone();
            partial["llm"] = serde_json::json!({"default_model": "cheap", "tier_key": null});
            let (status, body) = send(
                &state,
                request(Method::PUT, "/admin/api/settings", &cookie, Some(partial)),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let stored = store.load().await.unwrap().unwrap();
            assert_eq!(stored.llm.default_model.as_deref(), Some("cheap"));
            assert_eq!(stored.llm.failover, ["gemini:standard"], "kept");
            assert_eq!(stored.llm.retry.max_retries, 7, "kept");
            assert_eq!(stored.llm.tier_key, None, "null clears");
            assert_eq!(stored.llm.provider, "vllm", "kept");

            let mut mistyped = doc;
            mistyped["llm"] = serde_json::json!({"failover": "not-a-list"});
            let (status, body) = send(
                &state,
                request(Method::PUT, "/admin/api/settings", &cookie, Some(mistyped)),
            )
            .await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
            assert!(body.contains("expected a sequence"), "{body}");
            assert_eq!(
                store.load().await.unwrap().unwrap().llm.failover,
                ["gemini:standard"],
                "a refused save changes nothing"
            );
        })
        .await;
    }

    /// A tenant cannot raise its limits past its plan's ceilings: a save
    /// above one is refused naming the field, one at it is taken.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_plan_caps_what_a_settings_save_may_set_the_limits_to() {
        use workforce_common::session_postgres::PostgresSessionStore;
        use workforce_postgres::test_support::with_test_pool;

        with_test_pool(PostgresSessionStore::build_pool, |pool| async move {
            let plan = crate::tenant::Plan {
                rate_limit_per_min: Some(100),
                user_rate_limit_per_min: Some(10),
                max_concurrent_turns: Some(2),
                ..Default::default()
            };
            let (state, store, cookie) = settings_fixture_on(pool, plan).await;
            let (_, body) = send(
                &state,
                request(Method::GET, "/admin/api/settings", &cookie, None),
            )
            .await;
            let mut doc = json(&body);
            assert_eq!(
                doc["limits"]["rate_limit_per_min"], 2,
                "the file's 2, under 100"
            );
            assert_eq!(
                doc["limits"]["user_rate_limit_per_min"], 10,
                "the file's 60, capped"
            );
            for derived in [
                "llm_status",
                "has_llm_key",
                "has_token_exchange_secret",
                "has_admin_client_secret",
                "stored",
                "editable",
                "outbound",
            ] {
                doc.as_object_mut().unwrap().remove(derived);
            }
            for (field, over) in [
                ("rate_limit_per_min", 101),
                ("user_rate_limit_per_min", 11),
                ("max_concurrent_turns", 3),
            ] {
                let mut raised = doc.clone();
                raised["limits"][field] = Value::from(over);
                let (status, body) = send(
                    &state,
                    request(Method::PUT, "/admin/api/settings", &cookie, Some(raised)),
                )
                .await;
                assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{field}: {body}");
                assert!(body.contains(field) && body.contains("plan"), "{body}");
                assert!(store.load().await.unwrap().is_none(), "nothing saved");
            }
            let mut at_ceiling = doc;
            at_ceiling["limits"] = serde_json::json!({
                "rate_limit_per_min": 100,
                "user_rate_limit_per_min": 10,
                "max_concurrent_turns": 2,
            });
            let (status, body) = send(
                &state,
                request(
                    Method::PUT,
                    "/admin/api/settings",
                    &cookie,
                    Some(at_ceiling),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let Resolved::Tenant(rebuilt) = state.resolve("any.host") else {
                panic!("the tenant serves");
            };
            assert_eq!(rebuilt.settings.limits.rate_limit_per_min, 100);
            assert_eq!(rebuilt.turn_limit.available_permits(), 2);
        })
        .await;
    }

    /// A settings save rebuilds the tenant on the new values; one that names
    /// a secret, the outbound policy (the platform's to set), an unknown
    /// provider, a provider outside the file's allowlist, or settings the
    /// tenant cannot start on is refused, the last with the previous row put
    /// back.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_settings_save_rebuilds_the_tenant_and_a_bad_one_is_refused() {
        use workforce_common::session_postgres::PostgresSessionStore;
        use workforce_postgres::test_support::with_test_pool;

        with_test_pool(PostgresSessionStore::build_pool, |pool| async move {
            let (state, store, cookie) = settings_fixture(pool).await;
            let (_, body) = send(
                &state,
                request(Method::GET, "/admin/api/settings", &cookie, None),
            )
            .await;
            let mut doc = json(&body);
            assert_eq!(doc["stored"], false, "no row until the first save");
            assert_eq!(
                doc["limits"]["rate_limit_per_min"], 2,
                "the file's, meanwhile"
            );
            assert_eq!(doc["outbound"], "private", "shown, for the record");
            for derived in [
                "llm_status",
                "has_llm_key",
                "has_token_exchange_secret",
                "has_admin_client_secret",
                "stored",
                "editable",
            ] {
                doc.as_object_mut().unwrap().remove(derived);
            }

            let mut with_key = doc.clone();
            with_key["llm"]["api_key"] = Value::from("sk-smuggled");
            let (status, body) = send(
                &state,
                request(Method::PUT, "/admin/api/settings", &cookie, Some(with_key)),
            )
            .await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
            assert!(body.contains("llm.api_key"), "{body}");

            let mut loosened = doc.clone();
            loosened["outbound"] = Value::from("private");
            let (status, body) = send(
                &state,
                request(Method::PUT, "/admin/api/settings", &cookie, Some(loosened)),
            )
            .await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
            assert!(body.contains("outbound"), "{body}");
            assert!(store.load().await.unwrap().is_none(), "nothing saved");
            doc.as_object_mut().unwrap().remove("outbound");

            let mut unknown = doc.clone();
            unknown["llm"]["provider"] = Value::from("no-such-provider");
            let (status, body) = send(
                &state,
                request(Method::PUT, "/admin/api/settings", &cookie, Some(unknown)),
            )
            .await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
            assert!(body.contains("unknown llm provider"), "{body}");

            let mut edited = doc.clone();
            edited["limits"]["rate_limit_per_min"] = Value::from(77);
            edited["llm"]["default_model"] = Value::from("cheap");
            let (status, body) = send(
                &state,
                request(Method::PUT, "/admin/api/settings", &cookie, Some(edited)),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let stored = store.load().await.unwrap().expect("saved");
            assert_eq!(stored.limits.rate_limit_per_min, 77);
            assert_eq!(stored.llm.default_model.as_deref(), Some("cheap"));
            assert_eq!(
                stored.outbound,
                OutboundPolicy::Private,
                "the policy is what the tenant ran on"
            );
            let Resolved::Tenant(rebuilt) = state.resolve("any.host") else {
                panic!("the tenant serves");
            };
            assert_eq!(
                rebuilt.settings.limits.rate_limit_per_min, 77,
                "the running tenant was rebuilt on the saved settings"
            );
            assert_eq!(rebuilt.settings.llm.default_model.as_deref(), Some("cheap"));

            let mut unbootable = doc.clone();
            unbootable["admin"] = serde_json::json!({ "session_hours": 1 });
            let (status, body) = send(
                &state,
                request(
                    Method::PUT,
                    "/admin/api/settings",
                    &cookie,
                    Some(unbootable),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
            assert!(body.contains("sign-in method"), "{body}");
            assert_eq!(
                store
                    .load()
                    .await
                    .unwrap()
                    .unwrap()
                    .limits
                    .rate_limit_per_min,
                77,
                "the previous row is back"
            );
        })
        .await;
    }

    /// Without a master key the settings API shows the file's values and
    /// takes no writes: the install behaves as it did before the table.
    #[tokio::test]
    async fn without_a_master_key_settings_are_read_only() {
        let (state, acme, _) = two_tenants();
        let token = acme
            .admin
            .as_ref()
            .unwrap()
            .issue_session("alice@acme.test")
            .unwrap();
        let cookie = format!("{}={token}", crate::admin::SESSION_COOKIE);
        let on_acme = |method: Method, path: &str, body: Option<Value>| {
            let mut req = request(method, path, &cookie, body);
            req.headers_mut()
                .insert(header::HOST, "acme.test".parse().unwrap());
            req
        };
        let (status, body) = send(&state, on_acme(Method::GET, "/admin/api/settings", None)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let view = json(&body);
        assert_eq!(view["editable"], false);
        assert_eq!(view["stored"], false);
        assert_eq!(view["has_llm_key"], true, "the file's key is in effect");
        assert!(!body.contains("sk-test"), "and never shown");
        let (status, body) = send(
            &state,
            on_acme(
                Method::PUT,
                "/admin/api/settings/llm-key",
                Some(serde_json::json!({"key": "sk-new"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(body.contains("EDGE_MASTER_KEY"), "{body}");
    }

    /// A self-hosted state over a migrated database: `default` on the file's
    /// settings (anthropic, a key, no master key), one agent host `support`
    /// with a base tool allowlist, an operator session, and the config-repo
    /// loader pointed at `github` when given.
    async fn agent_fixture(
        pool: workforce_postgres::PostgresPool,
        github: Option<&str>,
    ) -> (Arc<EdgeState>, Arc<TenantRuntime>, String) {
        agent_fixture_on_plan(pool, github, crate::tenant::Plan::default()).await
    }

    /// [`agent_fixture`] with the tenant on `plan`.
    async fn agent_fixture_on_plan(
        pool: workforce_postgres::PostgresPool,
        github: Option<&str>,
        plan: crate::tenant::Plan,
    ) -> (Arc<EdgeState>, Arc<TenantRuntime>, String) {
        use workforce_common::session_postgres::run_session_migrations;

        use crate::analytics::run_edge_analytics_migrations;
        use crate::tenant::{run_edge_migrations, TenantStore};

        run_session_migrations(&pool).await.unwrap();
        run_edge_analytics_migrations(&pool).await.unwrap();
        run_edge_migrations(&pool).await.unwrap();
        TenantStore::new(pool.clone())
            .ensure_default()
            .await
            .unwrap();
        let mut infra = SharedInfra::for_tests_on(pool);
        if let Some(github) = github {
            infra = infra.with_github_api(github);
        }
        let infra = Arc::new(infra);
        let cfg = edge_toml();
        let settings = TenantSettings {
            plan,
            ..TenantSettings::from_edge_toml(&cfg)
        };
        let host = AgentHost::for_test(
            Arc::new(EdgeAgent {
                id: "support".into(),
                display_name: "Support".into(),
                embed: crate::agent::EmbedSettings {
                    keycloak_issuer: "https://idp.test".into(),
                    allowed_tools: vec!["acme.list_orders".into(), "acme.reports.*".into()],
                    ..Default::default()
                },
                ..Default::default()
            }),
            Arc::new(BlockingProvider),
            Arc::new(InMemorySessionStore::new()),
        );
        let runtime = TenantRuntime::for_tests(
            settings,
            HashMap::from([("support".to_string(), Arc::new(host))]),
            &infra,
        );
        let token = runtime
            .admin
            .as_ref()
            .unwrap()
            .issue_session("ops@harbor.example")
            .unwrap();
        let cookie = format!("{}={token}", crate::admin::SESSION_COOKIE);
        let state = EdgeState::for_tests(
            TenantMode::Single(TenantId::default_tenant()),
            vec![(Arc::clone(&runtime), vec![])],
            infra,
            cfg,
            1,
        );
        (state, runtime, cookie)
    }

    /// The `support` agent as the admin list shows it.
    async fn listed_agent(state: &Arc<EdgeState>, cookie: &str, id: &str) -> (Value, String) {
        let (status, body) = send(
            state,
            request(Method::GET, "/admin/api/agents", cookie, None),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let agent = json(&body)["agents"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["id"] == id)
            .cloned()
            .unwrap_or_else(|| panic!("agent {id} is listed: {body}"));
        (agent, body)
    }

    /// Every integration field round-trips through PUT and GET, the
    /// credential is stored and reported but never returned — not by the
    /// list, not by the revision — an edit that says nothing about it keeps
    /// it, and the empty string clears it.
    #[tokio::test(flavor = "multi_thread")]
    async fn agent_put_round_trips_integration_fields_and_never_returns_the_credential() {
        use workforce_common::session_postgres::PostgresSessionStore;
        use workforce_postgres::test_support::with_test_pool;

        with_test_pool(PostgresSessionStore::build_pool, |pool| async move {
            let (state, runtime, cookie) = agent_fixture(pool, None).await;
            // A credential makes the host discover at build, so the server
            // must exist — and is where the credential is allowed to go.
            let mcp = crate::mcp::tests::mock_mcp(&["acme.list_orders"]).await;
            let mcp_url = format!("{}/mcp", mcp.uri());
            const CREDENTIAL: &str = "svc-secret-9f8e7d";
            let edit = serde_json::json!({
                "mcp": {"url": format!(" {mcp_url} "), "discovery_credential": CREDENTIAL},
                "permissions": {"url": "https://api.acme.test/users/{user}/permissions", "cache_ttl_secs": 60},
                "tool_grants": [
                    {"source": "backend_permissions", "requires": "items:edit", "tools": ["acme.item_transition", " "]},
                    {"source": "host_hint", "requires": "manager", "tools": ["acme.bulk_advance"]}
                ],
                "policy": {"confirm_writes": true},
                "image_hosts": ["Cdn.Acme.Test", ".files.acme.test", ""],
                "spill_skip_tools": ["acme.reports.signed_urls", "acme.item_transition"],
                "passive_memory": false,
            });
            let (status, body) = send(
                &state,
                request(Method::PUT, "/admin/api/agents/support", &cookie, Some(edit)),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");

            let (agent, body) = listed_agent(&state, &cookie, "support").await;
            assert!(!body.contains(CREDENTIAL), "the credential never leaves the server: {body}");
            assert_eq!(agent["has_mcp"], true);
            assert_eq!(agent["mcp_url"], mcp_url);
            assert_eq!(agent["has_discovery_credential"], true);
            assert!(
                mcp.received_requests().await.unwrap().iter().any(|r| {
                    r.headers.get("authorization").and_then(|v| v.to_str().ok())
                        == Some(&format!("Bearer {CREDENTIAL}"))
                }),
                "the credential went to the MCP server, and nowhere else"
            );
            assert_eq!(
                agent["permissions"],
                serde_json::json!({"url": "https://api.acme.test/users/{user}/permissions", "cache_ttl_secs": 60})
            );
            assert_eq!(
                agent["tool_grants"],
                serde_json::json!([
                    {"source": "backend_permissions", "requires": "items:edit", "tools": ["acme.item_transition"]},
                    {"source": "host_hint", "requires": "manager", "tools": ["acme.bulk_advance"]}
                ])
            );
            assert_eq!(agent["policy"], serde_json::json!({"confirm_writes": true}));
            assert_eq!(
                agent["image_hosts"],
                serde_json::json!(["cdn.acme.test", ".files.acme.test"])
            );
            assert_eq!(
                agent["spill_skip_tools"],
                serde_json::json!(["acme.reports.signed_urls", "acme.item_transition"])
            );
            assert_eq!(agent["passive_memory"], false);
            assert!(
                agent.get("mcp").is_none() && agent.get("discovery_credential").is_none(),
                "{agent}"
            );

            let stored = runtime.config_store.load_all().await.unwrap();
            let mcp = stored[0].embed.mcp.as_ref().expect("stored with its MCP");
            assert_eq!(mcp.discovery_credential.as_deref(), Some(CREDENTIAL));
            assert!(stored[0].embed.confirm_writes_enabled());
            assert!(!stored[0].embed.passive_memory_enabled());
            let revisions = runtime.revisions.list("agent", "support").await.unwrap();
            assert_eq!(revisions.len(), 1);
            let content = runtime
                .revisions
                .get("agent", "support", revisions[0].revision)
                .await
                .unwrap()
                .unwrap();
            assert!(content.contains(&mcp_url), "{content}");
            assert!(!content.contains(CREDENTIAL), "{content}");

            // Silence keeps the credential; the empty string clears it.
            let (status, body) = send(
                &state,
                request(
                    Method::PUT,
                    "/admin/api/agents/support",
                    &cookie,
                    Some(serde_json::json!({"mcp": {"url": mcp_url}, "welcome": "hi"})),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let (agent, _) = listed_agent(&state, &cookie, "support").await;
            assert_eq!(agent["has_discovery_credential"], true, "absent = unchanged");
            assert_eq!(agent["welcome"], "hi");
            let (status, body) = send(
                &state,
                request(
                    Method::PUT,
                    "/admin/api/agents/support",
                    &cookie,
                    Some(serde_json::json!({"mcp": {"url": mcp_url, "discovery_credential": ""}})),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let (agent, _) = listed_agent(&state, &cookie, "support").await;
            assert_eq!(agent["has_discovery_credential"], false, "empty string clears");
            let stored = runtime.config_store.load_all().await.unwrap();
            assert_eq!(stored[0].embed.mcp.as_ref().unwrap().discovery_credential, None);

            // `null` blocks remove them; `mcp: null` makes a pure-chat agent.
            let (status, body) = send(
                &state,
                request(
                    Method::PUT,
                    "/admin/api/agents/support",
                    &cookie,
                    Some(serde_json::json!({"mcp": null, "permissions": null, "policy": null, "tool_grants": [], "spill_skip_tools": []})),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let (agent, _) = listed_agent(&state, &cookie, "support").await;
            assert_eq!(agent["has_mcp"], false);
            assert_eq!(agent["permissions"], Value::Null);
            assert_eq!(agent["policy"], Value::Null);
        })
        .await;
    }

    /// Additional servers through the admin API: listed without credentials,
    /// kept by an edit that does not name them, write-only credentials per
    /// id, normalised so an untouched save stores the same config, refused
    /// when orphaned, covered by the spill list, and stripped from revisions
    /// with restore re-attaching only what still points where it did.
    #[tokio::test(flavor = "multi_thread")]
    async fn additional_mcp_servers_round_trip_through_the_admin_api() {
        use workforce_common::session_postgres::PostgresSessionStore;
        use workforce_postgres::test_support::with_test_pool;

        with_test_pool(PostgresSessionStore::build_pool, |pool| async move {
            let (state, runtime, cookie) = agent_fixture(pool, None).await;
            let primary = crate::mcp::tests::mock_mcp(&["acme.list_orders"]).await;
            let docs = crate::mcp::tests::mock_mcp(&["search", "edit_page"]).await;
            let billing = crate::mcp::tests::mock_mcp(&["billing.list_invoices"]).await;
            let docs_url = format!("{}/mcp", docs.uri());
            const DOCS_SECRET: &str = "docs-svc-71ab";
            let put = |body: Value| {
                let state = Arc::clone(&state);
                let cookie = cookie.clone();
                async move {
                    send(
                        &state,
                        request(Method::PUT, "/admin/api/agents/support", &cookie, Some(body)),
                    )
                    .await
                }
            };
            let stored = || async { runtime.config_store.load_all().await.unwrap().remove(0) };
            let credential_of = |agent: &EdgeAgent, id: &str| {
                agent
                    .embed
                    .mcp_servers
                    .iter()
                    .find(|s| s.id == id)
                    .and_then(|s| s.connection.discovery_credential.clone())
            };

            let (status, body) = put(serde_json::json!({
                "mcp": {"url": format!("{}/mcp", primary.uri()), "required": true},
                "mcp_servers": [
                    {"id": " docs ", "url": format!(" {docs_url} "), "tool_prefix": "docs",
                     "audience": " ", "unavailable_skip_secs": 60, "required": false,
                     "allowed_tools": ["docs.search", " "], "discovery_credential": DOCS_SECRET,
                     "tool_grants": [{"source": "keycloak_role", "requires": "editor", "tools": ["docs.edit_page"]}]},
                    {"id": "billing", "url": format!("{}/mcp", billing.uri()), "auth": "forwarded",
                     "allowed_tools": ["billing.*"]},
                ],
                "spill_skip_tools": ["docs.search"],
            }))
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");

            let (agent, listing) = listed_agent(&state, &cookie, "support").await;
            assert!(!listing.contains(DOCS_SECRET), "{listing}");
            assert_eq!(
                agent["mcp_servers"],
                serde_json::json!([
                    {"id": "docs", "url": docs_url, "auth": "exchanged", "audience": null,
                     "required": false, "unavailable_skip_secs": 60, "has_discovery_credential": true,
                     "tool_prefix": "docs", "allowed_tools": ["docs.search"],
                     "tool_grants": [{"source": "keycloak_role", "requires": "editor", "tools": ["docs.edit_page"]}]},
                    {"id": "billing", "url": format!("{}/mcp", billing.uri()), "auth": "forwarded",
                     "audience": null, "required": false, "unavailable_skip_secs": 60,
                     "has_discovery_credential": false, "tool_prefix": null,
                     "allowed_tools": ["billing.*"], "tool_grants": []},
                ])
            );
            assert_eq!(agent["mcp_settings"]["required"], true);
            assert_eq!(agent["mcp_settings"]["id"], "default");
            let saved = stored().await;
            let raw = serde_json::to_value(&saved.embed).unwrap();
            assert_eq!(
                raw["mcp"],
                serde_json::json!({"url": format!("{}/mcp", primary.uri()), "discovery_credential": null}),
                "defaults are normalised away, so the primary stores as before"
            );
            assert!(raw["mcp_servers"][0].get("required").is_none(), "{raw}");
            assert!(raw["mcp_servers"][0].get("unavailable_skip_secs").is_none(), "{raw}");
            assert_eq!(credential_of(&saved, "docs").as_deref(), Some(DOCS_SECRET));

            // What the editor sends back unchanged stores exactly what was there.
            let echo = |view: &Value| {
                let mut view = view.as_object().unwrap().clone();
                view.remove("has_discovery_credential");
                Value::Object(view)
            };
            let untouched = serde_json::json!({
                "mcp": echo(&agent["mcp_settings"]),
                "mcp_servers": agent["mcp_servers"].as_array().unwrap().iter().map(echo).collect::<Vec<_>>(),
                "allowed_tools": agent["allowed_tools"],
                "tool_grants": agent["tool_grants"],
            });
            let (status, body) = put(untouched).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(
                serde_json::to_value(stored().await).unwrap(),
                serde_json::to_value(&saved).unwrap(),
                "an untouched save is byte-for-byte the stored config"
            );

            // A body that predates additional servers, or edits only the
            // primary, leaves them and their credentials alone.
            for body in [
                serde_json::json!({"welcome": "hi"}),
                serde_json::json!({"mcp": {"url": format!("{}/mcp", primary.uri())}}),
            ] {
                let (status, response) = put(body).await;
                assert_eq!(status, StatusCode::OK, "{response}");
                let kept = stored().await;
                assert_eq!(kept.embed.mcp_servers.len(), 2);
                assert_eq!(credential_of(&kept, "docs").as_deref(), Some(DOCS_SECRET));
            }

            let docs_row = |credential: Option<&str>, id: &str| {
                let mut row = serde_json::json!({"id": id, "url": docs_url, "tool_prefix": "docs",
                    "allowed_tools": ["docs.search"]});
                if let Some(c) = credential {
                    row["discovery_credential"] = c.into();
                }
                serde_json::json!({"mcp_servers": [row]})
            };
            let (status, body) = put(docs_row(None, "docs")).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(
                credential_of(&stored().await, "docs").as_deref(),
                Some(DOCS_SECRET),
                "absent keeps"
            );
            put(docs_row(Some("docs-svc-2"), "docs")).await;
            assert_eq!(
                credential_of(&stored().await, "docs").as_deref(),
                Some("docs-svc-2"),
                "a new value replaces"
            );
            put(docs_row(Some(""), "docs")).await;
            assert_eq!(credential_of(&stored().await, "docs"), None, "empty clears");
            put(docs_row(Some("docs-svc-3"), "docs")).await;
            put(docs_row(None, "pages")).await;
            assert_eq!(
                credential_of(&stored().await, "pages"),
                None,
                "a renamed server starts with no credential"
            );

            // The primary cannot be cleared from under its servers.
            let (status, body) = put(serde_json::json!({"mcp": null})).await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
            assert!(body.contains("need the primary server"), "{body}");

            // Revisions never hold a credential; restore re-attaches the live
            // one only where the server still points where it did.
            put(docs_row(Some(DOCS_SECRET), "docs")).await;
            let revisions = runtime.revisions.list("agent", "support").await.unwrap();
            let latest = revisions[0].revision;
            let content = runtime
                .revisions
                .get("agent", "support", latest)
                .await
                .unwrap()
                .unwrap();
            assert!(content.contains(&docs_url) && !content.contains(DOCS_SECRET), "{content}");
            let moved = format!("{}/moved", docs.uri());
            put(serde_json::json!({"mcp_servers": [{"id": "docs", "url": moved,
                "tool_prefix": "docs", "allowed_tools": ["docs.search"]}]}))
            .await;
            let restore = |revision: i32| {
                let state = Arc::clone(&state);
                let cookie = cookie.clone();
                async move {
                    send(
                        &state,
                        request(
                            Method::POST,
                            &format!("/admin/api/revisions/agent/support/{revision}/restore"),
                            &cookie,
                            None,
                        ),
                    )
                    .await
                }
            };
            let (status, body) = restore(latest).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(
                credential_of(&stored().await, "docs"),
                None,
                "the live credential was set for another URL, so it does not follow"
            );
            put(docs_row(Some(DOCS_SECRET), "docs")).await;
            let (status, body) = restore(latest).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(
                credential_of(&stored().await, "docs").as_deref(),
                Some(DOCS_SECRET),
                "same id, same URL: the live credential is kept"
            );

            let (status, body) = put(serde_json::json!({"mcp_servers": []})).await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
            assert!(
                body.contains("spill_skip_tools"),
                "the spill list still names a removed server's tool: {body}"
            );
            let (status, body) =
                put(serde_json::json!({"mcp_servers": [], "spill_skip_tools": []})).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert!(stored().await.embed.mcp_servers.is_empty(), "[] removes them all");
        })
        .await;
    }

    /// Every field of an additional server's edit follows one rule: absent
    /// keeps the stored value, `null` resets it to the default (refused where
    /// the default is not a valid config), a value replaces it. A new id
    /// starts from the defaults, and one without a URL is refused by name.
    #[tokio::test(flavor = "multi_thread")]
    async fn every_mcp_server_field_keeps_when_absent_resets_on_null_and_replaces_when_present() {
        use workforce_common::session_postgres::PostgresSessionStore;
        use workforce_postgres::test_support::with_test_pool;

        with_test_pool(PostgresSessionStore::build_pool, |pool| async move {
            let (state, _runtime, cookie) = agent_fixture(pool, None).await;
            let primary = crate::mcp::tests::mock_mcp(&["acme.list_orders"]).await;
            let docs = crate::mcp::tests::mock_mcp(&["search", "edit_page"]).await;
            let docs_url = format!("{}/mcp", docs.uri());
            let primary_url = format!("{}/mcp", primary.uri());
            let put = |body: Value| {
                let state = Arc::clone(&state);
                let cookie = cookie.clone();
                async move {
                    send(
                        &state,
                        request(Method::PUT, "/admin/api/agents/support", &cookie, Some(body)),
                    )
                    .await
                }
            };
            let listed = |id: &'static str| {
                let state = Arc::clone(&state);
                let cookie = cookie.clone();
                async move {
                    let (agent, _) = listed_agent(&state, &cookie, "support").await;
                    agent["mcp_servers"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|s| s["id"] == id)
                        .cloned()
                }
            };
            let grant = |requires: &str, tool: &str| {
                serde_json::json!([{"source": "keycloak_role", "requires": requires, "tools": [tool]}])
            };
            let merged = |patch: Value| {
                let mut row = serde_json::json!({
                    "id": "docs", "url": docs_url, "auth": "exchanged", "audience": "docs-aud",
                    "required": false, "unavailable_skip_secs": 15, "tool_prefix": "docs",
                    "allowed_tools": ["docs.search"], "tool_grants": grant("editor", "docs.edit_page"),
                });
                row.as_object_mut()
                    .unwrap()
                    .extend(patch.as_object().unwrap().clone());
                row
            };
            let (status, body) = put(serde_json::json!({"mcp": {"url": primary_url}})).await;
            assert_eq!(status, StatusCode::OK, "{body}");

            // (stored before, edit, the field it touches, what it reads after)
            let cases = [
                (serde_json::json!({}), serde_json::json!({"url": format!("{}/v2", docs.uri())}),
                    "url", Value::from(format!("{}/v2", docs.uri()))),
                (serde_json::json!({"audience": null}), serde_json::json!({"auth": "forwarded"}),
                    "auth", "forwarded".into()),
                (serde_json::json!({"auth": "forwarded", "audience": null}), serde_json::json!({"auth": null}),
                    "auth", "exchanged".into()),
                (serde_json::json!({}), serde_json::json!({"audience": "other-aud"}),
                    "audience", "other-aud".into()),
                (serde_json::json!({}), serde_json::json!({"audience": null}), "audience", Value::Null),
                (serde_json::json!({}), serde_json::json!({"required": true, "unavailable_skip_secs": null}),
                    "required", true.into()),
                (serde_json::json!({"required": true, "unavailable_skip_secs": null}),
                    serde_json::json!({"required": null}), "required", false.into()),
                (serde_json::json!({}), serde_json::json!({"unavailable_skip_secs": 30}),
                    "unavailable_skip_secs", 30.into()),
                (serde_json::json!({}), serde_json::json!({"unavailable_skip_secs": null}),
                    "unavailable_skip_secs", crate::agent::DEFAULT_UNAVAILABLE_SKIP_SECS.into()),
                (serde_json::json!({}), serde_json::json!({"tool_prefix": "pages",
                    "allowed_tools": ["pages.search"], "tool_grants": grant("editor", "pages.edit_page")}),
                    "tool_prefix", "pages".into()),
                (serde_json::json!({}), serde_json::json!({"tool_prefix": null, "allowed_tools": ["search"],
                    "tool_grants": null}), "tool_prefix", Value::Null),
                (serde_json::json!({}), serde_json::json!({"allowed_tools": ["docs.*"]}),
                    "allowed_tools", serde_json::json!(["docs.*"])),
                (serde_json::json!({}), serde_json::json!({"tool_grants": grant("admin", "docs.edit_page")}),
                    "tool_grants", grant("admin", "docs.edit_page")),
                (serde_json::json!({}), serde_json::json!({"tool_grants": null}),
                    "tool_grants", serde_json::json!([])),
            ];
            for (before, edit, field, after) in cases {
                let (status, body) = put(serde_json::json!({"mcp_servers": [merged(before.clone())]})).await;
                assert_eq!(status, StatusCode::OK, "{before}: {body}");
                let stored = listed("docs").await.unwrap();

                let (status, body) = put(serde_json::json!({"mcp_servers": [{"id": "docs"}]})).await;
                assert_eq!(status, StatusCode::OK, "{body}");
                assert_eq!(listed("docs").await.unwrap(), stored, "a row naming only its id keeps every field");

                let mut row = edit.clone();
                row["id"] = "docs".into();
                let (status, body) = put(serde_json::json!({"mcp_servers": [row]})).await;
                assert_eq!(status, StatusCode::OK, "{edit}: {body}");
                let now = listed("docs").await.unwrap();
                assert_eq!(now[field], after, "{edit}");
                let mut untouched = stored.as_object().unwrap().clone();
                untouched.retain(|k, _| edit.get(k).is_none());
                assert!(
                    untouched.iter().all(|(k, v)| now[k.as_str()] == *v),
                    "{edit} changed only what it named: {stored} -> {now}"
                );
            }

            // A reset whose default is not a valid config is refused by name.
            put(serde_json::json!({"mcp_servers": [merged(serde_json::json!({}))]})).await;
            for (edit, message) in [
                (serde_json::json!({"id": "docs", "url": null}), "mcp_servers[docs].url is required"),
                (serde_json::json!({"id": "docs", "url": " "}), "mcp_servers[docs].url is required"),
                (serde_json::json!({"id": "docs", "allowed_tools": null}),
                    "mcp_servers[docs].allowed_tools must name at least one tool"),
            ] {
                let (status, body) = put(serde_json::json!({"mcp_servers": [edit]})).await;
                assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
                assert!(body.contains(message), "{body}");
            }

            // A new id starts from the defaults, and needs a URL.
            let (status, body) = put(serde_json::json!({"mcp_servers": [
                {"id": "docs"},
                {"id": "wiki", "tool_prefix": "wiki", "allowed_tools": ["wiki.search"]},
            ]}))
            .await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
            assert!(body.contains("mcp_servers[wiki].url is required"), "{body}");
            let (status, body) = put(serde_json::json!({"mcp_servers": [
                {"id": "docs"},
                {"id": "wiki", "url": docs_url, "tool_prefix": "wiki", "allowed_tools": ["wiki.search"]},
            ]}))
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(
                listed("wiki").await.unwrap(),
                serde_json::json!({"id": "wiki", "url": docs_url, "auth": "exchanged", "audience": null,
                    "required": false, "unavailable_skip_secs": crate::agent::DEFAULT_UNAVAILABLE_SKIP_SECS,
                    "has_discovery_credential": false, "tool_prefix": "wiki",
                    "allowed_tools": ["wiki.search"], "tool_grants": []})
            );
            assert_eq!(listed("docs").await.unwrap()["audience"], "docs-aud");

            // The primary follows the same rule: a body without a URL keeps it.
            let (status, body) = put(serde_json::json!({"mcp": {"auth": "forwarded"}})).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let (agent, _) = listed_agent(&state, &cookie, "support").await;
            assert_eq!(agent["mcp_settings"]["url"], primary_url.as_str());
            assert_eq!(agent["mcp_settings"]["auth"], "forwarded");
        })
        .await;
    }

    /// Saving an optional server that never answers returns at once: its
    /// static config is checked, its discovery is left to the background, and
    /// the response claims nothing about its tools.
    #[tokio::test(flavor = "multi_thread")]
    async fn saving_a_blackholed_optional_server_does_not_wait_on_its_discovery() {
        use workforce_common::session_postgres::PostgresSessionStore;
        use workforce_postgres::test_support::with_test_pool;

        with_test_pool(PostgresSessionStore::build_pool, |pool| async move {
            let (state, _runtime, cookie) = agent_fixture(pool, None).await;
            let primary = crate::mcp::tests::mock_mcp(&["acme.list_orders"]).await;
            let blackhole = crate::mcp::tests::slow_mcp(std::time::Duration::from_secs(600)).await;
            let save = |docs_url: String| {
                let body = serde_json::json!({
                    "mcp": {"url": format!("{}/mcp", primary.uri())},
                    "mcp_servers": [{"id": "docs", "url": docs_url, "tool_prefix": "docs",
                        "allowed_tools": ["docs.*"], "discovery_credential": "docs-svc"}],
                });
                send(
                    &state,
                    request(
                        Method::PUT,
                        "/admin/api/agents/support",
                        &cookie,
                        Some(body),
                    ),
                )
            };

            let started = std::time::Instant::now();
            let (status, body) = save(format!("{}/mcp", blackhole.uri())).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert!(
                started.elapsed() < std::time::Duration::from_secs(10),
                "the save waited on discovery: {:?}",
                started.elapsed()
            );
            assert_eq!(json(&body), serde_json::json!({"ok": true}));

            let (status, body) = save("ftp://docs.acme.test".into()).await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
            assert!(body.contains("mcp_servers[docs].url"), "{body}");
        })
        .await;
    }

    /// A tenant with no database behind it, for the checks that answer
    /// before any store is touched.
    fn store_less_fixture(git_managed: bool) -> (Arc<EdgeState>, String) {
        let infra = Arc::new(SharedInfra::for_tests());
        let cfg = edge_toml();
        let mut settings = TenantSettings::from_edge_toml(&cfg);
        if git_managed {
            settings.git_config = Some(crate::config::GitConfig {
                repo: "acme/edge-config".into(),
                git_ref: "main".into(),
                token: "ghp_managed".into(),
                poll_secs: 0,
            });
        }
        let host = AgentHost::for_test(
            Arc::new(EdgeAgent {
                id: "support".into(),
                embed: crate::agent::EmbedSettings {
                    allowed_tools: vec!["acme.list_orders".into()],
                    ..Default::default()
                },
                ..Default::default()
            }),
            Arc::new(BlockingProvider),
            Arc::new(InMemorySessionStore::new()),
        );
        let runtime = TenantRuntime::for_tests(
            settings,
            HashMap::from([("support".to_string(), Arc::new(host))]),
            &infra,
        );
        let token = runtime
            .admin
            .as_ref()
            .unwrap()
            .issue_session("ops@harbor.example")
            .unwrap();
        let cookie = format!("{}={token}", crate::admin::SESSION_COOKIE);
        let state = EdgeState::for_tests(
            TenantMode::Single(TenantId::default_tenant()),
            vec![(runtime, vec![])],
            infra,
            cfg,
            1,
        );
        (state, cookie)
    }

    /// Each integration field is checked for shape, and the 422 names it.
    #[tokio::test]
    async fn agent_put_refuses_malformed_integration_fields() {
        let (state, cookie) = store_less_fixture(false);
        let refused = |edit: Value, names: &'static str| {
            let state = Arc::clone(&state);
            let cookie = cookie.clone();
            async move {
                let (status, body) = send(
                    &state,
                    request(
                        Method::PUT,
                        "/admin/api/agents/support",
                        &cookie,
                        Some(edit),
                    ),
                )
                .await;
                assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
                assert!(body.contains(names), "{body}");
            }
        };
        refused(
            serde_json::json!({"permissions": {"url": "https://api.acme.test/me/permissions"}}),
            "{user}",
        )
        .await;
        refused(
            serde_json::json!({"permissions": {"url": "not a url {user}"}}),
            "permissions.url",
        )
        .await;
        refused(
            serde_json::json!({"spill_skip_tools": ["acme.signed_urls"]}),
            "spill_skip_tools",
        )
        .await;
        refused(
            serde_json::json!({"tool_grants": [{"source": "keycloak_role", "requires": " ", "tools": ["acme.x"]}]}),
            "tool_grants[0]",
        )
        .await;
        refused(
            serde_json::json!({"tool_grants": [{"source": "backend_permissions", "requires": "items:edit", "tools": []}]}),
            "tool_grants[0]",
        )
        .await;
        refused(
            serde_json::json!({"tool_grants": [{"source": "ldap", "requires": "x", "tools": ["acme.x"]}]}),
            "source",
        )
        .await;
        refused(
            serde_json::json!({"image_hosts": ["https://cdn.acme.test"]}),
            "image_hosts",
        )
        .await;
        refused(
            serde_json::json!({"image_hosts": ["cdn.acme.test/images"]}),
            "image_hosts",
        )
        .await;
        refused(
            serde_json::json!({"mcp": {"url": "ftp://mcp.acme.test"}}),
            "mcp.url",
        )
        .await;
        let orphan = serde_json::json!({"mcp_servers": [
            {"id": "docs", "url": "https://docs.acme.test/mcp", "allowed_tools": ["docs.*"]}
        ]});
        refused(orphan, "need the primary server").await;
        let overlapping = serde_json::json!({
            "mcp": {"url": "https://mcp.acme.test/mcp"},
            "mcp_servers": [{"id": "crm", "url": "https://crm.acme.test/mcp", "allowed_tools": ["acme_list_orders"]}],
        });
        refused(overlapping.clone(), "mcp_servers[crm]").await;
        refused(overlapping, "on server default").await;
        refused(
            serde_json::json!({
                "mcp": {"url": "https://mcp.acme.test/mcp"},
                "mcp_servers": [{"id": "docs", "url": "ftp://docs.acme.test", "allowed_tools": ["docs.*"]}],
            }),
            "mcp_servers[docs].url",
        )
        .await;
        refused(
            serde_json::json!({
                "mcp": {"url": "https://mcp.acme.test/mcp"},
                "mcp_servers": [{"id": "docs", "url": "https://docs.acme.test/mcp", "allowed_tools": ["docs.*"],
                    "tool_grants": [{"source": "keycloak_role", "requires": "", "tools": ["docs.edit"]}]}],
            }),
            "mcp_servers[docs].tool_grants[0]",
        )
        .await;
        refused(
            serde_json::json!({"mcp": {"url": "https://mcp.acme.test/mcp", "auth": "forwarded", "audience": "acme-mcp"}}),
            "mcp.audience",
        )
        .await;
        // A grant unlocks tools the spill list may then name.
        let (status, body) = send(
            &state,
            request(
                Method::PUT,
                "/admin/api/agents/support",
                &cookie,
                Some(serde_json::json!({
                    "tool_grants": [{"source": "backend_permissions", "requires": "items:edit", "tools": ["acme.signed_urls"]}],
                    "spill_skip_tools": ["acme.signed_urls", "acme.list_orders"],
                })),
            ),
        )
        .await;
        assert_ne!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    }

    /// Git mode keeps these items read-only: the edit and the import are
    /// both 409 before anything is fetched or written.
    #[tokio::test]
    async fn a_git_managed_tenant_refuses_agent_edits_and_imports() {
        let (state, cookie) = store_less_fixture(true);
        let (status, body) = send(
            &state,
            request(
                Method::PUT,
                "/admin/api/agents/support",
                &cookie,
                Some(serde_json::json!({"passive_memory": false})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(body.contains("managed in git"), "{body}");
        let (status, body) = send(
            &state,
            request(
                Method::POST,
                "/admin/api/import/git",
                &cookie,
                Some(serde_json::json!({"repo": "acme/edge-config", "token": "ghp_x"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(body.contains("managed in git"), "{body}");
    }

    /// An operator id_token `issuer` would have minted for the admin client.
    fn operator_token(issuer: &str, email: &str) -> String {
        crate::keycloak::tests::sign(
            jsonwebtoken::Algorithm::RS256,
            serde_json::json!({
                "iss": issuer,
                "aud": "edge-admin",
                "sub": "op-1",
                "exp": 4_070_908_800u64,
                "email": email,
                "email_verified": true,
            }),
        )
    }

    /// The tenant's policy reaches the admin id-token validator: a `public`
    /// tenant whose admin issuer is a loopback `http` IdP refuses the sign-in
    /// before a byte reaches it; a `private` tenant verifies against the
    /// same IdP and mints a session.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_public_tenant_refuses_a_loopback_admin_issuer_at_login_and_a_private_one_signs_in() {
        use workforce_common::session_postgres::{run_session_migrations, PostgresSessionStore};
        use workforce_postgres::test_support::with_test_pool;

        use crate::analytics::run_edge_analytics_migrations;
        use crate::tenant::{run_edge_migrations, TenantStore};

        with_test_pool(PostgresSessionStore::build_pool, |pool| async move {
            run_session_migrations(&pool).await.unwrap();
            run_edge_analytics_migrations(&pool).await.unwrap();
            run_edge_migrations(&pool).await.unwrap();
            let registry = TenantStore::new(pool.clone());
            for id in ["acme", "contoso"] {
                registry
                    .create(&TenantId::parse(id).unwrap(), id)
                    .await
                    .unwrap();
            }
            let (idp, issuer) = mock_idp().await;
            let infra = Arc::new(SharedInfra::for_tests_on(pool));
            let admin = AdminConfig {
                oidc: Some(AdminOidc {
                    issuer: issuer.clone(),
                    audience: Some("edge-admin".into()),
                    client_id: Some("edge-admin".into()),
                    client_secret: None,
                    mode: Some("google".into()),
                }),
                ..admin_config()
            };
            let public = tenant_reaching(
                "acme",
                "https://idp.test",
                admin.clone(),
                OutboundPolicy::Public,
                &infra,
            );
            let private = tenant_reaching(
                "contoso",
                "https://idp.test",
                admin,
                OutboundPolicy::Private,
                &infra,
            );
            for runtime in [&public, &private] {
                runtime
                    .operators
                    .add("email", "alice@example.com", "test")
                    .await
                    .unwrap();
            }
            let state = EdgeState::for_tests(
                TenantMode::Multi,
                vec![
                    (Arc::clone(&public), vec!["acme.test"]),
                    (Arc::clone(&private), vec!["contoso.test"]),
                ],
                infra,
                edge_toml(),
                1,
            );
            let login = |host: &str| {
                let mut req = request(
                    Method::POST,
                    "/admin/api/login/oidc",
                    "",
                    Some(
                        serde_json::json!({"token": operator_token(&issuer, "alice@example.com")}),
                    ),
                );
                req.headers_mut()
                    .insert(header::HOST, host.parse().unwrap());
                req
            };
            let (status, body) = send(&state, login("acme.test")).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
            assert!(
                idp.received_requests().await.unwrap().is_empty(),
                "nothing may reach a loopback issuer under public"
            );

            let response = router(Arc::clone(&state))
                .oneshot(login("contoso.test"))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let cookie = response
                .headers()
                .get(header::SET_COOKIE)
                .and_then(|v| v.to_str().ok())
                .expect("a session cookie");
            assert!(cookie.starts_with(crate::admin::SESSION_COOKIE), "{cookie}");
            assert!(
                !idp.received_requests().await.unwrap().is_empty(),
                "the private tenant fetched the issuer's keys"
            );
        })
        .await;
    }

    /// Every log line the process emits from the first call on, so a test
    /// can assert what a secret never reached. One global subscriber for the
    /// test binary; installed on first use.
    fn captured_logs() -> Arc<std::sync::Mutex<Vec<u8>>> {
        static LOGS: std::sync::OnceLock<Arc<std::sync::Mutex<Vec<u8>>>> =
            std::sync::OnceLock::new();
        Arc::clone(LOGS.get_or_init(|| {
            struct Sink(Arc<std::sync::Mutex<Vec<u8>>>);
            impl std::io::Write for Sink {
                fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                    self.0.lock().unwrap().extend_from_slice(buf);
                    Ok(buf.len())
                }
                fn flush(&mut self) -> std::io::Result<()> {
                    Ok(())
                }
            }
            let logs = Arc::new(std::sync::Mutex::new(Vec::new()));
            let sink = Arc::clone(&logs);
            let subscriber = tracing_subscriber::fmt()
                .with_max_level(tracing::Level::TRACE)
                .with_ansi(false)
                .with_writer(move || Sink(Arc::clone(&sink)))
                .finish();
            tracing::subscriber::set_global_default(subscriber)
                .expect("the test binary's only global subscriber");
            logs
        }))
    }

    /// The repo the hosted cutover imports, in miniature: one agent with
    /// its issuer and MCP credential bound to placeholders, one playbook,
    /// one training doc.
    const IMPORT_REPO: &[(&str, &str)] = &[
        (
            "agents/assist/agent.toml",
            "id = \"assist\"\ndisplay_name = \"Assist\"\nsystem_prompt = \"You help.\"\nplaybooks = [\"escalate\"]\n\
             [embed]\nkeycloak_issuer = \"${EDGE_KC_ISSUER}\"\nallowed_origins = [\"https://app.acme.test\"]\n\
             allowed_tools = [\"acme.*\"]\n[embed.mcp]\nurl = \"${EDGE_MCP_URL}\"\n\
             discovery_credential = \"${EDGE_MCP_CREDENTIAL}\"\n",
        ),
        (
            "playbooks/escalate.toml",
            "[playbook]\nid = \"escalate\"\nname = \"Escalate\"\n[[step]]\nid = \"why\"\ntitle = \"Why\"\nprompt = \"Ask why.\"\n",
        ),
        ("agents/assist/training/shifts.md", "# Shifts\n\nShifts run 8 to 4."),
        ("README.md", "ignored"),
    ];

    /// An agent whose additional server's URL and credential are bound to
    /// placeholders, under an id no other fixture uses.
    const IMPORT_REPO_WITH_SERVERS: &[(&str, &str)] = &[(
        "agents/northwind-help/agent.toml",
        "id = \"northwind-help\"\ndisplay_name = \"Help\"\nsystem_prompt = \"You help.\"\n\
         [embed]\nkeycloak_issuer = \"https://kc.acme.test/realms/staff\"\n\
         allowed_tools = [\"acme.*\"]\n[embed.mcp]\nurl = \"${EDGE_MCP_URL}\"\n\
         [[embed.mcp_servers]]\nid = \"docs\"\nurl = \"${EDGE_DOCS_MCP_URL}\"\n\
         discovery_credential = \"${EDGE_DOCS_MCP_CREDENTIAL}\"\ntool_prefix = \"docs\"\n\
         allowed_tools = [\"docs.*\"]\n",
    )];

    #[tokio::test(flavor = "multi_thread")]
    async fn a_git_import_reports_placeholders_inside_additional_servers() {
        use workforce_common::session_postgres::PostgresSessionStore;
        use workforce_postgres::test_support::with_test_pool;

        const CREDENTIAL: &str = "docs-svc-5d2e";
        with_test_pool(PostgresSessionStore::build_pool, |pool| async move {
            let github = crate::git_config::tests::mock_github(
                "acme/edge-config",
                "main",
                IMPORT_REPO_WITH_SERVERS,
            )
            .await;
            let (state, _runtime, cookie) = agent_fixture(pool, Some(&github.uri())).await;
            let import =
                |body: Value| request(Method::POST, "/admin/api/import/git", &cookie, Some(body));
            let (status, body) = send(
                &state,
                import(serde_json::json!({"repo": "acme/edge-config", "token": "ghp_x"})),
            )
            .await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
            assert_eq!(
                json(&body)["unresolved"],
                serde_json::json!([
                    "EDGE_DOCS_MCP_CREDENTIAL",
                    "EDGE_DOCS_MCP_URL",
                    "EDGE_MCP_URL"
                ])
            );

            let primary = crate::mcp::tests::mock_mcp(&["acme.list_orders"]).await;
            let docs = crate::mcp::tests::mock_mcp(&["search"]).await;
            let (status, body) = send(
                &state,
                import(serde_json::json!({
                    "repo": "acme/edge-config",
                    "token": "ghp_x",
                    "env": {
                        "EDGE_MCP_URL": format!("{}/mcp", primary.uri()),
                        "EDGE_DOCS_MCP_URL": format!("{}/mcp", docs.uri()),
                        "EDGE_DOCS_MCP_CREDENTIAL": CREDENTIAL,
                    },
                })),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let (agent, listing) = listed_agent(&state, &cookie, "northwind-help").await;
            assert_eq!(agent["mcp_servers"][0]["has_discovery_credential"], true);
            assert!(!listing.contains(CREDENTIAL), "{listing}");
            let Resolved::Tenant(rebuilt) = state.resolve("any.host") else {
                panic!("the tenant serves");
            };
            let stored = rebuilt.config_store.load_all().await.unwrap();
            assert_eq!(
                stored[0].embed.mcp_servers[0]
                    .connection
                    .discovery_credential
                    .as_deref(),
                Some(CREDENTIAL)
            );
            let revisions = rebuilt
                .revisions
                .list("agent", "northwind-help")
                .await
                .unwrap();
            let content = rebuilt
                .revisions
                .get("agent", "northwind-help", revisions[0].revision)
                .await
                .unwrap()
                .unwrap();
            assert!(
                content.contains("mcp_servers") && !content.contains(CREDENTIAL),
                "{content}"
            );
        })
        .await;
    }

    /// The one-shot import: placeholders bind from the request alone and an
    /// unbound one refuses the whole import with nothing written; bound, the
    /// repo's agent, playbook and training land in the tenant's stores, the
    /// tenant is rebuilt on them, a revision and an audit row record who did
    /// it, and the token is in none of it — not the rows, not the logs.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_git_import_refuses_unbound_placeholders_and_writes_the_repo_when_bound() {
        use workforce_common::session_postgres::PostgresSessionStore;
        use workforce_postgres::test_support::with_test_pool;

        const TOKEN: &str = "ghp_7e1cdb9a4f3b2c1d";
        const CREDENTIAL: &str = "mcp-svc-4c9a1e";
        let logs = captured_logs();
        with_test_pool(PostgresSessionStore::build_pool, |pool| async move {
            let github =
                crate::git_config::tests::mock_github("acme/edge-config", "main", IMPORT_REPO).await;
            let (state, _runtime, cookie) = agent_fixture(pool, Some(&github.uri())).await;
            let import = |body: Value| {
                request(Method::POST, "/admin/api/import/git", &cookie, Some(body))
            };

            let (status, body) = send(
                &state,
                import(serde_json::json!({"repo": "acme/edge-config", "token": TOKEN})),
            )
            .await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
            let refused = json(&body);
            assert_eq!(
                refused["unresolved"],
                serde_json::json!(["EDGE_KC_ISSUER", "EDGE_MCP_CREDENTIAL", "EDGE_MCP_URL"])
            );
            let Resolved::Tenant(tenant) = state.resolve("any.host") else {
                panic!("served");
            };
            assert!(
                tenant.config_store.load_all().await.unwrap().is_empty(),
                "nothing is written on a refusal"
            );
            assert!(tenant.playbook_store.load_all().await.unwrap().is_empty());
            assert!(tenant
                .audit
                .recent(10)
                .await
                .unwrap()
                .iter()
                .all(|e| e.action != "import.git"));

            let mcp = crate::mcp::tests::mock_mcp(&["acme.list_orders"]).await;
            let bound = serde_json::json!({
                "repo": "acme/edge-config",
                "git_ref": "main",
                "token": TOKEN,
                "env": {
                    "EDGE_KC_ISSUER": "https://kc.acme.test/realms/staff",
                    "EDGE_MCP_CREDENTIAL": CREDENTIAL,
                    "EDGE_MCP_URL": format!("{}/mcp", mcp.uri()),
                },
            });
            let (status, body) = send(&state, import(bound.clone())).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(
                json(&body),
                serde_json::json!({"agents": 1, "playbooks": 1, "training_docs": 1, "unresolved": []})
            );

            let (agent, listing) = listed_agent(&state, &cookie, "assist").await;
            assert_eq!(agent["keycloak_issuer"], "https://kc.acme.test/realms/staff");
            assert_eq!(agent["playbooks"], serde_json::json!(["escalate"]));
            assert_eq!(agent["has_discovery_credential"], true);
            assert!(!listing.contains(CREDENTIAL) && !listing.contains(TOKEN), "{listing}");
            let Resolved::Tenant(rebuilt) = state.resolve("any.host") else {
                panic!("the tenant serves");
            };
            assert!(rebuilt.host("assist").is_some(), "rebuilt on the import");
            let stored = rebuilt.config_store.load_all().await.unwrap();
            assert_eq!(stored.len(), 1);
            assert_eq!(
                stored[0].embed.mcp.as_ref().unwrap().discovery_credential.as_deref(),
                Some(CREDENTIAL)
            );
            assert_eq!(
                rebuilt.playbook_store.load_all().await.unwrap(),
                vec![("escalate".to_string(), IMPORT_REPO[1].1.to_string())]
            );
            let revisions = rebuilt.revisions.list("agent", "assist").await.unwrap();
            assert_eq!(revisions.len(), 1);
            assert_eq!(revisions[0].edited_by, "ops@harbor.example");
            let content = rebuilt
                .revisions
                .get("agent", "assist", revisions[0].revision)
                .await
                .unwrap()
                .unwrap();
            assert!(content.contains("kc.acme.test"), "{content}");
            assert!(!content.contains(CREDENTIAL) && !content.contains(TOKEN), "{content}");
            assert_eq!(
                rebuilt.revisions.list("playbook", "escalate").await.unwrap().len(),
                1
            );
            let audit = rebuilt.audit.recent(10).await.unwrap();
            let row = audit
                .iter()
                .find(|e| e.action == "import.git")
                .expect("an audit row");
            assert_eq!(row.operator, "ops@harbor.example");
            assert_eq!(row.target, "acme/edge-config@main");
            assert_eq!(row.detail, "1 agent(s), 1 playbook(s), 1 training doc(s)");
            assert!(!row.detail.contains(TOKEN) && !row.target.contains(TOKEN));

            // Importing again replaces the agent in place and adds a revision.
            let (status, body) = send(&state, import(bound)).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let Resolved::Tenant(again) = state.resolve("any.host") else {
                panic!("the tenant serves");
            };
            assert_eq!(again.config_store.load_all().await.unwrap().len(), 1);
            assert_eq!(again.revisions.list("agent", "assist").await.unwrap().len(), 2);

            let sent = github.received_requests().await.unwrap();
            assert_eq!(sent.len(), 3, "one fetch per import attempt");
            let captured = String::from_utf8_lossy(&logs.lock().unwrap()).into_owned();
            assert!(captured.contains("config repo imported"), "{captured}");
            assert!(!captured.contains(TOKEN), "the token reached a log line:\n{captured}");
            assert!(!captured.contains(CREDENTIAL), "the credential reached a log line:\n{captured}");
        })
        .await;
    }

    /// A create body for agent `id`, complete enough to build.
    fn create_body(id: &str) -> Value {
        serde_json::json!({
            "id": id,
            "display_name": id,
            "keycloak_issuer": "https://idp.test",
            "identity": {"identity": "You help."},
        })
    }

    /// The plan's agent cap binds on a create: with `support` mounted under
    /// a cap of two, a second agent is created and a third is a 422 under
    /// the `plan_limit` code naming the cap, with nothing stored; the probe
    /// and the setup report carry the count beside the cap.
    #[tokio::test(flavor = "multi_thread")]
    async fn creating_an_agent_beyond_the_plan_cap_is_422_and_at_the_cap_succeeds() {
        use crate::tenant::{Plan, PlanLimit};
        use workforce_common::session_postgres::PostgresSessionStore;
        use workforce_postgres::test_support::with_test_pool;

        with_test_pool(PostgresSessionStore::build_pool, |pool| async move {
            let plan = Plan {
                agents: Some(2),
                ..Plan::default()
            };
            let (state, runtime, cookie) = agent_fixture_on_plan(pool, None, plan).await;
            let create = |id: &str| {
                request(
                    Method::POST,
                    "/admin/api/agents",
                    &cookie,
                    Some(create_body(id)),
                )
            };

            let (status, body) = send(&state, create("billing")).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(runtime.host_count(), 2, "at the cap");

            let (status, body) = send(&state, create("returns")).await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
            let refused = json(&body);
            assert_eq!(refused["error"], PlanLimit::CODE);
            assert_eq!(
                refused["message"],
                "the plan allows 2 agent(s); the workspace would have 3"
            );
            assert_eq!(runtime.host_count(), 2, "nothing mounted");
            assert!(
                !runtime.config_store.exists("returns").await.unwrap(),
                "nothing stored"
            );

            let (status, body) =
                send(&state, request(Method::GET, "/admin/api/me", &cookie, None)).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let me = json(&body);
            assert_eq!(me["plan"]["agents"], 2);
            assert_eq!(me["usage"]["agents"], 2);
            assert_eq!(me["over"], serde_json::json!(["agents"]));

            let (status, body) = send(
                &state,
                request(Method::GET, "/admin/api/setup", &cookie, None),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let report = json(&body);
            assert_eq!(report["agents"], 2);
            assert_eq!(report["agent_cap"], 2);
        })
        .await;
    }

    /// Two agents with nothing to bind or dial, one of them `support`, which
    /// the fixture already mounts.
    const CAPPED_IMPORT_REPO: &[(&str, &str)] = &[
        (
            "agents/support/agent.toml",
            "id = \"support\"\ndisplay_name = \"Support\"\nsystem_prompt = \"You help.\"\n\
             [embed]\nkeycloak_issuer = \"https://kc.acme.test/realms/staff\"\n",
        ),
        (
            "agents/assist/agent.toml",
            "id = \"assist\"\ndisplay_name = \"Assist\"\nsystem_prompt = \"You help.\"\n\
             [embed]\nkeycloak_issuer = \"https://kc.acme.test/realms/staff\"\n",
        ),
    ];

    /// A git import is admitted on the agents the tenant would hold after
    /// it — the repo's ids beside the mounted ones, an id replaced in place
    /// counted once. Over the cap the whole import is a 422 under the
    /// `plan_limit` code and nothing is written; at the cap it lands.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_git_import_with_more_agents_than_the_plan_allows_fails_whole() {
        use crate::tenant::{Plan, PlanLimit};
        use workforce_common::session_postgres::PostgresSessionStore;
        use workforce_postgres::test_support::with_test_pool;

        let import = |cookie: &str| {
            request(
                Method::POST,
                "/admin/api/import/git",
                cookie,
                Some(serde_json::json!({"repo": "acme/edge-config", "token": "ghp_x"})),
            )
        };
        let on_plan = |cap: u32| Plan {
            agents: Some(cap),
            ..Plan::default()
        };

        with_test_pool(PostgresSessionStore::build_pool, |pool| async move {
            let github = crate::git_config::tests::mock_github(
                "acme/edge-config",
                "main",
                CAPPED_IMPORT_REPO,
            )
            .await;
            let (state, runtime, cookie) =
                agent_fixture_on_plan(pool, Some(&github.uri()), on_plan(1)).await;
            let (status, body) = send(&state, import(&cookie)).await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
            let refused = json(&body);
            assert_eq!(refused["error"], PlanLimit::CODE);
            assert_eq!(
                refused["message"],
                "the plan allows 1 agent(s); the workspace would have 2"
            );
            assert!(
                runtime.config_store.load_all().await.unwrap().is_empty(),
                "nothing is written on a refusal, not even the agent it would replace"
            );
            assert!(runtime
                .audit
                .recent(10)
                .await
                .unwrap()
                .iter()
                .all(|e| e.action != "import.git"));
        })
        .await;

        with_test_pool(PostgresSessionStore::build_pool, |pool| async move {
            let github = crate::git_config::tests::mock_github(
                "acme/edge-config",
                "main",
                CAPPED_IMPORT_REPO,
            )
            .await;
            let (state, _runtime, cookie) =
                agent_fixture_on_plan(pool, Some(&github.uri()), on_plan(2)).await;
            let (status, body) = send(&state, import(&cookie)).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(json(&body)["agents"], 2);
            let Resolved::Tenant(rebuilt) = state.resolve("any.host") else {
                panic!("the tenant serves");
            };
            assert_eq!(
                rebuilt.host_count(),
                2,
                "support replaced in place, assist added"
            );
        })
        .await;
    }

    /// The `default` tenant on `plan`, served on a listener over a migrated
    /// database, its one agent verifying customers against `issuer` and
    /// recording turns. Returns the state, the address and the agent's
    /// analytics store.
    async fn served_on_plan(
        pool: &workforce_postgres::PostgresPool,
        issuer: &str,
        plan: crate::tenant::Plan,
    ) -> (
        Arc<EdgeState>,
        SocketAddr,
        Arc<crate::analytics::EdgeAnalyticsStore>,
    ) {
        let infra = Arc::new(SharedInfra::for_tests_on(pool.clone()));
        let analytics = Arc::new(crate::analytics::EdgeAnalyticsStore::new(
            crate::tenant::TenantPool::new(pool.clone()),
            TenantId::default_tenant(),
        ));
        let agent = Arc::new(EdgeAgent {
            id: "support".into(),
            embed: crate::agent::EmbedSettings {
                keycloak_issuer: issuer.into(),
                ..Default::default()
            },
            ..Default::default()
        });
        let host = AgentHost::for_test(
            agent,
            Arc::new(BlockingProvider),
            Arc::new(InMemorySessionStore::new()),
        )
        .tool_less()
        .with_analytics(Arc::clone(&analytics));
        let settings = TenantSettings {
            plan,
            ..TenantSettings::from_edge_toml(&edge_toml())
        };
        let runtime = TenantRuntime::for_tests(
            settings,
            HashMap::from([("support".to_string(), Arc::new(host))]),
            &infra,
        );
        let state = EdgeState::for_tests(
            TenantMode::Single(TenantId::default_tenant()),
            vec![(runtime, vec![])],
            infra,
            edge_toml(),
            1,
        );
        let addr = serve(Arc::clone(&state)).await;
        (state, addr, analytics)
    }

    async fn send_chat(socket: &mut ClientSocket, content: &str) {
        use tokio_tungstenite::tungstenite::Message;
        let frame = serde_json::json!({"type": "chat", "content": content}).to_string();
        socket.send(Message::Text(frame.into())).await.unwrap();
    }

    /// A tenant whose month's turns are used is told on the socket, with
    /// the `plan_limit` code and the turn message, and nothing is recorded
    /// for the refused turn.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_tenant_at_its_turn_cap_gets_the_plan_limit_frame_and_no_usage_row() {
        use crate::tenant::{plan_tests, Plan, PlanLimit};
        use workforce_common::session_postgres::PostgresSessionStore;
        use workforce_postgres::test_support::with_test_pool;

        with_test_pool(PostgresSessionStore::build_pool, |pool| async move {
            plan_tests::migrate(&pool).await;
            plan_tests::seed_turn(&pool, "someone", &chrono::Utc::now().to_rfc3339(), "turn").await;
            let (_idp, issuer) = mock_idp().await;
            let (_state, addr, analytics) = served_on_plan(
                &pool,
                &issuer,
                Plan {
                    turns_per_month: Some(1),
                    ..Plan::default()
                },
            )
            .await;
            let path = format!("/public/ws/support?token={}", customer_token(&issuer));
            let mut socket = connect(addr, "edge.test", &path).await;
            assert_eq!(next_frame(&mut socket).await["type"], "connected");

            send_chat(&mut socket, "hello").await;
            let frame = next_frame(&mut socket).await;
            assert_eq!(frame["type"], "error", "{frame}");
            assert_eq!(frame["code"], PlanLimit::CODE);
            assert_eq!(frame["content"], PlanLimit::Turns.message());

            let since = crate::tenant::month_start(chrono::Utc::now());
            assert_eq!(
                analytics.usage_since(since).await.unwrap().turns,
                1,
                "only the seeded turn: the refused one recorded nothing"
            );
        })
        .await;
    }

    /// With the user cap full, a customer the month has not seen is refused
    /// while one it has starts their turn on the same socket.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_new_user_beyond_the_user_cap_is_refused_while_an_existing_one_proceeds() {
        use crate::tenant::{plan_tests, Plan, PlanLimit};
        use workforce_common::session_postgres::PostgresSessionStore;
        use workforce_postgres::test_support::with_test_pool;

        with_test_pool(PostgresSessionStore::build_pool, |pool| async move {
            plan_tests::migrate(&pool).await;
            let now = chrono::Utc::now().to_rfc3339();
            plan_tests::seed_identity(&pool, "ext:other\u{1f}someone", &now).await;
            let (_idp, issuer) = mock_idp().await;
            let (_state, addr, _analytics) = served_on_plan(
                &pool,
                &issuer,
                Plan {
                    active_users: Some(1),
                    ..Plan::default()
                },
            )
            .await;
            let path = format!("/public/ws/support?token={}", customer_token(&issuer));
            let mut socket = connect(addr, "edge.test", &path).await;
            assert_eq!(next_frame(&mut socket).await["type"], "connected");

            send_chat(&mut socket, "hello").await;
            let frame = next_frame(&mut socket).await;
            assert_eq!(frame["type"], "error", "{frame}");
            assert_eq!(frame["code"], PlanLimit::CODE);
            assert_eq!(frame["content"], PlanLimit::ActiveUsers.message());

            // The same person, once the month has seen them, is not a new user.
            plan_tests::seed_identity(&pool, &external_owner(&issuer, "cust-1"), &now).await;
            send_chat(&mut socket, "hello again").await;
            assert_eq!(
                next_frame(&mut socket).await["type"],
                "typing",
                "the turn started"
            );
        })
        .await;
    }

    /// The admin's session probe carries the plan, the usage and which caps
    /// are reached; a knowledge document the plan refuses is a 422 under the
    /// `plan_limit` code and writes nothing.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_admin_sees_usage_against_the_plan_and_a_refused_document_is_422() {
        use crate::tenant::{plan_tests, Plan, PlanLimit};
        use workforce_common::session_postgres::PostgresSessionStore;
        use workforce_memory::in_memory::InMemoryStore;
        use workforce_memory::store::MemoryStore;
        use workforce_memory::types::MemoryFilter;
        use workforce_postgres::test_support::with_test_pool;

        with_test_pool(PostgresSessionStore::build_pool, |pool| async move {
            plan_tests::migrate(&pool).await;
            plan_tests::seed_turn(&pool, "a", &chrono::Utc::now().to_rfc3339(), "turn").await;
            let infra = Arc::new(SharedInfra::for_tests_on(pool.clone()));
            let memory = Arc::new(InMemoryStore::new());
            let host = AgentHost::for_test(
                Arc::new(EdgeAgent {
                    id: "support".into(),
                    ..Default::default()
                }),
                Arc::new(BlockingProvider),
                Arc::new(InMemorySessionStore::new()),
            );
            let settings = TenantSettings {
                plan: Plan {
                    turns_per_month: Some(1),
                    knowledge_chunks: Some(1),
                    memory_objects: Some(50),
                    ..Plan::default()
                },
                ..TenantSettings::from_edge_toml(&edge_toml())
            };
            let runtime = TenantRuntime::for_tests_with_memory(
                settings,
                HashMap::from([("support".to_string(), Arc::new(host))]),
                &infra,
                memory.clone(),
            );
            let token = runtime
                .admin
                .as_ref()
                .unwrap()
                .issue_session("ops@harbor.example")
                .unwrap();
            let cookie = format!("{}={token}", crate::admin::SESSION_COOKIE);
            let state = EdgeState::for_tests(
                TenantMode::Single(TenantId::default_tenant()),
                vec![(runtime, vec![])],
                infra,
                edge_toml(),
                1,
            );

            let (status, body) = send(
                &state,
                request(
                    Method::PUT,
                    "/admin/api/agents/support/knowledge/faq",
                    &cookie,
                    Some(serde_json::json!({"title": "FAQ", "content": "one chunk"})),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(json(&body)["chunks"], 1);

            let (status, body) = send(
                &state,
                request(
                    Method::PUT,
                    "/admin/api/agents/support/knowledge/policy",
                    &cookie,
                    Some(serde_json::json!({"title": "Policy", "content": "one more"})),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
            let refused = json(&body);
            assert_eq!(refused["error"], PlanLimit::CODE);
            assert_eq!(refused["message"], PlanLimit::KnowledgeChunks.message());
            assert_eq!(
                memory.count(MemoryFilter::system_unscoped()).await.unwrap(),
                1,
                "the refused document embedded nothing"
            );

            let (status, body) =
                send(&state, request(Method::GET, "/admin/api/me", &cookie, None)).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let me = json(&body);
            assert_eq!(me["plan"]["turns_per_month"], 1);
            assert_eq!(me["plan"]["knowledge_chunks"], 1);
            assert_eq!(me["usage"]["turns_this_month"], 1);
            assert_eq!(me["usage"]["active_users_this_month"], 0);
            assert_eq!(me["usage"]["knowledge_chunks"], 1);
            assert_eq!(me["usage"]["memory_objects"], 1);
            assert_eq!(
                me["over"],
                serde_json::json!(["turns_per_month", "knowledge_chunks"])
            );
        })
        .await;
    }

    /// A single-tenant state with no database behind it: `default` reaching
    /// its URLs under `outbound`, serving `hosts`, the file's key replaced
    /// by `api_key`, the demo page on, and an operator session.
    fn wizard_state(
        outbound: OutboundPolicy,
        hosts: HashMap<String, Arc<AgentHost>>,
        api_key: Option<&str>,
    ) -> (Arc<EdgeState>, String) {
        let infra = Arc::new(SharedInfra::for_tests());
        let mut settings = TenantSettings {
            admin: Some(admin_config()),
            demo: DemoConfig {
                enabled: true,
                google_client_id: Some("demo-client".into()),
            },
            outbound,
            ..TenantSettings::from_edge_toml(&edge_toml())
        };
        settings.llm.api_key = api_key.map(str::to_string);
        let runtime = TenantRuntime::for_tests(settings, hosts, &infra);
        let token = runtime
            .admin
            .as_ref()
            .unwrap()
            .issue_session("ops@harbor.example")
            .unwrap();
        let cookie = format!("{}={token}", crate::admin::SESSION_COOKIE);
        let state = EdgeState::for_tests(
            TenantMode::Single(TenantId::default_tenant()),
            vec![(runtime, vec![])],
            infra,
            edge_toml(),
            1,
        );
        (state, cookie)
    }

    /// An agent host as the wizard would have created it: bound to an MCP
    /// server, an origin and an issuer.
    fn wizard_agent(mcp_url: &str) -> Arc<AgentHost> {
        Arc::new(AgentHost::for_test(
            Arc::new(EdgeAgent {
                id: "support".into(),
                display_name: "Support".into(),
                embed: crate::agent::EmbedSettings {
                    allowed_origins: vec!["https://app.acme.test".into()],
                    keycloak_issuer: "https://idp.test/realms/acme".into(),
                    allowed_tools: vec!["acme.list_orders".into()],
                    mcp: Some(crate::agent::McpSettings {
                        url: mcp_url.into(),
                        discovery_credential: None,
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                ..Default::default()
            }),
            Arc::new(BlockingProvider),
            Arc::new(InMemorySessionStore::new()),
        ))
    }

    /// The setup report reads every step from the live state: a bare
    /// tenant has nothing done, one with a key and a fully bound agent has
    /// everything, and nothing answers without a session.
    #[tokio::test]
    async fn the_setup_report_tracks_each_step_from_the_live_state() {
        let (state, cookie) = wizard_state(OutboundPolicy::Private, HashMap::new(), None);
        let (status, body) = send(
            &state,
            request(Method::GET, "/admin/api/setup", &cookie, None),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let report = json(&body);
        assert_eq!(report["llm_key"], false);
        assert_eq!(report["llm_status"]["state"], "unset");
        assert_eq!(report["editable"], false, "no master key: the file governs");
        assert_eq!(report["agents"], 0);
        assert_eq!(report["agent"], Value::Null);
        assert_eq!(report["agent_cap"], Value::Null, "no plan, no cap");
        for step in ["mcp", "origins", "oidc", "complete"] {
            assert_eq!(report[step], false, "{step}");
        }
        assert_eq!(report["demo"], true);

        let (status, _) = send(
            &state,
            Request::builder()
                .method(Method::GET)
                .uri("/admin/api/setup")
                .extension(peer("203.0.113.1"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "the report is guarded");

        let (state, cookie) = wizard_state(
            OutboundPolicy::Private,
            HashMap::from([(
                "support".to_string(),
                wizard_agent("https://mcp.acme.test/mcp"),
            )]),
            Some("sk-file"),
        );
        let (status, body) = send(
            &state,
            request(Method::GET, "/admin/api/setup", &cookie, None),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let report = json(&body);
        assert_eq!(report["llm_key"], true, "the file's key counts");
        assert_eq!(report["agents"], 1);
        assert_eq!(report["agent"], "support");
        for step in ["mcp", "origins", "oidc", "complete"] {
            assert_eq!(report[step], true, "{step}");
        }
        assert!(!body.contains("sk-file"), "the key never leaves the server");
    }

    /// The MCP probe lists a server's tools through the admin API, under the
    /// tenant's policy: a private tenant reaches a loopback server and its
    /// typed credential goes there and nowhere else; a public tenant has the
    /// same URL refused before a byte is sent; a server that does not answer
    /// is a 502 rather than a hang.
    #[tokio::test]
    async fn the_mcp_probe_lists_tools_under_the_tenants_policy() {
        let mcp = crate::mcp::tests::mock_mcp(&["acme.list_orders", "acme.item_transition"]).await;
        let url = format!("{}/mcp", mcp.uri());
        let (state, cookie) = wizard_state(OutboundPolicy::Private, HashMap::new(), None);

        let (status, body) = send(
            &state,
            request(
                Method::POST,
                "/admin/api/mcp/discover",
                &cookie,
                Some(serde_json::json!({"url": url, "discovery_credential": "svc-9f8e"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let survey = &json(&body)["survey"];
        assert_eq!(survey["server"], "fake-mcp");
        assert_eq!(survey["truncated"], false);
        assert_eq!(
            survey["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t["name"].as_str().unwrap().to_string())
                .collect::<Vec<_>>(),
            ["acme.list_orders", "acme.item_transition"]
        );
        assert!(
            !body.contains("svc-9f8e"),
            "the credential is not echoed: {body}"
        );
        assert!(
            mcp.received_requests().await.unwrap().iter().any(|r| {
                r.headers.get("authorization").and_then(|v| v.to_str().ok())
                    == Some("Bearer svc-9f8e")
            }),
            "the credential went to the MCP server"
        );

        let (status, body) = send(
            &state,
            request(
                Method::POST,
                "/admin/api/mcp/discover",
                &cookie,
                Some(serde_json::json!({"url": "not a url"})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert!(json(&body)["error"].as_str().unwrap().contains("not a URL"));

        let silent = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(503))
            .mount(&silent)
            .await;
        let (status, body) = send(
            &state,
            request(
                Method::POST,
                "/admin/api/mcp/discover",
                &cookie,
                Some(serde_json::json!({"url": silent.uri()})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
        assert!(json(&body)["error"]
            .as_str()
            .unwrap()
            .contains("initialize"));

        let (public, cookie) = wizard_state(OutboundPolicy::Public, HashMap::new(), None);
        let before = mcp.received_requests().await.unwrap().len();
        let (status, body) = send(
            &public,
            request(
                Method::POST,
                "/admin/api/mcp/discover",
                &cookie,
                Some(serde_json::json!({"url": url})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert!(json(&body)["error"]
            .as_str()
            .unwrap()
            .contains("refusing the MCP URL"));
        assert_eq!(
            mcp.received_requests().await.unwrap().len(),
            before,
            "a public tenant's loopback URL is never dialled"
        );
    }

    /// The issuer check reads the discovery document and JWKS through the
    /// tenant's policy and verifies a pasted token through the validator
    /// the public socket uses — so a token the socket would refuse (wrong
    /// audience) is refused here too, and a public tenant's loopback issuer
    /// is refused outright.
    #[tokio::test]
    async fn the_oidc_check_reports_the_issuer_and_verifies_a_token_the_sockets_way() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let idp = MockServer::start().await;
        let realm = format!("realms/{}", uuid::Uuid::new_v4().simple());
        let issuer = format!("{}/{realm}", idp.uri());
        Mock::given(method("GET"))
            .and(path(format!("/{realm}/.well-known/openid-configuration")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "issuer": issuer,
                "jwks_uri": format!("{}/jwks", idp.uri()),
                "id_token_signing_alg_values_supported": ["RS256"],
            })))
            .mount(&idp)
            .await;
        Mock::given(method("GET"))
            .and(path("/jwks"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "keys": [{
                    "kty": "RSA", "kid": "test-key", "use": "sig", "alg": "RS256",
                    "n": TEST_KEY_N, "e": "AQAB",
                }],
            })))
            .mount(&idp)
            .await;
        let (state, cookie) = wizard_state(OutboundPolicy::Private, HashMap::new(), None);

        let token = customer_token(&issuer);
        let (status, body) = send(
            &state,
            request(
                Method::POST,
                "/admin/api/oidc/check",
                &cookie,
                Some(serde_json::json!({"issuer": issuer, "token": token})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let check = json(&body);
        assert_eq!(check["issuer_matches"], true);
        assert_eq!(check["report"]["issuer"], issuer);
        assert_eq!(check["report"]["jwks_uri"], format!("{}/jwks", idp.uri()));
        assert_eq!(check["report"]["algs"], serde_json::json!(["RS256"]));
        assert_eq!(check["report"]["keys"], 1);
        assert_eq!(check["token"]["ok"], true);
        assert_eq!(check["token"]["sub"], "cust-1");
        assert_eq!(check["token"]["email"], "person@example.com");

        // A trailing slash reads as the same document, but the validator
        // pins `iss` to the string as typed — so the token fails exactly
        // as it would at the socket, and the wizard binds the document's
        // spelling instead.
        let (status, body) = send(
            &state,
            request(
                Method::POST,
                "/admin/api/oidc/check",
                &cookie,
                Some(serde_json::json!({"issuer": format!("{issuer}/"), "token": token})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let check = json(&body);
        assert_eq!(check["issuer_matches"], true);
        assert_eq!(check["report"]["issuer"], issuer);
        assert_eq!(check["token"]["ok"], false);

        // The validator is the socket's: an audience the token does not
        // carry fails it, and the document is still reported.
        let (status, body) = send(
            &state,
            request(
                Method::POST,
                "/admin/api/oidc/check",
                &cookie,
                Some(serde_json::json!({"issuer": issuer, "audience": "widget", "token": token})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let check = json(&body);
        assert_eq!(check["token"]["ok"], false);
        assert!(
            check["token"]["error"]
                .as_str()
                .unwrap()
                .contains("audience"),
            "{check}"
        );
        assert_eq!(check["report"]["keys"], 1);

        // No token: the document alone.
        let (status, body) = send(
            &state,
            request(
                Method::POST,
                "/admin/api/oidc/check",
                &cookie,
                Some(serde_json::json!({"issuer": issuer})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(json(&body)["token"], Value::Null);

        let (public, cookie) = wizard_state(OutboundPolicy::Public, HashMap::new(), None);
        let (status, body) = send(
            &public,
            request(
                Method::POST,
                "/admin/api/oidc/check",
                &cookie,
                Some(serde_json::json!({"issuer": issuer})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert!(json(&body)["error"]
            .as_str()
            .unwrap()
            .contains("refusing to fetch"));
    }
}
