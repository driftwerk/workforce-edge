//! Lean single-server MCP wiring.
//!
//! An edge agent connects to exactly one company MCP server and acts against it
//! **as the logged-in customer**. The customer's token is carried by the
//! per-turn client's [`AuthProvider`], which rides the `Arc` graph
//! (`EdgeMcpTool` → `McpClient` → transport) into the runner's spawned tool
//! task — so the token reaches `tools/call` without any task-local or shared
//! `SessionContext` change. Tool schemas are discovered once and cached: at
//! startup if a service `discovery_credential` is configured, otherwise lazily
//! on the first customer turn using that customer's own token (for MCP servers
//! that accept any valid user token). Install-agnostic — nothing here is
//! specific to any one deployment.

use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant};

use std::future::Future;

use async_trait::async_trait;
use eyre::{eyre, Result, WrapErr};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use url::Url;
use workforce_agent::runner::to_llm_name;
use workforce_agent::tools::registry::ToolRegistry;
use workforce_agent::tools::{Tool, ToolResult};
use workforce_mcp_client::{
    AuthProvider, McpClient, McpError, McpLimits, McpToolDef, NoAuth, StaticBearer, ToolNameInfo,
};

use crate::agent::{allowlist_admits, EmbedSettings};
use crate::host::BUILT_IN_TOOLS;
use crate::outbound::{self, OutboundPolicy};

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

/// Run an MCP operation under a hard deadline, flattening timeout and protocol
/// errors into one contextual `Err`.
async fn deadline<T, E>(
    op: &str,
    limit: Duration,
    fut: impl Future<Output = Result<T, E>>,
) -> Result<T>
where
    E: std::fmt::Display,
{
    match tokio::time::timeout(limit, fut).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(eyre!("MCP {op} failed: {error}")),
        Err(_) => Err(eyre!("MCP {op} timed out after {}s", limit.as_secs())),
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

/// One agent's company MCP tool catalogue. The allowlisted tool defs are
/// discovered once and cached — eagerly at startup when a `discovery_credential`
/// is configured, otherwise lazily on the first customer turn with that
/// customer's token. Per-turn registries are built from the cache, each bound to
/// a single customer's token.
#[derive(Clone)]
pub struct McpToolset {
    endpoint: Url,
    /// How `endpoint` is reached. Under `Public` the shared `http` is unused:
    /// each turn's client is pinned to the address checked for that turn.
    policy: OutboundPolicy,
    http: Arc<reqwest::Client>,
    /// Optional service credential for eager startup discovery. When absent, the
    /// tools are discovered lazily with the first customer's own token.
    discovery_credential: Option<String>,
    /// Closed allowlist to filter the discovered tools by.
    allowed_tools: Vec<String>,
    /// Cached allowlisted tool defs, populated on first discovery and
    /// refreshed once older than [`MCP_DEFS_TTL`].
    defs: Arc<RwLock<Option<CachedDefs>>>,
}

/// A discovered tool list plus its age, for TTL-based refresh.
#[derive(Clone)]
struct CachedDefs {
    defs: Arc<Vec<McpToolDef>>,
    at: std::time::Instant,
}

impl CachedDefs {
    fn now(defs: Arc<Vec<McpToolDef>>) -> Self {
        Self {
            defs,
            at: std::time::Instant::now(),
        }
    }
}

// Manual Debug so the discovery credential never lands in a debug log.
impl std::fmt::Debug for McpToolset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpToolset")
            .field("endpoint", &self.endpoint)
            .field("policy", &self.policy)
            .field(
                "has_discovery_credential",
                &self.discovery_credential.is_some(),
            )
            .field("allowed_tools", &self.allowed_tools)
            .finish_non_exhaustive()
    }
}

