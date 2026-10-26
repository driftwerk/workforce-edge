//! MCP wiring: an agent's company MCP servers.
//!
//! An edge agent acts against a primary company MCP server and any number of
//! additional ones, always **as the logged-in customer**. Each server is its own
//! [`McpToolset`] (connection, allowlist, cache, auth mode); [`McpCatalogue`]
//! merges them into one registry per turn, reaching every server concurrently
//! so one slow or down server costs only its own tools — unless it is
//! `required`, in which case the turn fails as a single-server agent's always
//! has. The customer's token is carried by the
//! per-turn client's [`AuthProvider`], which rides the `Arc` graph
//! (`EdgeMcpTool` → `McpClient` → transport) into the runner's spawned tool
//! task — so the token reaches `tools/call` without any task-local or shared
//! `SessionContext` change. Tool schemas are discovered once and cached: at
//! startup if a service `discovery_credential` is configured, otherwise lazily
//! on the first customer turn using that customer's own token (for MCP servers
//! that accept any valid user token). Install-agnostic — nothing here is
//! specific to any one deployment.

use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap};
use std::future::Future;
use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use eyre::{eyre, Result, WrapErr};
use futures::future::join_all;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use url::Url;
use workforce_agent::runner::to_llm_name;
use workforce_agent::tools::registry::ToolRegistry;
use workforce_agent::tools::{Tool, ToolResult};
use workforce_mcp_client::{
    AuthProvider, McpClient, McpError, McpLimits, McpToolDef, NoAuth, StaticBearer, ToolNameInfo,
};

use crate::agent::{allowlist_admits, EmbedSettings, McpAuth, McpServerSpec};
use crate::host::BUILT_IN_TOOLS;
use crate::metrics::{self, CacheResult, CallOutcome, DiscoveryPhase};
use crate::outbound::{self, OutboundPolicy};
use crate::token_exchange::TokenExchanger;

/// Cap on a single company-MCP `tools/call` result body — well above the
/// client's 1 MiB default so a customer's full record set arrives to be
/// auto-spilled into a staged artifact (and browsed via a server-backed table)
/// rather than rejected at the transport. Bounded so one response is still a
/// fraction of the context window; ~50k table-row-scale results fit comfortably.
const EDGE_MAX_MCP_CONTENT_BYTES: usize = 16 * 1024 * 1024;

/// Per-request connect/read timeout on the reqwest client. This does NOT bound a
/// streamed (SSE) response body, so the per-op timeouts below are the real guard
/// against a server that holds a stream open without ever answering.
const MCP_REQUEST_TIMEOUT_SECS: u64 = 120;

/// Wall-clock bound on the MCP handshake + tool discovery in a turn — a stalled
/// server surfaces as a clean error, never a silent hang.
const MCP_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// Wall-clock bound on a single tool call (a real query can be slower than the
/// handshake, but must still terminate).
const MCP_CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// How long a discovered tool list stays authoritative before the next turn
/// re-lists it — so a customer evolving their MCP schemas (new parameters,
/// richer enums) is picked up without an edge restart. A failed refresh keeps
/// serving the stale list: schema drift is never worth a tool-less turn.
const MCP_DEFS_TTL: Duration = Duration::from_secs(300);

/// The longest tool name a provider accepts — the bound `McpLimits::check_name`
/// puts on a raw name, applied again once a prefix is added.
const MAX_TOOL_NAME_LEN: usize = 64;

/// Why an MCP operation under [`deadline`] did not produce a value. A timeout
/// is told apart so a stalled server can be skipped for a while; a fast
/// failure is retried on the next turn.
#[derive(Debug)]
pub(crate) enum McpOpError {
    Failed { op: &'static str, detail: String },
    TimedOut { op: &'static str, secs: u64 },
}

impl std::fmt::Display for McpOpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Failed { op, detail } => write!(f, "MCP {op} failed: {detail}"),
            Self::TimedOut { op, secs } => write!(f, "MCP {op} timed out after {secs}s"),
        }
    }
}

impl std::error::Error for McpOpError {}

/// Run an MCP operation under a hard deadline, flattening timeout and protocol
/// errors into one [`McpOpError`].
async fn deadline<T, E>(
    op: &'static str,
    limit: Duration,
    fut: impl Future<Output = Result<T, E>>,
) -> Result<T, McpOpError>
where
    E: std::fmt::Display,
{
    match tokio::time::timeout(limit, fut).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(McpOpError::Failed {
            op,
            detail: error.to_string(),
        }),
        Err(_) => Err(McpOpError::TimedOut {
            op,
            secs: limit.as_secs(),
        }),
    }
}

fn timed_out(error: &eyre::Report) -> bool {
    matches!(
        error.downcast_ref::<McpOpError>(),
        Some(McpOpError::TimedOut { .. })
    )
}

/// The `outcome` a failed MCP operation is logged under.
fn outcome_of(error: &eyre::Report) -> &'static str {
    if timed_out(error) {
        "timed_out"
    } else {
        "failed"
    }
}

/// Whether `tool` reaches the model under a built-in's name. Compared in LLM-name
/// space, where `data_query` and `data.query` are the same name: the runner
/// resolves a direct match first, so an MCP twin would take the built-in's
/// calls.
fn shadows_builtin(tool: &str) -> bool {
    let wire = to_llm_name(tool);
    BUILT_IN_TOOLS
        .iter()
        .any(|builtin| to_llm_name(builtin) == wire)
}

/// The customer's MCP bearer, shared between the socket that refreshes it from
/// `token` frames and the in-flight turn's MCP client that reads it on every
/// request. A turn can outlive the ~5-minute Keycloak token, so reading the live
/// value per call — rather than a turn-start snapshot — lets a token pushed
/// mid-turn reach later tool calls instead of failing them with 401.
#[derive(Clone)]
pub struct TokenCell(Arc<std::sync::RwLock<Arc<str>>>);

impl TokenCell {
    pub fn new(token: impl Into<Arc<str>>) -> Self {
        Self(Arc::new(std::sync::RwLock::new(token.into())))
    }

    pub fn get(&self) -> Arc<str> {
        self.0
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn set(&self, token: impl Into<Arc<str>>) {
        *self.0.write().unwrap_or_else(PoisonError::into_inner) = token.into();
    }
}

/// An [`AuthProvider`] that reads its [`TokenCell`] on every request, so a token
/// refreshed mid-turn is used by the turn's later MCP calls.
pub(crate) struct SharedBearer(TokenCell);

impl SharedBearer {
    pub(crate) fn new(cell: TokenCell) -> Self {
        Self(cell)
    }
}

#[async_trait]
impl AuthProvider for SharedBearer {
    async fn authorization_header(&self) -> Result<String, McpError> {
        Ok(format!("Bearer {}", self.0.get()))
    }
}

/// One company MCP server's tool catalogue. The allowlisted tool defs are
/// discovered once and cached — eagerly at startup when a `discovery_credential`
/// is configured, otherwise lazily on the first customer turn with that
/// customer's token. Per-turn tools are built from the cache, each bound to a
/// single customer's token.
#[derive(Clone)]
pub struct McpToolset {
    /// The agent this server serves, for its metrics.
    agent: Arc<str>,
    /// The server's id, as logs, messages and metrics name it.
    id: Arc<str>,
    endpoint: Url,
    /// How `endpoint` is reached. Under `Public` the shared `http` is unused:
    /// each turn's client is pinned to the address checked for that turn.
    policy: OutboundPolicy,
    http: Arc<reqwest::Client>,
    /// Optional service credential for eager startup discovery. When absent, the
    /// tools are discovered lazily with the first customer's own token.
    discovery_credential: Option<String>,
    auth: McpAuth,
    /// The audience this server's exchanged token is minted for: its own, else
    /// the install's. Servers with the same one share a turn's exchange.
    audience: Option<String>,
    tool_prefix: Option<String>,
    /// The closed allowlist to filter the discovered tools by — the base plus
    /// every grant tool, in exposed names.
    catalogue: Vec<String>,
    /// Whether this server being unavailable fails startup and the turn.
    required: bool,
    /// How long a server that is not required is skipped after a timed-out
    /// turn start; zero never skips.
    unavailable_skip: Duration,
    /// Deadline on every turn-start operation.
    handshake_timeout: Duration,
    skip_until: Arc<std::sync::Mutex<Option<Instant>>>,
    /// Cached served tools, populated on first discovery and refreshed once
    /// older than [`MCP_DEFS_TTL`].
    defs: Arc<RwLock<Option<CachedDefs>>>,
}

/// A discovered tool as the agent serves it: the name the model sees (prefix
/// included) and the server's own def, whose raw name `tools/call` sends.
#[derive(Clone)]
struct ServedTool {
    name: String,
    def: McpToolDef,
}

/// A discovered tool list plus its age, for TTL-based refresh.
#[derive(Clone)]
struct CachedDefs {
    tools: Arc<Vec<ServedTool>>,
    at: Instant,
}

impl CachedDefs {
    fn now(tools: Arc<Vec<ServedTool>>) -> Self {
        Self {
            tools,
            at: Instant::now(),
        }
    }
}

// Manual Debug so the discovery credential never lands in a debug log.
impl std::fmt::Debug for McpToolset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpToolset")
            .field("id", &self.id)
            .field("endpoint", &self.endpoint)
            .field("policy", &self.policy)
            .field(
                "has_discovery_credential",
                &self.discovery_credential.is_some(),
            )
            .field("auth", &self.auth)
            .field("audience", &self.audience)
            .field("tool_prefix", &self.tool_prefix)
            .field("catalogue", &self.catalogue)
            .field("required", &self.required)
            .finish_non_exhaustive()
    }
}

