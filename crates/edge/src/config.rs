//! Edge service configuration.
//!
//! The edge runs against its OWN data plane — a dedicated Postgres and a
//! dedicated vector store (`[weaviate]` or `[vellev]`, exactly one), never the
//! internal hub's. Both are mandatory: the boot connects to them fail-closed
//! (retry then abort) rather than silently degrading to a lesser store.

use std::path::Path;

use eyre::{Result, WrapErr};
use serde::{Deserialize, Serialize};
use workforce_agent::llm::provider::ProviderKind;

use crate::agent::EdgeAgent;
use crate::outbound::OutboundPolicy;

/// Top-level edge configuration, parsed from `edge.toml`.
#[derive(Debug, Clone, Deserialize)]
pub struct EdgeConfig {
    pub server: ServerConfig,
    pub postgres: PostgresConfig,
    /// The vector store, one of the two; see [`Self::memory_backend`].
    #[serde(default)]
    pub weaviate: Option<WeaviateConfig>,
    #[serde(default)]
    pub vellev: Option<VellevConfig>,
    pub llm: LlmConfig,
    /// The built-in manual test page (`/demo/{agent}`). Off by default; enable
    /// only on test installs.
    #[serde(default)]
    pub demo: DemoConfig,
    /// The customer-facing agents this install serves.
    #[serde(default)]
    pub agents: Vec<EdgeAgent>,
    /// Directory of playbook TOMLs (guided customer flows) agents may reference
    /// via `playbook = "<id>"`. Absent ⇒ no playbooks are loaded.
    #[serde(default)]
    pub playbooks_dir: Option<String>,
    /// Operator auth for the admin surface (`/admin`). Absent ⇒ every admin
    /// route is 404.
    #[serde(default)]
    pub admin: Option<crate::admin::AdminConfig>,
    /// Git-backed config (opt-in). When set, agent definitions, playbooks, and
    /// training data are loaded from a GitHub repo (the source of truth) on every
    /// boot instead of being DB-authoritative, and the admin UI goes read-only for
    /// those items (appearance stays editable via a DB overlay). Absent ⇒ the
    /// existing DB-authoritative behaviour, so other installs are unaffected.
    #[serde(default)]
    pub git_config: Option<GitConfig>,
    /// OAuth2 Token Exchange (RFC 8693): when set, the edge — as this
    /// confidential Keycloak client — exchanges each customer's token for a
    /// fresh (optionally audience-scoped) downstream token for every turn's MCP
    /// calls, instead of forwarding the customer's raw token. Install-level and
    /// opt-in: absent ⇒ exact pass-through as before (non-Keycloak installs are
    /// unaffected). The client must belong to the same realm as the agents'
    /// `keycloak_issuer` and have the "Standard token exchange" capability
    /// (Keycloak ≥ 26.2).
    #[serde(default)]
    pub token_exchange: Option<TokenExchangeConfig>,
    /// Automated answer-quality grader (opt-in). Absent ⇒ off, so other installs
    /// are unaffected.
    #[serde(default)]
    pub grader: Option<GraderConfig>,
    /// Retention of each turn's tool evidence for after-the-fact auditing
    /// (opt-in). Absent ⇒ off, so no install starts storing customer data
    /// because of an upgrade.
    #[serde(default)]
    pub audit: Option<AuditConfig>,
    /// File-artifact storage (opt-in) — enables the `export_xlsx` built-in and
    /// its authenticated download route. Absent ⇒ off, so other installs are
    /// unaffected.
    #[serde(default)]
    pub artifacts: Option<ArtifactsConfig>,
    /// The platform surface (`/platform`): the operators of the deployment
    /// itself, who create, suspend and rescue tenants. Absent ⇒ every
    /// platform route is 404, so a self-hosted install exposes nothing.
    /// Meaningful with `server.multi_tenant`; allowed without it.
    #[serde(default)]
    pub platform: Option<crate::platform::PlatformConfig>,
}

