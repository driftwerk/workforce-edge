//! MCP wiring: an agent's company MCP servers.
//!
//! An edge agent acts against a primary company MCP server and any number of
//! additional ones, always **as the logged-in customer**. Each server is its own
//! [`McpToolset`] (connection, allowlist, cache, auth mode); [`McpCatalogue`]
//! merges them into one registry per turn, reaching every server concurrently
//! so one slow or down server costs only its own tools — unless it is
//! `required`, in which case the turn fails as a single-server agent's always
//! has. A server that is not required and whose URL cannot be used is left
//! out of the agent rather than failing it. The customer's token is carried
//! by the per-turn client's [`AuthProvider`], which rides the `Arc` graph
//! (`EdgeMcpTool` → `McpClient` → transport) into the runner's spawned tool
//! task — so the token reaches `tools/call` without any task-local or shared
//! `SessionContext` change. Tool schemas are discovered once and cached: at
//! startup if a service `discovery_credential` is configured (awaited for a
//! required server, in the background for any other), otherwise lazily on the
//! first customer turn using that customer's own token (for MCP servers that
//! accept any valid user token). Install-agnostic — nothing here is specific
//! to any one deployment.

use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap};
use std::future::Future;
use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use eyre::{eyre, Result, WrapErr};
use futures::future::{join_all, try_join_all};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use url::Url;
use workforce_agent::runner::to_llm_name;
use workforce_agent::tools::registry::ToolRegistry;
use workforce_agent::tools::{Tool, ToolResult};
use workforce_mcp_client::{
    AuthProvider, McpClient, McpError, McpLimits, McpToolDef, NoAuth, StaticBearer, ToolNameInfo,
};

use crate::agent::{
    allowlist_admits, allowlist_admits_on_the_wire, EmbedSettings, McpAuth, McpServerSpec,
};
use crate::host::BUILT_IN_TOOLS;
use crate::metrics::{self, AgentScope, CacheResult, CallOutcome, DiscoveryPhase};
use crate::outbound::{self, OutboundError, OutboundPolicy};
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
/// or an unreachable server is told apart so a server that is down can be
/// skipped for a while; any other failure is retried on the next turn.
#[derive(Debug)]
pub(crate) enum McpOpError {
    Failed { op: &'static str, detail: String },
    Unreachable { op: &'static str, detail: String },
    TimedOut { op: &'static str, secs: u64 },
}

impl McpOpError {
    fn of(op: &'static str, error: McpError) -> Self {
        let detail = error.to_string();
        if error.is_unreachable() {
            Self::Unreachable { op, detail }
        } else {
            Self::Failed { op, detail }
        }
    }
}

impl std::fmt::Display for McpOpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Failed { op, detail } | Self::Unreachable { op, detail } => {
                write!(f, "MCP {op} failed: {detail}")
            }
            Self::TimedOut { op, secs } => write!(f, "MCP {op} timed out after {secs}s"),
        }
    }
}

impl std::error::Error for McpOpError {}

/// Run an MCP operation under a hard deadline, classifying its failure as an
/// [`McpOpError`].
async fn deadline<T>(
    op: &'static str,
    limit: Duration,
    fut: impl Future<Output = Result<T, McpError>>,
) -> Result<T, McpOpError> {
    match tokio::time::timeout(limit, fut).await {
        Ok(result) => result.map_err(|error| McpOpError::of(op, error)),
        Err(_) => Err(McpOpError::TimedOut {
            op,
            secs: limit.as_secs(),
        }),
    }
}

/// Why a server counts as down rather than as having answered badly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Unavailable {
    TimedOut,
    Unreachable,
}

/// Whether `error` means the server is down: an operation timed out, the
/// server could not be reached, or its host did not resolve. A policy refusal
/// or an answered failure is not.
fn unavailable(error: &eyre::Report) -> Option<Unavailable> {
    match error.downcast_ref::<McpOpError>() {
        Some(McpOpError::TimedOut { .. }) => Some(Unavailable::TimedOut),
        Some(McpOpError::Unreachable { .. }) => Some(Unavailable::Unreachable),
        Some(McpOpError::Failed { .. }) => None,
        None => error
            .downcast_ref::<OutboundError>()
            .filter(|e| e.is_unreachable())
            .map(|_| Unavailable::Unreachable),
    }
}

/// The `outcome` a failed MCP operation is logged and counted under.
fn outcome_of(error: &eyre::Report) -> &'static str {
    match unavailable(error) {
        Some(Unavailable::TimedOut) => "timed_out",
        Some(Unavailable::Unreachable) => "unreachable",
        None => "failed",
    }
}

/// Why a server's URL cannot be used. Never carries the URL, which may hold a
/// secret; reads after the field it names (`mcp_servers[docs].url is required`).
#[derive(Debug)]
pub(crate) enum McpUrlError {
    Missing,
    Invalid(url::ParseError),
    NotHttp,
    Refused(OutboundError),
}

impl McpUrlError {
    /// The `reason` an excluded server is counted under.
    fn reason(&self) -> &'static str {
        match self {
            Self::Missing => "missing_url",
            Self::Invalid(_) | Self::NotHttp => "invalid_url",
            Self::Refused(_) => "refused_url",
        }
    }
}

impl std::fmt::Display for McpUrlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => f.write_str("is required"),
            Self::Invalid(e) => write!(f, "is not a URL: {e}"),
            Self::NotHttp => f.write_str("must be an http(s) URL"),
            Self::Refused(e) => write!(f, "is refused by the outbound policy: {e}"),
        }
    }
}

impl std::error::Error for McpUrlError {}

/// The endpoint `url` names, as far as `policy` can judge it without resolving it.
pub(crate) fn endpoint(url: &str, policy: OutboundPolicy) -> Result<Url, McpUrlError> {
    Some(url.trim())
        .filter(|u| !u.is_empty())
        .ok_or(McpUrlError::Missing)
        .and_then(|u| Url::parse(u).map_err(McpUrlError::Invalid))
        .and_then(|e| {
            outbound::check_url_as_written(policy, &e)
                .map(|()| e)
                .map_err(McpUrlError::Refused)
        })
}