impl McpToolset {
    /// Prepare one server for an agent: the URL is parsed and checked against
    /// `policy` here, so a disallowed server fails the agent at build rather
    /// than on a customer's first turn. Nothing is dialled.
    fn prepare(
        agent: &str,
        spec: &McpServerSpec<'_>,
        policy: OutboundPolicy,
        install_audience: Option<&str>,
    ) -> Result<Self> {
        let url = &spec.connection.url;
        let endpoint = Url::parse(url)
            .wrap_err_with(|| format!("invalid MCP url {url:?} for server {}", spec.id))?;
        outbound::check_url(policy, &endpoint)
            .wrap_err_with(|| format!("refusing MCP url {url:?} for server {}", spec.id))?;
        Ok(Self {
            agent: agent.into(),
            id: spec.id.into(),
            endpoint,
            policy,
            http: Arc::new(outbound::client(
                policy,
                Duration::from_secs(MCP_REQUEST_TIMEOUT_SECS),
            )),
            discovery_credential: spec.connection.discovery_credential.clone(),
            auth: spec.connection.auth,
            audience: spec
                .connection
                .audience
                .as_deref()
                .or(install_audience)
                .map(str::to_string),
            tool_prefix: spec.tool_prefix.map(str::to_string),
            catalogue: spec.tools.catalogue(),
            required: spec.required,
            unavailable_skip: spec.unavailable_skip(),
            handshake_timeout: MCP_HANDSHAKE_TIMEOUT,
            skip_until: Arc::new(std::sync::Mutex::new(None)),
            defs: Arc::new(RwLock::new(None)),
        })
    }

    /// With a `discovery_credential`, fetch the tool list now; without one,
    /// discovery is deferred to the first customer turn.
    async fn discover_at_startup(&self) -> Result<()> {
        let Some(credential) = self.discovery_credential.clone() else {
            return Ok(());
        };
        let client = self.client_for(Arc::new(StaticBearer::new(credential)))?;
        let tools = self
            .timed_discovery(DiscoveryPhase::Startup, self.discover_on(&client))
            .await
            .wrap_err_with(|| format!("MCP tool discovery at startup (server {})", self.id))?;
        *self.defs.write().await = Some(CachedDefs::now(Arc::new(tools)));
        Ok(())
    }

    /// A fresh MCP client that authenticates every request with `auth`. Under
    /// `Public` the HTTP client is rebuilt per call: the endpoint is re-resolved,
    /// re-checked and pinned for this turn, so a DNS change at the tenant's
    /// server between turns is checked again rather than trusted from startup.
    fn client_for(&self, auth: Arc<dyn AuthProvider>) -> Result<Arc<McpClient>> {
        let http = match self.policy {
            OutboundPolicy::Private => Arc::clone(&self.http),
            OutboundPolicy::Public => Arc::new(
                outbound::client_for(
                    self.policy,
                    Duration::from_secs(MCP_REQUEST_TIMEOUT_SECS),
                    &self.endpoint,
                )
                .wrap_err_with(|| format!("refusing MCP url {}", self.endpoint))?,
            ),
        };
        Ok(Arc::new(McpClient::new(
            self.endpoint.clone(),
            http,
            auth,
            McpLimits {
                // A customer's whole record set (e.g. every lead) can serialise
                // past the default 1 MiB — and the edge WANTS those big tabular
                // results: they auto-spill to a staged artifact (only a preview
                // reaches context) and drive a server-backed data table. Raise
                // the body cap so the result arrives to be staged rather than
                // rejected at the transport. Bounds a pathological response to a
                // fraction of the context and matches the table row cap.
                max_content_bytes: EDGE_MAX_MCP_CONTENT_BYTES,
                ..McpLimits::default()
            },
        )))
    }

    pub fn policy(&self) -> OutboundPolicy {
        self.policy
    }

    /// Initialize the session and list the allowlisted tools on `client`, under
    /// the handshake deadline. Filtering at the client means only allowlisted
    /// tools are counted against McpLimits, so a server far larger than the tool
    /// cap (Harbor exposes 500+) still connects once the allowlist trims it.
    async fn discover_on(&self, client: &McpClient) -> Result<Vec<ServedTool>> {
        deadline("initialize", self.handshake_timeout, client.initialize()).await?;
        self.list_allowlisted(client).await
    }

    /// List the served tools on an already-initialized client.
    async fn list_allowlisted(&self, client: &McpClient) -> Result<Vec<ServedTool>> {
        let defs = deadline(
            "tools/list",
            self.handshake_timeout,
            client.tools_list_filtered(|raw| self.serves(&self.exposed_name(raw))),
        )
        .await?;
        Ok(defs
            .into_iter()
            .map(|def| ServedTool {
                name: self.exposed_name(&def.name).into_owned(),
                def,
            })
            .collect())
    }

    /// Run one discovery, counting it and its duration by how it ended.
    async fn timed_discovery<T>(
        &self,
        phase: DiscoveryPhase,
        discovery: impl Future<Output = Result<T>>,
    ) -> Result<T> {
        let started = Instant::now();
        let result = discovery.await;
        let outcome = result.as_ref().map_or_else(outcome_of, |_| "ok");
        metrics::discovery(&self.agent, &self.id, phase, outcome, started.elapsed());
        result
    }

    /// The name the model sees for the server's tool `raw`.
    fn exposed_name<'a>(&self, raw: &'a str) -> Cow<'a, str> {
        match &self.tool_prefix {
            Some(prefix) => Cow::Owned(format!("{prefix}.{raw}")),
            None => Cow::Borrowed(raw),
        }
    }

    /// Whether a discovered tool, by its exposed name, is served: the closed
    /// allowlist admits it, it does not take a built-in's name on the wire, and
    /// a prefix has not pushed it past the provider's name limit.
    fn serves(&self, tool: &str) -> bool {
        if !allowlist_admits(&self.catalogue, tool) {
            return false;
        }
        let refusal = if shadows_builtin(tool) {
            Some("MCP tool shadows a built-in; dropped")
        } else if tool.len() > MAX_TOOL_NAME_LEN {
            Some("MCP tool name is too long once prefixed; dropped")
        } else {
            None
        };
        if let Some(reason) = refusal {
            tracing::warn!(server = %self.id, tool, "{reason}");
        }
        refusal.is_none()
    }

    /// Names of the served tools — best-effort snapshot for logging (empty
    /// before the first discovery).
    pub fn tool_names(&self) -> Vec<String> {
        self.defs
            .try_read()
            .ok()
            .and_then(|guard| guard.as_ref().map(|c| Arc::clone(&c.tools)))
            .map(|tools| tools.iter().map(|t| t.name.clone()).collect())
            .unwrap_or_default()
    }

    /// Whether this server sits out the turn: only a server that is not
    /// required, and only inside the window after a timed-out turn start.
    fn in_skip_window(&self) -> bool {
        !self.required
            && self
                .skip_until
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .is_some_and(|until| Instant::now() < until)
    }

    /// Start the skip window after a timed-out turn start.
    fn note_timeout(&self) {
        if self.required || self.unavailable_skip.is_zero() {
            return;
        }
        *self
            .skip_until
            .lock()
            .unwrap_or_else(PoisonError::into_inner) =
            Instant::now().checked_add(self.unavailable_skip);
        metrics::skip_window_entered(&self.agent, &self.id);
    }

    /// Backdate the cached defs past the TTL so the next turn re-lists.
    /// Returns `false` (test skips) on a platform whose monotonic clock can't
    /// represent the offset.
    #[cfg(test)]
    pub(crate) async fn force_defs_stale(&self) -> bool {
        match Instant::now().checked_sub(MCP_DEFS_TTL + Duration::from_secs(1)) {
            Some(past) => {
                if let Some(cached) = self.defs.write().await.as_mut() {
                    cached.at = past;
                }
                true
            }
            None => false,
        }
    }

    /// End the skip window now, as if it had run out.
    #[cfg(test)]
    pub(crate) fn force_skip_expired(&self) {
        *self
            .skip_until
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
    }

    /// Bound every turn-start operation by `limit`, so a slow-server test runs
    /// in milliseconds.
    #[cfg(test)]
    pub(crate) fn with_handshake_timeout(mut self, limit: Duration) -> Self {
        self.handshake_timeout = limit;
        self
    }

    #[cfg(test)]
    pub(crate) fn set_tool_prefix_for_test(&mut self, prefix: &str) {
        self.tool_prefix = Some(prefix.into());
    }

    /// A toolset with no tools and no server, for tests that drive a turn
    /// through a directly-supplied registry rather than a live MCP.
    #[cfg(test)]
    pub(crate) fn empty() -> Self {
        Self {
            agent: "test".into(),
            id: crate::agent::PRIMARY_MCP_ID.into(),
            endpoint: Url::parse("http://unused.invalid").expect("static url"),
            policy: OutboundPolicy::Private,
            http: Arc::new(reqwest::Client::new()),
            discovery_credential: None,
            auth: McpAuth::Exchanged,
            audience: None,
            tool_prefix: None,
            catalogue: Vec::new(),
            required: true,
            unavailable_skip: Duration::ZERO,
            handshake_timeout: MCP_HANDSHAKE_TIMEOUT,
            skip_until: Arc::new(std::sync::Mutex::new(None)),
            defs: Arc::new(RwLock::new(Some(CachedDefs::now(Arc::new(Vec::new()))))),
        }
    }

    /// This server's tools for one turn, calling the company MCP **as the given
    /// customer** — their token rides the client's auth provider into
    /// execution. A fresh client negotiates its own MCP session, keeping one
    /// customer's calls off another's session. Discovers the tool list lazily on
    /// the first turn when no startup credential was configured.
    async fn tools_for(&self, auth: Arc<dyn AuthProvider>) -> Result<Vec<Arc<dyn Tool>>> {
        let client = self.client_for(auth)?;
        // Snapshot the cache into an owned value so the read guard is released
        // here. Matching on `self.defs.read().await.clone()` directly would keep
        // the guard alive for the whole match, and the write arms'
        // `self.defs.write().await` would then deadlock waiting on a reader held
        // by this same task.
        let cached = self.defs.read().await.clone();
        let tools = match cached {
            Some(cached) if cached.at.elapsed() < MCP_DEFS_TTL => {
                metrics::tools_cache(&self.agent, &self.id, CacheResult::Hit);
                deadline("initialize", self.handshake_timeout, client.initialize()).await?;
                cached.tools
            }
            // Stale: re-list on this turn's already-required session so schema
            // changes land without a restart; on failure keep serving the stale
            // list — never degrade a working toolset over a refresh.
            Some(cached) => {
                metrics::tools_cache(&self.agent, &self.id, CacheResult::Stale);
                deadline("initialize", self.handshake_timeout, client.initialize()).await?;
                match self
                    .timed_discovery(DiscoveryPhase::Refresh, self.list_allowlisted(&client))
                    .await
                {
                    Ok(fresh) => {
                        let fresh = Arc::new(fresh);
                        *self.defs.write().await = Some(CachedDefs::now(Arc::clone(&fresh)));
                        fresh
                    }
                    Err(e) => {
                        // Restamp so a persistent outage re-lists once per TTL,
                        // not on every turn; the stale defs keep serving.
                        tracing::warn!(
                            server = %self.id,
                            phase = "refresh",
                            outcome = outcome_of(&e),
                            error = %e,
                            "MCP tool refresh failed; keeping the cached list"
                        );
                        *self.defs.write().await = Some(CachedDefs::now(Arc::clone(&cached.tools)));
                        cached.tools
                    }
                }
            }
            None => {
                metrics::tools_cache(&self.agent, &self.id, CacheResult::Miss);
                let fetched = Arc::new(
                    self.timed_discovery(DiscoveryPhase::Lazy, self.discover_on(&client))
                        .await?,
                );
                *self.defs.write().await = Some(CachedDefs::now(Arc::clone(&fetched)));
                fetched
            }
        };
        Ok(tools
            .iter()
            .map(|tool| {
                Arc::new(EdgeMcpTool {
                    client: Arc::clone(&client),
                    agent: Arc::clone(&self.agent),
                    server: Arc::clone(&self.id),
                    name: tool.name.clone(),
                    def: tool.def.clone(),
                }) as Arc<dyn Tool>
            })
            .collect())
    }
}