/// `[git_config]` — the GitHub repo that holds agent definitions, playbooks, and
/// training data. Fetched as a zipball via the API (no `git` binary needed), so
/// the token needs only `contents: read` on this one repo.
#[derive(Clone, Deserialize)]
pub struct GitConfig {
    /// `owner/repo`, e.g. `harborlabs/workforce-edge-config`.
    pub repo: String,
    /// Branch, tag, or commit SHA to load. Defaults to `main`.
    #[serde(default = "default_git_ref")]
    pub git_ref: String,
    /// Fine-grained PAT (`${ENV}`-injected) with `contents: read` on `repo`.
    pub token: String,
    /// How often (seconds) to poll the repo and hot-apply changes without a
    /// restart. Default 120; `0` disables the poll (sync only via the admin
    /// button or a restart). A HEAD-SHA check makes an unchanged poll cheap.
    #[serde(default = "default_git_poll_secs")]
    pub poll_secs: u64,
}

fn default_git_poll_secs() -> u64 {
    120
}

fn default_git_ref() -> String {
    "main".to_string()
}

/// Manual `Debug` so an accidental `{:?}` of the config can't print the token.
impl std::fmt::Debug for GitConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitConfig")
            .field("repo", &self.repo)
            .field("git_ref", &self.git_ref)
            .field("token", &"<redacted>")
            .finish()
    }
}

/// `[token_exchange]` — the edge's confidential Keycloak client. The secret is
/// injected via `${ENV}` expansion, or held sealed in the tenant's settings
/// row, and lives only in process memory: it is never serialised, so neither
/// the settings row's JSON nor the admin API carries it.
#[derive(Clone, Serialize, Deserialize)]
pub struct TokenExchangeConfig {
    /// The realm issuer this client belongs to — the exchange endpoint is built
    /// from THIS pinned value, never from per-agent (admin-editable) config, so
    /// an agent edit can never redirect the client secret + customer tokens to
    /// an arbitrary host. Agents whose `keycloak_issuer` doesn't match are
    /// skipped (pass-through) with a warning at host build.
    pub issuer: String,
    pub client_id: String,
    /// Empty when the file omits it, which the runtime refuses at build; a
    /// settings row supplies it from its sealed column instead.
    #[serde(default, skip_serializing)]
    pub client_secret: String,
    /// The MCP server's Keycloak clientId; when set, the exchanged token is
    /// audience-scoped to it. Absent ⇒ the token still gains a fresh TTL, just
    /// without the audience restriction.
    #[serde(default)]
    pub audience: Option<String>,
    /// Space-separated scopes to request on the exchanged token. Keycloak
    /// mints it for this client and includes `openid` only when asked, and its
    /// `userinfo` endpoint refuses a token without `openid` — so an MCP server
    /// that checks tokens against `userinfo` needs `scope = "openid"`.
    #[serde(default)]
    pub scope: Option<String>,
}

/// Manual `Debug` so an accidental `{:?}` of the config can never print the
/// client secret.
impl std::fmt::Debug for TokenExchangeConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenExchangeConfig")
            .field("issuer", &self.issuer)
            .field("client_id", &self.client_id)
            .field("client_secret", &"<redacted>")
            .field("audience", &self.audience)
            .field("scope", &self.scope)
            .finish()
    }
}

/// The `/demo/{agent}` manual test page: a host page that signs the tester in
/// with Google and hands the id_token to the embedded widget — a stand-in for a
/// real customer webapp. Serves nothing unless explicitly enabled, so a
/// production install never exposes it by accident.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DemoConfig {
    /// Master switch; `false` (default) ⇒ `/demo/{agent}` is 404.
    #[serde(default)]
    pub enabled: bool,
    /// Google OAuth client id used by the page's Sign-in-with-Google button.
    /// Public by design (it ships to every browser); required when enabled.
    #[serde(default)]
    pub google_client_id: Option<String>,
}