/// [`endpoint`], and one the MCP client can speak to: http(s).
pub(crate) fn http_endpoint(url: &str, policy: OutboundPolicy) -> Result<Url, McpUrlError> {
    endpoint(url, policy).and_then(|e| {
        if matches!(e.scheme(), "http" | "https") {
            Ok(e)
        } else {
            Err(McpUrlError::NotHttp)
        }
    })
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
    /// The tenant and agent this server serves, for its metrics.
    scope: AgentScope,
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
    /// How long a server that is not required is skipped after its startup
    /// discovery or turn start found it unavailable (timed out or
    /// unreachable); zero never skips.
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
    /// Prepare one server for an agent; nothing is dialled. Every server's URL
    /// is judged under `policy` as written. A required server's host is also
    /// resolved and checked now, so a disallowed one fails the agent at build;
    /// any other server's host is checked whenever it is reached, as every
    /// server's is.
    fn prepare(
        scope: &AgentScope,
        spec: &McpServerSpec<'_>,
        policy: OutboundPolicy,
        install_audience: Option<&str>,
    ) -> Result<Self, McpUrlError> {
        let endpoint = endpoint(&spec.connection.url, policy)?;
        if spec.required {
            outbound::check_url(policy, &endpoint).map_err(McpUrlError::Refused)?;
        }
        Ok(Self {
            scope: scope.clone(),
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
        let tools = self
            .timed_discovery(DiscoveryPhase::Startup, async {
                let client = self.client_for(Arc::new(StaticBearer::new(credential)))?;
                self.discover_on(&client).await
            })
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
                .wrap_err_with(|| format!("refusing MCP url for server {}", self.id))?,
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
        metrics::discovery(&self.scope, &self.id, phase, outcome, started.elapsed());
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
    /// required, and only inside the window after it was found unavailable.
    fn in_skip_window(&self) -> bool {
        !self.required
            && self
                .skip_until
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .is_some_and(|until| Instant::now() < until)
    }

    /// Start the skip window after a startup discovery or turn start found
    /// the server unavailable.
    fn note_unavailable(&self) {
        if self.required || self.unavailable_skip.is_zero() {
            return;
        }
        *self
            .skip_until
            .lock()
            .unwrap_or_else(PoisonError::into_inner) =
            Instant::now().checked_add(self.unavailable_skip);
        metrics::skip_window_entered(&self.scope, &self.id);
    }

    /// Backdate the cached defs past the TTL so the next turn re-lists.
    /// Returns `false` (test skips) on a platform whose monotonic clock can't
    /// represent the offset.
    #[cfg(test)]
    pub(crate) async fn force_defs_stale(&self) -> bool {
        match Instant::now().checked_sub(MCP_DEFS_TTL + Duration::from_secs(1)) {
            Some(past) => {
                self.defs
                    .write()
                    .await
                    .as_mut()
                    .expect("only discovered defs can go stale")
                    .at = past;
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

    /// Serve every tool under `prefix`, as `tool_prefix = prefix` with
    /// `allowed_tools = ["prefix.*"]` would.
    #[cfg(test)]
    pub(crate) fn set_tool_prefix_for_test(&mut self, prefix: &str) {
        self.tool_prefix = Some(prefix.into());
        self.catalogue = vec![format!("{prefix}.*")];
    }

    /// A toolset with no tools and no server, for tests that drive a turn
    /// through a directly-supplied registry rather than a live MCP.
    #[cfg(test)]
    pub(crate) fn empty() -> Self {
        Self {
            scope: AgentScope::default(),
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
                metrics::tools_cache(&self.scope, &self.id, CacheResult::Hit);
                deadline("initialize", self.handshake_timeout, client.initialize()).await?;
                cached.tools
            }
            // Stale: re-list on this turn's already-initialized session so schema
            // changes land without a restart; on failure keep serving the stale
            // list — never degrade a working toolset over a refresh.
            Some(cached) => {
                metrics::tools_cache(&self.scope, &self.id, CacheResult::Stale);
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
                metrics::tools_cache(&self.scope, &self.id, CacheResult::Miss);
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
                    scope: self.scope.clone(),
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
        scope: &AgentScope,
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
                metrics::token_exchange(scope, audience.as_deref(), outcome, started.elapsed());
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
                        agent = scope.agent(),
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

/// The startup discovery of the servers that are not required, run off the
/// build so an unreachable one holds neither boot nor an admin save. Cancelled
/// when its catalogue is retired or the last clone of it is dropped.
#[derive(Default)]
struct BackgroundDiscovery {
    cancel: CancellationToken,
    tasks: TaskTracker,
}

impl BackgroundDiscovery {
    /// Discover `server` in the background. The outcome is metered by
    /// [`McpToolset::discover_at_startup`] and logged here; a timeout or an
    /// unreachable server opens the server's skip window, so the first turns
    /// do not wait on it again.
    fn spawn(&self, server: McpToolset) {
        let cancel = self.cancel.clone();
        self.tasks.spawn(async move {
            match cancel
                .run_until_cancelled(server.discover_at_startup())
                .await
            {
                None => {}
                Some(Ok(())) => tracing::info!(
                    agent = server.scope.agent(),
                    server = %server.id,
                    phase = "startup_discovery",
                    outcome = "ok",
                    tools = ?server.tool_names(),
                    "MCP server discovered"
                ),
                Some(Err(e)) => {
                    if unavailable(&e).is_some() {
                        server.note_unavailable();
                    }
                    tracing::warn!(
                        agent = server.scope.agent(),
                        server = %server.id,
                        phase = "startup_discovery",
                        outcome = outcome_of(&e),
                        error = %format!("{e:#}"),
                        "MCP server unavailable"
                    )
                }
            }
        });
    }
}

impl Drop for BackgroundDiscovery {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// Every company MCP server one agent uses, the primary first. Empty for a
/// tool-less, pure-chat agent.
#[derive(Clone, Default)]
pub struct McpCatalogue {
    scope: AgentScope,
    servers: Vec<McpToolset>,
    /// Configured servers whose URL could not be used, left out of this build.
    excluded: Vec<Arc<str>>,
    /// RFC 8693 exchanger (install-level, opt-in): mints a fresh, optionally
    /// audience-scoped downstream token per turn instead of forwarding the
    /// customer's raw token.
    exchanger: Option<Arc<TokenExchanger>>,
    background: Arc<BackgroundDiscovery>,
}

impl McpCatalogue {
    /// Check the agent's servers, prepare each one, then [`Self::start`]
    /// them. Every server is reached under `policy`.
    pub async fn build(
        scope: AgentScope,
        embed: &EmbedSettings,
        policy: OutboundPolicy,
        exchanger: Option<Arc<TokenExchanger>>,
    ) -> Result<Self> {
        let (servers, unusable) = Self::prepare(&scope, embed, policy, exchanger.as_deref())?;
        Self::start(scope, servers, unusable, exchanger).await
    }

    /// Check the agent's servers and prepare each one; nothing is dialled. A
    /// required server whose URL cannot be used fails the build; any other is
    /// returned with why, for [`Self::start`] to exclude.
    fn prepare<'a>(
        scope: &AgentScope,
        embed: &'a EmbedSettings,
        policy: OutboundPolicy,
        exchanger: Option<&TokenExchanger>,
    ) -> Result<(Vec<McpToolset>, Vec<Unusable<'a>>)> {
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
                        agent = scope.agent(),
                        server = spec.id,
                        "audience is set but token exchange is off for this agent; the server gets the customer's own token"
                    )
                });
        }
        let install_audience = exchanger.and_then(TokenExchanger::audience);
        specs.into_iter().try_fold(
            (Vec::new(), Vec::new()),
            |(mut servers, mut unusable), spec| {
                match McpToolset::prepare(scope, &spec, policy, install_audience) {
                    Ok(server) => servers.push(server),
                    Err(e) if spec.required => return Err(eyre!("{}.url {e}", spec.field())),
                    Err(e) => unusable.push((spec, e)),
                }
                Ok((servers, unusable))
            },
        )
    }

    /// Discover at startup. A required server's discovery is awaited and its
    /// failure fails the build (fail-closed). Every other server with a
    /// discovery credential is discovered in the background: its tools appear
    /// once found, and a turn before then discovers it lazily. Each `unusable`
    /// server is excluded once the build is known to go on, so a build that
    /// fails, and is retried, names and counts none of them.
    async fn start(
        scope: AgentScope,
        servers: Vec<McpToolset>,
        unusable: Vec<Unusable<'_>>,
        exchanger: Option<Arc<TokenExchanger>>,
    ) -> Result<Self> {
        let background = Arc::new(BackgroundDiscovery::default());
        servers
            .iter()
            .filter(|s| !s.required && s.discovery_credential.is_some())
            .for_each(|s| background.spawn(s.clone()));
        background.tasks.close();
        try_join_all(
            servers
                .iter()
                .filter(|s| s.required)
                .map(McpToolset::discover_at_startup),
        )
        .await?;
        let excluded = unusable
            .iter()
            .map(|(spec, e)| exclude(&scope, spec, e))
            .collect();
        Ok(Self {
            scope,
            servers,
            excluded,
            exchanger,
            background,
        })
    }

    /// Stop the background discovery now rather than when the last clone
    /// goes: a replaced host can outlive its swap while a turn finishes on it.
    pub fn retire(&self) {
        self.background.cancel.cancel();
    }

    /// Whether no server is configured, so the agent is a tool-less chat
    /// agent. A catalogue whose every server was excluded is not empty.
    pub fn is_empty(&self) -> bool {
        self.servers.is_empty() && self.excluded.is_empty()
    }

    /// The configured servers this build left out because their URL could
    /// not be used.
    pub fn excluded(&self) -> &[Arc<str>] {
        &self.excluded
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
            metrics::server_skipped(&self.scope, &s.id);
            tracing::debug!(
                agent = self.scope.agent(),
                server = %s.id,
                phase = "turn_start",
                outcome = "skipped",
                "MCP server skipped inside its skip window"
            )
        });
        let auth = TurnAuth::resolve(
            &self.scope,
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
                    if unavailable(&e).is_some() {
                        server.note_unavailable();
                    }
                    tracing::warn!(
                        agent = self.scope.agent(),
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
    /// form; a built-in, and a tool no prefixed server serves, come back as
    /// they are.
    pub fn local_tool_name<'a>(&self, tool: &'a str) -> &'a str {
        if shadows_builtin(tool) {
            return tool;
        }
        self.servers
            .iter()
            .filter(|s| allowlist_admits_on_the_wire(&s.catalogue, tool))
            .find_map(|s| {
                tool.strip_prefix(s.tool_prefix.as_deref()?)?
                    .strip_prefix(['.', '_'])
            })
            .unwrap_or(tool)
    }

    /// A catalogue of exactly `servers`, bypassing the build's checks and
    /// startup discovery.
    #[cfg(test)]
    pub(crate) fn of(servers: Vec<McpToolset>) -> Self {
        Self {
            servers,
            ..Self::default()
        }
    }

    /// Wait for the background discovery to finish or be cancelled.
    #[cfg(test)]
    pub(crate) async fn settled(&self) {
        self.background.tasks.wait().await;
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

/// A server that is not required, with why its URL cannot be used.
type Unusable<'a> = (McpServerSpec<'a>, McpUrlError);

/// Leave `spec` out of the build: log it once, count it, and name it.
fn exclude(scope: &AgentScope, spec: &McpServerSpec<'_>, e: &McpUrlError) -> Arc<str> {
    tracing::warn!(
        agent = scope.agent(),
        server = %spec.id,
        reason = e.reason(),
        error = %format!("{}.url {e}", spec.field()),
        "MCP server excluded: its url cannot be used, so the agent serves without its tools"
    );
    metrics::server_excluded(scope, spec.id, e.reason());
    Arc::from(spec.id)
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
        http_endpoint(url, policy).map_err(|e| SurveyError::Url(format!("the MCP URL {e}")))?;
    let http = outbound::client_for(
        policy,
        Duration::from_secs(MCP_REQUEST_TIMEOUT_SECS),
        &endpoint,
    )
    .map_err(|e| SurveyError::Url(format!("the MCP URL {}", McpUrlError::Refused(e))))?;
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
/// carries. Deliberately minimal — no multi-workspace gating.
struct EdgeMcpTool {
    client: Arc<McpClient>,
    /// The tenant, agent and serving server, for logs and metrics.
    scope: AgentScope,
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

/// How a call's result counts: a tool-level failure is the server's answer; an
/// unreachable server, a transport failure and a timeout each kept an answer
/// from arriving.
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
        Ok(Err(error)) if error.is_unreachable() => CallOutcome::Unreachable,
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
            &self.scope,
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
                let outcome = if error.is_unreachable() {
                    "unreachable"
                } else {
                    "failed"
                };
                tracing::warn!(server = %self.server, tool = %self.name, phase = "call", outcome, error = %error, "edge MCP tool call failed");
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

    fn scope() -> AgentScope {
        AgentScope::new("acme", "northwind-help")
    }

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
        McpCatalogue::build(scope(), embed, OutboundPolicy::Private, None).await
    }

    /// [`build`] with every server's turn-start operations bounded by `limit`.
    async fn build_with_timeout(embed: &EmbedSettings, limit: Duration) -> Result<McpCatalogue> {
        let (servers, unusable) =
            McpCatalogue::prepare(&scope(), embed, OutboundPolicy::Private, None)?;
        let servers = servers
            .into_iter()
            .map(|s| s.with_handshake_timeout(limit))
            .collect();
        McpCatalogue::start(scope(), servers, unusable, None).await
    }

    /// A primary with `harbor.*` and a startup credential, plus `docs`: an
    /// optional server at `docs_url` with a credential of its own.
    fn with_optional_docs(primary_url: &str, docs_url: &str) -> EmbedSettings {
        let mut settings = embed(primary_url, Some("svc-cred"));
        settings.allowed_tools = vec!["harbor.*".into()];
        let mut docs = extra("docs", docs_url, Some("docs"), &["docs.*"]);
        docs.connection.discovery_credential = Some("docs-cred".into());
        settings.mcp_servers = vec![docs];
        settings
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

    /// A loopback URL nothing listens on: the port was bound and released.
    fn closed_port_url() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        format!("http://{addr}/mcp")
    }

    /// `server` reached under `Public` at `url`, as if its host had changed
    /// since the build.
    fn under_public(catalogue: &McpCatalogue, server: usize, url: &str) -> McpCatalogue {
        let mut servers = catalogue.servers().to_vec();
        servers[server].policy = OutboundPolicy::Public;
        servers[server].endpoint = Url::parse(url).unwrap();
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
    /// build under `Public`: a required server fails the build, an optional
    /// one is left out of it. The same url is accepted under `Private` (the
    /// self-hosted, in-cluster case) exactly as before.
    #[tokio::test]
    async fn public_refuses_a_required_server_and_excludes_an_optional_one_at_build() {
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
            let mut required = additional.clone();
            required.mcp_servers[0].connection.required = Some(true);
            for (settings, field) in [(&primary, "mcp.url"), (&required, "mcp_servers[docs].url")] {
                let err = McpCatalogue::build(scope(), settings, OutboundPolicy::Public, None)
                    .await
                    .err()
                    .expect("refused under Public")
                    .to_string();
                assert!(
                    err.contains(&format!("{field} is refused by the outbound policy")),
                    "{raw}: {err}"
                );
            }
            let catalogue = McpCatalogue::build(scope(), &additional, OutboundPolicy::Public, None)
                .await
                .unwrap();
            assert_eq!(server_ids(&catalogue), [PRIMARY_MCP_ID]);
            assert_eq!(excluded_ids(&catalogue), ["docs"]);
            for settings in [&primary, &additional, &required] {
                let catalogue = build(settings).await.unwrap();
                assert!(catalogue
                    .servers()
                    .iter()
                    .all(|s| s.policy() == OutboundPolicy::Private));
            }
        }
        // A public https literal passes the build-time check without a call.
        let catalogue = McpCatalogue::build(
            scope(),
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
        assert!(err.contains("mcp.url is not a URL"), "got: {err}");
    }

    /// A structural fault refuses the build even when the offending server's
    /// URL also cannot be used: the check runs first, so nothing is prepared
    /// and nothing is excluded.
    #[test]
    fn the_build_refuses_a_misconfigured_server() {
        let (_, recorded) = recording(|| async {
            let mut settings = embed("http://mcp.local", None);
            settings.allowed_tools = vec!["harbor.*".into()];
            settings.mcp_servers = vec![extra("crm", "", None, &["harbor_x"])];
            let err = build(&settings).await.err().expect("overlap refused");
            assert!(err.to_string().contains("overlaps"), "{err}");
        });
        assert_eq!(excluded_counts(&recorded), 0, "{recorded:?}");
    }

    /// Run `test` on a current-thread runtime under a local metrics recorder,
    /// returning its value and every metric it recorded.
    fn recording<T, F: Future<Output = T>>(
        test: impl FnOnce() -> F,
    ) -> (T, std::collections::BTreeMap<String, u64>) {
        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let value = ::metrics::with_local_recorder(&recorder, || runtime.block_on(test()));
        (value, crate::metrics::tests::recorded(&snapshotter))
    }

    /// How many `edge_mcp_server_excluded_total` increments `recorded` holds.
    fn excluded_counts(recorded: &std::collections::BTreeMap<String, u64>) -> u64 {
        recorded
            .iter()
            .filter(|(key, _)| key.starts_with("edge_mcp_server_excluded_total"))
            .map(|(_, count)| count)
            .sum()
    }

    fn server_ids(catalogue: &McpCatalogue) -> Vec<&str> {
        catalogue.servers().iter().map(|s| &*s.id).collect()
    }

    pub(crate) fn excluded_ids(catalogue: &McpCatalogue) -> Vec<&str> {
        catalogue.excluded().iter().map(|id| &**id).collect()
    }

    #[test]
    fn an_endpoint_names_what_is_wrong_and_never_the_url() {
        for blank in ["", "  "] {
            let e = endpoint(blank, OutboundPolicy::Private).unwrap_err();
            assert!(matches!(e, McpUrlError::Missing), "{e:?}");
            assert_eq!(
                (e.reason(), e.to_string().as_str()),
                ("missing_url", "is required")
            );
        }
        let e = endpoint("docs.example.com/mcp?key=s3cret", OutboundPolicy::Private).unwrap_err();
        assert!(matches!(e, McpUrlError::Invalid(_)), "{e:?}");
        assert_eq!(e.reason(), "invalid_url");
        assert!(e.to_string().starts_with("is not a URL:"), "{e}");
        assert!(!e.to_string().contains("s3cret"), "{e}");

        let ftp = "ftp://kb.example.com/mcp";
        assert!(endpoint(ftp, OutboundPolicy::Private).is_ok());
        let e = http_endpoint(ftp, OutboundPolicy::Private).unwrap_err();
        assert!(matches!(e, McpUrlError::NotHttp), "{e:?}");
        assert_eq!(
            (e.reason(), e.to_string().as_str()),
            ("invalid_url", "must be an http(s) URL")
        );

        for check in [endpoint, http_endpoint] {
            let e = check("http://docs.example.com/mcp", OutboundPolicy::Public).unwrap_err();
            assert!(
                matches!(e, McpUrlError::Refused(OutboundError::NotHttps)),
                "{e:?}"
            );
            assert_eq!(e.reason(), "refused_url");
            assert_eq!(
                e.to_string(),
                "is refused by the outbound policy: only https URLs may be called"
            );
            assert_eq!(
                check("HTTPS://Docs.Example.com/mcp", OutboundPolicy::Public)
                    .unwrap()
                    .as_str(),
                "https://docs.example.com/mcp"
            );
        }
    }

    /// An optional server whose URL cannot be used is left out of the build,
    /// named once and counted, and never reached, even with a startup
    /// credential; the agent serves the rest.
    #[test]
    fn an_optional_server_whose_url_cannot_be_used_is_excluded() {
        let (_, recorded) = recording(|| async {
            let primary = mock_mcp(&["harbor.list_orders"]).await;
            let mut settings = embed(&primary.uri(), None);
            settings.allowed_tools = vec!["harbor.*".into()];
            let mut kb = extra("kb", "kb.example.com/mcp", Some("kb"), &["kb.*"]);
            kb.connection.discovery_credential = Some("kb-cred".into());
            settings.mcp_servers = vec![
                extra("docs", "", Some("docs"), &["docs.*"]),
                extra("wiki", "not a url", Some("wiki"), &["wiki.*"]),
                kb,
            ];
            let catalogue = build(&settings).await.unwrap();
            assert_eq!(server_ids(&catalogue), [PRIMARY_MCP_ID]);
            assert_eq!(excluded_ids(&catalogue), ["docs", "wiki", "kb"]);
            assert!(!catalogue.is_empty());
            catalogue.settled().await;
            let registry = catalogue.registry_for(&customer()).await.unwrap();
            assert!(registry.has("harbor.list_orders") && registry.len() == 1);
        });
        crate::metrics::tests::assert_bounded(&recorded);
        for (server, reason) in [
            ("docs", "missing_url"),
            ("wiki", "invalid_url"),
            ("kb", "invalid_url"),
        ] {
            let key = format!(
                "edge_mcp_server_excluded_total{{agent=northwind-help,reason={reason},server={server},tenant=acme}}"
            );
            assert_eq!(recorded.get(&key), Some(&1), "{key} in {recorded:?}");
            assert!(
                recorded
                    .keys()
                    .all(|key| !key.contains(&format!("server={server},"))
                        || key.starts_with("edge_mcp_server_excluded_total")),
                "{server} was reached: {recorded:?}"
            );
        }
    }

    /// A required server whose URL cannot be used fails the build by field,
    /// without the URL, and excludes nothing: not even an optional server
    /// prepared before it, nor one beside a required server down at startup,
    /// whose build never served.
    #[test]
    fn a_required_server_whose_url_cannot_be_used_fails_the_build() {
        let (_, recorded) = recording(|| async {
            let healthy = mock_mcp(&["harbor.list_orders"]).await;
            for raw in ["", "not a url", "docs.example.com/mcp?key=s3cret"] {
                let err = build(&embed(raw, None)).await.err().expect("refused");
                let err = format!("{err:#}");
                assert!(err.contains("mcp.url is"), "{raw}: {err}");
                assert!(!err.contains("s3cret"), "{raw}: {err}");

                let mut settings = embed(&healthy.uri(), None);
                settings.allowed_tools = vec!["harbor.*".into()];
                let mut docs = extra("docs", raw, Some("docs"), &["docs.*"]);
                docs.connection.required = Some(true);
                settings.mcp_servers = vec![docs];
                let err = build(&settings).await.err().expect("refused");
                let err = format!("{err:#}");
                assert!(err.contains("mcp_servers[docs].url is"), "{raw}: {err}");
                assert!(!err.contains("s3cret"), "{raw}: {err}");
            }

            let mut settings = embed(&healthy.uri(), None);
            settings.allowed_tools = vec!["harbor.*".into()];
            let mut wiki = extra("wiki", "not a url", Some("wiki"), &["wiki.*"]);
            wiki.connection.required = Some(true);
            settings.mcp_servers = vec![extra("docs", "", Some("docs"), &["docs.*"]), wiki];
            let err = build(&settings).await.err().expect("refused");
            assert!(
                err.to_string()
                    .contains("mcp_servers[wiki].url is not a URL"),
                "{err}"
            );

            let mut settings = embed(&closed_port_url(), Some("svc-cred"));
            settings.allowed_tools = vec!["harbor.*".into()];
            settings.mcp_servers = vec![extra("docs", "", Some("docs"), &["docs.*"])];
            let err = build(&settings).await.err().expect("the primary is down");
            assert!(
                format!("{err:#}").contains("MCP tool discovery at startup"),
                "{err:#}"
            );
        });
        assert_eq!(excluded_counts(&recorded), 0, "{recorded:?}");
    }

    /// An agent whose only server was excluded is not a tool-less chat agent:
    /// its turns find no server available, as when every server is down.
    #[tokio::test]
    async fn an_agent_whose_every_server_was_excluded_is_not_tool_less() {
        let mut settings = embed("", None);
        if let Some(mcp) = settings.mcp.as_mut() {
            mcp.required = Some(false);
        }
        let catalogue = build(&settings).await.unwrap();
        assert!(!catalogue.is_empty());
        let err = catalogue
            .registry_for(&customer())
            .await
            .err()
            .expect("no server to answer");
        assert!(
            err.to_string().contains("no MCP server is available"),
            "{err}"
        );
    }

    /// Under `Public` an optional server is judged at build from its URL as
    /// written: a plain-http or private-literal one is excluded, while one
    /// whose host does not resolve is kept and found unreachable when it is
    /// reached. A required server's host is resolved at build. Relies on
    /// `.invalid` resolving to nothing (RFC 6761).
    #[test]
    fn under_public_an_optional_server_is_judged_as_written_and_resolved_when_reached() {
        let with_kb = |required: Option<bool>| {
            let mut settings = embed("https://93.184.216.34/mcp", None);
            settings.allowed_tools = vec!["harbor.*".into()];
            let mut kb = extra(
                "kb",
                "https://kb.unresolvable.invalid/mcp",
                Some("kb"),
                &["kb.*"],
            );
            kb.connection.discovery_credential = Some("kb-cred".into());
            kb.connection.required = required;
            settings.mcp_servers = vec![
                extra(
                    "docs",
                    "http://docs.example.com/mcp",
                    Some("docs"),
                    &["docs.*"],
                ),
                extra("wiki", "https://10.0.0.7/mcp", Some("wiki"), &["wiki.*"]),
                kb,
            ];
            settings
        };
        let (_, recorded) = recording(|| async {
            let catalogue =
                McpCatalogue::build(scope(), &with_kb(None), OutboundPolicy::Public, None)
                    .await
                    .unwrap();
            assert_eq!(server_ids(&catalogue), [PRIMARY_MCP_ID, "kb"]);
            assert_eq!(excluded_ids(&catalogue), ["docs", "wiki"]);
            catalogue.settled().await;
            assert!(catalogue.servers()[1].in_skip_window());

            let err =
                McpCatalogue::build(scope(), &with_kb(Some(true)), OutboundPolicy::Public, None)
                    .await
                    .err()
                    .expect("a required server's host is resolved at build");
            assert!(
                err.to_string().contains(
                    "mcp_servers[kb].url is refused by the outbound policy: kb.unresolvable.invalid could not be resolved"
                ),
                "{err}"
            );
        });
        for (key, expected) in [
            ("edge_mcp_server_excluded_total{agent=northwind-help,reason=refused_url,server=docs,tenant=acme}", 1),
            ("edge_mcp_server_excluded_total{agent=northwind-help,reason=refused_url,server=wiki,tenant=acme}", 1),
            ("edge_mcp_discovery_total{agent=northwind-help,outcome=unreachable,phase=startup,server=kb,tenant=acme}", 1),
            ("edge_mcp_skip_window_entered_total{agent=northwind-help,server=kb,tenant=acme}", 1),
        ] {
            assert_eq!(recorded.get(key).copied().unwrap_or(0), expected, "{key} in {recorded:?}");
        }
    }

    #[tokio::test]
    async fn deadline_bounds_a_stalled_op() {
        let stalled = std::future::pending::<Result<(), McpError>>();
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
        assert_eq!(
            unavailable(&report),
            Some(Unavailable::TimedOut),
            "a timeout survives conversion to a report"
        );
        assert_eq!(outcome_of(&report), "timed_out");
    }

    #[tokio::test]
    async fn deadline_passes_success_and_labels_errors() {
        assert_eq!(
            deadline("x", Duration::from_secs(5), async { Ok::<_, McpError>(7) })
                .await
                .unwrap(),
            7
        );
        let answered = [
            (
                McpError::Transport("boom".into()),
                "MCP tools/list failed: transport error: boom",
            ),
            (
                McpError::ServerError {
                    code: -32000,
                    message: "no such tool".into(),
                    data: None,
                },
                "MCP tools/list failed: server error (code -32000): no such tool",
            ),
        ];
        for (error, text) in answered {
            let err = deadline("tools/list", Duration::from_secs(5), async {
                Err::<(), _>(error)
            })
            .await
            .unwrap_err();
            assert!(matches!(err, McpOpError::Failed { .. }), "{err}");
            assert_eq!(err.to_string(), text);
            let report: eyre::Report = err.into();
            assert_eq!(outcome_of(&report), "failed");
            assert_eq!(
                unavailable(&report),
                None,
                "an answer never opens the window"
            );
        }
    }

    #[tokio::test]
    async fn deadline_tells_an_unreachable_server_from_a_failure() {
        let err = deadline("initialize", Duration::from_secs(5), async {
            Err::<(), _>(McpError::Unreachable("connection refused".into()))
        })
        .await
        .unwrap_err();
        assert!(
            matches!(
                err,
                McpOpError::Unreachable {
                    op: "initialize",
                    ..
                }
            ),
            "{err}"
        );
        assert_eq!(
            err.to_string(),
            "MCP initialize failed: server unreachable: connection refused"
        );
        let report: eyre::Report = err.into();
        assert_eq!(outcome_of(&report), "unreachable");
        assert_eq!(unavailable(&report), Some(Unavailable::Unreachable));
    }

    /// The report shapes that reach the call sites: an error wrapped as
    /// `client_for` wraps a refusal, and again as `discover_at_startup` does.
    #[test]
    fn an_unresolvable_host_is_unavailable_and_a_refusal_is_not() {
        fn shapes<E>(error: impl Fn() -> E) -> [eyre::Report; 2]
        where
            E: std::error::Error + Send + Sync + 'static,
        {
            let refused = || Err::<(), _>(error()).wrap_err("refusing MCP url for server docs");
            [
                refused().unwrap_err(),
                refused()
                    .wrap_err("MCP tool discovery at startup (server docs)")
                    .unwrap_err(),
            ]
        }
        let assert_shapes = |reports: [eyre::Report; 2], expected: Option<Unavailable>, outcome| {
            reports.iter().for_each(|report| {
                assert_eq!(unavailable(report), expected, "{report:#}");
                assert_eq!(outcome_of(report), outcome, "{report:#}");
            })
        };
        assert_shapes(
            shapes(|| OutboundError::Unresolvable("docs.example.com".into())),
            Some(Unavailable::Unreachable),
            "unreachable",
        );
        [
            OutboundError::PrivateAddress("10.0.0.1".parse().unwrap()),
            OutboundError::NotHttps,
            OutboundError::NoHost,
        ]
        .into_iter()
        .for_each(|refusal| assert_shapes(shapes(|| refusal.clone()), None, "failed"));
        assert_shapes(
            shapes(|| McpOpError::TimedOut {
                op: "initialize",
                secs: 30,
            }),
            Some(Unavailable::TimedOut),
            "timed_out",
        );
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
    pub(crate) async fn slow_mcp(delay: Duration) -> MockServer {
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
        catalogue.settled().await;
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

    /// An optional server's startup discovery runs off the build: its tools
    /// appear once found, without a turn having to discover them.
    #[tokio::test]
    async fn an_optional_servers_tools_appear_once_discovered_in_the_background() {
        let primary = mock_mcp(&["harbor.list_orders"]).await;
        let docs = mock_mcp(&["search"]).await;
        let catalogue = build(&with_optional_docs(&primary.uri(), &docs.uri()))
            .await
            .unwrap();
        catalogue.settled().await;
        assert_eq!(catalogue.servers()[1].tool_names(), vec!["docs.search"]);
        assert_eq!(bearers(&docs).await[0], "Bearer docs-cred");
        let registry = catalogue.registry_for(&customer()).await.unwrap();
        assert!(registry.has("harbor.list_orders") && registry.has("docs.search"));
        assert_eq!(
            rpc_count(&docs, "tools/list").await,
            1,
            "no lazy re-discovery"
        );
    }

    /// A blackholed optional server holds neither the build nor the first
    /// turn: the build returns before its discovery could time out, the
    /// timeout is metered and opens its skip window, and the turn goes on
    /// without it.
    #[test]
    fn a_blackholed_optional_server_does_not_hold_the_build() {
        const LIMIT: Duration = Duration::from_secs(2);
        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        ::metrics::with_local_recorder(&recorder, || {
            runtime.block_on(async {
                let primary = mock_mcp(&["harbor.list_orders"]).await;
                let blackhole = slow_mcp(Duration::from_secs(600)).await;
                let started = Instant::now();
                let catalogue = build_with_timeout(
                    &with_optional_docs(&primary.uri(), &blackhole.uri()),
                    LIMIT,
                )
                .await
                .unwrap();
                assert!(started.elapsed() < LIMIT / 2, "{:?}", started.elapsed());
                assert_eq!(
                    catalogue.servers()[0].tool_names(),
                    vec!["harbor.list_orders"]
                );

                catalogue.settled().await;
                assert!(catalogue.servers()[1].in_skip_window());
                let turn = Instant::now();
                let registry = catalogue.registry_for(&customer()).await.unwrap();
                assert!(turn.elapsed() < LIMIT / 2, "{:?}", turn.elapsed());
                assert!(registry.has("harbor.list_orders") && !registry.has("docs.search"));
            })
        });
        let recorded = crate::metrics::tests::recorded(&snapshotter);
        let count = |key: &str| recorded.get(key).copied().unwrap_or(0);
        let agent = "agent=northwind-help";
        assert_eq!(
            count(&format!(
                "edge_mcp_discovery_total{{{agent},outcome=timed_out,phase=startup,server=docs,tenant=acme}}"
            )),
            1
        );
        assert_eq!(
            count(&format!(
                "edge_mcp_skip_window_entered_total{{{agent},server=docs,tenant=acme}}"
            )),
            1
        );
        assert_eq!(
            count(&format!(
                "edge_mcp_server_skipped_total{{{agent},server=docs,tenant=acme}}"
            )),
            1
        );
    }

    /// Retiring a catalogue, or dropping its last clone, cancels the
    /// background discovery it still has in flight.
    #[tokio::test]
    async fn retiring_or_dropping_a_catalogue_cancels_its_background_discovery() {
        let primary = mock_mcp(&["harbor.list_orders"]).await;
        let blackhole = slow_mcp(Duration::from_secs(600)).await;
        let settings = with_optional_docs(&primary.uri(), &blackhole.uri());
        let prompt = Duration::from_secs(5);

        let retired = build(&settings).await.unwrap();
        retired.retire();
        tokio::time::timeout(prompt, retired.settled())
            .await
            .expect("retire cancels the discovery");
        assert!(
            !retired.servers()[1].in_skip_window(),
            "a cancelled discovery is no outcome"
        );

        let dropped = build(&settings).await.unwrap();
        let tasks = dropped.background.tasks.clone();
        let clone = dropped.clone();
        drop(dropped);
        assert!(!tasks.is_empty(), "a clone keeps it running");
        drop(clone);
        tokio::time::timeout(prompt, tasks.wait())
            .await
            .expect("dropping the last clone cancels the discovery");
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

    /// An optional server that refuses connections is found unavailable at
    /// once, on the startup and the lazy path alike, and sits out its window
    /// without a turn waiting on the handshake deadline.
    #[test]
    fn an_unreachable_optional_server_sits_out_its_window() {
        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        ::metrics::with_local_recorder(&recorder, || {
            runtime.block_on(async {
                let primary = mock_mcp(&["harbor.list_orders"]).await;
                let catalogue = build(&with_optional_docs(&primary.uri(), &closed_port_url()))
                    .await
                    .unwrap();
                catalogue.settled().await;
                assert!(catalogue.servers()[1].in_skip_window());
                let registry = catalogue.registry_for(&customer()).await.unwrap();
                assert!(registry.has("harbor.list_orders") && !registry.has("docs.search"));

                let mut settings = embed(&primary.uri(), None);
                settings.allowed_tools = vec!["harbor.*".into()];
                settings.mcp_servers =
                    vec![extra("wiki", &closed_port_url(), Some("wiki"), &["wiki.*"])];
                let lazy = build(&settings).await.unwrap();
                let turn = Instant::now();
                let registry = lazy.registry_for(&customer()).await.unwrap();
                assert!(
                    turn.elapsed() < Duration::from_secs(5),
                    "{:?}",
                    turn.elapsed()
                );
                assert!(registry.has("harbor.list_orders") && registry.len() == 1);
                assert!(lazy.servers()[1].in_skip_window());

                lazy.servers()[1].force_skip_expired();
                lazy.registry_for(&customer()).await.unwrap();
                assert!(
                    lazy.servers()[1].in_skip_window(),
                    "attempted again once the window ends"
                );
            })
        });
        let recorded = crate::metrics::tests::recorded(&snapshotter);
        let count = |key: &str| recorded.get(key).copied().unwrap_or(0);
        for (key, expected) in [
            ("edge_mcp_discovery_total{agent=northwind-help,outcome=unreachable,phase=startup,server=docs,tenant=acme}", 1),
            ("edge_mcp_skip_window_entered_total{agent=northwind-help,server=docs,tenant=acme}", 1),
            ("edge_mcp_server_skipped_total{agent=northwind-help,server=docs,tenant=acme}", 1),
            ("edge_mcp_discovery_total{agent=northwind-help,outcome=unreachable,phase=lazy,server=wiki,tenant=acme}", 2),
            ("edge_mcp_skip_window_entered_total{agent=northwind-help,server=wiki,tenant=acme}", 2),
            ("edge_mcp_server_skipped_total{agent=northwind-help,server=wiki,tenant=acme}", 0),
        ] {
            assert_eq!(count(key), expected, "{key} in {recorded:?}");
        }
    }

    /// Under `Public` a host that no longer resolves sits out its window,
    /// while one that now resolves inward is refused again every turn; neither
    /// is dialled. Relies on `.invalid` resolving to nothing (RFC 6761); the
    /// classification itself is pinned without DNS above.
    #[tokio::test]
    async fn under_public_an_unresolvable_host_sits_out_and_a_refused_address_does_not() {
        let primary = mock_mcp(&["harbor.list_orders"]).await;
        let docs = mock_mcp(&["search"]).await;
        let wiki = mock_mcp(&["page"]).await;
        let mut settings = embed(&primary.uri(), None);
        settings.allowed_tools = vec!["harbor.*".into()];
        settings.mcp_servers = vec![
            extra("docs", &docs.uri(), Some("docs"), &["docs.*"]),
            extra("wiki", &wiki.uri(), Some("wiki"), &["wiki.*"]),
        ];
        let built = build(&settings).await.unwrap();
        let catalogue = under_public(
            &under_public(&built, 1, "https://docs.unresolvable.invalid/mcp"),
            2,
            "https://127.0.0.1/mcp",
        );

        let first = catalogue.registry_for(&customer()).await.unwrap();
        assert!(first.has("harbor.list_orders") && first.len() == 1);
        assert!(catalogue.servers()[1].in_skip_window());
        assert!(!catalogue.servers()[2].in_skip_window());

        catalogue.registry_for(&customer()).await.unwrap();
        assert!(!catalogue.servers()[2].in_skip_window());
        assert_eq!((requests(&docs).await, requests(&wiki).await), (0, 0));
    }

    /// A required server that refuses connections fails the turn, quickly,
    /// and is attempted again on the next; with a startup credential it fails
    /// the build. An optional one with a zero window never sits out.
    #[tokio::test]
    async fn an_unreachable_required_server_is_never_skipped() {
        let catalogue = build(&embed(&closed_port_url(), None)).await.unwrap();
        for _ in 0..2 {
            let turn = Instant::now();
            let err = catalogue
                .registry_for(&customer())
                .await
                .err()
                .expect("the turn fails closed");
            assert!(
                err.to_string().contains("required MCP server default"),
                "{err}"
            );
            assert!(
                turn.elapsed() < Duration::from_secs(5),
                "{:?}",
                turn.elapsed()
            );
            assert!(!catalogue.servers()[0].in_skip_window());
        }

        let err = build(&embed(&closed_port_url(), Some("svc-cred")))
            .await
            .err()
            .expect("the build fails closed");
        assert!(
            format!("{err:#}").contains("MCP tool discovery at startup"),
            "{err:#}"
        );

        let primary = mock_mcp(&["harbor.list_orders"]).await;
        let mut settings = embed(&primary.uri(), None);
        settings.allowed_tools = vec!["harbor.*".into()];
        let mut docs = extra("docs", &closed_port_url(), Some("docs"), &["docs.*"]);
        docs.connection.unavailable_skip_secs = Some(0);
        settings.mcp_servers = vec![docs];
        let catalogue = build(&settings).await.unwrap();
        catalogue.registry_for(&customer()).await.unwrap();
        assert!(!catalogue.servers()[1].in_skip_window());
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
        catalogue.settled().await;
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
            scope(),
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
            scope(),
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
            scope(),
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
        assert_eq!(
            call_outcome(&Ok(Err(McpError::Unreachable("refused".into())))),
            CallOutcome::Unreachable
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
                    scope(),
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
                "edge_mcp_discovery_total{{{agent},outcome=ok,phase=startup,server=default,tenant=acme}}"
            )),
            1
        );
        assert_eq!(
            count(&format!(
                "edge_mcp_discovery_total{{{agent},outcome=ok,phase=lazy,server=docs,tenant=acme}}"
            )),
            1
        );
        assert_eq!(
            count(&format!(
                "edge_mcp_discovery_total{{{agent},outcome=timed_out,phase=lazy,server=wiki,tenant=acme}}"
            )),
            1
        );
        assert_eq!(
            count(&format!(
                "edge_mcp_discovery_duration_seconds{{{agent},phase=lazy,server=wiki,tenant=acme}}"
            )),
            1
        );
        assert_eq!(
            count(&format!(
                "edge_mcp_tools_cache_total{{{agent},result=hit,server=default,tenant=acme}}"
            )),
            2
        );
        assert_eq!(
            count(&format!(
                "edge_mcp_tools_cache_total{{{agent},result=miss,server=docs,tenant=acme}}"
            )),
            1
        );
        assert_eq!(
            count(&format!(
                "edge_mcp_tools_cache_total{{{agent},result=hit,server=docs,tenant=acme}}"
            )),
            1
        );
        assert_eq!(
            count(&format!(
                "edge_mcp_skip_window_entered_total{{{agent},server=wiki,tenant=acme}}"
            )),
            1
        );
        assert_eq!(
            count(&format!(
                "edge_mcp_server_skipped_total{{{agent},server=wiki,tenant=acme}}"
            )),
            1
        );
        assert_eq!(
            count(&format!(
                "edge_mcp_token_exchanges_total{{{agent},audience=harbor-mcp,outcome=ok,tenant=acme}}"
            )),
            2,
            "the primary's audience, once per turn"
        );
        assert_eq!(
            count(&format!(
                "edge_mcp_token_exchanges_total{{{agent},audience=docs-mcp,outcome=failed,tenant=acme}}"
            )),
            2
        );
        assert_eq!(
            count(&format!(
                "edge_mcp_token_exchange_duration_seconds{{{agent},audience=docs-mcp,tenant=acme}}"
            )),
            2
        );
        assert_eq!(
            count(&format!(
                "edge_mcp_tool_calls_total{{{agent},outcome=ok,server=default,tenant=acme,tool=harbor.list_orders}}"
            )),
            1
        );
        assert_eq!(
            count(&format!(
                "edge_mcp_tool_call_duration_seconds{{{agent},server=default,tenant=acme,tool=harbor.list_orders}}"
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

        let mut data = McpToolset::empty();
        data.set_tool_prefix_for_test("data");
        let catalogue = McpCatalogue::of(vec![McpToolset::empty(), data]);
        assert_eq!(
            catalogue.local_tool_name("data.query"),
            "data.query",
            "a built-in keeps its own name"
        );
        assert_eq!(catalogue.local_tool_name("data.orders"), "orders");
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
                assert!(
                    reason.contains("refused by the outbound policy"),
                    "{reason}"
                )
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