/// The ways a turn's MCP calls authenticate, resolved once per turn: the live
/// customer token, and one exchanged token per distinct audience.
struct TurnAuth {
    forwarded: Arc<dyn AuthProvider>,
    exchanged: HashMap<Option<String>, Arc<dyn AuthProvider>>,
}

impl TurnAuth {
    /// Exchange the customer's token once for each of `audiences`,
    /// concurrently. Without an exchanger nothing is minted; an audience whose
    /// exchange fails is logged once and its servers fall back to the live
    /// customer token, so a mid-turn refresh still reaches their later calls.
    async fn resolve(
        agent: &str,
        cell: &TokenCell,
        exchanger: Option<&TokenExchanger>,
        audiences: impl IntoIterator<Item = Option<String>>,
    ) -> Self {
        let forwarded: Arc<dyn AuthProvider> = Arc::new(SharedBearer::new(cell.clone()));
        let Some(exchanger) = exchanger else {
            return Self {
                forwarded,
                exchanged: HashMap::new(),
            };
        };
        let subject = cell.get();
        let distinct: BTreeSet<Option<String>> = audiences.into_iter().collect();
        let minted = join_all(distinct.into_iter().map(|audience| {
            let subject = &subject;
            async move {
                let started = Instant::now();
                let token = exchanger.exchange(subject, audience.as_deref()).await;
                let outcome = if token.is_ok() { "ok" } else { "failed" };
                metrics::token_exchange(agent, audience.as_deref(), outcome, started.elapsed());
                (audience, token)
            }
        }))
        .await;
        let exchanged = minted
            .into_iter()
            .filter_map(|(audience, token)| match token {
                Ok(token) => Some((
                    audience,
                    Arc::new(StaticBearer::new(token)) as Arc<dyn AuthProvider>,
                )),
                Err(e) => {
                    tracing::warn!(
                        agent,
                        audience = audience.as_deref().unwrap_or_default(),
                        phase = "token_exchange",
                        outcome = "failed",
                        error = %e,
                        "token exchange failed; falling back to the customer's own token"
                    );
                    None
                }
            })
            .collect();
        Self {
            forwarded,
            exchanged,
        }
    }

    fn for_server(&self, server: &McpToolset) -> Arc<dyn AuthProvider> {
        match server.auth {
            McpAuth::Forwarded => Arc::clone(&self.forwarded),
            McpAuth::Exchanged => self
                .exchanged
                .get(&server.audience)
                .map_or_else(|| Arc::clone(&self.forwarded), Arc::clone),
        }
    }
}

/// Every company MCP server one agent uses, the primary first. Empty for a
/// tool-less, pure-chat agent.
#[derive(Clone, Default)]
pub struct McpCatalogue {
    agent: Arc<str>,
    servers: Vec<McpToolset>,
    /// RFC 8693 exchanger (install-level, opt-in): mints a fresh, optionally
    /// audience-scoped downstream token per turn instead of forwarding the
    /// customer's raw token.
    exchanger: Option<Arc<TokenExchanger>>,
}

impl McpCatalogue {
    /// Check the agent's servers, prepare each one, then discover at startup:
    /// a required server's failure fails the build (fail-closed), another's is
    /// logged and left to the first turn. Every server is reached under
    /// `policy`.
    pub async fn build(
        agent: &str,
        embed: &EmbedSettings,
        policy: OutboundPolicy,
        exchanger: Option<Arc<TokenExchanger>>,
    ) -> Result<Self> {
        embed.check_mcp_servers().map_err(|e| {
            metrics::config_refused(e.reason);
            eyre::Report::new(e)
        })?;
        let specs = embed.mcp_server_specs();
        if exchanger.is_none() {
            specs
                .iter()
                .filter(|spec| spec.connection.audience.is_some())
                .for_each(|spec| {
                    tracing::warn!(
                        agent,
                        server = spec.id,
                        "audience is set but token exchange is off for this agent; the server gets the customer's own token"
                    )
                });
        }
        let install_audience = exchanger.as_deref().and_then(TokenExchanger::audience);
        let servers = specs
            .iter()
            .map(|spec| McpToolset::prepare(agent, spec, policy, install_audience))
            .collect::<Result<Vec<_>>>()?;
        let discovered = join_all(servers.iter().map(McpToolset::discover_at_startup)).await;
        for (server, outcome) in servers.iter().zip(discovered) {
            match outcome {
                Err(e) if server.required => return Err(e),
                Err(e) => tracing::warn!(
                    agent,
                    server = %server.id,
                    phase = "startup_discovery",
                    outcome = outcome_of(&e),
                    error = %format!("{e:#}"),
                    "MCP server unavailable"
                ),
                Ok(()) => {}
            }
        }
        Ok(Self {
            agent: agent.into(),
            servers,
            exchanger,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    /// The served tools per server — best-effort snapshot for logging.
    pub fn tool_names(&self) -> Vec<(String, Vec<String>)> {
        self.servers
            .iter()
            .map(|s| (s.id.to_string(), s.tool_names()))
            .collect()
    }

    /// The registry for one turn, bound to the customer whose token `cell`
    /// holds. Every server outside its skip window is reached concurrently and
    /// registered in declared order. `Err` when a required server failed, or
    /// when no server answered at all.
    pub async fn registry_for(&self, cell: &TokenCell) -> Result<Arc<ToolRegistry>> {
        let (attempted, skipped): (Vec<&McpToolset>, Vec<&McpToolset>) =
            self.servers.iter().partition(|s| !s.in_skip_window());
        skipped.iter().for_each(|s| {
            metrics::server_skipped(&self.agent, &s.id);
            tracing::debug!(
                agent = %self.agent,
                server = %s.id,
                phase = "turn_start",
                outcome = "skipped",
                "MCP server skipped after a timeout"
            )
        });
        let auth = TurnAuth::resolve(
            &self.agent,
            cell,
            self.exchanger.as_deref(),
            attempted
                .iter()
                .filter(|s| s.auth.is_exchanged())
                .map(|s| s.audience.clone()),
        )
        .await;
        let results = join_all(attempted.iter().map(|s| s.tools_for(auth.for_server(s)))).await;
        let registry = ToolRegistry::new();
        let mut answered = false;
        let mut required_down = None;
        for (server, result) in attempted.iter().zip(results) {
            match result {
                Ok(tools) => {
                    answered = true;
                    tools.into_iter().for_each(|tool| registry.register(tool));
                }
                Err(e) => {
                    if timed_out(&e) {
                        server.note_timeout();
                    }
                    tracing::warn!(
                        agent = %self.agent,
                        server = %server.id,
                        phase = "turn_start",
                        outcome = outcome_of(&e),
                        error = %e,
                        "MCP server unavailable"
                    );
                    if server.required {
                        required_down.get_or_insert(Arc::clone(&server.id));
                    }
                }
            }
        }
        match required_down {
            Some(id) => Err(eyre!("required MCP server {id} is unavailable")),
            None if !answered => Err(eyre!("no MCP server is available")),
            None => Ok(Arc::new(registry)),
        }
    }

    /// `tool` with its serving server's prefix removed, so a prefixed tool
    /// labels exactly as it would unprefixed. Accepts the dotted or the LLM
    /// form; unprefixed names come back as they are.
    pub fn local_tool_name<'a>(&self, tool: &'a str) -> &'a str {
        self.servers
            .iter()
            .filter_map(|s| s.tool_prefix.as_deref())
            .find_map(|prefix| {
                tool.strip_prefix(prefix)
                    .and_then(|rest| rest.strip_prefix(['.', '_']))
            })
            .unwrap_or(tool)
    }

    /// A catalogue of exactly `servers`, bypassing the build's checks and
    /// startup discovery.
    #[cfg(test)]
    pub(crate) fn of(servers: Vec<McpToolset>) -> Self {
        Self {
            agent: "test".into(),
            servers,
            exchanger: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn single(toolset: McpToolset) -> Self {
        Self::of(vec![toolset])
    }

    #[cfg(test)]
    pub(crate) fn servers(&self) -> &[McpToolset] {
        &self.servers
    }
}

/// What a server said about itself when an operator surveyed it, and every
/// tool it advertises — the catalogue the admin picks an allowlist from.
#[derive(Debug, Clone, serde::Serialize)]
pub struct McpSurvey {
    /// The server's own name, when it gave one.
    pub server: Option<String>,
    pub version: Option<String>,
    pub protocol_version: String,
    /// Name and description only: the schemas are the host's business, and
    /// a catalogue too large to register is still worth reading.
    pub tools: Vec<ToolNameInfo>,
    /// Paging stopped at the raw-tool cap, so the list is a floor.
    pub truncated: bool,
}

/// Why a survey yielded nothing: the URL was never dialled, or the server
/// was dialled and the handshake did not finish.
#[derive(Debug)]
pub enum SurveyError {
    /// Not a URL, or one the tenant's outbound policy refuses.
    Url(String),
    /// Connect, initialize or `tools/list` failed under the deadline.
    Server(String),
}

impl std::fmt::Display for SurveyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Url(reason) | Self::Server(reason) => f.write_str(reason),
        }
    }
}