/// LLM provider selection, credentials, and tier resolution.
#[derive(Clone, Serialize, Deserialize)]
pub struct LlmConfig {
    /// Provider id: `anthropic` (default), `bedrock`, `gemini`, `openrouter`, …
    #[serde(default = "default_provider")]
    pub provider: String,
    /// API key / credential. Optional for providers that use ambient auth —
    /// Bedrock reads the task's AWS role, so leave it unset there. Never
    /// serialised: a settings row keeps it sealed in its own column.
    #[serde(default, skip_serializing)]
    pub api_key: Option<String>,
    /// Providers a tenant may run on. Empty (the default) leaves the choice
    /// unrestricted, as a self-hosted install expects; a cloud deployment pins
    /// the bearer-key providers, since Ollama takes a URL in the key's place
    /// and Bedrock would run on the deployment's own role. Install policy:
    /// read from the file alone, never from a tenant's settings row.
    #[serde(default)]
    pub allowed_providers: Vec<String>,
    /// Tier-resolution family (e.g. `bedrock.anthropic`, `anthropic`). Defaults
    /// from the provider, and lets an agent's `model = "powerful"` map to the
    /// right concrete model for this provider.
    #[serde(default)]
    pub tier_key: Option<String>,
    /// Default model when an agent does not pin its own — a tier (`powerful` /
    /// `standard` / `cheap`) or a concrete model id.
    #[serde(default)]
    pub default_model: Option<String>,
    /// Ordered fallback models to try when the primary model fails (same
    /// grammar as the hub's `[llm] failover`). Empty ⇒ no fallback chain —
    /// transient errors still retry on the primary model.
    #[serde(default)]
    pub failover: Vec<String>,
    /// Retry policy for transient LLM errors (5xx, timeouts). Absent ⇒
    /// defaults: 3 retries with 1s/2s/4s exponential backoff.
    #[serde(default)]
    pub retry: workforce_agent::llm::failover::RetryConfig,
}

fn default_provider() -> String {
    "anthropic".to_string()
}

/// Manual `Debug` so an accidental `{:?}` of the config can never print the
/// API key.
impl std::fmt::Debug for LlmConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmConfig")
            .field("provider", &self.provider)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("allowed_providers", &self.allowed_providers)
            .field("tier_key", &self.tier_key)
            .field("default_model", &self.default_model)
            .field("failover", &self.failover)
            .field("retry", &self.retry)
            .finish()
    }
}

impl LlmConfig {
    /// Whether `provider` is one the install allows: any provider when
    /// `allowed_providers` is empty, else one that names the same provider as
    /// an entry. An unknown provider is refused the same way, with the field
    /// named so the operator knows where to look.
    pub fn provider_allowed(&self) -> Result<()> {
        if self.allowed_providers.is_empty() {
            return Ok(());
        }
        let kind = ProviderKind::from_str_loose(&self.provider);
        let allowed = self
            .allowed_providers
            .iter()
            .any(|name| kind.is_some() && ProviderKind::from_str_loose(name) == kind);
        if allowed {
            Ok(())
        } else {
            Err(eyre::eyre!(
                "llm provider {:?} is not in [llm] allowed_providers ({})",
                self.provider,
                self.allowed_providers.join(", ")
            ))
        }
    }
}

/// `[audit]` — optional retention of the tool evidence behind each answer.
///
/// Without it an answer's grounding cannot be checked after the fact: the
/// transcript keeps the reply and nothing about the calls that produced it, so
/// "did it invent that figure?" is unanswerable once the turn ends.
///
/// OFF by default, deliberately. Tool results are customer data — leads, orders,
/// accounts, names — and this stores a second copy of them outside the transcript.
/// Turning that on is an install's decision, not an upgrade's side effect. What
/// is stored is already redacted (`redact_secrets`) and bounded to the same
/// per-turn budget the grader sees, and it is swept after `retention_days`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditConfig {
    /// Master switch. An absent `[audit]` section ⇒ off; `enabled = false` ⇒ off.
    #[serde(default)]
    pub enabled: bool,
    /// How long evidence is kept before the sweeper drops it. Short by default:
    /// this exists to explain a recent answer, not to build a data lake.
    #[serde(default = "default_audit_retention_days")]
    pub retention_days: u32,
}

fn default_audit_retention_days() -> u32 {
    14
}