impl McpToolset {
    /// Prepare the toolset for an agent, or `None` when the agent has no MCP
    /// configured (a pure-chat, tool-less agent). With a `discovery_credential`
    /// the tool list is fetched now (fail-closed at startup); without one,
    /// discovery is deferred to the first customer turn. Reaches the server
    /// under [`OutboundPolicy::Private`]; a tenant-supplied server goes through
    /// [`McpToolset::discover_with`].
    pub async fn discover(embed: &EmbedSettings) -> Result<Option<Self>> {
        Self::discover_with(embed, OutboundPolicy::Private).await
    }

    /// [`McpToolset::discover`] under `policy`. Under `Public` the URL is
    /// checked here, so a disallowed server fails the agent at build rather
    /// than on a customer's first turn.
    pub async fn discover_with(
        embed: &EmbedSettings,
        policy: OutboundPolicy,
    ) -> Result<Option<Self>> {
        let Some(mcp) = &embed.mcp else {
            return Ok(None);
        };
        let endpoint =
            Url::parse(&mcp.url).wrap_err_with(|| format!("invalid MCP url {:?}", mcp.url))?;
        outbound::check_url(policy, &endpoint)
            .wrap_err_with(|| format!("refusing MCP url {:?}", mcp.url))?;
        let http = Arc::new(outbound::client(
            policy,
            Duration::from_secs(MCP_REQUEST_TIMEOUT_SECS),
        ));
        let toolset = Self {
            endpoint,
            policy,
            http,
            discovery_credential: mcp.discovery_credential.clone(),
            // Discover the UNION of the base allowlist + every tool any grant
            // could unlock, so write-tier tools exist in the catalogue; run_turn
            // narrows this to each user's effective set (see host::run_turn).
            allowed_tools: embed.discoverable_tools(),
            defs: Arc::new(RwLock::new(None)),
        };
        if let Some(credential) = toolset.discovery_credential.clone() {
            let client = toolset.client_for(Arc::new(StaticBearer::new(credential)))?;
            let defs = toolset
                .discover_on(&client)
                .await
                .wrap_err("MCP tool discovery at startup")?;
            *toolset.defs.write().await = Some(CachedDefs::now(Arc::new(defs)));
        }
        Ok(Some(toolset))
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
    async fn discover_on(&self, client: &McpClient) -> Result<Vec<McpToolDef>> {
        deadline("initialize", MCP_HANDSHAKE_TIMEOUT, client.initialize()).await?;
        self.list_allowlisted(client).await
    }

    /// List the allowlisted tools on an already-initialized client.
    async fn list_allowlisted(&self, client: &McpClient) -> Result<Vec<McpToolDef>> {
        deadline(
            "tools/list",
            MCP_HANDSHAKE_TIMEOUT,
            client.tools_list_filtered(|name| self.serves(name)),
        )
        .await
    }

    /// Whether a discovered tool is served: the closed allowlist admits it, and
    /// it does not take a built-in's name on the wire.
    fn serves(&self, tool: &str) -> bool {
        if !allowlist_admits(&self.allowed_tools, tool) {
            return false;
        }
        let shadows = shadows_builtin(tool);
        if shadows {
            tracing::warn!(tool, "MCP tool shadows a built-in; dropped");
        }
        !shadows
    }

    /// Names of the discovered, allowlisted tools — best-effort snapshot for
    /// logging (empty before the first discovery).
    pub fn tool_names(&self) -> Vec<String> {
        self.defs
            .try_read()
            .ok()
            .and_then(|guard| guard.as_ref().map(|c| Arc::clone(&c.defs)))
            .map(|defs| defs.iter().map(|def| def.name.clone()).collect())
            .unwrap_or_default()
    }

    /// Backdate the cached defs past the TTL so the next `registry_for` re-lists.
    /// Returns `false` (test skips) on a platform whose monotonic clock can't
    /// represent the offset.
    #[cfg(test)]
    pub(crate) async fn force_defs_stale(&self) -> bool {
        match std::time::Instant::now().checked_sub(MCP_DEFS_TTL + Duration::from_secs(1)) {
            Some(past) => {
                if let Some(cached) = self.defs.write().await.as_mut() {
                    cached.at = past;
                }
                true
            }
            None => false,
        }
    }

    /// A toolset with no tools and no server, for tests that drive a turn
    /// through a directly-supplied registry rather than a live MCP.
    #[cfg(test)]
    pub(crate) fn empty() -> Self {
        Self {
            endpoint: Url::parse("http://unused.invalid").expect("static url"),
            policy: OutboundPolicy::Private,
            http: Arc::new(reqwest::Client::new()),
            discovery_credential: None,
            allowed_tools: Vec::new(),
            defs: Arc::new(RwLock::new(Some(CachedDefs::now(Arc::new(Vec::new()))))),
        }
    }

    /// Build a per-turn registry whose tools call the company MCP **as the
    /// given customer** — their token rides the client's auth provider into
    /// execution. A fresh client negotiates its own MCP session, keeping one
    /// customer's calls off another's session. Discovers the tool list lazily on
    /// the first turn when no startup credential was configured.
    pub async fn registry_for(&self, auth: Arc<dyn AuthProvider>) -> Result<Arc<ToolRegistry>> {
        let client = self.client_for(auth)?;
        // Snapshot the cache into an owned value so the read guard is released
        // here. Matching on `self.defs.read().await.clone()` directly would keep
        // the guard alive for the whole match, and the write arms'
        // `self.defs.write().await` would then deadlock waiting on a reader held
        // by this same task.
        let cached = self.defs.read().await.clone();
        let defs = match cached {
            Some(cached) if cached.at.elapsed() < MCP_DEFS_TTL => {
                deadline("initialize", MCP_HANDSHAKE_TIMEOUT, client.initialize()).await?;
                cached.defs
            }
            // Stale: re-list on this turn's already-required session so schema
            // changes land without a restart; on failure keep serving the stale
            // list — never degrade a working toolset over a refresh.
            Some(cached) => {
                deadline("initialize", MCP_HANDSHAKE_TIMEOUT, client.initialize()).await?;
                match self.list_allowlisted(&client).await {
                    Ok(fresh) => {
                        let fresh = Arc::new(fresh);
                        *self.defs.write().await = Some(CachedDefs::now(Arc::clone(&fresh)));
                        fresh
                    }
                    Err(e) => {
                        // Restamp so a persistent outage re-lists once per TTL,
                        // not on every turn; the stale defs keep serving.
                        tracing::warn!(error = %e, "MCP tool refresh failed; keeping the cached list");
                        *self.defs.write().await = Some(CachedDefs::now(Arc::clone(&cached.defs)));
                        cached.defs
                    }
                }
            }
            None => {
                let fetched = Arc::new(self.discover_on(&client).await?);
                *self.defs.write().await = Some(CachedDefs::now(Arc::clone(&fetched)));
                fetched
            }
        };
        let registry = ToolRegistry::new();
        for def in defs.iter() {
            registry.register(Arc::new(EdgeMcpTool {
                client: Arc::clone(&client),
                def: def.clone(),
            }));
        }
        Ok(Arc::new(registry))
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
/// and the connection pinned, exactly as `discover_with` does.
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

#[async_trait]
impl Tool for EdgeMcpTool {
    fn name(&self) -> &str {
        &self.def.name
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
            tracing::warn!(tool = %self.def.name, "edge MCP tool call auth-expired; retrying once with the live token");
            result = self.timed_call(input).await;
        }
        match result {
            Ok(Ok(output)) => Ok(ToolResult::success(output, elapsed())),
            // The message stays opaque, but the typed refusal signal survives —
            // it is the only channel this deliberately-scrubbed surface has.
            Ok(Err(McpError::RateLimited {
                retry_after_secs, ..
            })) => {
                tracing::warn!(tool = %self.def.name, "edge MCP tool call rate limited");
                Ok(ToolResult::rate_limited(
                    "the tool call did not succeed",
                    retry_after_secs,
                    elapsed(),
                ))
            }
            Ok(Err(error)) => {
                tracing::warn!(tool = %self.def.name, error = %error, "edge MCP tool call failed");
                Ok(ToolResult::failure(
                    "the tool call did not succeed",
                    elapsed(),
                ))
            }
            Err(_) => {
                tracing::warn!(tool = %self.def.name, "edge MCP tool call timed out");
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
    use crate::agent::{Branding, McpSettings};

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
            allowed_origins: vec![],
            allowed_tools: vec![],
            allowed_emails: vec![],
            keycloak_issuer: "https://kc/realms/x".into(),
            keycloak_audience: None,
            mcp: Some(McpSettings {
                url: url.into(),
                discovery_credential: credential.map(str::to_string),
            }),
            branding: Branding::default(),
            passive_memory: None,
            data_tables: None,
            charts: None,
            forms: None,
            maps: None,
            diagrams: None,
            excalidraw_export: None,
            uploads: vec![],
            upload_max_mb: None,
            drawing: None,
            inline_images: None,
            images: None,
            image_hosts: vec![],
            spill_skip_tools: vec![],
            tool_grants: vec![],
            permissions: None,
            policy: None,
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
        if let Some(past) = std::time::Instant::now().checked_sub(MCP_DEFS_TTL) {
            let stale = CachedDefs {
                defs: Arc::new(Vec::new()),
                at: past,
            };
            assert!(stale.at.elapsed() >= MCP_DEFS_TTL);
        }
    }

    #[tokio::test]
    async fn discover_without_credential_defers_to_first_turn() {
        // No credential + a valid url: discover succeeds without contacting the
        // server; the tool list is fetched lazily on the first customer turn.
        let toolset = McpToolset::discover(&embed("http://mcp.local", None))
            .await
            .unwrap()
            .expect("an mcp is configured");
        assert!(toolset.tool_names().is_empty());
    }

    #[tokio::test]
    async fn discover_returns_none_for_a_tool_less_agent() {
        // No `[mcp]` at all: a pure-chat agent, no discovery, no toolset.
        let mut e = embed("http://unused", None);
        e.mcp = None;
        assert!(McpToolset::discover(&e).await.unwrap().is_none());
    }

    /// A tenant's MCP url at a private or plain-http address is refused at
    /// build under `Public`; the same url is accepted under `Private` (the
    /// self-hosted, in-cluster case) exactly as before.
    #[tokio::test]
    async fn public_refuses_a_private_or_http_mcp_url_at_build() {
        for raw in [
            "http://mcp.example.com/mcp",
            "https://10.0.0.1/mcp",
            "https://127.0.0.1/mcp",
            "https://169.254.169.254/mcp",
            "https://[fd00::1]/mcp",
        ] {
            let err = McpToolset::discover_with(&embed(raw, None), OutboundPolicy::Public)
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains("refusing MCP url"), "{raw}: {err}");
            let toolset = McpToolset::discover_with(&embed(raw, None), OutboundPolicy::Private)
                .await
                .unwrap()
                .expect("an mcp is configured");
            assert_eq!(toolset.policy(), OutboundPolicy::Private);
        }
        // A public https literal passes the build-time check without a call.
        let toolset = McpToolset::discover_with(
            &embed("https://93.184.216.34/mcp", None),
            OutboundPolicy::Public,
        )
        .await
        .unwrap()
        .expect("an mcp is configured");
        assert_eq!(toolset.policy(), OutboundPolicy::Public);
    }

    #[tokio::test]
    async fn discover_rejects_an_invalid_url() {
        let err = McpToolset::discover(&embed("not a url", None))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid MCP url"), "got: {err}");
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
        let toolset = McpToolset::discover(&settings)
            .await
            .unwrap()
            .expect("an mcp is configured");
        assert_eq!(toolset.tool_names(), vec!["orders".to_string()]);
    }

    #[tokio::test]
    async fn deadline_bounds_a_stalled_op() {
        let stalled = std::future::pending::<Result<(), String>>();
        let err = deadline("initialize", Duration::from_millis(20), stalled)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("timed out"), "got: {err}");
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
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("tools/list") && err.contains("boom"),
            "got: {err}"
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

    /// Regression: the lazy-discovery path (`defs == None`) must release the
    /// `defs` read guard before taking the write lock. Holding it across the
    /// write self-deadlocks the RwLock, so the first customer turn hangs forever
    /// *after* a successful discover — exactly the prod symptom (initialize
    /// logged, then silence, no timeout firing because the write has none).
    #[tokio::test]
    async fn lazy_registry_for_does_not_deadlock_on_the_defs_lock() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, Request, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(BodyRpc("initialize"))
            .respond_with(|req: &Request| {
                json_rpc(
                    rpc_id(req),
                    serde_json::json!({
                        "protocolVersion": "2025-06-18",
                        "serverInfo": {"name": "fake-mcp", "version": "0.1.0"},
                        "capabilities": {"tools": {}}
                    }),
                )
            })
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(BodyRpc("notifications/initialized"))
            .respond_with(ResponseTemplate::new(202))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(BodyRpc("tools/list"))
            .respond_with(|req: &Request| {
                json_rpc(
                    rpc_id(req),
                    serde_json::json!({
                        "tools": [
                            {"name": "ping", "description": "d", "inputSchema": {"type": "object"}}
                        ]
                    }),
                )
            })
            .mount(&server)
            .await;

        let mut settings = embed(&server.uri(), None);
        settings.allowed_tools = vec!["ping".into()];
        let toolset = McpToolset::discover(&settings)
            .await
            .unwrap()
            .expect("an mcp is configured");

        // Bound each call so a re-introduced deadlock fails the test fast rather
        // than hanging the suite.
        let first = tokio::time::timeout(
            Duration::from_secs(5),
            toolset.registry_for(Arc::new(StaticBearer::new("customer-token"))),
        )
        .await
        .expect("registry_for deadlocked on the first (lazy-discovery) turn")
        .expect("discovery should succeed");
        assert!(first.has("ping"), "discovered tool should be registered");

        // Cache is now populated → the `Some` branch must also return promptly.
        let second = tokio::time::timeout(
            Duration::from_secs(5),
            toolset.registry_for(Arc::new(StaticBearer::new("customer-token"))),
        )
        .await
        .expect("registry_for deadlocked on a cached turn")
        .expect("cached discovery should succeed");
        assert!(second.has("ping"));
    }

    /// Mount initialize + notifications/initialized on `server`, so a test only
    /// has to script `tools/list`.
    async fn mount_handshake(server: &wiremock::MockServer) {
        use wiremock::matchers::method;
        use wiremock::{Mock, Request, ResponseTemplate};
        Mock::given(method("POST"))
            .and(BodyRpc("initialize"))
            .respond_with(|req: &Request| {
                json_rpc(
                    rpc_id(req),
                    serde_json::json!({
                        "protocolVersion": "2025-06-18",
                        "serverInfo": {"name": "fake-mcp", "version": "0.1.0"},
                        "capabilities": {"tools": {}}
                    }),
                )
            })
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
    /// recorded requests' `authorization` header.
    pub(crate) async fn mock_mcp(tools: &[&str]) -> wiremock::MockServer {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, Request};
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
        server
    }

    #[tokio::test]
    async fn a_stale_cache_re_lists_and_picks_up_a_new_tool() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, Request};

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
        let toolset = McpToolset::discover(&settings)
            .await
            .unwrap()
            .expect("an mcp is configured");
        assert_eq!(toolset.tool_names(), vec!["ping".to_string()]);

        if !toolset.force_defs_stale().await {
            return; // monotonic clock can't represent the offset; skip
        }
        let registry = toolset
            .registry_for(Arc::new(StaticBearer::new("customer-token")))
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
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, Request, ResponseTemplate};

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
        let toolset = McpToolset::discover(&settings)
            .await
            .unwrap()
            .expect("an mcp is configured");

        if !toolset.force_defs_stale().await {
            return;
        }
        // The refresh 500s; the turn must still get the previously-cached tool.
        let registry = toolset
            .registry_for(Arc::new(StaticBearer::new("customer-token")))
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