impl std::error::Error for SurveyError {}

/// Connect to `url` under `policy`, initialise, and list every tool the
/// server advertises — the admin's one-shot look at a server before an
/// agent is bound to it. `credential` is the bearer the agent would keep as
/// its `discovery_credential`; without one the request carries no
/// `Authorization` header, so a server that admits anonymous discovery
/// answers and one that does not says so. Same client, limits and
/// deadlines as a turn's discovery; the outbound policy is checked here
/// and the connection pinned, exactly as a server prepared for an agent is.
pub async fn survey(
    url: &str,
    credential: Option<&str>,
    policy: OutboundPolicy,
) -> std::result::Result<McpSurvey, SurveyError> {
    let endpoint =
        Url::parse(url.trim()).map_err(|e| SurveyError::Url(format!("not a URL: {e}")))?;
    if !matches!(endpoint.scheme(), "http" | "https") {
        return Err(SurveyError::Url("the MCP URL must be http(s)".into()));
    }
    let http = outbound::client_for(
        policy,
        Duration::from_secs(MCP_REQUEST_TIMEOUT_SECS),
        &endpoint,
    )
    .map_err(|e| SurveyError::Url(format!("refusing the MCP URL: {e}")))?;
    let auth: Arc<dyn AuthProvider> = match credential.map(str::trim).filter(|c| !c.is_empty()) {
        Some(credential) => Arc::new(StaticBearer::new(credential)),
        None => Arc::new(NoAuth),
    };
    let client = McpClient::new(endpoint, Arc::new(http), auth, McpLimits::default());
    let init = deadline("initialize", MCP_HANDSHAKE_TIMEOUT, client.initialize())
        .await
        .map_err(|e| SurveyError::Server(e.to_string()))?;
    let listed = deadline(
        "tools/list",
        MCP_HANDSHAKE_TIMEOUT,
        client.list_tool_names(),
    )
    .await
    .map_err(|e| SurveyError::Server(e.to_string()))?;
    Ok(McpSurvey {
        server: init.server_info.as_ref().map(|info| info.name.clone()),
        version: init.server_info.and_then(|info| info.version),
        protocol_version: init.protocol_version,
        tools: listed.names,
        truncated: listed.truncated,
    })
}

/// One discovered MCP tool, callable as the customer whose token the client
/// carries. Deliberately minimal — no metrics, no multi-workspace gating.
struct EdgeMcpTool {
    client: Arc<McpClient>,
    /// The agent and serving server, for logs and metrics.
    agent: Arc<str>,
    server: Arc<str>,
    /// The name the model sees; `def.name` is the server's own.
    name: String,
    def: McpToolDef,
}

impl EdgeMcpTool {
    async fn timed_call(
        &self,
        input: serde_json::Value,
    ) -> Result<std::result::Result<serde_json::Value, McpError>, tokio::time::error::Elapsed> {
        let call = self
            .client
            .tools_call(&self.def.name, input, CancellationToken::new());
        tokio::time::timeout(MCP_CALL_TIMEOUT, call).await
    }
}

/// How a call's result counts: a tool-level failure is the server's answer,
/// anything that kept an answer from arriving is the transport's.
fn call_outcome(
    result: &Result<std::result::Result<serde_json::Value, McpError>, tokio::time::error::Elapsed>,
) -> CallOutcome {
    match result {
        Ok(Ok(output))
            if output.get("isError").and_then(serde_json::Value::as_bool) == Some(true) =>
        {
            CallOutcome::ToolError
        }
        Ok(Ok(_)) => CallOutcome::Ok,
        Ok(Err(McpError::ServerError { .. })) => CallOutcome::ToolError,
        Ok(Err(_)) => CallOutcome::TransportError,
        Err(_) => CallOutcome::Timeout,
    }
}