/// `[grader]` — optional automated answer-quality grader. Absent ⇒ off. When
/// enabled, each terminal turn is graded off the critical path (after the reply
/// is sent) into `edge_analytics.answer_grades`, so operators can see what the
/// agent answers well, badly, or not at all — and mine the gaps for training.
#[derive(Clone, Serialize, Deserialize)]
pub struct GraderConfig {
    /// Master switch. An absent `[grader]` section ⇒ off; `enabled = false` ⇒ off.
    #[serde(default)]
    pub enabled: bool,
    /// Grader model, in the same grammar every model field accepts: a tier
    /// (`cheap` / `standard` / `powerful`), a concrete id (`claude-sonnet-4-6`),
    /// or a routed spec (`anthropic:powerful`, `gemini:standard`,
    /// `openrouter:anthropic:standard`). Absent ⇒ the agent's own model.
    ///
    /// COST: the default (the agent's model) is often Sonnet/Opus, and grading
    /// runs ONE extra LLM call per graded turn — so with `sample_rate = 1.0` a
    /// busy agent roughly doubles its LLM spend. Set `model = "cheap"` and/or a
    /// lower `sample_rate` to bound it.
    ///
    /// A spec that routes to a provider other than the agent's requires `api_key`
    /// and must be fully configured for that provider (e.g. Vertex also needs its
    /// project/location env) — an incomplete cross-provider provider fails host
    /// boot, same as the agent's own provider.
    #[serde(default)]
    pub model: Option<String>,
    /// Credential for the grader's provider — required only when `model` routes to
    /// a provider other than the agent's (the agent's Bedrock uses the task role,
    /// so same-provider grading needs nothing here). `${ENV}`-injected like other
    /// secrets, and like them never serialised.
    #[serde(default, skip_serializing)]
    pub api_key: Option<String>,
    /// Fraction (0.0..=1.0) of `Ok` turns to LLM-grade — the main cost lever (see
    /// `model`). A `max_rounds` turn gets a free deterministic verdict regardless;
    /// out-of-range values are clamped by the grader (>=1 grades all, <=0 none).
    /// Default 1.0 (grade every answered turn).
    #[serde(default = "default_grader_sample_rate")]
    pub sample_rate: f64,
}

fn default_grader_sample_rate() -> f64 {
    1.0
}

/// Manual `Debug` so an accidental `{:?}` of the config can never print the
/// grader's key.
impl std::fmt::Debug for GraderConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GraderConfig")
            .field("enabled", &self.enabled)
            .field("model", &self.model)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("sample_rate", &self.sample_rate)
            .finish()
    }
}

/// `[artifacts]` — filesystem store for files the agent generates (e.g. the
/// spreadsheets `export_xlsx` produces), served to the widget through an
/// authenticated download route. `root` MUST be storage shared across all edge
/// replicas (a bind-mounted volume): a file one replica writes is fetched by a
/// separate HTTP request that may land on another.
#[derive(Debug, Clone, Deserialize)]
pub struct ArtifactsConfig {
    /// Directory the store owns; created at boot if absent.
    pub root: String,
    /// How long a generated file stays downloadable before the sweeper purges
    /// it. Default 24h.
    #[serde(default = "default_artifact_ttl_secs")]
    pub ttl_secs: u64,
}

fn default_artifact_ttl_secs() -> u64 {
    24 * 60 * 60
}

/// Public-surface HTTP server settings.
#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    /// Address the public + admin router binds to, e.g. `0.0.0.0:8080`.
    pub bind: String,
    /// Proxy hops trusted to set `X-Forwarded-For`, so per-IP rate limiting
    /// keys on the real client address rather than the proxy's.
    #[serde(default)]
    pub trusted_proxies: Vec<String>,
    /// Per-IP public request budget per minute.
    ///
    /// A FLOOD GUARD, not a fair-use limit: it is checked before any crypto so an
    /// unauthenticated spray cannot burn signature-verifies, and a whole office
    /// behind one NAT gateway shares a single address here. Size it for the
    /// busiest single egress IP you expect, and bound one customer's own usage
    /// with `user_rate_limit_per_min` instead.
    #[serde(default = "default_rate_limit_per_min")]
    pub rate_limit_per_min: u32,
    /// Per-signed-in-user turn budget per minute, checked after the caller's
    /// token is verified.
    ///
    /// This is the fair-use control. Keying it on the user rather than the
    /// address is what lets a shared office IP carry many people without any of
    /// them starving the others.
    #[serde(default = "default_user_rate_limit_per_min")]
    pub user_rate_limit_per_min: u32,
    /// Ceiling on agent turns running concurrently across all sockets, so a
    /// flood of connections can't spawn unbounded runs.
    ///
    /// COUPLED TO `[postgres] pool_size` — keep it below the pool, or turns
    /// starve each other on a DB connection. Raise the two together and check
    /// the server's own `max_connections` first: this value is PER REPLICA, so
    /// the real demand on the database is `pool_size × replicas`, and on a
    /// cluster shared with other services that is a shared budget, not a spare
    /// one. A turn also borrows briefly outside its own work — session saves,
    /// analytics, the turn-registry claim/release and its cancel poll — so the
    /// pool absorbs more than one connection per concurrent turn.
    #[serde(default = "default_max_concurrent_turns")]
    pub max_concurrent_turns: usize,
    /// Serve every tenant in the registry, each on its registered hostnames,
    /// instead of the one `default` tenant on any host. Off by default: a
    /// self-hosted install is one tenant configured by this file. Requires
    /// `[vellev]` (Weaviate has no tenant boundary) and a database role the
    /// row-level-security policies bind.
    #[serde(default)]
    pub multi_tenant: bool,
    /// How a tenant's own URLs — OIDC issuer and `jwks_uri`, the admin IdP,
    /// MCP, permissions and token-exchange endpoints — are reached when its
    /// settings row names no policy of its own (and for `default`, which the
    /// file configures). `private` (the default) is the self-hosted shape:
    /// in-cluster hosts over plain `http`. `public` is the cloud's: `https`
    /// only, no private or metadata address, every redirect re-checked. A
    /// stored `outbound` on the tenant's settings row overrides this.
    #[serde(default)]
    pub default_outbound: OutboundPolicy,
}

/// The edge's own Postgres — customer sessions live here.
#[derive(Debug, Clone, Deserialize)]
pub struct PostgresConfig {
    /// Connection string for the edge's dedicated database.
    pub url: String,
    /// TLS mode: `require` (default — the edge talks to a managed Postgres over
    /// the network) or `disable` (local plaintext dev only).
    #[serde(default = "default_sslmode")]
    pub sslmode: String,
    /// Seconds to keep retrying the initial connection before aborting boot.
    #[serde(default = "default_connect_budget_secs")]
    pub connect_budget_secs: u64,
}

/// The edge's own Weaviate — the customer-facing training corpus and agent
/// memory live here.
#[derive(Debug, Clone, Deserialize)]
pub struct WeaviateConfig {
    /// Base URL of the edge's dedicated Weaviate instance.
    pub url: String,
    /// Optional API key, when the instance is authenticated.
    #[serde(default)]
    pub api_key: Option<String>,
    /// Seconds to keep retrying readiness before aborting boot.
    #[serde(default = "default_connect_budget_secs")]
    pub connect_budget_secs: u64,
}

/// The edge's own vellev tenant — the alternative to `[weaviate]`. The node
/// embeds for the edge; the model defaults to the one Weaviate's
/// `text2vec-transformers` ran, which every recall threshold in this crate is
/// tuned on, so an install moved from Weaviate recalls as it did.
#[derive(Debug, Clone, Deserialize)]
pub struct VellevConfig {
    /// A vellev node's HTTP URL.
    pub url: String,
    /// The key presented to the node; it reaches `tenant` alone.
    pub api_key: String,
    /// The tenant this edge's memory and corpus live in.
    pub tenant: String,
    /// The model the node runs for this edge's collection.
    #[serde(default = "default_vellev_model")]
    pub model: String,
    /// How many dimensions `model` produces.
    #[serde(default = "default_vellev_dimensions")]
    pub dimensions: u32,
    /// Seconds to keep retrying readiness before aborting boot.
    #[serde(default = "default_connect_budget_secs")]
    pub connect_budget_secs: u64,
}