#[async_trait]
impl Tool for EdgeMcpTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.def.description
    }

    fn input_schema(&self) -> serde_json::Value {
        self.def.input_schema.clone()
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let started = Instant::now();
        let elapsed = || started.elapsed().as_millis() as u64;
        // The real error (which can carry internal hosts/URLs) stays server-side;
        // the model only sees a generic failure. The deadline stops a server that
        // never answers from hanging the turn. An auth-expired failure retries
        // once: the client re-reads the token per request, so a refresh that
        // landed mid-turn is picked up on the retry rather than failing the call.
        let mut result = self.timed_call(input.clone()).await;
        if matches!(&result, Ok(Err(error)) if error.is_auth()) {
            tracing::warn!(server = %self.server, tool = %self.name, "edge MCP tool call auth-expired; retrying once with the live token");
            result = self.timed_call(input).await;
        }
        metrics::tool_call(
            &self.agent,
            &self.server,
            &self.name,
            call_outcome(&result),
            started.elapsed(),
        );
        match result {
            Ok(Ok(output)) => Ok(ToolResult::success(output, elapsed())),
            // The message stays opaque, but the typed refusal signal survives —
            // it is the only channel this deliberately-scrubbed surface has.
            Ok(Err(McpError::RateLimited {
                retry_after_secs, ..
            })) => {
                tracing::warn!(server = %self.server, tool = %self.name, phase = "call", outcome = "failed", "edge MCP tool call rate limited");
                Ok(ToolResult::rate_limited(
                    "the tool call did not succeed",
                    retry_after_secs,
                    elapsed(),
                ))
            }
            Ok(Err(error)) => {
                tracing::warn!(server = %self.server, tool = %self.name, phase = "call", outcome = "failed", error = %error, "edge MCP tool call failed");
                Ok(ToolResult::failure(
                    "the tool call did not succeed",
                    elapsed(),
                ))
            }
            Err(_) => {
                tracing::warn!(server = %self.server, tool = %self.name, phase = "call", outcome = "timed_out", "edge MCP tool call timed out");
                Ok(ToolResult::failure(
                    "the tool call did not succeed",
                    elapsed(),
                ))
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::agent::{GrantSource, McpServerSettings, McpSettings, ToolGrant, PRIMARY_MCP_ID};
    use crate::config::TokenExchangeConfig;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    const AGENT: &str = "northwind-help";

    #[test]
    fn token_cell_reflects_set_and_shares_across_clones() {
        let cell = TokenCell::new("first");
        assert_eq!(&*cell.get(), "first");
        // The socket and the turn hold clones of the same cell: a refresh through
        // one handle must be visible through the other.
        let held_by_turn = cell.clone();
        cell.set("second");
        assert_eq!(&*held_by_turn.get(), "second");
    }

    #[tokio::test]
    async fn shared_bearer_reads_the_live_token_each_call() {
        let cell = TokenCell::new("early");
        let bearer = SharedBearer::new(cell.clone());
        assert_eq!(bearer.authorization_header().await.unwrap(), "Bearer early");
        cell.set("refreshed");
        assert_eq!(
            bearer.authorization_header().await.unwrap(),
            "Bearer refreshed"
        );
    }

    fn embed(url: &str, credential: Option<&str>) -> EmbedSettings {
        EmbedSettings {
            keycloak_issuer: "https://kc/realms/x".into(),
            mcp: Some(McpSettings {
                url: url.into(),
                discovery_credential: credential.map(str::to_string),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn extra(id: &str, url: &str, prefix: Option<&str>, tools: &[&str]) -> McpServerSettings {
        McpServerSettings {
            id: id.into(),
            connection: McpSettings {
                url: url.into(),
                ..Default::default()
            },
            tool_prefix: prefix.map(str::to_string),
            allowed_tools: tools.iter().map(|t| t.to_string()).collect(),
            tool_grants: vec![],
        }
    }

    async fn build(embed: &EmbedSettings) -> Result<McpCatalogue> {
        McpCatalogue::build(AGENT, embed, OutboundPolicy::Private, None).await
    }

    fn customer() -> TokenCell {
        TokenCell::new("customer-token")
    }

    /// `server` with every timeout on turn-start operations at `limit`.
    fn with_timeout(catalogue: &McpCatalogue, server: usize, limit: Duration) -> McpCatalogue {
        let mut servers = catalogue.servers().to_vec();
        servers[server] = servers[server].clone().with_handshake_timeout(limit);
        McpCatalogue {
            servers,
            ..catalogue.clone()
        }
    }

    #[test]
    fn cached_defs_age_decides_refresh() {
        let fresh = CachedDefs::now(Arc::new(Vec::new()));
        assert!(
            fresh.at.elapsed() < MCP_DEFS_TTL,
            "a new cache must be fresh"
        );
        // A cache stamped a TTL ago is due for refresh (skipped on platforms
        // whose monotonic clock can't represent the past offset).
        if let Some(past) = Instant::now().checked_sub(MCP_DEFS_TTL) {
            let stale = CachedDefs {
                tools: Arc::new(Vec::new()),
                at: past,
            };
            assert!(stale.at.elapsed() >= MCP_DEFS_TTL);
        }
    }

    #[tokio::test]
    async fn discover_without_credential_defers_to_first_turn() {
        // No credential + a valid url: the build succeeds without contacting the
        // server; the tool list is fetched lazily on the first customer turn.
        let catalogue = build(&embed("http://mcp.local", None)).await.unwrap();
        assert!(!catalogue.is_empty());
        assert_eq!(
            catalogue.tool_names(),
            vec![(PRIMARY_MCP_ID.to_string(), Vec::new())]
        );
    }

    #[tokio::test]
    async fn a_tool_less_agent_has_an_empty_catalogue() {
        // No `[mcp]` at all: a pure-chat agent, no discovery, no servers.
        let mut e = embed("http://unused", None);
        e.mcp = None;
        assert!(build(&e).await.unwrap().is_empty());
    }

    /// A tenant's MCP url at a private or plain-http address is refused at
    /// build under `Public` — for the primary and for an additional server
    /// alike; the same url is accepted under `Private` (the self-hosted,
    /// in-cluster case) exactly as before.
    #[tokio::test]
    async fn public_refuses_a_private_or_http_mcp_url_at_build() {
        for raw in [
            "http://mcp.example.com/mcp",
            "https://10.0.0.1/mcp",
            "https://127.0.0.1/mcp",
            "https://169.254.169.254/mcp",
            "https://[fd00::1]/mcp",
        ] {
            let primary = embed(raw, None);
            let mut additional = embed("https://93.184.216.34/mcp", None);
            additional.mcp_servers = vec![extra("docs", raw, Some("docs"), &["docs.*"])];
            for (settings, server) in [(&primary, PRIMARY_MCP_ID), (&additional, "docs")] {
                let err = McpCatalogue::build(AGENT, settings, OutboundPolicy::Public, None)
                    .await
                    .err()
                    .expect("refused under Public")
                    .to_string();
                assert!(err.contains("refusing MCP url"), "{raw}: {err}");
                assert!(err.contains(&format!("server {server}")), "{raw}: {err}");
                let catalogue = build(settings).await.unwrap();
                assert!(catalogue
                    .servers()
                    .iter()
                    .all(|s| s.policy() == OutboundPolicy::Private));
            }
        }
        // A public https literal passes the build-time check without a call.
        let catalogue = McpCatalogue::build(
            AGENT,
            &embed("https://93.184.216.34/mcp", None),
            OutboundPolicy::Public,
            None,
        )
        .await
        .unwrap();
        assert_eq!(catalogue.servers()[0].policy(), OutboundPolicy::Public);
    }

    #[tokio::test]
    async fn discover_rejects_an_invalid_url() {
        let err = build(&embed("not a url", None))
            .await
            .err()
            .expect("refused")
            .to_string();
        assert!(err.contains("invalid MCP url"), "got: {err}");
    }

    #[tokio::test]
    async fn the_build_refuses_a_misconfigured_server() {
        let mut settings = embed("http://mcp.local", None);
        settings.allowed_tools = vec!["harbor.*".into()];
        settings.mcp_servers = vec![extra("crm", "http://crm.local", None, &["harbor_x"])];
        let err = build(&settings).await.err().expect("overlap refused");
        assert!(err.to_string().contains("overlaps"), "{err}");
    }

    #[tokio::test]
    async fn deadline_bounds_a_stalled_op() {
        let stalled = std::future::pending::<Result<(), String>>();
        let err = deadline("initialize", Duration::from_millis(20), stalled)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("timed out"), "got: {err}");
        assert!(matches!(
            err,
            McpOpError::TimedOut {
                op: "initialize",
                ..
            }
        ));
        let report: eyre::Report = err.into();
        assert!(
            timed_out(&report),
            "a timeout survives conversion to a report"
        );
        assert_eq!(outcome_of(&report), "timed_out");
    }

    #[tokio::test]
    async fn deadline_passes_success_and_labels_errors() {
        assert_eq!(
            deadline("x", Duration::from_secs(5), async { Ok::<_, String>(7) })
                .await
                .unwrap(),
            7
        );
        let err = deadline("tools/list", Duration::from_secs(5), async {
            Err::<(), _>("boom".to_string())
        })
        .await
        .unwrap_err();
        assert_eq!(err.to_string(), "MCP tools/list failed: boom");
        assert_eq!(outcome_of(&err.into()), "failed");
    }

    /// Match a POST whose JSON-RPC body has `method == expected`.
    struct BodyRpc(&'static str);
    impl wiremock::Match for BodyRpc {
        fn matches(&self, req: &wiremock::Request) -> bool {
            serde_json::from_slice::<serde_json::Value>(&req.body)
                .ok()
                .and_then(|v| v["method"].as_str().map(str::to_string))
                .as_deref()
                == Some(self.0)
        }
    }

    fn rpc_id(req: &wiremock::Request) -> u64 {
        serde_json::from_slice::<serde_json::Value>(&req.body)
            .ok()
            .and_then(|v| v["id"].as_u64())
            .unwrap_or(0)
    }

    fn json_rpc(id: u64, result: serde_json::Value) -> wiremock::ResponseTemplate {
        wiremock::ResponseTemplate::new(200)
            .insert_header("content-type", "application/json")
            .insert_header("mcp-session-id", "sess-1")
            .set_body_json(serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result}))
    }

    fn initialize_result() -> serde_json::Value {
        serde_json::json!({
            "protocolVersion": "2025-06-18",
            "serverInfo": {"name": "fake-mcp", "version": "0.1.0"},
            "capabilities": {"tools": {}}
        })
    }

    /// Regression: the lazy-discovery path (`defs == None`) must release the
    /// `defs` read guard before taking the write lock. Holding it across the
    /// write self-deadlocks the RwLock, so the first customer turn hangs forever
    /// *after* a successful discover — exactly the prod symptom (initialize
    /// logged, then silence, no timeout firing because the write has none).
    #[tokio::test]
    async fn lazy_registry_for_does_not_deadlock_on_the_defs_lock() {
        let server = mock_mcp(&["ping"]).await;
        let mut settings = embed(&server.uri(), None);
        settings.allowed_tools = vec!["ping".into()];
        let catalogue = build(&settings).await.unwrap();

        // Bound each call so a re-introduced deadlock fails the test fast rather
        // than hanging the suite.
        let first =
            tokio::time::timeout(Duration::from_secs(5), catalogue.registry_for(&customer()))
                .await
                .expect("registry_for deadlocked on the first (lazy-discovery) turn")
                .expect("discovery should succeed");
        assert!(first.has("ping"), "discovered tool should be registered");

        // Cache is now populated → the `Some` branch must also return promptly.
        let second =
            tokio::time::timeout(Duration::from_secs(5), catalogue.registry_for(&customer()))
                .await
                .expect("registry_for deadlocked on a cached turn")
                .expect("cached discovery should succeed");
        assert!(second.has("ping"));
    }

    /// Mount initialize + notifications/initialized on `server`, so a test only
    /// has to script `tools/list`.
    async fn mount_handshake(server: &MockServer) {
        Mock::given(method("POST"))
            .and(BodyRpc("initialize"))
            .respond_with(|req: &Request| json_rpc(rpc_id(req), initialize_result()))
            .mount(server)
            .await;
        Mock::given(method("POST"))
            .and(BodyRpc("notifications/initialized"))
            .respond_with(ResponseTemplate::new(202))
            .mount(server)
            .await;
    }

    fn tool_json(name: &str) -> serde_json::Value {
        serde_json::json!({"name": name, "description": "d", "inputSchema": {"type": "object"}})
    }

    /// An MCP server exposing `tools`, for tests elsewhere that build a host
    /// whose agent carries a discovery credential (which discovers at build).
    /// Any path on the server answers; the credential is visible in the
    /// recorded requests' `authorization` header. A `tools/call` answers with
    /// the called name.
    pub(crate) async fn mock_mcp(tools: &[&str]) -> MockServer {
        let server = MockServer::start().await;
        mount_handshake(&server).await;
        let tools: Vec<serde_json::Value> = tools.iter().map(|t| tool_json(t)).collect();
        Mock::given(method("POST"))
            .and(BodyRpc("tools/list"))
            .respond_with(move |req: &Request| {
                json_rpc(rpc_id(req), serde_json::json!({ "tools": tools }))
            })
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(BodyRpc("tools/call"))
            .respond_with(|req: &Request| {
                let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap_or_default();
                json_rpc(
                    rpc_id(req),
                    serde_json::json!({ "called": body["params"]["name"] }),
                )
            })
            .mount(&server)
            .await;
        server
    }

    /// A server that answers every request with `status`.
    async fn broken_mcp(status: u16) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(status))
            .mount(&server)
            .await;
        server
    }

    /// A server whose `initialize` answers only after `delay`.
    async fn slow_mcp(delay: Duration) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(move |req: &Request| {
                json_rpc(rpc_id(req), initialize_result()).set_delay(delay)
            })
            .mount(&server)
            .await;
        server
    }

    async fn requests(server: &MockServer) -> usize {
        server.received_requests().await.unwrap_or_default().len()
    }

    /// The authorization headers `server` has seen, in order.
    async fn bearers(server: &MockServer) -> Vec<String> {
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter_map(|r| {
                r.headers
                    .get("authorization")?
                    .to_str()
                    .ok()
                    .map(str::to_string)
            })
            .collect()
    }

    async fn rpc_count(server: &MockServer, rpc: &str) -> usize {
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter_map(|r| serde_json::from_slice::<serde_json::Value>(&r.body).ok())
            .filter(|v| v["method"] == rpc)
            .count()
    }

    async fn called_names(server: &MockServer) -> Vec<String> {
        server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter_map(|r| serde_json::from_slice::<serde_json::Value>(&r.body).ok())
            .filter(|v| v["method"] == "tools/call")
            .filter_map(|v| v["params"]["name"].as_str().map(str::to_string))
            .collect()
    }

    /// A server advertising a built-in's name — exactly, or as its underscore
    /// twin — never reaches the registry, even when the allowlist admits it.
    #[tokio::test]
    async fn a_tool_shadowing_a_built_in_is_dropped() {
        let server = mock_mcp(&["data_query", "memory", "edge_version", "orders"]).await;
        let mut settings = embed(&server.uri(), Some("svc-cred"));
        settings.allowed_tools = vec![
            "data_query".into(),
            "memory".into(),
            "edge_version".into(),
            "orders".into(),
        ];
        let catalogue = build(&settings).await.unwrap();
        assert_eq!(
            catalogue.servers()[0].tool_names(),
            vec!["orders".to_string()]
        );
    }

    /// A prefix can push a name past the provider's 64-character limit; such a
    /// tool is dropped rather than failing the server, and a short one stays.
    #[tokio::test]
    async fn an_overlong_prefixed_name_is_dropped() {
        let long = "l".repeat(60);
        let primary = mock_mcp(&["harbor.ping"]).await;
        let docs = mock_mcp(&[long.as_str(), "search"]).await;
        let mut settings = embed(&primary.uri(), None);
        settings.allowed_tools = vec!["harbor.*".into()];
        settings.mcp_servers = vec![extra("docs", &docs.uri(), Some("docs"), &["docs.*"])];
        let registry = build(&settings)
            .await
            .unwrap()
            .registry_for(&customer())
            .await
            .unwrap();
        assert!(registry.has("docs.search"));
        assert!(!registry.has(&format!("docs.{long}")));
    }

    #[tokio::test]
    async fn an_additional_server_down_at_startup_does_not_fail_the_build() {
        let primary = mock_mcp(&["harbor.list_orders"]).await;
        let docs = broken_mcp(500).await;
        let mut settings = embed(&primary.uri(), Some("svc-cred"));
        settings.allowed_tools = vec!["harbor.*".into()];
        let mut docs_server = extra("docs", &docs.uri(), Some("docs"), &["docs.*"]);
        docs_server.connection.discovery_credential = Some("docs-cred".into());
        settings.mcp_servers = vec![docs_server];
        let catalogue = build(&settings).await.expect("an optional server degrades");
        assert_eq!(catalogue.servers()[1].tool_names(), Vec::<String>::new());

        // The server recovers: the first turn discovers it lazily.
        docs.reset().await;
        mount_handshake(&docs).await;
        Mock::given(method("POST"))
            .and(BodyRpc("tools/list"))
            .respond_with(|req: &Request| {
                json_rpc(
                    rpc_id(req),
                    serde_json::json!({ "tools": [tool_json("search")] }),
                )
            })
            .mount(&docs)
            .await;
        let registry = catalogue.registry_for(&customer()).await.unwrap();
        assert!(registry.has("harbor.list_orders") && registry.has("docs.search"));
    }

    /// Today's contract, kept by the `required` default: a primary that cannot
    /// be discovered at startup fails the build. The same holds for an
    /// additional server marked required, and a primary marked optional
    /// degrades like any other optional server.
    #[tokio::test]
    async fn a_required_server_down_at_startup_fails_the_build() {
        let down = broken_mcp(500).await;
        let healthy = mock_mcp(&["docs.search"]).await;
        let mut settings = embed(&down.uri(), Some("svc-cred"));
        let err = build(&settings)
            .await
            .err()
            .expect("the primary fails closed");
        assert!(
            format!("{err:#}").contains("MCP tool discovery at startup"),
            "{err:#}"
        );

        settings.mcp.as_mut().unwrap().required = Some(false);
        settings.mcp_servers = vec![extra("docs", &healthy.uri(), None, &["docs.*"])];
        build(&settings)
            .await
            .expect("an optional primary degrades");

        let mut required_extra = embed(&healthy.uri(), None);
        required_extra.allowed_tools = vec!["docs.*".into()];
        let mut docs = extra("crm", &down.uri(), None, &["crm.*"]);
        docs.connection.discovery_credential = Some("crm-cred".into());
        docs.connection.required = Some(true);
        required_extra.mcp_servers = vec![docs];
        let err = build(&required_extra)
            .await
            .err()
            .expect("a required extra fails closed");
        assert!(format!("{err:#}").contains("server crm"), "{err:#}");
    }

    #[tokio::test]
    async fn a_turn_merges_every_servers_tools_and_calls_the_raw_name() {
        let primary = mock_mcp(&["harbor.list_orders"]).await;
        let docs = mock_mcp(&["search"]).await;
        let mut settings = embed(&primary.uri(), None);
        settings.allowed_tools = vec!["harbor.list_orders".into()];
        settings.mcp_servers = vec![extra("docs", &docs.uri(), Some("docs"), &["docs.search"])];
        let catalogue = build(&settings).await.unwrap();
        let registry = catalogue.registry_for(&customer()).await.unwrap();
        assert_eq!(
            {
                let mut names = registry.names();
                names.sort();
                names
            },
            ["docs.search", "harbor.list_orders"]
        );
        let result = registry
            .get("docs.search")
            .unwrap()
            .execute(serde_json::json!({"q": "returns"}))
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(called_names(&docs).await, ["search"]);
        assert!(called_names(&primary).await.is_empty());
        assert_eq!(
            catalogue.tool_names(),
            vec![
                (
                    PRIMARY_MCP_ID.to_string(),
                    vec!["harbor.list_orders".to_string()]
                ),
                ("docs".to_string(), vec!["docs.search".to_string()]),
            ]
        );
    }

    #[tokio::test]
    async fn one_server_failing_at_turn_start_leaves_the_others_tools() {
        let primary = mock_mcp(&["harbor.list_orders"]).await;
        let docs = broken_mcp(500).await;
        let mut settings = embed(&primary.uri(), None);
        settings.allowed_tools = vec!["harbor.*".into()];
        settings.mcp_servers = vec![extra("docs", &docs.uri(), Some("docs"), &["docs.*"])];
        let catalogue = build(&settings).await.unwrap();
        let registry = catalogue.registry_for(&customer()).await.unwrap();
        assert!(registry.has("harbor.list_orders"));
        assert_eq!(registry.len(), 1);

        primary.reset().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&primary)
            .await;
        assert!(catalogue.registry_for(&customer()).await.is_err());

        // With only optional servers, the turn fails when none answers.
        let mut optional = settings.clone();
        optional.mcp.as_mut().unwrap().required = Some(false);
        let catalogue = build(&optional).await.unwrap();
        let err = catalogue
            .registry_for(&customer())
            .await
            .err()
            .expect("the turn fails");
        assert!(err.to_string().contains("no MCP server"), "{err}");
    }

    /// A required server failing at turn start fails the turn even when every
    /// other server answered.
    #[tokio::test]
    async fn a_required_server_failing_at_turn_start_fails_the_turn() {
        let primary = mock_mcp(&["harbor.list_orders"]).await;
        let crm = broken_mcp(500).await;
        let mut settings = embed(&primary.uri(), None);
        settings.allowed_tools = vec!["harbor.*".into()];
        let mut required = extra("crm", &crm.uri(), None, &["crm.*"]);
        required.connection.required = Some(true);
        settings.mcp_servers = vec![required];
        let err = build(&settings)
            .await
            .unwrap()
            .registry_for(&customer())
            .await
            .err()
            .expect("the turn fails");
        assert!(err.to_string().contains("required MCP server crm"), "{err}");
    }

    #[tokio::test]
    async fn a_timed_out_additional_server_is_skipped_until_its_window_ends() {
        let primary = mock_mcp(&["harbor.list_orders"]).await;
        let slow = slow_mcp(Duration::from_millis(500)).await;
        let failing = broken_mcp(500).await;
        let mut settings = embed(&primary.uri(), None);
        settings.allowed_tools = vec!["harbor.*".into()];
        settings.mcp_servers = vec![
            extra("docs", &slow.uri(), Some("docs"), &["docs.*"]),
            extra("billing", &failing.uri(), None, &["billing.*"]),
        ];
        let catalogue = with_timeout(
            &build(&settings).await.unwrap(),
            1,
            Duration::from_millis(50),
        );

        let first = catalogue.registry_for(&customer()).await.unwrap();
        assert!(
            first.has("harbor.list_orders"),
            "the primary's tools proceed"
        );
        let (slow_seen, failing_seen) = (requests(&slow).await, requests(&failing).await);
        assert!(slow_seen > 0 && failing_seen > 0);

        catalogue.registry_for(&customer()).await.unwrap();
        assert_eq!(
            requests(&slow).await,
            slow_seen,
            "skipped inside its window"
        );
        assert!(
            requests(&failing).await > failing_seen,
            "a fast failure is retried every turn"
        );

        catalogue.servers()[1].force_skip_expired();
        catalogue.registry_for(&customer()).await.unwrap();
        assert!(
            requests(&slow).await > slow_seen,
            "retried once the window ends"
        );
    }

    #[tokio::test]
    async fn a_zero_window_never_skips_and_a_required_server_is_never_skipped() {
        let slow = slow_mcp(Duration::from_millis(500)).await;
        let primary = mock_mcp(&["harbor.list_orders"]).await;
        let mut settings = embed(&primary.uri(), None);
        settings.allowed_tools = vec!["harbor.*".into()];
        let mut docs = extra("docs", &slow.uri(), Some("docs"), &["docs.*"]);
        docs.connection.unavailable_skip_secs = Some(0);
        settings.mcp_servers = vec![docs];
        let catalogue = with_timeout(
            &build(&settings).await.unwrap(),
            1,
            Duration::from_millis(50),
        );
        catalogue.registry_for(&customer()).await.unwrap();
        let seen = requests(&slow).await;
        catalogue.registry_for(&customer()).await.unwrap();
        assert!(requests(&slow).await > seen, "a zero window never skips");

        let alone = slow_mcp(Duration::from_millis(500)).await;
        let catalogue = with_timeout(
            &build(&embed(&alone.uri(), None)).await.unwrap(),
            0,
            Duration::from_millis(50),
        );
        let err = catalogue
            .registry_for(&customer())
            .await
            .err()
            .expect("the turn fails");
        assert!(
            err.to_string().contains("required MCP server default"),
            "{err}"
        );
        let seen = requests(&alone).await;
        assert!(catalogue.registry_for(&customer()).await.is_err());
        assert!(
            requests(&alone).await > seen,
            "a required server is attempted again on the very next turn"
        );
    }

    #[tokio::test]
    async fn each_server_refreshes_its_own_cache() {
        let primary = mock_mcp(&["harbor.list_orders"]).await;
        let docs = mock_mcp(&["search"]).await;
        let mut settings = embed(&primary.uri(), Some("svc-cred"));
        settings.allowed_tools = vec!["harbor.*".into()];
        let mut docs_server = extra("docs", &docs.uri(), Some("docs"), &["docs.*"]);
        docs_server.connection.discovery_credential = Some("docs-cred".into());
        settings.mcp_servers = vec![docs_server];
        let catalogue = build(&settings).await.unwrap();
        if !catalogue.servers()[1].force_defs_stale().await {
            return; // monotonic clock can't represent the offset; skip
        }
        docs.reset().await;
        mount_handshake(&docs).await;
        Mock::given(method("POST"))
            .and(BodyRpc("tools/list"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&docs)
            .await;
        let registry = catalogue.registry_for(&customer()).await.unwrap();
        assert_eq!(
            rpc_count(&primary, "tools/list").await,
            1,
            "the fresh server is not re-listed"
        );
        assert_eq!(
            rpc_count(&docs, "tools/list").await,
            1,
            "only the stale server re-lists"
        );
        assert!(
            registry.has("docs.search") && registry.has("harbor.list_orders"),
            "the failed refresh keeps its stale tools; the other is untouched"
        );
    }

    #[tokio::test]
    async fn a_failed_call_on_one_server_does_not_affect_another() {
        let primary = mock_mcp(&["harbor.list_orders"]).await;
        let docs = MockServer::start().await;
        mount_handshake(&docs).await;
        Mock::given(method("POST"))
            .and(BodyRpc("tools/list"))
            .respond_with(|req: &Request| {
                json_rpc(
                    rpc_id(req),
                    serde_json::json!({ "tools": [tool_json("search")] }),
                )
            })
            .mount(&docs)
            .await;
        Mock::given(method("POST"))
            .and(BodyRpc("tools/call"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&docs)
            .await;
        let mut settings = embed(&primary.uri(), None);
        settings.allowed_tools = vec!["harbor.*".into()];
        settings.mcp_servers = vec![extra("docs", &docs.uri(), Some("docs"), &["docs.*"])];
        let registry = build(&settings)
            .await
            .unwrap()
            .registry_for(&customer())
            .await
            .unwrap();
        let input = serde_json::json!({});
        let failed = registry
            .get("docs.search")
            .unwrap()
            .execute(input.clone())
            .await
            .unwrap();
        assert!(!failed.success);
        assert_eq!(
            failed.error.as_deref(),
            Some("the tool call did not succeed")
        );
        let ok = registry
            .get("harbor.list_orders")
            .unwrap()
            .execute(input)
            .await
            .unwrap();
        assert!(ok.success);
    }

    /// A Keycloak token endpoint that mints `tok-<audience>` (or `tok-none`)
    /// and refuses the audiences in `refused`.
    async fn mock_exchange(refused: &'static [&'static str]) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wiremock::matchers::path(
                "/realms/r/protocol/openid-connect/token",
            ))
            .respond_with(move |req: &Request| {
                let form: HashMap<String, String> = url::form_urlencoded::parse(&req.body)
                    .into_owned()
                    .collect();
                let audience = form.get("audience").cloned();
                match audience.as_deref() {
                    Some(a) if refused.contains(&a) => ResponseTemplate::new(400),
                    other => ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "access_token": format!("tok-{}", other.unwrap_or("none"))
                    })),
                }
            })
            .mount(&server)
            .await;
        server
    }

    fn exchanger(keycloak: &MockServer, audience: Option<&str>) -> Arc<TokenExchanger> {
        Arc::new(TokenExchanger::new(TokenExchangeConfig {
            issuer: format!("{}/realms/r", keycloak.uri()),
            client_id: "edge".into(),
            client_secret: "s3cret".into(),
            audience: audience.map(str::to_string),
            scope: None,
        }))
    }

    async fn token_requests(keycloak: &MockServer) -> Vec<Option<String>> {
        let mut audiences: Vec<Option<String>> = keycloak
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .map(|r| {
                url::form_urlencoded::parse(&r.body)
                    .find(|(k, _)| k == "audience")
                    .map(|(_, v)| v.into_owned())
            })
            .collect();
        audiences.sort();
        audiences
    }

    /// Four servers over two audiences: the primary inherits the install's
    /// audience, `billing` names the same one, `docs` its own, and `wiki` is
    /// forwarded. One exchange per distinct audience, shared by the servers
    /// that name it; the forwarded server carries the customer's live token.
    #[tokio::test]
    async fn servers_authenticate_by_their_own_mode_and_audience() {
        let keycloak = mock_exchange(&[]).await;
        let (primary, docs, billing, wiki) = (
            mock_mcp(&["harbor.list_orders"]).await,
            mock_mcp(&["search"]).await,
            mock_mcp(&["billing.list_invoices"]).await,
            mock_mcp(&["wiki.page"]).await,
        );
        let mut settings = embed(&primary.uri(), None);
        settings.allowed_tools = vec!["harbor.*".into()];
        let mut docs_server = extra("docs", &docs.uri(), Some("docs"), &["docs.*"]);
        docs_server.connection.audience = Some("docs-mcp".into());
        let mut billing_server = extra("billing", &billing.uri(), None, &["billing.*"]);
        billing_server.connection.audience = Some("harbor-mcp".into());
        let mut wiki_server = extra("wiki", &wiki.uri(), None, &["wiki.*"]);
        wiki_server.connection.auth = McpAuth::Forwarded;
        settings.mcp_servers = vec![docs_server, billing_server, wiki_server];
        let catalogue = McpCatalogue::build(
            AGENT,
            &settings,
            OutboundPolicy::Private,
            Some(exchanger(&keycloak, Some("harbor-mcp"))),
        )
        .await
        .unwrap();

        let cell = customer();
        let registry = catalogue.registry_for(&cell).await.unwrap();
        assert_eq!(
            token_requests(&keycloak).await,
            [Some("docs-mcp".to_string()), Some("harbor-mcp".to_string())],
            "exactly one exchange per distinct audience"
        );
        let only = |seen: Vec<String>, expected: &str| {
            assert!(
                !seen.is_empty() && seen.iter().all(|b| b == expected),
                "{seen:?} vs {expected}"
            )
        };
        only(bearers(&primary).await, "Bearer tok-harbor-mcp");
        only(bearers(&billing).await, "Bearer tok-harbor-mcp");
        only(bearers(&docs).await, "Bearer tok-docs-mcp");
        only(bearers(&wiki).await, "Bearer customer-token");

        cell.set("refreshed-token");
        registry
            .get("wiki.page")
            .unwrap()
            .execute(serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(
            bearers(&wiki).await.last().map(String::as_str),
            Some("Bearer refreshed-token"),
            "a forwarded server reads the live token on every call"
        );
    }

    /// An audience whose exchange fails falls back to the customer's own token
    /// for its servers only; the others keep their exchanged token.
    #[tokio::test]
    async fn a_failed_exchange_falls_back_to_the_customer_token_for_its_servers() {
        let keycloak = mock_exchange(&["docs-mcp"]).await;
        let (primary, docs) = (
            mock_mcp(&["harbor.list_orders"]).await,
            mock_mcp(&["search"]).await,
        );
        let mut settings = embed(&primary.uri(), None);
        settings.allowed_tools = vec!["harbor.*".into()];
        let mut docs_server = extra("docs", &docs.uri(), Some("docs"), &["docs.*"]);
        docs_server.connection.audience = Some("docs-mcp".into());
        settings.mcp_servers = vec![docs_server];
        let catalogue = McpCatalogue::build(
            AGENT,
            &settings,
            OutboundPolicy::Private,
            Some(exchanger(&keycloak, None)),
        )
        .await
        .unwrap();
        let registry = catalogue.registry_for(&customer()).await.unwrap();
        assert!(registry.has("docs.search") && registry.has("harbor.list_orders"));
        assert_eq!(
            token_requests(&keycloak).await,
            [None, Some("docs-mcp".to_string())]
        );
        assert!(bearers(&primary)
            .await
            .iter()
            .all(|b| b == "Bearer tok-none"));
        assert!(bearers(&docs)
            .await
            .iter()
            .all(|b| b == "Bearer customer-token"));
    }

    /// No exchange happens for a server sitting out its skip window, nor when
    /// every attempted server is forwarded.
    #[tokio::test]
    async fn only_attempted_exchanged_servers_are_exchanged_for() {
        let keycloak = mock_exchange(&[]).await;
        let primary = mock_mcp(&["harbor.list_orders"]).await;
        let mut settings = embed(&primary.uri(), None);
        settings.allowed_tools = vec!["harbor.*".into()];
        settings.mcp.as_mut().unwrap().auth = McpAuth::Forwarded;
        let catalogue = McpCatalogue::build(
            AGENT,
            &settings,
            OutboundPolicy::Private,
            Some(exchanger(&keycloak, Some("harbor-mcp"))),
        )
        .await
        .unwrap();
        catalogue.registry_for(&customer()).await.unwrap();
        assert!(token_requests(&keycloak).await.is_empty());
    }

    /// Write tiers on an additional server are discovered as part of its
    /// catalogue, in exposed names, exactly like the primary's.
    #[tokio::test]
    async fn an_additional_servers_grant_tools_are_discovered() {
        let primary = mock_mcp(&["harbor.list_orders"]).await;
        let docs = mock_mcp(&["search", "edit_page", "delete_page"]).await;
        let mut settings = embed(&primary.uri(), None);
        settings.allowed_tools = vec!["harbor.*".into()];
        let mut docs_server = extra("docs", &docs.uri(), Some("docs"), &["docs.search"]);
        docs_server.tool_grants = vec![ToolGrant {
            source: GrantSource::KeycloakRole,
            requires: "editor".into(),
            tools: vec!["docs.edit_page".into()],
        }];
        settings.mcp_servers = vec![docs_server];
        let registry = build(&settings)
            .await
            .unwrap()
            .registry_for(&customer())
            .await
            .unwrap();
        assert!(registry.has("docs.search") && registry.has("docs.edit_page"));
        assert!(!registry.has("docs.delete_page"));
    }

    #[tokio::test]
    async fn a_calls_outcome_tells_the_tools_answer_from_the_transports() {
        let ok: serde_json::Value = serde_json::json!({"content": []});
        let failed = serde_json::json!({"isError": true, "content": []});
        let timed_out =
            tokio::time::timeout(Duration::from_millis(1), std::future::pending::<()>())
                .await
                .unwrap_err();
        let server_error = McpError::ServerError {
            code: -32000,
            message: "no such order".into(),
            data: None,
        };
        assert_eq!(call_outcome(&Ok(Ok(ok))), CallOutcome::Ok);
        assert_eq!(call_outcome(&Ok(Ok(failed))), CallOutcome::ToolError);
        assert_eq!(call_outcome(&Ok(Err(server_error))), CallOutcome::ToolError);
        assert_eq!(
            call_outcome(&Ok(Err(McpError::Transport("refused".into())))),
            CallOutcome::TransportError
        );
        assert_eq!(call_outcome(&Err(timed_out)), CallOutcome::Timeout);
    }

    /// One agent's turns, recorded: startup and lazy discovery, cache hits
    /// and misses, a timed-out server entering and then sitting out its
    /// window, an exchange per audience with its outcome, a tool call, and a
    /// refused config — each under its server's (or audience's) label and
    /// nothing unbounded.
    #[test]
    fn mcp_metrics_are_emitted_per_server_with_bounded_labels() {
        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        ::metrics::with_local_recorder(&recorder, || {
            runtime.block_on(async {
                let keycloak = mock_exchange(&["docs-mcp"]).await;
                let primary = mock_mcp(&["harbor.list_orders"]).await;
                let docs = mock_mcp(&["search"]).await;
                let wiki = slow_mcp(Duration::from_millis(500)).await;
                let mut settings = embed(&primary.uri(), Some("svc-cred"));
                settings.allowed_tools = vec!["harbor.*".into()];
                let mut docs_server = extra("docs", &docs.uri(), Some("docs"), &["docs.*"]);
                docs_server.connection.audience = Some("docs-mcp".into());
                settings.mcp_servers = vec![
                    docs_server,
                    extra("wiki", &wiki.uri(), Some("wiki"), &["wiki.*"]),
                ];
                let built = McpCatalogue::build(
                    AGENT,
                    &settings,
                    OutboundPolicy::Private,
                    Some(exchanger(&keycloak, Some("harbor-mcp"))),
                )
                .await
                .unwrap();
                let catalogue = with_timeout(&built, 2, Duration::from_millis(50));
                let first = catalogue.registry_for(&customer()).await.unwrap();
                first
                    .get("harbor.list_orders")
                    .unwrap()
                    .execute(serde_json::json!({}))
                    .await
                    .unwrap();
                catalogue.registry_for(&customer()).await.unwrap();

                let mut colliding = embed("http://mcp.local", None);
                colliding.allowed_tools = vec!["harbor.*".into()];
                colliding.mcp_servers = vec![extra("crm", "http://crm.local", None, &["harbor_x"])];
                assert!(build(&colliding).await.is_err());
            })
        });
        let recorded = crate::metrics::tests::recorded(&snapshotter);
        crate::metrics::tests::assert_bounded(&recorded);
        let count = |key: &str| recorded.get(key).copied().unwrap_or(0);
        let agent = "agent=northwind-help";
        assert_eq!(
            count(&format!(
                "edge_mcp_discovery_total{{{agent},outcome=ok,phase=startup,server=default}}"
            )),
            1
        );
        assert_eq!(
            count(&format!(
                "edge_mcp_discovery_total{{{agent},outcome=ok,phase=lazy,server=docs}}"
            )),
            1
        );
        assert_eq!(
            count(&format!(
                "edge_mcp_discovery_total{{{agent},outcome=timed_out,phase=lazy,server=wiki}}"
            )),
            1
        );
        assert_eq!(
            count(&format!(
                "edge_mcp_discovery_duration_seconds{{{agent},phase=lazy,server=wiki}}"
            )),
            1
        );
        assert_eq!(
            count(&format!(
                "edge_mcp_tools_cache_total{{{agent},result=hit,server=default}}"
            )),
            2
        );
        assert_eq!(
            count(&format!(
                "edge_mcp_tools_cache_total{{{agent},result=miss,server=docs}}"
            )),
            1
        );
        assert_eq!(
            count(&format!(
                "edge_mcp_tools_cache_total{{{agent},result=hit,server=docs}}"
            )),
            1
        );
        assert_eq!(
            count(&format!(
                "edge_mcp_skip_window_entered_total{{{agent},server=wiki}}"
            )),
            1
        );
        assert_eq!(
            count(&format!(
                "edge_mcp_server_skipped_total{{{agent},server=wiki}}"
            )),
            1
        );
        assert_eq!(
            count(&format!(
                "edge_mcp_token_exchanges_total{{{agent},audience=harbor-mcp,outcome=ok}}"
            )),
            2,
            "the primary's audience, once per turn"
        );
        assert_eq!(
            count(&format!(
                "edge_mcp_token_exchanges_total{{{agent},audience=docs-mcp,outcome=failed}}"
            )),
            2
        );
        assert_eq!(
            count(&format!(
                "edge_mcp_token_exchange_duration_seconds{{{agent},audience=docs-mcp}}"
            )),
            2
        );
        assert_eq!(
            count(&format!(
                "edge_mcp_tool_calls_total{{{agent},outcome=ok,server=default,tool=harbor.list_orders}}"
            )),
            1
        );
        assert_eq!(
            count(&format!(
                "edge_mcp_tool_call_duration_seconds{{{agent},server=default,tool=harbor.list_orders}}"
            )),
            1
        );
        assert_eq!(
            count("edge_mcp_config_refusals_total{reason=name_collision}"),
            1
        );
    }

    #[test]
    fn a_prefixed_tool_is_named_locally_without_its_prefix() {
        let mut docs = McpToolset::empty();
        docs.set_tool_prefix_for_test("docs");
        let catalogue = McpCatalogue::of(vec![McpToolset::empty(), docs]);
        assert_eq!(catalogue.local_tool_name("docs.list_leads"), "list_leads");
        assert_eq!(catalogue.local_tool_name("docs_create_lead"), "create_lead");
        assert_eq!(
            catalogue.local_tool_name("harbor.list_orders"),
            "harbor.list_orders"
        );
        assert_eq!(catalogue.local_tool_name("docsx.list"), "docsx.list");
    }

    #[tokio::test]
    async fn a_stale_cache_re_lists_and_picks_up_a_new_tool() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let server = MockServer::start().await;
        mount_handshake(&server).await;
        // First `tools/list` (at discover) exposes one tool; the next exposes a
        // second — the customer having evolved their schema.
        let calls = Arc::new(AtomicUsize::new(0));
        Mock::given(method("POST"))
            .and(BodyRpc("tools/list"))
            .respond_with(move |req: &Request| {
                let n = calls.fetch_add(1, Ordering::SeqCst);
                let tools = if n == 0 {
                    serde_json::json!([tool_json("ping")])
                } else {
                    serde_json::json!([tool_json("ping"), tool_json("pong")])
                };
                json_rpc(rpc_id(req), serde_json::json!({ "tools": tools }))
            })
            .mount(&server)
            .await;

        let mut settings = embed(&server.uri(), Some("svc-cred"));
        settings.allowed_tools = vec!["ping".into(), "pong".into()];
        let catalogue = build(&settings).await.unwrap();
        let toolset = &catalogue.servers()[0];
        assert_eq!(toolset.tool_names(), vec!["ping".to_string()]);

        if !toolset.force_defs_stale().await {
            return; // monotonic clock can't represent the offset; skip
        }
        let registry = catalogue
            .registry_for(&customer())
            .await
            .expect("stale refresh should succeed");
        assert!(
            registry.has("ping") && registry.has("pong"),
            "new tool picked up"
        );
        assert_eq!(
            toolset.tool_names(),
            vec!["ping".to_string(), "pong".to_string()]
        );
    }

    #[tokio::test]
    async fn a_failed_refresh_keeps_serving_the_stale_tools() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let server = MockServer::start().await;
        mount_handshake(&server).await;
        let calls = Arc::new(AtomicUsize::new(0));
        Mock::given(method("POST"))
            .and(BodyRpc("tools/list"))
            .respond_with(move |req: &Request| {
                let n = calls.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    json_rpc(
                        rpc_id(req),
                        serde_json::json!({ "tools": [tool_json("ping")] }),
                    )
                } else {
                    ResponseTemplate::new(500)
                }
            })
            .mount(&server)
            .await;

        let mut settings = embed(&server.uri(), Some("svc-cred"));
        settings.allowed_tools = vec!["ping".into()];
        let catalogue = build(&settings).await.unwrap();

        if !catalogue.servers()[0].force_defs_stale().await {
            return;
        }
        // The refresh 500s; the turn must still get the previously-cached tool.
        let registry = catalogue
            .registry_for(&customer())
            .await
            .expect("a failed refresh must not fail the turn");
        assert!(registry.has("ping"), "stale tool kept on refresh failure");
    }

    /// The admin's survey lists every tool the server advertises, names the
    /// server, and carries exactly the credential it was given: a bearer
    /// when one was typed, no `Authorization` header at all otherwise.
    #[tokio::test]
    async fn a_survey_lists_every_tool_and_carries_only_the_credential_it_was_given() {
        let server = mock_mcp(&["acme.list_orders", "acme.item_transition"]).await;
        let url = format!("{}/mcp", server.uri());

        let anonymous = survey(&url, None, OutboundPolicy::Private)
            .await
            .expect("surveyed");
        assert_eq!(anonymous.server.as_deref(), Some("fake-mcp"));
        assert_eq!(anonymous.version.as_deref(), Some("0.1.0"));
        assert_eq!(anonymous.protocol_version, "2025-06-18");
        assert_eq!(
            anonymous
                .tools
                .iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>(),
            ["acme.list_orders", "acme.item_transition"]
        );
        assert!(!anonymous.truncated);
        let requests = server.received_requests().await.unwrap();
        assert!(!requests.is_empty());
        assert!(
            requests
                .iter()
                .all(|r| !r.headers.contains_key("authorization")),
            "no credential ⇒ no Authorization header"
        );

        let with_credential = survey(&url, Some(" svc-token "), OutboundPolicy::Private)
            .await
            .expect("surveyed");
        assert_eq!(with_credential.tools.len(), 2);
        let requests = server.received_requests().await.unwrap();
        assert!(
            requests.iter().any(|r| {
                r.headers.get("authorization").and_then(|v| v.to_str().ok())
                    == Some("Bearer svc-token")
            }),
            "the typed credential goes to the server, trimmed"
        );
    }

    /// A URL the policy refuses is never dialled, and reads as a URL fault;
    /// a server that does not complete the handshake reads as a server fault.
    #[tokio::test]
    async fn a_survey_tells_a_refused_url_from_a_failed_handshake() {
        let server = mock_mcp(&["ping"]).await;
        let url = format!("{}/mcp", server.uri());
        match survey(&url, None, OutboundPolicy::Public).await {
            Err(SurveyError::Url(reason)) => {
                assert!(reason.contains("refusing the MCP URL"), "{reason}")
            }
            other => panic!("a loopback http URL must be refused under Public: {other:?}"),
        }
        assert!(
            server.received_requests().await.unwrap().is_empty(),
            "nothing reaches a refused server"
        );
        assert!(matches!(
            survey("not a url", None, OutboundPolicy::Private).await,
            Err(SurveyError::Url(_))
        ));
        assert!(matches!(
            survey("ftp://mcp.example/", None, OutboundPolicy::Private).await,
            Err(SurveyError::Url(_))
        ));

        let broken = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(500))
            .mount(&broken)
            .await;
        match survey(&broken.uri(), None, OutboundPolicy::Private).await {
            Err(SurveyError::Server(reason)) => {
                assert!(reason.contains("initialize"), "{reason}")
            }
            other => panic!("a 500 on initialize is a server fault: {other:?}"),
        }
    }
}