/// Which vector store an install named.
#[derive(Debug, Clone, Copy)]
pub enum MemoryBackend<'a> {
    Weaviate(&'a WeaviateConfig),
    Vellev(&'a VellevConfig),
}

fn default_vellev_model() -> String {
    // fastembed's name for the full-precision all-MiniLM-L6-v2; vellev matches a
    // local model on the last path segment, and this one is unambiguous where
    // `sentence-transformers/all-MiniLM-L6-v2` would also match the quantised
    // copy.
    "Qdrant/all-MiniLM-L6-v2-onnx".to_string()
}

fn default_vellev_dimensions() -> u32 {
    384
}

fn default_rate_limit_per_min() -> u32 {
    // Sized for a shared egress: one office reaching us through a single NAT
    // address is the normal case, not an attack.
    600
}

fn default_user_rate_limit_per_min() -> u32 {
    60
}

fn default_max_concurrent_turns() -> usize {
    8
}

fn default_sslmode() -> String {
    "require".to_string()
}

fn default_connect_budget_secs() -> u64 {
    300
}

impl EdgeConfig {
    /// Parse an [`EdgeConfig`] from a TOML file on disk. `${VAR}` references are
    /// expanded from the process environment first, so a config baked into the
    /// image can carry secrets as `url = "${EDGE_PG_URL}"` and have them injected
    /// at runtime from SSM-backed task-definition env vars.
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .wrap_err_with(|| format!("reading edge config {}", path.display()))?;
        let expanded = workforce_common::config_expand::expand_env_vars(&raw);
        let config: Self = toml::from_str(&expanded)
            .wrap_err_with(|| format!("parsing edge config {}", path.display()))?;
        config
            .memory_backend()
            .wrap_err_with(|| format!("parsing edge config {}", path.display()))?;
        Ok(config)
    }

    /// The first secret the file carries that a deployment serving many
    /// tenants must not, by field name: a key or client secret in the file
    /// would be the `default` tenant's alone to inherit, and the file of a
    /// cloud deployment configures no tenant of its own. Every tenant's
    /// secrets live sealed in its settings row instead.
    pub fn shared_file_secret(&self) -> Option<&'static str> {
        let set = |secret: Option<&str>| secret.is_some_and(|s| !s.trim().is_empty());
        if set(self.llm.api_key.as_deref()) {
            return Some("[llm] api_key");
        }
        if self
            .token_exchange
            .as_ref()
            .is_some_and(|te| set(Some(&te.client_secret)))
        {
            return Some("[token_exchange] client_secret");
        }
        if self
            .admin
            .as_ref()
            .and_then(|admin| admin.oidc.as_ref())
            .is_some_and(|oidc| set(oidc.client_secret.as_deref()))
        {
            return Some("[admin.oidc] client_secret");
        }
        None
    }

    /// The vector store this install runs against. Exactly one of `[weaviate]`
    /// and `[vellev]` must be set: neither leaves the edge without memory, and
    /// both would leave which one holds the corpus to chance.
    pub fn memory_backend(&self) -> Result<MemoryBackend<'_>> {
        match (&self.weaviate, &self.vellev) {
            (Some(weaviate), None) => Ok(MemoryBackend::Weaviate(weaviate)),
            (None, Some(vellev)) => Ok(MemoryBackend::Vellev(vellev)),
            (None, None) => Err(eyre::eyre!("one of [weaviate] or [vellev] is required")),
            (Some(_), Some(_)) => Err(eyre::eyre!(
                "[weaviate] and [vellev] are both set; the edge uses exactly one vector store"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn parses_minimal_config_with_defaults() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"
            [server]
            bind = "0.0.0.0:8080"

            [postgres]
            url = "postgres://edge/edge"

            [weaviate]
            url = "http://weaviate:8080"

            [llm]
            api_key = "sk-test"
            "#
        )
        .unwrap();

        let config = EdgeConfig::load(file.path()).unwrap();
        assert_eq!(config.server.bind, "0.0.0.0:8080");
        assert_eq!(config.server.rate_limit_per_min, 600);
        assert_eq!(config.server.user_rate_limit_per_min, 60);
        assert_eq!(config.postgres.connect_budget_secs, 300);
        let MemoryBackend::Weaviate(weaviate) = config.memory_backend().unwrap() else {
            panic!("a [weaviate] section names Weaviate");
        };
        assert!(weaviate.api_key.is_none());
        assert_eq!(config.llm.provider, "anthropic");
        assert_eq!(config.llm.api_key.as_deref(), Some("sk-test"));
        assert!(config.agents.is_empty());
    }

    fn config_from(body: &str) -> Result<EdgeConfig> {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(body.as_bytes()).unwrap();
        EdgeConfig::load(file.path())
    }

    const WITHOUT_VECTOR_STORE: &str = r#"
        [server]
        bind = "0.0.0.0:8080"

        [postgres]
        url = "postgres://edge/edge"

        [llm]
        api_key = "sk-test"
    "#;

    #[test]
    fn a_vellev_section_names_vellev_and_defaults_to_the_weaviate_era_model() {
        let config = config_from(&format!(
            "{WITHOUT_VECTOR_STORE}\n[vellev]\nurl = \"http://vellev:18080\"\napi_key = \"k\"\ntenant = \"acme\""
        ))
        .unwrap();
        let MemoryBackend::Vellev(vellev) = config.memory_backend().unwrap() else {
            panic!("a [vellev] section names vellev");
        };
        assert_eq!(vellev.tenant, "acme");
        assert_eq!(vellev.model, "Qdrant/all-MiniLM-L6-v2-onnx");
        assert_eq!(vellev.dimensions, 384);
        assert_eq!(vellev.connect_budget_secs, 300);
    }

    #[test]
    fn a_vellev_tenant_needs_its_key() {
        let err = config_from(&format!(
            "{WITHOUT_VECTOR_STORE}\n[vellev]\nurl = \"http://vellev:18080\"\ntenant = \"acme\""
        ))
        .unwrap_err();
        assert!(format!("{err:#}").contains("api_key"), "{err:#}");
    }

    #[test]
    fn exactly_one_vector_store_is_required() {
        let neither = config_from(WITHOUT_VECTOR_STORE).unwrap_err();
        assert!(
            format!("{neither:#}").contains("one of [weaviate] or [vellev]"),
            "{neither:#}"
        );

        let both = config_from(&format!(
            "{WITHOUT_VECTOR_STORE}\n[weaviate]\nurl = \"http://weaviate:8080\"\n\n[vellev]\nurl = \"http://vellev:18080\"\napi_key = \"k\"\ntenant = \"acme\""
        ))
        .unwrap_err();
        assert!(format!("{both:#}").contains("both set"), "{both:#}");
    }

    fn config_with(extra: &str) -> EdgeConfig {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"
            [server]
            bind = "0.0.0.0:8080"

            [postgres]
            url = "postgres://edge/edge"

            [weaviate]
            url = "http://weaviate:8080"

            [llm]
            api_key = "sk-test"

            {extra}
            "#
        )
        .unwrap();
        EdgeConfig::load(file.path()).unwrap()
    }

    /// Storing tool evidence is a second copy of customer data, so an upgrade
    /// must never switch it on by itself.
    #[test]
    fn auditing_is_off_unless_an_install_asks_for_it() {
        assert!(config_with("").audit.is_none(), "absent section ⇒ off");

        let disabled = config_with("[audit]\n            enabled = false")
            .audit
            .expect("section present");
        assert!(!disabled.enabled, "an explicit false stays off");
    }

    #[test]
    fn enabled_auditing_keeps_a_bounded_default_retention() {
        let audit = config_with("[audit]\n            enabled = true")
            .audit
            .expect("section present");
        assert!(audit.enabled);
        assert_eq!(
            audit.retention_days, 14,
            "an install that enables auditing without naming a retention still gets one"
        );

        let explicit =
            config_with("[audit]\n            enabled = true\n            retention_days = 3")
                .audit
                .expect("section present");
        assert_eq!(explicit.retention_days, 3);
    }

    #[test]
    fn missing_data_plane_section_is_an_error() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(file, "[server]\nbind = \"0.0.0.0:8080\"").unwrap();
        assert!(EdgeConfig::load(file.path()).is_err());
    }

    #[test]
    fn expands_env_var_references_from_the_environment() {
        std::env::set_var("EDGE_TEST_PG_URL", "postgres://real/db");
        let content = r#"
[server]
bind = "0.0.0.0:8080"

[postgres]
url = "${EDGE_TEST_PG_URL}"

[weaviate]
url = "http://weaviate:8080"

[llm]
provider = "bedrock"
"#;
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(content.as_bytes()).unwrap();
        let config = EdgeConfig::load(file.path()).unwrap();
        assert_eq!(config.postgres.url, "postgres://real/db");
        std::env::remove_var("EDGE_TEST_PG_URL");
    }
}
