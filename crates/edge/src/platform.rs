//! The platform surface: the deployment's own operators, who create,
//! suspend, rescue and delete tenants.
//!
//! A third auth tier beside customers ([`crate::keycloak`]) and tenant
//! admins ([`crate::admin`]). Platform operators sign in through the
//! product's own OIDC realm with the same Authorization Code + PKCE flow
//! the admin surface runs, against a file-only allowlist — a handful of
//! people, never a table a tenant could reach. Their sessions carry the
//! platform role under a cookie of their own, scoped to `/platform`, and
//! the platform routes answer only on the platform's own hostname (the host
//! of `[platform.oidc] public_url`), so a tenant's hostname never exposes
//! them. Absent `[platform]`, every route here is 404 and a self-hosted
//! install exposes nothing.
//!
//! Superadmin by design: a platform operator can open any tenant's admin
//! through a rescue session (`POST /platform/api/tenants/{id}/rescue`), as
//! a hub superadmin can. The tenant's audit trail records it.
//!
//! Every action the API takes on a tenant is also written to the platform's
//! own log (`platform_audit`, [`PlatformAuditStore`]), by the principal that
//! took it — an operator's email or `service:<name>` — and read back through
//! `GET /platform/api/audit`.
//!
//! The staff's console is a SPA (`crates/edge/ui-platform`) built into the
//! binary with `rust-embed`, as the tenant admin's is, and served under
//! `/platform/` on the platform host alone. The shell is public — it renders
//! a sign-in button until `/platform/api/me` answers — and every figure it
//! shows comes through the guarded API.
//!
//! Also here: `GET /internal/tls/ask`, the question Caddy's on-demand TLS
//! puts to the edge before issuing a certificate for a name. Unauthenticated
//! by design and answered from the hostname cache alone — never Postgres —
//! and refused when the request came through a public site rather than the
//! ingress's loopback gate.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use eyre::{Result, WrapErr};
use rust_embed::RustEmbed;
use serde::{Deserialize, Serialize};
use tokio_postgres::error::SqlState;
use workforce_common::postgres::PostgresPool;

use crate::admin::{AdminAuth, AdminConfig, AdminOidc};
use crate::boot::SessionRoot;
use crate::config_store::{EdgeAuditStore, EdgeOperatorStore, Operator};
use crate::ingress::{client_ip, request_host, RateLimiter};
use crate::outbound::OutboundPolicy;
use crate::server::EdgeState;
use crate::settings::LlmStatus;
use crate::tenant::{Plan, Resolved, ServedTenant, Tenant, TenantId, TenantStatus, Usage};

/// The `[platform]` config block. Its presence enables the platform surface.
#[derive(Clone, Deserialize)]
pub struct PlatformConfig {
    /// The product's own OIDC realm, where platform operators sign in.
    pub oidc: PlatformOidc,
    /// Platform staff, by exact email. File-only: there is no table of
    /// platform operators to administer.
    #[serde(default)]
    pub allowed_emails: Vec<String>,
    /// Platform staff, by verified email domain.
    #[serde(default)]
    pub allowed_domains: Vec<String>,
    /// Session-signing secret (≥32 bytes). Unset ⇒ a random per-boot key:
    /// restarts sign every platform operator out.
    #[serde(default)]
    pub session_secret: Option<String>,
    /// Session lifetime in hours.
    #[serde(default = "default_session_hours")]
    pub session_hours: u64,
    /// Services that call the platform API with a bearer token instead of
    /// an operator's session — the self-serve billing service creating
    /// workspaces. Platform-level by configuration: a token's requests skip
    /// the staff allowlist and the CSRF rule, and act as `service:<name>`.
    /// `service_tokens = [{ name = "billing", token = "${PLATFORM_SERVICE_TOKEN_BILLING}" }]`.
    #[serde(default)]
    pub service_tokens: Vec<ServiceToken>,
}

/// One `[platform] service_tokens` entry: a caller's name and the bearer
/// token it presents, at least [`MIN_SERVICE_TOKEN_LEN`] characters, as the
/// `${ENV}` expansion of edge.toml supplies it.
#[derive(Clone, Deserialize)]
#[serde(try_from = "RawServiceToken")]
pub struct ServiceToken {
    pub name: String,
    pub token: String,
}

/// The shortest bearer token `service_tokens` accepts.
pub const MIN_SERVICE_TOKEN_LEN: usize = 32;

/// A `service_tokens` entry as the file spells it, before the checks.
#[derive(Deserialize)]
struct RawServiceToken {
    name: String,
    token: String,
}

impl TryFrom<RawServiceToken> for ServiceToken {
    type Error = String;

    fn try_from(raw: RawServiceToken) -> std::result::Result<Self, Self::Error> {
        let name = raw.name.trim().to_string();
        if name.is_empty() {
            return Err("[platform] service_tokens: a token needs a name".to_string());
        }
        if raw.token.chars().count() < MIN_SERVICE_TOKEN_LEN {
            return Err(format!(
                "[platform] service_tokens: the token of {name:?} must be at least \
                 {MIN_SERVICE_TOKEN_LEN} characters"
            ));
        }
        Ok(Self {
            name,
            token: raw.token,
        })
    }
}

/// Manual `Debug` so an accidental `{:?}` can't print the token.
impl std::fmt::Debug for ServiceToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServiceToken")
            .field("name", &self.name)
            .field("token", &"<redacted>")
            .finish()
    }
}

/// `[platform.oidc]`: the Authorization Code + PKCE client platform
/// operators sign in with.
#[derive(Clone, Debug, Deserialize)]
pub struct PlatformOidc {
    pub issuer: String,
    /// Expected audience; defaults to `client_id`.
    #[serde(default)]
    pub audience: Option<String>,
    pub client_id: String,
    /// The platform's public base URL, e.g. `https://platform.marrilo.com`:
    /// the exact `redirect_uri` the IdP registered, and the one host the
    /// platform routes answer on.
    pub public_url: String,
}

fn default_session_hours() -> u64 {
    12
}

/// Manual `Debug` so an accidental `{:?}` of the config can't print the
/// session secret.
impl std::fmt::Debug for PlatformConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlatformConfig")
            .field("oidc", &self.oidc)
            .field("allowed_emails", &self.allowed_emails)
            .field("allowed_domains", &self.allowed_domains)
            .field(
                "session_secret",
                &self.session_secret.as_ref().map(|_| "<redacted>"),
            )
            .field("session_hours", &self.session_hours)
            .field("service_tokens", &self.service_tokens)
            .finish()
    }
}

/// How long a rescue code may be exchanged for the tenant's admin cookie.
const RESCUE_TTL: Duration = Duration::from_secs(60);

/// A minted rescue session, waiting to be exchanged once on its tenant's
/// host.
struct Rescue {
    tenant: TenantId,
    /// The tenant's admin session token, signed by its own authenticator.
    token: String,
    expires: Instant,
}

/// The platform surface's running state, built once from `[platform]`.
pub struct PlatformState {
    auth: AdminAuth,
    /// The lowercase host of `public_url`: the one host the routes answer on.
    host: String,
    /// The origin of `public_url` (`scheme://host[:port]`): the one `Origin`
    /// a state-changing request may carry.
    origin: String,
    allowed_emails: Vec<String>,
    allowed_domains: Vec<String>,
    /// The configured services by name, each with the SHA-256 of its bearer
    /// token: a presented token is hashed and compared digest to digest in
    /// constant time, so the comparison leaks neither length nor prefix.
    service_tokens: Vec<(String, TokenDigest)>,
    /// How the platform IdP is reached: the install's default policy, since
    /// the realm is ours, not a tenant's.
    outbound: OutboundPolicy,
    /// A tight per-IP budget for the sign-in endpoints.
    login_limiter: RateLimiter,
    /// Rescue codes by their one-time value, in memory: a code is minted and
    /// exchanged on one process, and a restart losing one costs a click.
    rescues: Mutex<HashMap<String, Rescue>>,
}

impl PlatformState {
    /// Build the platform surface. Its session key follows the tenants'
    /// rule: `[platform].session_secret` when set, else one derived from
    /// the install's `session_root` (the master key's), else random per
    /// boot — so with a master key the staff stay signed in across restarts
    /// without a secret of their own in the file.
    pub fn new(
        config: &PlatformConfig,
        outbound: OutboundPolicy,
        session_root: Option<&SessionRoot>,
    ) -> Result<Self> {
        let allowed_emails: Vec<String> = config
            .allowed_emails
            .iter()
            .map(|e| e.trim().to_lowercase())
            .filter(|e| !e.is_empty())
            .collect();
        let allowed_domains: Vec<String> = config
            .allowed_domains
            .iter()
            .map(|d| d.trim().trim_start_matches('@').to_lowercase())
            .filter(|d| !d.is_empty())
            .collect();
        if allowed_emails.is_empty() && allowed_domains.is_empty() {
            eyre::bail!(
                "[platform] needs allowed_emails or allowed_domains: nobody could sign in otherwise"
            );
        }
        let public_url = url::Url::parse(config.oidc.public_url.trim())
            .ok()
            .filter(|url| url.host_str().is_some())
            .ok_or_else(|| {
                eyre::eyre!(
                    "[platform.oidc] public_url must be the platform's base URL, e.g. \
                     https://platform.example.com"
                )
            })?;
        let host = public_url
            .host_str()
            .expect("checked above")
            .to_ascii_lowercase();
        let origin = public_url.origin().ascii_serialization();
        let mut service_tokens: Vec<(String, TokenDigest)> = Vec::new();
        for service in &config.service_tokens {
            if service_tokens.iter().any(|(name, _)| name == &service.name) {
                eyre::bail!("[platform] service_tokens names {:?} twice", service.name);
            }
            service_tokens.push((service.name.clone(), token_digest(&service.token)));
        }
        let auth = AdminAuth::platform(
            &AdminConfig {
                oidc: Some(AdminOidc {
                    issuer: config.oidc.issuer.clone(),
                    audience: config.oidc.audience.clone(),
                    client_id: Some(config.oidc.client_id.clone()),
                    client_secret: None,
                    mode: Some("redirect".to_string()),
                }),
                password_hash: None,
                allowed_emails: allowed_emails.clone(),
                allowed_domains: allowed_domains.clone(),
                session_secret: config
                    .session_secret
                    .clone()
                    .or_else(|| session_root.map(SessionRoot::platform_session_secret)),
                public_url: Some(config.oidc.public_url.clone()),
                session_hours: config.session_hours,
            },
            outbound,
        )
        .wrap_err("[platform]")?;
        Ok(Self {
            auth,
            host,
            origin,
            allowed_emails,
            allowed_domains,
            service_tokens,
            outbound,
            login_limiter: RateLimiter::new(10, Duration::from_secs(60)),
            rescues: Mutex::new(HashMap::new()),
        })
    }

    /// Whether `origin` (an `Origin` header value) is the platform's own.
    fn is_origin(&self, origin: &str) -> bool {
        origin.eq_ignore_ascii_case(&self.origin)
    }

    /// The platform's authenticator, which mints and verifies its sessions.
    pub fn auth(&self) -> &AdminAuth {
        &self.auth
    }

    /// Whether `host` (lowercase, no port) is the platform's own.
    pub fn is_host(&self, host: &str) -> bool {
        host.eq_ignore_ascii_case(&self.host)
    }

    /// Whether a verified, lowercased email is platform staff.
    fn is_allowed(&self, email: &str) -> bool {
        if self.allowed_emails.iter().any(|e| e == email) {
            return true;
        }
        email
            .rsplit_once('@')
            .map(|(_, domain)| domain)
            .filter(|domain| !domain.is_empty())
            .is_some_and(|domain| self.allowed_domains.iter().any(|d| d == domain))
    }

    /// What the request's `Authorization: Bearer` says, if it carries one.
    /// Every configured token is compared, each in constant time, so a
    /// miss costs the same whichever token it missed.
    fn bearer(&self, headers: &HeaderMap) -> BearerAuth {
        let Some(token) = crate::ingress::bearer_token(headers, None) else {
            return BearerAuth::Absent;
        };
        let presented = token_digest(&token);
        let mut matched = None;
        for (name, digest) in &self.service_tokens {
            if digests_match(digest, &presented) {
                matched = matched.or(Some(name.clone()));
            }
        }
        match matched {
            Some(name) => BearerAuth::Service(name),
            None => BearerAuth::Rejected,
        }
    }

    /// Hold `token`, a session of `tenant`'s admin, behind a one-time code.
    fn issue_rescue(&self, tenant: TenantId, token: String) -> String {
        let code = crate::oidc::random_token();
        let mut rescues = self.rescues.lock().expect("rescues lock");
        let now = Instant::now();
        rescues.retain(|_, rescue| rescue.expires > now);
        rescues.insert(
            code.clone(),
            Rescue {
                tenant,
                token,
                expires: now + RESCUE_TTL,
            },
        );
        code
    }

    /// The rescue behind `code`, taken: a second call finds nothing, and an
    /// expired one is nothing already.
    fn take_rescue(&self, code: &str) -> Option<Rescue> {
        self.rescues
            .lock()
            .expect("rescues lock")
            .remove(code)
            .filter(|rescue| rescue.expires > Instant::now())
    }

    /// Age every outstanding rescue past its life.
    #[cfg(test)]
    pub(crate) fn expire_rescues(&self) {
        let now = Instant::now();
        for rescue in self.rescues.lock().expect("rescues lock").values_mut() {
            rescue.expires = now;
        }
    }
}

/// The SHA-256 of a bearer token: what the platform keeps and compares,
/// never the token itself.
type TokenDigest = [u8; 32];

fn token_digest(token: &str) -> TokenDigest {
    use sha2::Digest;
    sha2::Sha256::digest(token.as_bytes()).into()
}

/// Whether two digests are equal, in time that does not depend on where
/// they first differ.
fn digests_match(a: &TokenDigest, b: &TokenDigest) -> bool {
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The platform's action log: one row per change the API made to a tenant,
/// by the principal that made it. Append-only, on the shared pool without a
/// tenant scope — the log is the platform's, not any tenant's, and a
/// deleted tenant's entries stay. Recording is best-effort, as the tenant
/// trail's is: a failed write is logged and never fails the action it
/// describes.
#[derive(Clone)]
pub struct PlatformAuditStore {
    pool: PostgresPool,
}

/// One platform audit entry, as `GET /platform/api/audit` lists it.
#[derive(Debug, Serialize)]
pub struct PlatformAuditEntry {
    pub id: i64,
    /// The operator's email, or `service:<name>`.
    pub principal: String,
    pub action: String,
    /// The tenant acted on; empty for an action on none.
    pub tenant: String,
    pub detail: String,
    pub at: String,
}

/// The most entries one audit page returns, and the page when `limit` is
/// not given.
const AUDIT_PAGE_MAX: i64 = 500;
const AUDIT_PAGE_DEFAULT: i64 = 100;

impl PlatformAuditStore {
    pub fn new(pool: PostgresPool) -> Self {
        Self { pool }
    }

    /// Append one entry, logging a failure rather than returning it.
    pub async fn record(&self, principal: &str, action: &str, tenant: &str, detail: &str) {
        if let Err(e) = self.try_record(principal, action, tenant, detail).await {
            tracing::warn!(principal, action, tenant, error = %format!("{e:#}"), "platform audit write failed");
        }
    }

    async fn try_record(
        &self,
        principal: &str,
        action: &str,
        tenant: &str,
        detail: &str,
    ) -> Result<()> {
        let conn = self.pool.get().await?;
        conn.execute(
            "INSERT INTO platform_audit (principal, action, tenant, detail) VALUES ($1, $2, $3, $4)",
            &[&principal, &action, &tenant, &detail],
        )
        .await
        .wrap_err("writing the platform audit entry")?;
        Ok(())
    }

    /// Up to `limit` entries newest first, those before entry `before` when
    /// given: the page after the one whose last id it is.
    pub async fn recent(&self, limit: i64, before: Option<i64>) -> Result<Vec<PlatformAuditEntry>> {
        let limit = limit.clamp(1, AUDIT_PAGE_MAX);
        let conn = self.pool.get().await?;
        let rows = match before {
            Some(before) => {
                conn.query(
                    "SELECT id, principal, action, tenant, detail, at FROM platform_audit \
                     WHERE id < $1 ORDER BY id DESC LIMIT $2",
                    &[&before, &limit],
                )
                .await
            }
            None => {
                conn.query(
                    "SELECT id, principal, action, tenant, detail, at FROM platform_audit \
                     ORDER BY id DESC LIMIT $1",
                    &[&limit],
                )
                .await
            }
        }
        .wrap_err("reading the platform audit log")?;
        Ok(rows
            .iter()
            .map(|r| {
                let at: chrono::DateTime<chrono::Utc> = r.get(5);
                PlatformAuditEntry {
                    id: r.get(0),
                    principal: r.get(1),
                    action: r.get(2),
                    tenant: r.get(3),
                    detail: r.get(4),
                    at: at.to_rfc3339(),
                }
            })
            .collect())
    }
}

/// The platform's audit log, on the install's pool.
fn platform_audit(state: &EdgeState) -> PlatformAuditStore {
    PlatformAuditStore::new(state.infra().sessions_pool.clone())
}

/// The staff console (crates/edge/ui-platform), built to `ui-platform/dist`
/// and baked in like the admin SPA.
#[derive(RustEmbed)]
#[folder = "ui-platform/dist/"]
struct PlatformAssets;

/// One built console asset by its path under `/platform/`, as
/// `(content_type, bytes)`. A path that is no asset is the shell, so a
/// client route survives a reload; `api/…` is never the shell, so an API
/// path nothing routes stays a 404 rather than a page. `None` when the
/// bundle was not built into this binary, or for an `api/` path.
pub fn platform_asset(path: &str) -> Option<(String, std::borrow::Cow<'static, [u8]>)> {
    let rel = path.trim_start_matches('/');
    if rel == "api" || rel.starts_with("api/") {
        return None;
    }
    let (rel, file) = match PlatformAssets::get(rel) {
        Some(f) if !rel.is_empty() => (rel, f),
        _ => ("index.html", PlatformAssets::get("index.html")?),
    };
    let mime = mime_guess::from_path(rel).first_or_octet_stream();
    Some((mime.as_ref().to_string(), file.data))
}

/// `GET /platform`, `/platform/` and `/platform/{*path}` — the console. 404
/// without `[platform]` or off the platform host, like the API; 503 when
/// the bundle is not in the binary. Hashed assets are immutable, the shell
/// is not; the page may run its own bundle and nothing else, and is never
/// framed.
async fn platform_spa(
    state: &EdgeState,
    headers: &HeaderMap,
    peer: SocketAddr,
    path: &str,
) -> Response {
    if platform_for(state, headers, Some(peer)).is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    let rel = path.trim_start_matches('/');
    if rel == "api" || rel.starts_with("api/") {
        return StatusCode::NOT_FOUND.into_response();
    }
    match platform_asset(rel) {
        Some((content_type, bytes)) => {
            let cache = if rel.starts_with("assets/") {
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
                        "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
                         img-src 'self' data:; object-src 'none'; base-uri 'self'; \
                         frame-ancestors 'none'"
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

async fn platform_root(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(state): State<Arc<EdgeState>>,
    headers: HeaderMap,
) -> Response {
    platform_spa(&state, &headers, peer, "").await
}

async fn platform_path(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(state): State<Arc<EdgeState>>,
    headers: HeaderMap,
    Path(path): Path<String>,
) -> Response {
    platform_spa(&state, &headers, peer, &path).await
}

/// What a request's `Authorization: Bearer` header amounts to.
enum BearerAuth {
    /// No bearer: the cookie decides.
    Absent,
    /// The token of this configured service.
    Service(String),
    /// A token no configured service holds.
    Rejected,
}

/// Who a guarded request acts as, placed in the request extensions by
/// [`platform_guard`]: a signed-in operator by verified email, or a
/// configured service by its bearer token.
#[derive(Clone)]
struct PlatformPrincipal {
    /// As audit rows and logs record it: the operator's email, or
    /// `service:<name>`.
    name: String,
    service: bool,
}

impl PlatformPrincipal {
    fn operator(email: String) -> Self {
        Self {
            name: email,
            service: false,
        }
    }

    fn service(name: &str) -> Self {
        Self {
            name: format!("service:{name}"),
            service: true,
        }
    }

    fn name(&self) -> &str {
        &self.name
    }
}

/// An API failure, as the JSON the platform routes answer with.
struct ApiError(StatusCode, String);

impl ApiError {
    fn not_found() -> Self {
        Self(StatusCode::NOT_FOUND, "no such tenant".to_string())
    }

    fn internal(context: &str, error: eyre::Report) -> Self {
        tracing::error!(error = %format!("{error:#}"), "{context}");
        Self(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("{context}: {error:#}"),
        )
    }

    fn upstream(context: &str, error: eyre::Report) -> Self {
        tracing::error!(error = %format!("{error:#}"), "{context}");
        Self(StatusCode::BAD_GATEWAY, format!("{context}: {error:#}"))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({"error": self.1}))).into_response()
    }
}

type ApiResult = std::result::Result<Response, ApiError>;

/// Whether `error` is Postgres refusing a duplicate key — a tenant id or a
/// hostname already registered.
fn is_unique_violation(error: &eyre::Report) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<tokio_postgres::Error>()
            .and_then(tokio_postgres::Error::code)
            == Some(&SqlState::UNIQUE_VIOLATION)
    })
}

/// The platform surface the request reaches, or 404: the platform is
/// configured and the request names its host.
fn platform_for(
    state: &EdgeState,
    headers: &HeaderMap,
    peer: Option<SocketAddr>,
) -> Option<Arc<PlatformState>> {
    let platform = state.platform()?;
    let host = request_host(state.trusted_proxies(), headers, peer.map(|p| p.ip()))?;
    platform.is_host(&host).then(|| Arc::clone(platform))
}

/// Middleware fronting `/platform/api/*`: default-deny. 404 without
/// `[platform]` or off the platform host.
///
/// A request with `Authorization: Bearer` is a service's: the token must be
/// one of `[platform] service_tokens` (401 otherwise, whatever cookie rides
/// along), and the request acts as `service:<name>` — platform-level by
/// configuration, so neither the staff allowlist nor the CSRF rule applies;
/// there is no cookie for a cross-site page to ride.
///
/// Without one, the platform session cookie decides: 401 without a
/// session, 403 for an admin session presented in its place, for an
/// operator the file no longer lists (the list is re-read on every request,
/// so a removal takes effect before the session expires), and for a write
/// that did not come from the platform's own page (see
/// [`crate::admin::csrf_allows`]: `Sec-Fetch-Site: same-origin`, or an
/// `Origin` of `[platform.oidc] public_url`'s — an API client sends one of
/// the two).
async fn platform_guard(
    State(state): State<Arc<EdgeState>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let (mut parts, body) = request.into_parts();
    let peer = parts
        .extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0);
    let Some(platform) = platform_for(&state, &parts.headers, peer) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let principal = match platform.bearer(&parts.headers) {
        BearerAuth::Service(name) => PlatformPrincipal::service(&name),
        BearerAuth::Rejected => {
            tracing::warn!("platform bearer token matches no configured service");
            return StatusCode::UNAUTHORIZED.into_response();
        }
        BearerAuth::Absent => {
            match crate::admin::require_platform(Some(platform.auth()), &parts.headers) {
                Ok(operator) => {
                    if !platform.is_allowed(&operator) {
                        tracing::warn!(operator = %operator, "platform session of someone no longer on the staff allowlist");
                        return StatusCode::FORBIDDEN.into_response();
                    }
                    if !crate::admin::csrf_allows(&parts.method, &parts.headers, |origin| {
                        platform.is_origin(origin)
                    }) {
                        return StatusCode::FORBIDDEN.into_response();
                    }
                    PlatformPrincipal::operator(operator)
                }
                Err(code) => return code.into_response(),
            }
        }
    };
    parts.extensions.insert(principal);
    next.run(axum::extract::Request::from_parts(parts, body))
        .await
}

/// Name of the short-lived cookie carrying the platform sign-in flow's
/// state/nonce/PKCE between `/start` and `/callback`.
const FLOW_COOKIE: &str = "wf_platform_oidc_flow";

fn flow_cookie(token: &str) -> String {
    format!(
        "{FLOW_COOKIE}={token}; HttpOnly; Secure; SameSite=Lax; Path=/platform/api/login; Max-Age=600"
    )
}

fn clear_flow_cookie() -> String {
    format!("{FLOW_COOKIE}=; HttpOnly; Secure; SameSite=Lax; Path=/platform/api/login; Max-Age=0")
}

fn redirect_uri(auth: &AdminAuth) -> String {
    format!(
        "{}/platform/api/login/oidc/callback",
        auth.public_url().unwrap_or_default()
    )
}

/// Bounce to the platform page with a human error, clearing the flow cookie.
fn login_error(msg: &str) -> Response {
    let location = format!(
        "/platform/?login_error={}",
        url::form_urlencoded::byte_serialize(msg.as_bytes()).collect::<String>()
    );
    (
        StatusCode::FOUND,
        axum::response::AppendHeaders([
            (header::SET_COOKIE, clear_flow_cookie()),
            (header::LOCATION, location),
        ]),
    )
        .into_response()
}

/// `GET /platform/api/login/oidc/start` — begin the Authorization Code +
/// PKCE flow against the platform realm.
async fn login_start(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(state): State<Arc<EdgeState>>,
    headers: HeaderMap,
) -> Response {
    let Some(platform) = platform_for(&state, &headers, Some(peer)) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let ip = client_ip(state.trusted_proxies(), &headers, Some(peer.ip()));
    if !platform.login_limiter.check(ip) {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    }
    let auth = platform.auth();
    let (Some(issuer), Some(client_id)) = (auth.oidc_issuer(), auth.oidc_client_id()) else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let endpoints = match crate::keycloak::discover_endpoints_with(issuer, platform.outbound).await
    {
        Ok(e) => e,
        Err(e) => {
            tracing::error!(error = %e, "platform OIDC discovery failed");
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };
    let pkce = crate::oidc::pkce();
    let st = crate::oidc::random_token();
    let nonce = crate::oidc::random_token();
    let flow = crate::oidc::FlowState {
        state: st.clone(),
        verifier: pkce.verifier,
        nonce: nonce.clone(),
        exp: (chrono::Utc::now().timestamp() + 600) as usize,
    };
    let cookie = match auth.issue_flow(&flow) {
        Ok(t) => flow_cookie(&t),
        Err(e) => {
            tracing::error!(error = %e, "signing platform OIDC flow state failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let Ok(url) = crate::oidc::authorize_url(
        &endpoints.authorization_endpoint,
        client_id,
        &redirect_uri(auth),
        &st,
        &nonce,
        &pkce.challenge,
    ) else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    (
        StatusCode::FOUND,
        axum::response::AppendHeaders([(header::SET_COOKIE, cookie), (header::LOCATION, url)]),
    )
        .into_response()
}

/// `GET /platform/api/login/oidc/callback?code&state` — finish the flow:
/// match state, exchange the code, verify the id_token and nonce, check the
/// file allowlist, mint the platform session.
async fn login_callback(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(state): State<Arc<EdgeState>>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let Some(platform) = platform_for(&state, &headers, Some(peer)) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let auth = platform.auth();
    let Some(flow) =
        crate::server::cookie_value(&headers, FLOW_COOKIE).and_then(|t| auth.verify_flow(&t))
    else {
        return login_error("Your sign-in link expired — please try again.");
    };
    let ip = client_ip(state.trusted_proxies(), &headers, Some(peer.ip()));
    if !platform.login_limiter.check(ip) {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    }
    if params.contains_key("error") {
        tracing::warn!(error = ?params.get("error"), "platform IdP returned an error");
        return login_error("Sign-in was cancelled or failed at the identity provider.");
    }
    let (Some(code), Some(st)) = (params.get("code"), params.get("state")) else {
        return login_error("Malformed sign-in response.");
    };
    if st != &flow.state {
        tracing::warn!("platform OIDC state mismatch");
        return login_error("Sign-in verification failed — please try again.");
    }
    let (Some(issuer), Some(client_id)) = (auth.oidc_issuer(), auth.oidc_client_id()) else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let endpoints = match crate::keycloak::discover_endpoints_with(issuer, platform.outbound).await
    {
        Ok(e) => e,
        Err(e) => {
            tracing::error!(error = %e, "platform OIDC discovery failed");
            return login_error("Couldn't reach the identity provider.");
        }
    };
    let id_token = match crate::oidc::exchange_code_with(
        platform.outbound,
        &endpoints.token_endpoint,
        client_id,
        auth.oidc_client_secret(),
        &redirect_uri(auth),
        code,
        &flow.verifier,
    )
    .await
    {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(error = %e, "platform OIDC code exchange failed");
            return login_error("Sign-in could not be completed.");
        }
    };
    let verified = match auth.verify_oidc_claims(&id_token).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "platform OIDC id_token verification failed");
            return login_error("Sign-in token could not be verified.");
        }
    };
    if verified.nonce.as_deref() != Some(flow.nonce.as_str()) {
        tracing::warn!("platform OIDC nonce mismatch");
        return login_error("Sign-in verification failed — please try again.");
    }
    if !platform.is_allowed(&verified.email) {
        tracing::warn!(email = %verified.email, "platform sign-in by someone not on the staff allowlist");
        return login_error("Your account is not on the platform allowlist.");
    }
    let token = match auth.issue_session(&verified.email) {
        Ok(t) => t,
        Err(e) => {
            tracing::error!(error = %e, "platform session issue failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    tracing::info!(operator = %verified.email, "platform operator signed in");
    (
        StatusCode::FOUND,
        axum::response::AppendHeaders([
            (header::SET_COOKIE, auth.session_cookie(&token)),
            (header::SET_COOKIE, clear_flow_cookie()),
            (header::LOCATION, "/platform/".to_string()),
        ]),
    )
        .into_response()
}

/// `POST /platform/api/logout` — clear the platform session cookie.
async fn logout(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(state): State<Arc<EdgeState>>,
    headers: HeaderMap,
) -> Response {
    let Some(platform) = platform_for(&state, &headers, Some(peer)) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    (
        [(header::SET_COOKIE, platform.auth().clear_cookie())],
        Json(serde_json::json!({"ok": true})),
    )
        .into_response()
}

/// `GET /platform/api/me` — session probe: who is signed in.
async fn me(operator: axum::Extension<PlatformPrincipal>) -> Response {
    Json(serde_json::json!({"operator": operator.name(), "role": "platform"})).into_response()
}

/// A tenant as the platform API shows it: its registry row, whether it
/// has a settings row, its admin sign-in and key states and how it is
/// being served right now. Never a secret.
#[derive(Serialize)]
struct TenantView {
    #[serde(flatten)]
    tenant: Tenant,
    has_settings: bool,
    /// Whether the running tenant has an admin sign-in — what a rescue
    /// opens; false while it is not served.
    has_admin: bool,
    /// Whether its settings row seals an OIDC client secret for that
    /// sign-in.
    has_admin_client_secret: bool,
    /// The key's state, from the running tenant; `None` while it is not
    /// served.
    llm_status: Option<LlmStatus>,
    /// What the tenant has used of its plan, from the running tenant;
    /// `None` while it is not served or when its stores could not be
    /// counted (which the log reports).
    usage: Option<Usage>,
    /// Whether a runtime answers on its hostnames.
    serving: bool,
    /// Why not, when not: the suspension message its callers see.
    unavailable: Option<String>,
    /// How the tenant's own URLs are reached, as `PATCH` sets it: the
    /// settings row's policy, else the install's default.
    outbound: OutboundPolicy,
    /// The tenant's admin sign-in, secrets omitted (its serializer skips
    /// them); `None` when it has none.
    admin: Option<AdminConfig>,
}

async fn view(state: &EdgeState, tenant: Tenant) -> Result<TenantView> {
    let (has_settings, has_admin_client_secret) = match state.infra().settings_store_for(&tenant.id)
    {
        Some(store) => match store.holds_admin_client_secret().await? {
            Some(held) => (true, held),
            None => (false, false),
        },
        None => (false, false),
    };
    let settings = state.settings_for(&tenant).await?;
    let (has_admin, llm_status, usage, serving, unavailable) = match state.serving(&tenant.id) {
        Resolved::Tenant(runtime) => {
            let usage = match runtime.usage().await {
                Ok(usage) => Some(usage),
                Err(e) => {
                    tracing::warn!(tenant = %tenant.id, error = %e, "counting plan usage failed");
                    None
                }
            };
            (
                runtime.admin.is_some(),
                Some(runtime.llm_status.current()),
                usage,
                true,
                None,
            )
        }
        // The platform sees the build failure itself, where the tenant's
        // callers see a fixed message.
        Resolved::Suspended(_) => (false, None, None, false, state.unavailable(&tenant.id)),
        Resolved::Unknown => (false, None, None, false, None),
    };
    Ok(TenantView {
        tenant,
        has_settings,
        has_admin,
        has_admin_client_secret,
        llm_status,
        usage,
        serving,
        unavailable,
        outbound: settings.outbound,
        admin: settings.admin,
    })
}

fn tenant_id(raw: &str) -> std::result::Result<TenantId, ApiError> {
    TenantId::parse(raw).map_err(|_| ApiError::not_found())
}

/// Whether `name` is a lowercase DNS name: labels of letters, digits and
/// hyphens, none empty or hyphen-edged, 253 bytes at most.
fn dns_shaped(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 253
        && name.split('.').all(|label| {
            !label.is_empty()
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
}

/// A hostname as the registry keys it: lowercase DNS labels, no scheme,
/// port or path, and not the platform's own.
fn hostname(raw: &str, platform: &PlatformState) -> std::result::Result<String, ApiError> {
    let host = raw.trim().trim_end_matches('.').to_ascii_lowercase();
    if !dns_shaped(&host) {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!("{raw:?} is not a hostname (lowercase labels, no scheme, port or path)"),
        ));
    }
    if platform.is_host(&host) {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "that hostname is the platform's own".to_string(),
        ));
    }
    Ok(host)
}

async fn registry_row(state: &EdgeState, id: &TenantId) -> std::result::Result<Tenant, ApiError> {
    state
        .infra()
        .tenants
        .get(id)
        .await
        .map_err(|e| ApiError::internal("reading the registry", e))?
        .ok_or_else(ApiError::not_found)
}

/// `GET /platform/api/tenants` — every tenant in the registry.
async fn list_tenants(State(state): State<Arc<EdgeState>>) -> ApiResult {
    let rows = state
        .infra()
        .tenants
        .list()
        .await
        .map_err(|e| ApiError::internal("listing tenants", e))?;
    let mut views = Vec::with_capacity(rows.len());
    for row in rows {
        views.push(
            view(&state, row)
                .await
                .map_err(|e| ApiError::internal("describing a tenant", e))?,
        );
    }
    Ok(Json(views).into_response())
}

/// `GET /platform/api/tenants/{id}`.
async fn get_tenant(State(state): State<Arc<EdgeState>>, Path(id): Path<String>) -> ApiResult {
    let id = tenant_id(&id)?;
    let row = registry_row(&state, &id).await?;
    let view = view(&state, row)
        .await
        .map_err(|e| ApiError::internal("describing the tenant", e))?;
    Ok(Json(view).into_response())
}

#[derive(Deserialize)]
struct CreateTenant {
    id: String,
    display_name: String,
    /// The hostnames the tenant answers on. None by default: the platform
    /// decides names, and the product stack passes `<id>.marrilo.com`.
    #[serde(default)]
    hostnames: Vec<String>,
}

/// The shortest id the platform hands out. The platform's rule on top of
/// the slug grammar, which allows `a1`: a one- or two-character subdomain
/// is a name the product keeps.
const MIN_TENANT_ID_LEN: usize = 3;

/// `POST /platform/api/tenants` — the only way a tenant comes into being:
/// the registry row, a vellev tenant with a key of its own (sealed into
/// the tenant's settings row), its hostnames, and a running runtime. A
/// failure before the runtime leaves nothing behind; a runtime that fails
/// to build leaves the tenant suspended with the reason, as boot would, and
/// the response says so. An id that is not a slug is 400; one the platform
/// keeps for itself (reserved, or under [`MIN_TENANT_ID_LEN`]) is 422.
async fn create_tenant(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(state): State<Arc<EdgeState>>,
    headers: HeaderMap,
    operator: axum::Extension<PlatformPrincipal>,
    Json(body): Json<CreateTenant>,
) -> ApiResult {
    let platform = platform_for(&state, &headers, Some(peer)).ok_or_else(ApiError::not_found)?;
    let id = TenantId::parse(&body.id)
        .map_err(|e| ApiError(StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    if id.as_str().len() < MIN_TENANT_ID_LEN {
        return Err(ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!(
                "tenant id {:?} is too short: at least {MIN_TENANT_ID_LEN} characters",
                id.as_str()
            ),
        ));
    }
    if id.is_reserved() {
        return Err(ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("tenant id {:?} is reserved", id.as_str()),
        ));
    }
    let display_name = body.display_name.trim();
    if display_name.is_empty() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "display_name is required".to_string(),
        ));
    }
    let mut hostnames = Vec::with_capacity(body.hostnames.len());
    for raw in &body.hostnames {
        let host = hostname(raw, &platform)?;
        if !hostnames.contains(&host) {
            hostnames.push(host);
        }
    }
    let infra = state.infra();
    for host in &hostnames {
        let taken = infra
            .tenants
            .resolve_hostname(host)
            .await
            .map_err(|e| ApiError::internal("checking a hostname", e))?;
        if taken.is_some() {
            return Err(ApiError(
                StatusCode::CONFLICT,
                format!("hostname {host} is already registered"),
            ));
        }
    }
    let settings_store = infra.settings_store_for(&id).ok_or_else(|| {
        ApiError(
            StatusCode::CONFLICT,
            format!(
                "{} is not set: a tenant's vellev key has nowhere to be held",
                crate::boot::MASTER_KEY_ENV
            ),
        )
    })?;

    let row = match infra.tenants.create(&id, display_name).await {
        Ok(row) => row,
        Err(e) if is_unique_violation(&e) => {
            return Err(ApiError(
                StatusCode::CONFLICT,
                format!("tenant {id} already exists"),
            ))
        }
        Err(e) => return Err(ApiError::internal("creating the tenant", e)),
    };
    // From here on a failure takes the row with it. The vellev tenant goes
    // too once it exists, so a retried create starts clean.
    let provisioned = async {
        let key = infra.provision_vellev_tenant(&id).await.map_err(|e| {
            (
                false,
                ApiError::upstream("provisioning the vellev tenant", e),
            )
        })?;
        let seed = state
            .settings_for(&row)
            .await
            .map_err(|e| (true, ApiError::internal("seeding the tenant's settings", e)))?;
        settings_store
            .seed_if_empty(&seed)
            .await
            .map_err(|e| (true, ApiError::internal("seeding the tenant's settings", e)))?;
        settings_store.set_vellev_api_key(&key).await.map_err(|e| {
            (
                true,
                ApiError::internal("storing the tenant's vellev key", e),
            )
        })?;
        for host in &hostnames {
            match infra.tenants.add_hostname(&id, host).await {
                Ok(()) => state.hostname_added(&id, host),
                Err(e) if is_unique_violation(&e) => {
                    return Err((
                        true,
                        ApiError(
                            StatusCode::CONFLICT,
                            format!("hostname {host} is already registered"),
                        ),
                    ))
                }
                Err(e) => return Err((true, ApiError::internal("registering a hostname", e))),
            }
        }
        Ok(())
    }
    .await;
    if let Err((drop_vellev, error)) = provisioned {
        discard(&state, &id, drop_vellev).await;
        return Err(error);
    }

    if let Err(e) = state.start(&id).await {
        tracing::error!(tenant = %id, error = %format!("{e:#}"), "created tenant failed to start; suspended");
    }
    tracing::info!(
        tenant = %id,
        operator = %operator.name(),
        hostnames = ?hostnames,
        "tenant created through the platform API"
    );
    platform_audit(&state)
        .record(
            operator.name(),
            "tenant.create",
            id.as_str(),
            &format!("{display_name:?} hostnames={}", hostnames.join(",")),
        )
        .await;
    let row = registry_row(&state, &id).await?;
    let view = view(&state, row)
        .await
        .map_err(|e| ApiError::internal("describing the tenant", e))?;
    Ok((StatusCode::CREATED, Json(view)).into_response())
}

/// Undo a half-made tenant: its hostnames leave the cache, its vellev
/// tenant goes when it was made, and its row goes (settings and hostnames
/// with it). Best-effort and loud: there is nothing better to do with a
/// failure here than report it.
async fn discard(state: &EdgeState, id: &TenantId, drop_vellev: bool) {
    state.forget(id);
    if drop_vellev {
        if let Err(e) = state.infra().drop_vellev_tenant(id).await {
            tracing::error!(tenant = %id, error = %format!("{e:#}"), "vellev tenant left behind by a failed create");
        }
    }
    if let Err(e) = state.infra().tenants.delete(id).await {
        tracing::error!(tenant = %id, error = %format!("{e:#}"), "registry row left behind by a failed create");
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PatchTenant {
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    plan: Option<Plan>,
    #[serde(default)]
    outbound: Option<OutboundPolicy>,
    /// The tenant's admin sign-in, the non-secret section exactly as
    /// `PUT /admin/api/settings` takes it: `allowed_emails` and
    /// `allowed_domains` seed its operator allowlist when that is empty.
    /// The platform's way to provision a sign-in for a tenant that has
    /// none, whose own admin API is unreachable until it does.
    #[serde(default)]
    admin: Option<AdminConfig>,
    /// The admin OIDC client secret, write-only: `""` clears it, absent
    /// leaves it.
    #[serde(default)]
    admin_client_secret: Option<String>,
}

/// `PATCH /platform/api/tenants/{id}` `{display_name?, plan?, outbound?,
/// admin?, admin_client_secret?}` — the row (and, for the last three, the
/// tenant's settings row), then the runtime rebuilt on it when the tenant
/// is being served, so a plan's concurrency cap, a policy change or a new
/// admin sign-in binds at once — and a rescue can open the admin the
/// moment it exists. `outbound` is set here and nowhere else: a tenant's
/// own settings save cannot loosen it. An `admin` section the tenant could
/// not start on — a redirect-mode OIDC without its `public_url`, say — is
/// refused before anything is written, by the check the rebuild would run.
/// An unknown field, or a secret anywhere but `admin_client_secret`, is 422.
async fn patch_tenant(
    State(state): State<Arc<EdgeState>>,
    Path(id): Path<String>,
    operator: axum::Extension<PlatformPrincipal>,
    Json(body): Json<serde_json::Value>,
) -> ApiResult {
    let id = tenant_id(&id)?;
    let row = registry_row(&state, &id).await?;
    if let Some(field) = crate::server::secret_in_settings_body(&body) {
        return Err(ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!(
                "{field} is a secret: the platform sets a tenant's OIDC client secret through \
                 admin_client_secret and no other"
            ),
        ));
    }
    let body: PatchTenant = serde_json::from_value(body)
        .map_err(|e| ApiError(StatusCode::UNPROCESSABLE_ENTITY, e.to_string()))?;
    let registry = &state.infra().tenants;
    if let Some(name) = body.display_name.as_deref().map(str::trim) {
        if name.is_empty() {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                "display_name cannot be empty".to_string(),
            ));
        }
        registry
            .set_display_name(&id, name)
            .await
            .map_err(|e| ApiError::internal("renaming the tenant", e))?;
    }
    if let Some(plan) = &body.plan {
        registry
            .set_plan(&id, plan)
            .await
            .map_err(|e| ApiError::internal("setting the plan", e))?;
    }
    let settings_changed =
        body.outbound.is_some() || body.admin.is_some() || body.admin_client_secret.is_some();
    if settings_changed {
        let store = state.infra().settings_store_for(&id).ok_or_else(|| {
            ApiError(
                StatusCode::CONFLICT,
                format!(
                    "{} is not set: the tenant has no settings row to hold its policy or admin",
                    crate::boot::MASTER_KEY_ENV
                ),
            )
        })?;
        // These live on the settings row; a tenant that has never saved
        // settings gets one seeded from what it runs on.
        let seed = state
            .settings_for(&row)
            .await
            .map_err(|e| ApiError::internal("reading the tenant's settings", e))?;
        if let Some(admin) = &body.admin {
            let outbound = body.outbound.unwrap_or(seed.outbound);
            AdminAuth::new(admin, id.clone(), outbound).map_err(|e| {
                ApiError(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    format!("the tenant would not start on this admin section: {e:#}"),
                )
            })?;
        }
        store
            .seed_if_empty(&seed)
            .await
            .map_err(|e| ApiError::internal("creating the tenant's settings row", e))?;
        if let Some(outbound) = body.outbound {
            store
                .set_outbound(outbound)
                .await
                .map_err(|e| ApiError::internal("setting the outbound policy", e))?;
        }
        if let Some(admin) = &body.admin {
            store
                .set_admin(admin)
                .await
                .map_err(|e| ApiError::internal("setting the admin section", e))?;
        }
        if let Some(secret) = body.admin_client_secret.as_deref().map(str::trim) {
            store
                .set_admin_client_secret(Some(secret).filter(|s| !s.is_empty()))
                .await
                .map_err(|e| ApiError::internal("setting the admin client secret", e))?;
        }
    }
    let changed = body.display_name.is_some() || body.plan.is_some() || settings_changed;
    if changed && matches!(state.serving(&id), Resolved::Tenant(_)) {
        state.reload(&id).await.map_err(|e| {
            ApiError::upstream(
                "the tenant was updated, but its runtime could not be rebuilt on the change",
                e,
            )
        })?;
    }
    tracing::info!(
        tenant = %id,
        operator = %operator.name(),
        admin = body.admin.is_some(),
        admin_client_secret = body.admin_client_secret.is_some(),
        "tenant updated through the platform API"
    );
    let fields: Vec<&str> = [
        ("display_name", body.display_name.is_some()),
        ("plan", body.plan.is_some()),
        ("outbound", body.outbound.is_some()),
        ("admin", body.admin.is_some()),
        ("admin_client_secret", body.admin_client_secret.is_some()),
    ]
    .into_iter()
    .filter_map(|(field, set)| set.then_some(field))
    .collect();
    platform_audit(&state)
        .record(
            operator.name(),
            "tenant.update",
            id.as_str(),
            &fields.join(","),
        )
        .await;
    let row = registry_row(&state, &id).await?;
    let view = view(&state, row)
        .await
        .map_err(|e| ApiError::internal("describing the tenant", e))?;
    Ok(Json(view).into_response())
}

#[derive(Deserialize, Default)]
struct Suspend {
    #[serde(default)]
    message: Option<String>,
}

/// `POST /platform/api/tenants/{id}/suspend` `{message?}` — the registry
/// and the runtime both. The body is a JSON object, `{}` at least: a
/// bodiless POST is a simple request a cross-site page could send, and the
/// JSON content type is what makes a browser ask first.
async fn suspend_tenant(
    State(state): State<Arc<EdgeState>>,
    Path(id): Path<String>,
    operator: axum::Extension<PlatformPrincipal>,
    Json(body): Json<Suspend>,
) -> ApiResult {
    let id = tenant_id(&id)?;
    let message = body
        .message
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| crate::server::SUSPENDED_MESSAGE.to_string());
    let known = state
        .infra()
        .tenants
        .set_status(&id, TenantStatus::Suspended)
        .await
        .map_err(|e| ApiError::internal("suspending the tenant", e))?;
    if !known {
        return Err(ApiError::not_found());
    }
    state.suspend(&id, message.clone()).await;
    tracing::warn!(tenant = %id, operator = %operator.name(), "tenant suspended through the platform API");
    platform_audit(&state)
        .record(operator.name(), "tenant.suspend", id.as_str(), &message)
        .await;
    let row = registry_row(&state, &id).await?;
    let view = view(&state, row)
        .await
        .map_err(|e| ApiError::internal("describing the tenant", e))?;
    Ok(Json(view).into_response())
}

/// `POST /platform/api/tenants/{id}/resume` — the registry, then the
/// runtime rebuilt. A build failure leaves the tenant suspended with the
/// reason and answers 502.
async fn resume_tenant(
    State(state): State<Arc<EdgeState>>,
    Path(id): Path<String>,
    operator: axum::Extension<PlatformPrincipal>,
) -> ApiResult {
    let id = tenant_id(&id)?;
    let known = state
        .infra()
        .tenants
        .set_status(&id, TenantStatus::Active)
        .await
        .map_err(|e| ApiError::internal("resuming the tenant", e))?;
    if !known {
        return Err(ApiError::not_found());
    }
    state
        .resume(&id)
        .await
        .map_err(|e| ApiError::upstream("the tenant is active again but failed to start", e))?;
    tracing::info!(tenant = %id, operator = %operator.name(), "tenant resumed through the platform API");
    platform_audit(&state)
        .record(operator.name(), "tenant.resume", id.as_str(), "")
        .await;
    let row = registry_row(&state, &id).await?;
    let view = view(&state, row)
        .await
        .map_err(|e| ApiError::internal("describing the tenant", e))?;
    Ok(Json(view).into_response())
}

#[derive(Deserialize)]
struct AddHostname {
    hostname: String,
}

/// `POST /platform/api/tenants/{id}/hostnames` `{hostname}` — registered
/// and reaching the tenant at once, no reload.
async fn add_hostname(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(state): State<Arc<EdgeState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    operator: axum::Extension<PlatformPrincipal>,
    Json(body): Json<AddHostname>,
) -> ApiResult {
    let platform = platform_for(&state, &headers, Some(peer)).ok_or_else(ApiError::not_found)?;
    let id = tenant_id(&id)?;
    registry_row(&state, &id).await?;
    let host = hostname(&body.hostname, &platform)?;
    match state.infra().tenants.add_hostname(&id, &host).await {
        Ok(()) => state.hostname_added(&id, &host),
        Err(e) if is_unique_violation(&e) => {
            return Err(ApiError(
                StatusCode::CONFLICT,
                format!("hostname {host} is already registered"),
            ))
        }
        Err(e) => return Err(ApiError::internal("registering the hostname", e)),
    }
    tracing::info!(tenant = %id, host = %host, operator = %operator.name(), "hostname added through the platform API");
    platform_audit(&state)
        .record(operator.name(), "hostname.add", id.as_str(), &host)
        .await;
    let row = registry_row(&state, &id).await?;
    let view = view(&state, row)
        .await
        .map_err(|e| ApiError::internal("describing the tenant", e))?;
    Ok(Json(view).into_response())
}

/// `DELETE /platform/api/tenants/{id}/hostnames/{hostname}` — gone from the
/// registry and the cache at once.
async fn remove_hostname(
    State(state): State<Arc<EdgeState>>,
    Path((id, host)): Path<(String, String)>,
    operator: axum::Extension<PlatformPrincipal>,
) -> ApiResult {
    let id = tenant_id(&id)?;
    let row = registry_row(&state, &id).await?;
    let host = host.trim().to_ascii_lowercase();
    if !row.hostnames.contains(&host) {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            format!("{host} is not a hostname of tenant {id}"),
        ));
    }
    state
        .infra()
        .tenants
        .remove_hostname(&host)
        .await
        .map_err(|e| ApiError::internal("removing the hostname", e))?;
    state.hostname_removed(&host);
    tracing::info!(tenant = %id, host = %host, operator = %operator.name(), "hostname removed through the platform API");
    platform_audit(&state)
        .record(operator.name(), "hostname.remove", id.as_str(), &host)
        .await;
    let row = registry_row(&state, &id).await?;
    let view = view(&state, row)
        .await
        .map_err(|e| ApiError::internal("describing the tenant", e))?;
    Ok(Json(view).into_response())
}

/// The tenant's admin allowlist and audit trail as the platform reaches
/// them: on the shared pool under the tenant's id, served or not, so a
/// sign-in can be granted before the tenant's own admin surface exists.
/// The allowlist is read at each sign-in, so a change needs no reload.
fn operator_stores(state: &EdgeState, id: &TenantId) -> (EdgeOperatorStore, EdgeAuditStore) {
    let pool = &state.infra().tenants_pool;
    (
        EdgeOperatorStore::new(pool.clone(), id.clone()),
        EdgeAuditStore::new(pool.clone(), id.clone()),
    )
}

/// `email` or `domain`, the two kinds of allowlist entry, or 422.
fn operator_kind(raw: &str) -> std::result::Result<&str, ApiError> {
    match raw {
        "email" | "domain" => Ok(raw),
        _ => Err(ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "kind must be \"email\" or \"domain\"".to_string(),
        )),
    }
}

/// An allowlist value as the store keys it — trimmed and lowercased — and
/// shaped as its kind wants: an email with one `@`, a local part and a
/// dotted DNS domain; a domain as a dotted DNS name, with or without a
/// leading `@`. Anything else is 422.
fn operator_value(kind: &str, raw: &str) -> std::result::Result<String, ApiError> {
    let value = raw.trim().to_lowercase();
    let (shaped, wanted) = match kind {
        "email" => (
            value.len() <= 320
                && value.rsplit_once('@').is_some_and(|(local, domain)| {
                    !local.is_empty()
                        && !local
                            .chars()
                            .any(|c| c == '@' || c.is_whitespace() || c.is_control())
                        && domain.contains('.')
                        && dns_shaped(domain)
                }),
            "an email address",
        ),
        _ => {
            let domain = value.trim_start_matches('@');
            (domain.contains('.') && dns_shaped(domain), "a domain")
        }
    };
    if !shaped {
        return Err(ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("{raw:?} is not {wanted}"),
        ));
    }
    Ok(value.trim_start_matches('@').to_string())
}

/// `GET /platform/api/tenants/{id}/operators` — the tenant's admin
/// allowlist, `[{kind, value, added_by, added_at}]`.
async fn list_operators(State(state): State<Arc<EdgeState>>, Path(id): Path<String>) -> ApiResult {
    let id = tenant_id(&id)?;
    registry_row(&state, &id).await?;
    let (operators, _) = operator_stores(&state, &id);
    let entries: Vec<Operator> = operators
        .list()
        .await
        .map_err(|e| ApiError::internal("listing the tenant's operators", e))?;
    Ok(Json(entries).into_response())
}

#[derive(Deserialize)]
struct AddOperator {
    kind: String,
    value: String,
}

/// `POST /platform/api/tenants/{id}/operators` `{kind, value}` — an entry
/// on the tenant's admin allowlist, `added_by` the platform operator: how
/// the portal makes a signer-up the operator of the tenant it just made.
/// 201 with the entry; 409 when it is there already. Recorded in the
/// tenant's audit trail and in force at the tenant's next sign-in.
async fn add_operator(
    State(state): State<Arc<EdgeState>>,
    Path(id): Path<String>,
    operator: axum::Extension<PlatformPrincipal>,
    Json(body): Json<AddOperator>,
) -> ApiResult {
    let id = tenant_id(&id)?;
    registry_row(&state, &id).await?;
    let kind = operator_kind(&body.kind)?;
    let value = operator_value(kind, &body.value)?;
    let (operators, audit) = operator_stores(&state, &id);
    let by = operator.name();
    let added = operators
        .add(kind, &value, by)
        .await
        .map_err(|e| ApiError::internal("adding the operator", e))?;
    if !added {
        return Err(ApiError(
            StatusCode::CONFLICT,
            format!("{kind} {value} is already an operator of tenant {id}"),
        ));
    }
    audit
        .record(
            by,
            "operator.add",
            &format!("{kind}:{value}"),
            "by platform",
        )
        .await;
    tracing::info!(tenant = %id, kind, value = %value, operator = %by, "operator added through the platform API");
    platform_audit(&state)
        .record(by, "operator.add", id.as_str(), &format!("{kind}:{value}"))
        .await;
    let entry = operators
        .get(kind, &value)
        .await
        .map_err(|e| ApiError::internal("reading the operator back", e))?
        .ok_or_else(|| {
            ApiError::internal(
                "reading the operator back",
                eyre::eyre!("the entry just added is not there"),
            )
        })?;
    Ok((StatusCode::CREATED, Json(entry)).into_response())
}

/// `DELETE /platform/api/tenants/{id}/operators/{kind}/{value}` — 204, or
/// 404 when the tenant has no such entry. The tenant's own admin API keeps
/// its last entry; the platform may remove it, since a rescue can always
/// open the admin again.
async fn remove_operator(
    State(state): State<Arc<EdgeState>>,
    Path((id, kind, value)): Path<(String, String, String)>,
    operator: axum::Extension<PlatformPrincipal>,
) -> ApiResult {
    let id = tenant_id(&id)?;
    registry_row(&state, &id).await?;
    let kind = operator_kind(&kind)?;
    let value = value.trim().to_lowercase();
    let (operators, audit) = operator_stores(&state, &id);
    let removed = operators
        .remove(kind, &value)
        .await
        .map_err(|e| ApiError::internal("removing the operator", e))?;
    if !removed {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            format!("{kind} {value} is not an operator of tenant {id}"),
        ));
    }
    let by = operator.name();
    audit
        .record(
            by,
            "operator.remove",
            &format!("{kind}:{value}"),
            "by platform",
        )
        .await;
    tracing::info!(tenant = %id, kind, value = %value, operator = %by, "operator removed through the platform API");
    platform_audit(&state)
        .record(
            by,
            "operator.remove",
            id.as_str(),
            &format!("{kind}:{value}"),
        )
        .await;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// `DELETE /platform/api/tenants/{id}?confirm={id}` — the runtime retired,
/// the vellev tenant dropped (its data, keys and backups with it), the
/// artifact root removed, every row of the tenant's deleted. A failure
/// part-way leaves the tenant retired and its rows in place, and the
/// deletion is retried as a whole. `default` is never deleted.
async fn delete_tenant(
    State(state): State<Arc<EdgeState>>,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    operator: axum::Extension<PlatformPrincipal>,
) -> ApiResult {
    let id = tenant_id(&id)?;
    if id.is_default() {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            "the default tenant cannot be deleted".to_string(),
        ));
    }
    registry_row(&state, &id).await?;
    if params.get("confirm").map(String::as_str) != Some(id.as_str()) {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!("deleting a tenant needs ?confirm={id}"),
        ));
    }
    let infra = state.infra();
    state.retire(&id);
    infra.drop_vellev_tenant(&id).await.map_err(|e| {
        ApiError::upstream("dropping the vellev tenant; the deletion can be retried", e)
    })?;
    infra.tenants.delete(&id).await.map_err(|e| {
        ApiError::internal("deleting the tenant's rows; the deletion can be retried", e)
    })?;
    if let Err(e) = infra.remove_artifacts_for(&id) {
        tracing::error!(tenant = %id, error = %format!("{e:#}"), "artifact directory left behind by a deleted tenant");
    }
    state.forget(&id);
    tracing::warn!(tenant = %id, operator = %operator.name(), "tenant deleted through the platform API");
    platform_audit(&state)
        .record(operator.name(), "tenant.delete", id.as_str(), "")
        .await;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// How long a rescue session lives, whatever the tenant's own sessions do:
/// an operator's visit, not a working day.
const RESCUE_SESSION: Duration = Duration::from_secs(60 * 60);

/// `POST /platform/api/tenants/{id}/rescue` — an admin session on the
/// tenant, minted for the platform operator by the tenant's own
/// authenticator for [`RESCUE_SESSION`] at most and recorded in the
/// tenant's audit trail, behind a one-time code the tenant's host exchanges
/// for the cookie within a minute. The audit entry is written first and a
/// rescue that cannot be recorded is not issued. Superadmin by design — a
/// person's: a service's bearer token mints no admin session (403).
async fn rescue_tenant(
    State(state): State<Arc<EdgeState>>,
    Path(id): Path<String>,
    operator: axum::Extension<PlatformPrincipal>,
) -> ApiResult {
    if operator.service {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            "rescue is for a signed-in operator".to_string(),
        ));
    }
    let id = tenant_id(&id)?;
    let row = registry_row(&state, &id).await?;
    let Resolved::Tenant(runtime) = state.serving(&id) else {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "the tenant is not being served; resume it first".to_string(),
        ));
    };
    let auth = runtime.admin.as_ref().ok_or_else(|| {
        ApiError(
            StatusCode::CONFLICT,
            "the tenant has no admin surface to rescue".to_string(),
        )
    })?;
    let host = row.hostnames.first().ok_or_else(|| {
        ApiError(
            StatusCode::CONFLICT,
            "the tenant has no hostname to open its admin on".to_string(),
        )
    })?;
    let platform = state.platform().ok_or_else(ApiError::not_found)?;
    let operator = &operator.name();
    runtime
        .audit
        .try_record(
            operator,
            "rescue",
            id.as_str(),
            &format!("rescued_by={operator}"),
        )
        .await
        .map_err(|e| ApiError::internal("recording the rescue in the tenant's audit trail", e))?;
    let token = auth
        .issue_session_for(operator, RESCUE_SESSION)
        .map_err(|e| ApiError::internal("minting the rescue session", e))?;
    let code = platform.issue_rescue(id.clone(), token);
    tracing::warn!(tenant = %id, operator = %operator, "rescue session minted through the platform API");
    platform_audit(&state)
        .record(operator, "tenant.rescue", id.as_str(), host)
        .await;
    Ok(Json(serde_json::json!({
        "url": format!("https://{host}/admin/api/login/rescue?code={code}"),
        "code": code,
        "expires_in_secs": RESCUE_TTL.as_secs(),
    }))
    .into_response())
}

/// `GET /platform/api/tenants/{id}/usage` — the tenant's plan beside what it
/// has used of it, counted afresh from its stores (within the minute's
/// cache). `usage` is `null` while the tenant is not served, or when its
/// stores could not be counted, which the log reports.
async fn tenant_usage(State(state): State<Arc<EdgeState>>, Path(id): Path<String>) -> ApiResult {
    let id = tenant_id(&id)?;
    let row = registry_row(&state, &id).await?;
    let (usage, serving) = match state.serving(&id) {
        Resolved::Tenant(runtime) => match runtime.usage().await {
            Ok(usage) => (Some(usage), true),
            Err(e) => {
                tracing::warn!(tenant = %id, error = %e, "counting plan usage failed");
                (None, true)
            }
        },
        Resolved::Suspended(_) | Resolved::Unknown => (None, false),
    };
    Ok(Json(serde_json::json!({
        "plan": row.plan,
        "usage": usage,
        "serving": serving,
    }))
    .into_response())
}

#[derive(Deserialize, Default)]
struct AuditQuery {
    #[serde(default)]
    limit: Option<i64>,
    #[serde(default)]
    before: Option<i64>,
}

/// `GET /platform/api/audit?limit&before` — the platform's action log,
/// newest first: `{entries: [{id, principal, action, tenant, detail, at}]}`.
/// `limit` is 100 by default and 500 at most; `before` is the last id of
/// the page before, for the next.
async fn list_audit(
    State(state): State<Arc<EdgeState>>,
    Query(query): Query<AuditQuery>,
) -> ApiResult {
    let entries = platform_audit(&state)
        .recent(query.limit.unwrap_or(AUDIT_PAGE_DEFAULT), query.before)
        .await
        .map_err(|e| ApiError::internal("reading the platform audit log", e))?;
    Ok(Json(serde_json::json!({ "entries": entries })).into_response())
}

/// `GET /admin/api/login/rescue?code=…` on a tenant's host — exchange a
/// rescue code for that tenant's admin cookie and land on its admin. The
/// code is spent whether or not the tenant matches, so a code leaked to
/// another tenant's host is worthless afterwards too.
async fn admin_login_rescue(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(state): State<Arc<EdgeState>>,
    ServedTenant(tenant): ServedTenant,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let (Some(platform), Some(auth)) = (state.platform(), tenant.admin.as_ref()) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let ip = client_ip(state.trusted_proxies(), &headers, Some(peer.ip()));
    if !tenant.login_limiter.check(ip) {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    }
    let Some(code) = params.get("code").filter(|c| !c.is_empty()) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Some(rescue) = platform.take_rescue(code) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    if rescue.tenant != tenant.id {
        tracing::warn!(tenant = %tenant.id, minted_for = %rescue.tenant, "rescue code presented on another tenant's host");
        return StatusCode::UNAUTHORIZED.into_response();
    }
    (
        StatusCode::FOUND,
        axum::response::AppendHeaders([
            (header::SET_COOKIE, auth.session_cookie(&rescue.token)),
            (header::LOCATION, "/admin/".to_string()),
        ]),
    )
        .into_response()
}

/// `GET /internal/tls/ask?domain=<name>` — Caddy's on-demand TLS gate: 200
/// when the name is a registered hostname of a tenant being served (or the
/// platform's own), else 404, so a stranger pointing DNS at the ingress
/// gets no certificate. Answered from the hostname cache: never Postgres.
///
/// The ingress refuses `/internal/*` on every public site and calls this
/// through a loopback gate, so a request whose `Host` is a tenant's or the
/// platform's came through a public site and is refused here as well. A
/// refusal draws on the unresolved-host budget of the caller's address, so
/// a run of made-up names is cut off; a registered name costs nothing, so
/// a legitimate burst of first handshakes never trips it.
async fn tls_ask(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(state): State<Arc<EdgeState>>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> StatusCode {
    let platform = state.platform();
    if let Some(host) = request_host(state.trusted_proxies(), &headers, Some(peer.ip())) {
        let public_site = platform.is_some_and(|p| p.is_host(&host))
            || !matches!(state.resolve(&host), Resolved::Unknown);
        if public_site {
            return StatusCode::NOT_FOUND;
        }
    }
    let domain = params
        .get("domain")
        .map(|d| d.trim().trim_end_matches('.').to_ascii_lowercase())
        .unwrap_or_default();
    if !domain.is_empty()
        && (state.hostname_served(&domain) || platform.is_some_and(|p| p.is_host(&domain)))
    {
        return StatusCode::OK;
    }
    let ip = client_ip(state.trusted_proxies(), &headers, Some(peer.ip()));
    if !state.admit_unresolved(ip) {
        return StatusCode::TOO_MANY_REQUESTS;
    }
    StatusCode::NOT_FOUND
}

/// The platform routes, the tenant-side rescue exchange and the TLS gate,
/// merged into the public router before its state is attached.
pub fn router(state: Arc<EdgeState>) -> Router<Arc<EdgeState>> {
    let api = Router::new()
        .route("/platform/api/me", get(me))
        .route(
            "/platform/api/tenants",
            get(list_tenants).post(create_tenant),
        )
        .route(
            "/platform/api/tenants/{id}",
            get(get_tenant).patch(patch_tenant).delete(delete_tenant),
        )
        .route("/platform/api/tenants/{id}/suspend", post(suspend_tenant))
        .route("/platform/api/tenants/{id}/resume", post(resume_tenant))
        .route("/platform/api/tenants/{id}/hostnames", post(add_hostname))
        .route(
            "/platform/api/tenants/{id}/hostnames/{hostname}",
            axum::routing::delete(remove_hostname),
        )
        .route(
            "/platform/api/tenants/{id}/operators",
            get(list_operators).post(add_operator),
        )
        .route(
            "/platform/api/tenants/{id}/operators/{kind}/{value}",
            axum::routing::delete(remove_operator),
        )
        .route("/platform/api/tenants/{id}/rescue", post(rescue_tenant))
        .route("/platform/api/tenants/{id}/usage", get(tenant_usage))
        .route("/platform/api/audit", get(list_audit))
        .layer(axum::middleware::from_fn_with_state(state, platform_guard));
    Router::new()
        .route("/platform/api/login/oidc/start", get(login_start))
        .route("/platform/api/login/oidc/callback", get(login_callback))
        .route("/platform/api/logout", post(logout))
        .route("/platform", get(platform_root))
        .route("/platform/", get(platform_root))
        .route("/platform/{*path}", get(platform_path))
        .route("/admin/api/login/rescue", get(admin_login_rescue))
        .route("/internal/tls/ask", get(tls_ask))
        .merge(api)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::net::{IpAddr, SocketAddr};
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{header, Method, Request, StatusCode};
    use serde_json::{json, Value};
    use tower::ServiceExt;
    use wiremock::matchers::{header as header_is, method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    use workforce_common::session_postgres::{run_session_migrations, PostgresSessionStore};
    use workforce_postgres::test_support::with_test_pool;
    use workforce_postgres::PostgresPool;

    use super::*;
    use crate::admin::{AdminAuth, PLATFORM_SESSION_COOKIE, SESSION_COOKIE};
    use crate::analytics::run_edge_analytics_migrations;
    use crate::boot::SharedInfra;
    use crate::config::EdgeConfig;
    use crate::runtime::TenantRuntime;
    use crate::server::router as public_router;
    use crate::settings::tests::test_master_key;
    use crate::settings::TenantSettingsStore;
    use crate::tenant::{run_edge_migrations, TenantMode, TenantPool, TenantSettings, TenantStore};

    const ADMIN_KEY: &str = "vlv-admin-key";
    const TENANT_KEY: &str = "vlv_acme_7f3a9c1d";
    const PLATFORM_HOST: &str = "platform.test";
    const OPERATOR: &str = "ops@marrilo.com";
    /// The billing service's bearer token, as `${PLATFORM_SERVICE_TOKEN_BILLING}`
    /// would expand it.
    const BILLING_TOKEN: &str = "billing-service-token-0123456789abcdef";

    /// The cloud-shaped file: many tenants by hostname, `[vellev]`, a shared
    /// Google-mode `[admin]`, and `[platform]`.
    fn cloud_toml() -> EdgeConfig {
        toml::from_str(&format!(
            r#"
            [server]
            bind = "0.0.0.0:8080"
            multi_tenant = true
            max_concurrent_turns = 8

            [postgres]
            url = "postgres://edge/edge"

            [vellev]
            url = "http://vellev:18080"
            api_key = "{ADMIN_KEY}"
            tenant = "default"

            [llm]
            provider = "anthropic"
            allowed_providers = ["anthropic", "openai", "gemini"]

            [admin]
            session_secret = "0123456789abcdef0123456789abcdef"
            [admin.oidc]
            issuer = "https://accounts.google.com"
            client_id = "edge-admin"

            [platform]
            allowed_emails = ["{OPERATOR}"]
            allowed_domains = ["marrilo.com"]
            session_secret = "fedcba9876543210fedcba9876543210"
            service_tokens = [{{ name = "billing", token = "{BILLING_TOKEN}" }}]
            [platform.oidc]
            issuer = "https://kc.marrilo.com/realms/platform"
            client_id = "edge-platform"
            public_url = "https://{PLATFORM_HOST}"
            "#
        ))
        .expect("a valid test config")
    }

    /// A vellev node as the platform API and a new tenant's store see it:
    /// the admin key creates tenants and issues keys, the issued key may not
    /// create tenants, the memory collection does not exist until created.
    async fn vellev_mock() -> MockServer {
        let server = MockServer::start().await;
        let tenant = r"^/v1/tenants/[a-z0-9-]+$";
        let collection = r"^/v1/tenants/[a-z0-9-]+/collections/WorkforceMemory$";
        Mock::given(method("PUT"))
            .and(path_regex(tenant))
            .and(header_is("authorization", format!("Bearer {ADMIN_KEY}")))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path_regex(tenant))
            .and(header_is("authorization", format!("Bearer {TENANT_KEY}")))
            .respond_with(ResponseTemplate::new(403).set_body_json(json!({
                "error": {"code": "forbidden", "message": "a tenant key cannot create tenants"}
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/v1/tenants/[a-z0-9-]+/keys$"))
            .and(header_is("authorization", format!("Bearer {ADMIN_KEY}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "6f1c2a3b-4d5e-4f60-8a7b-9c0d1e2f3a4b",
                "key": TENANT_KEY,
                "created": "2026-10-08T09:00:00Z",
            })))
            .mount(&server)
            .await;
        // The tenant's collections, listed first so a missing tenant is told
        // from a node that is down; a fresh tenant has none.
        Mock::given(method("GET"))
            .and(path_regex(r"^/v1/tenants/[a-z0-9-]+/collections$"))
            .and(header_is("authorization", format!("Bearer {TENANT_KEY}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"collections": []})))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path_regex(collection))
            .respond_with(ResponseTemplate::new(404).set_body_json(json!({
                "error": {"code": "not_found", "message": "no such collection"}
            })))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path_regex(collection))
            .and(header_is("authorization", format!("Bearer {TENANT_KEY}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "incarnation": 1, "position": 1, "outcomes": [],
            })))
            .mount(&server)
            .await;
        // The plan's memory counter, which the tenant view reads.
        Mock::given(method("POST"))
            .and(path_regex(
                r"^/v1/tenants/[a-z0-9-]+/collections/WorkforceMemory/count$",
            ))
            .and(header_is("authorization", format!("Bearer {TENANT_KEY}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"count": 3})))
            .mount(&server)
            .await;
        // The boot-time model probe: an empty nearest search from a node that
        // embeds is what a healthy tenant answers.
        Mock::given(method("POST"))
            .and(path_regex(
                r"^/v1/tenants/[a-z0-9-]+/collections/WorkforceMemory/nearest$",
            ))
            .and(header_is("authorization", format!("Bearer {TENANT_KEY}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"results": []})))
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .and(path_regex(tenant))
            .and(header_is("authorization", format!("Bearer {ADMIN_KEY}")))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        server
    }

    /// Requests the node received, as `METHOD path` with the bearer's kind.
    async fn calls(server: &MockServer) -> Vec<String> {
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| {
                let bearer = r
                    .headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                let who = if bearer.ends_with(ADMIN_KEY) {
                    "admin"
                } else if bearer.ends_with(TENANT_KEY) {
                    "tenant"
                } else {
                    "?"
                };
                format!("{} {} ({who})", r.method, r.url.path())
            })
            .collect()
    }

    fn peer(ip: &str) -> ConnectInfo<SocketAddr> {
        ConnectInfo(SocketAddr::new(ip.parse::<IpAddr>().unwrap(), 40000))
    }

    /// A request as a browser on `host`'s own page sends it: with the
    /// same-origin word a write needs.
    fn request(
        method: Method,
        host: &str,
        path: &str,
        cookie: Option<&str>,
        body: Option<Value>,
    ) -> Request<Body> {
        let mut req = Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, host)
            .header("sec-fetch-site", "same-origin");
        if let Some(cookie) = cookie {
            req = req.header(header::COOKIE, cookie);
        }
        let body = match body {
            Some(json) => {
                req = req.header(header::CONTENT_TYPE, "application/json");
                Body::from(json.to_string())
            }
            None => Body::empty(),
        };
        req.extension(peer("203.0.113.1")).body(body).unwrap()
    }

    async fn send(state: &Arc<EdgeState>, req: Request<Body>) -> (StatusCode, Value, HeaderMap) {
        let response = public_router(Arc::clone(state)).oneshot(req).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
        };
        (status, body, headers)
    }

    fn platform_cookie(state: &EdgeState, operator: &str) -> String {
        let token = state
            .platform()
            .expect("[platform] set")
            .auth()
            .issue_session(operator)
            .unwrap();
        format!("{PLATFORM_SESSION_COOKIE}={token}")
    }

    /// A platform request as the staff send it: on the platform host, with
    /// a platform session.
    fn api(state: &EdgeState, method: Method, path: &str, body: Option<Value>) -> Request<Body> {
        let cookie = platform_cookie(state, OPERATOR);
        request(method, PLATFORM_HOST, path, Some(&cookie), body)
    }

    /// A platform request as a service sends it: on the platform host with
    /// a bearer token, and nothing a browser would add — no cookie, no
    /// `Sec-Fetch-Site`, no `Origin`.
    fn service_api(method: Method, path: &str, token: &str, body: Option<Value>) -> Request<Body> {
        let mut req = Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, PLATFORM_HOST)
            .header(header::AUTHORIZATION, format!("Bearer {token}"));
        let body = match body {
            Some(json) => {
                req = req.header(header::CONTENT_TYPE, "application/json");
                Body::from(json.to_string())
            }
            None => Body::empty(),
        };
        req.extension(peer("203.0.113.1")).body(body).unwrap()
    }

    /// `req` with one more header.
    fn with_header(mut req: Request<Body>, name: header::HeaderName, value: &str) -> Request<Body> {
        req.headers_mut().insert(name, value.parse().unwrap());
        req
    }

    /// A multi-tenant state over a migrated database, `default` on
    /// `default.test`, the platform on `platform.test`, vellev at `node`.
    async fn cloud_state(
        pool: PostgresPool,
        node: &MockServer,
        artifacts: &std::path::Path,
    ) -> Arc<EdgeState> {
        cloud_state_on(pool, node, artifacts, cloud_toml()).await
    }

    /// [`cloud_state`] booted from `config` instead of [`cloud_toml`].
    async fn cloud_state_on(
        pool: PostgresPool,
        node: &MockServer,
        artifacts: &std::path::Path,
        config: EdgeConfig,
    ) -> Arc<EdgeState> {
        run_session_migrations(&pool).await.unwrap();
        run_edge_analytics_migrations(&pool).await.unwrap();
        run_edge_migrations(&pool).await.unwrap();
        let registry = TenantStore::new(pool.clone());
        registry.ensure_default().await.unwrap();
        registry
            .add_hostname(&TenantId::default_tenant(), "default.test")
            .await
            .unwrap();
        let mut infra = SharedInfra::for_tests_on(pool)
            .with_master_key(test_master_key())
            .with_vellev(&node.uri(), ADMIN_KEY)
            .with_artifacts(artifacts);
        infra.rls_enforced = true;
        EdgeState::build(&config, Arc::new(infra))
            .await
            .expect("the cloud shape boots")
    }

    async fn create_acme(state: &Arc<EdgeState>) -> Value {
        let (status, body, _) = send(
            state,
            api(
                state,
                Method::POST,
                "/platform/api/tenants",
                Some(json!({"id": "acme", "display_name": "Acme Ltd", "hostnames": ["Acme.test"]})),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        body
    }

    #[test]
    fn a_platform_section_needs_staff_and_a_platform_url() {
        let mut cfg = cloud_toml().platform.unwrap();
        assert!(PlatformState::new(&cfg, OutboundPolicy::Public, None).is_ok());
        cfg.allowed_emails.clear();
        cfg.allowed_domains.clear();
        let Err(err) = PlatformState::new(&cfg, OutboundPolicy::Public, None) else {
            panic!("nobody could sign in");
        };
        assert!(format!("{err:#}").contains("allowed_emails"), "{err:#}");
        let mut cfg = cloud_toml().platform.unwrap();
        cfg.oidc.public_url = "platform.test".into();
        let Err(err) = PlatformState::new(&cfg, OutboundPolicy::Public, None) else {
            panic!("not a base URL");
        };
        assert!(format!("{err:#}").contains("public_url"), "{err:#}");
        let platform = PlatformState::new(
            &cloud_toml().platform.unwrap(),
            OutboundPolicy::Public,
            None,
        )
        .unwrap();
        assert!(platform.is_host("PLATFORM.test"));
        assert!(platform.is_allowed(OPERATOR));
        assert!(platform.is_allowed("anyone@marrilo.com"));
        assert!(!platform.is_allowed("anyone@marrilo.com.evil"));
        assert!(!platform.is_allowed("ops@acme.test"));
        assert!(!format!("{:?}", cloud_toml().platform.unwrap()).contains("fedcba98"));
    }

    /// `service_tokens` is checked as the file is parsed: a token under 32
    /// characters or without a name never reaches a running platform, two
    /// entries of one name do not boot, and no `{:?}` prints a token.
    #[test]
    fn a_service_token_is_refused_at_parse_when_short_or_nameless_and_is_never_printed() {
        let platform_with = |tokens: &str| {
            toml::from_str::<PlatformConfig>(&format!(
                r#"
                allowed_emails = ["{OPERATOR}"]
                service_tokens = {tokens}
                [oidc]
                issuer = "https://kc.marrilo.com/realms/platform"
                client_id = "edge-platform"
                public_url = "https://{PLATFORM_HOST}"
                "#
            ))
        };
        let short =
            platform_with(r#"[{ name = "billing", token = "0123456789abcdef0123456789abcde" }]"#)
                .expect_err("31 characters is too short");
        assert!(short.to_string().contains("at least 32"), "{short}");
        let nameless = platform_with(&format!(r#"[{{ name = " ", token = "{BILLING_TOKEN}" }}]"#))
            .expect_err("a token needs a name");
        assert!(nameless.to_string().contains("needs a name"), "{nameless}");

        let config = cloud_toml().platform.unwrap();
        assert_eq!(config.service_tokens.len(), 1);
        assert_eq!(config.service_tokens[0].name, "billing");
        assert_eq!(config.service_tokens[0].token, BILLING_TOKEN);
        let printed = format!("{config:?}");
        assert!(printed.contains("billing"), "{printed}");
        assert!(!printed.contains(BILLING_TOKEN), "{printed}");
        assert!(
            !format!("{:?}", config.service_tokens[0]).contains(BILLING_TOKEN),
            "the entry's own Debug redacts too"
        );

        let twice = platform_with(&format!(
            r#"[{{ name = "billing", token = "{BILLING_TOKEN}" }}, {{ name = "billing", token = "{BILLING_TOKEN}-again" }}]"#
        ))
        .unwrap();
        let Err(err) = PlatformState::new(&twice, OutboundPolicy::Public, None) else {
            panic!("one name, one token");
        };
        assert!(format!("{err:#}").contains("twice"), "{err:#}");
    }

    /// A bearer of a configured service token passes the guard with none of
    /// what a browser sends — no cookie, no `Sec-Fetch-Site`, no `Origin` —
    /// and acts as `service:<name>`. Any other bearer is 401, and a valid
    /// operator cookie beside it does not rescue the request: the bearer
    /// wins.
    #[tokio::test]
    async fn a_service_bearer_passes_the_guard_and_a_wrong_one_is_401_whatever_cookie_rides_along()
    {
        let infra = Arc::new(SharedInfra::for_tests());
        let state = EdgeState::for_tests(TenantMode::Multi, vec![], infra, cloud_toml(), 1);

        let (status, body, _) = send(
            &state,
            service_api(Method::GET, "/platform/api/me", BILLING_TOKEN, None),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["operator"], "service:billing");
        assert_eq!(body["role"], "platform");

        let wrong = format!("{BILLING_TOKEN}x");
        let (status, _, _) = send(
            &state,
            service_api(Method::GET, "/platform/api/me", &wrong, None),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "a token nobody holds");
        let (status, _, _) = send(
            &state,
            service_api(Method::GET, "/platform/api/me", "", None),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "an empty bearer is no bearer, and there is no cookie either"
        );

        let cookie = platform_cookie(&state, OPERATOR);
        let (status, _, _) = send(
            &state,
            with_header(
                service_api(Method::GET, "/platform/api/me", &wrong, None),
                header::COOKIE,
                &cookie,
            ),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "a wrong bearer is refused even beside a valid operator session"
        );
        let (status, body, _) = send(
            &state,
            with_header(
                service_api(Method::GET, "/platform/api/me", BILLING_TOKEN, None),
                header::COOKIE,
                &cookie,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            body["operator"], "service:billing",
            "the bearer names the principal, not the cookie"
        );

        let (status, _, _) = send(
            &state,
            with_header(
                request(Method::GET, "acme.test", "/platform/api/me", None, None),
                header::AUTHORIZATION,
                &format!("Bearer {BILLING_TOKEN}"),
            ),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "a service too reaches the platform on its host only"
        );
    }

    /// The billing service, by bearer: it lists and creates tenants and
    /// grants their operators, each change on the tenant's audit trail as
    /// `service:billing` — and it cannot rescue, which mints a person's
    /// admin session.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_service_creates_and_manages_tenants_by_bearer_is_audited_by_name_and_cannot_rescue()
    {
        with_test_pool(PostgresSessionStore::build_pool, |pool| async move {
            let node = vellev_mock().await;
            let tmp = tempfile::tempdir().unwrap();
            let state = cloud_state(pool.clone(), &node, tmp.path()).await;

            let (status, body, _) = send(
                &state,
                service_api(Method::GET, "/platform/api/tenants", BILLING_TOKEN, None),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(body.as_array().map(Vec::len), Some(1), "default only: {body}");

            let (status, body, _) = send(
                &state,
                service_api(
                    Method::POST,
                    "/platform/api/tenants",
                    BILLING_TOKEN,
                    Some(json!({"id": "acme", "display_name": "Acme Ltd", "hostnames": ["acme.test"]})),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::CREATED, "{body}");
            assert_eq!(body["id"], "acme");
            assert_eq!(body["serving"], true, "{body}");

            let (status, body, _) = send(
                &state,
                service_api(
                    Method::POST,
                    "/platform/api/tenants/acme/operators",
                    BILLING_TOKEN,
                    Some(json!({"kind": "email", "value": "alice@acme.test"})),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::CREATED, "{body}");

            let (status, body, _) = send(
                &state,
                service_api(
                    Method::POST,
                    "/platform/api/tenants/acme/rescue",
                    BILLING_TOKEN,
                    None,
                ),
            )
            .await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
            assert_eq!(body["error"], "rescue is for a signed-in operator");

            let Resolved::Tenant(runtime) = state.resolve("acme.test") else {
                panic!("serving");
            };
            let audit: Vec<(String, String, String)> = runtime
                .audit
                .recent(10)
                .await
                .unwrap()
                .into_iter()
                .map(|row| (row.operator, row.action, row.target))
                .collect();
            assert_eq!(
                audit,
                [(
                    "service:billing".to_string(),
                    "operator.add".to_string(),
                    "email:alice@acme.test".to_string(),
                )],
                "the service is on the trail by name, and the refused rescue is not"
            );

            let (status, body, _) = send(
                &state,
                api(&state, Method::POST, "/platform/api/tenants/acme/rescue", None),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "an operator still rescues: {body}");
        })
        .await;
    }

    /// Without `[platform].session_secret` the platform signs under a key
    /// derived from the master key's session root: two builds on the same
    /// root (a restart) honour each other's sessions, a tenant keyed from
    /// the same root does not, and a file secret still takes precedence.
    #[test]
    fn the_platform_session_key_comes_from_the_session_root_when_the_file_sets_none() {
        let root = crate::boot::test_install_keys().session_root;
        let mut cfg = cloud_toml().platform.unwrap();
        cfg.session_secret = None;
        let first = PlatformState::new(&cfg, OutboundPolicy::Public, Some(&root)).unwrap();
        let token = first.auth().issue_session(OPERATOR).unwrap();
        let restarted = PlatformState::new(&cfg, OutboundPolicy::Public, Some(&root)).unwrap();
        assert_eq!(
            restarted.auth().verify_platform_session(&token).as_deref(),
            Some(OPERATOR),
            "the same root signs the same key across restarts"
        );
        let per_boot = PlatformState::new(&cfg, OutboundPolicy::Public, None).unwrap();
        assert!(
            per_boot.auth().session_role(&token).is_none(),
            "no root, no stable key"
        );
        let acme = AdminAuth::new(
            &AdminConfig {
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
                session_secret: Some(root.tenant_session_secret(&TenantId::parse("acme").unwrap())),
                public_url: None,
                session_hours: 1,
            },
            TenantId::parse("acme").unwrap(),
            OutboundPolicy::Private,
        )
        .unwrap();
        assert!(
            acme.session_role(&token).is_none(),
            "a tenant's root-derived key is not the platform's"
        );
        let with_secret = PlatformState::new(
            &cloud_toml().platform.unwrap(),
            OutboundPolicy::Public,
            Some(&root),
        )
        .unwrap();
        assert!(
            with_secret.auth().session_role(&token).is_none(),
            "the file's secret wins over the root"
        );
    }

    /// Without `[platform]` nothing under `/platform` exists, and with it
    /// the guard wants a platform session on the platform host: 401 without
    /// one, 403 for an admin session under the platform key, 404 on a
    /// tenant's host however the session reads.
    #[tokio::test]
    async fn platform_routes_are_404_without_a_section_and_the_guard_wants_a_platform_session() {
        let infra = Arc::new(SharedInfra::for_tests());
        let mut self_hosted = cloud_toml();
        self_hosted.platform = None;
        let state = EdgeState::for_tests(
            TenantMode::Multi,
            vec![],
            Arc::clone(&infra),
            self_hosted,
            1,
        );
        let (status, _, _) = send(
            &state,
            request(
                Method::GET,
                PLATFORM_HOST,
                "/platform/api/tenants",
                None,
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "no section, no surface");
        let (status, _, _) = send(
            &state,
            request(
                Method::GET,
                PLATFORM_HOST,
                "/platform/api/login/oidc/start",
                None,
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let cfg = cloud_toml();
        let settings = TenantSettings {
            id: TenantId::parse("acme").unwrap(),
            name: "Acme".into(),
            hostnames: vec!["acme.test".into()],
            ..TenantSettings::from_edge_toml(&cfg)
        };
        let acme = TenantRuntime::for_tests(settings, HashMap::new(), &infra);
        let state = EdgeState::for_tests(
            TenantMode::Multi,
            vec![(acme, vec!["acme.test"])],
            infra,
            cfg,
            1,
        );
        let (status, _, _) = send(
            &state,
            request(
                Method::GET,
                PLATFORM_HOST,
                "/platform/api/tenants",
                None,
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "no session");

        // A tenant admin signing under the platform's own secret: the one
        // way an admin session could ever verify under the platform key.
        let platform_auth = state.platform().unwrap().auth();
        let admin_under_platform_key = AdminAuth::new(
            &AdminConfig {
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
                session_secret: Some("fedcba9876543210fedcba9876543210".into()),
                public_url: None,
                session_hours: 1,
            },
            TenantId::parse("acme").unwrap(),
            OutboundPolicy::Private,
        )
        .unwrap();
        let admin_token = admin_under_platform_key
            .issue_session("alice@acme.test")
            .unwrap();
        assert!(
            platform_auth.session_role(&admin_token).is_some(),
            "the token verifies under the platform key"
        );
        let (status, _, _) = send(
            &state,
            request(
                Method::GET,
                PLATFORM_HOST,
                "/platform/api/me",
                Some(&format!("{PLATFORM_SESSION_COOKIE}={admin_token}")),
                None,
            ),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "an admin session is the wrong kind"
        );

        let platform_token = platform_auth.issue_session(OPERATOR).unwrap();
        let (status, _, _) = send(
            &state,
            request(
                Method::GET,
                "acme.test",
                "/admin/api/me",
                Some(&format!("{SESSION_COOKIE}={platform_token}")),
                None,
            ),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "a platform session opens no admin"
        );

        let (status, body, _) =
            send(&state, api(&state, Method::GET, "/platform/api/me", None)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["operator"], OPERATOR);
        assert_eq!(body["role"], "platform");
        let cookie = platform_cookie(&state, OPERATOR);
        let (status, _, _) = send(
            &state,
            request(
                Method::GET,
                "acme.test",
                "/platform/api/me",
                Some(&cookie),
                None,
            ),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "the platform answers on its own host only"
        );
        let cookie_line = platform_auth.session_cookie("t");
        assert!(
            cookie_line.starts_with(PLATFORM_SESSION_COOKIE),
            "{cookie_line}"
        );
        assert!(cookie_line.contains("Path=/platform;"), "{cookie_line}");
    }

    /// Create: the row with an empty plan, the vellev tenant created and a
    /// key issued under the admin key, the key sealed into the settings row
    /// and opened by the store, the hostname registered, the runtime
    /// serving on it with that key. A second create is a 409.
    #[tokio::test(flavor = "multi_thread")]
    async fn creating_a_tenant_provisions_vellev_seals_its_key_and_serves_it() {
        with_test_pool(PostgresSessionStore::build_pool, |pool| async move {
            let node = vellev_mock().await;
            let tmp = tempfile::tempdir().unwrap();
            let state = cloud_state(pool.clone(), &node, tmp.path()).await;
            let body = create_acme(&state).await;
            assert_eq!(body["id"], "acme");
            assert_eq!(body["display_name"], "Acme Ltd");
            assert_eq!(body["status"], "active");
            assert_eq!(body["hostnames"], json!(["acme.test"]));
            assert_eq!(body["plan"], json!({}));
            assert_eq!(body["has_settings"], true);
            assert_eq!(body["has_admin"], true, "the file's shared [admin]: {body}");
            assert_eq!(body["has_admin_client_secret"], false);
            assert_eq!(body["serving"], true, "{body}");
            assert_eq!(body["llm_status"]["state"], "unset");
            assert!(body["created_at"].is_string());
            assert_eq!(
                body["usage"],
                json!({
                    "turns_this_month": 0,
                    "active_users_this_month": 0,
                    "knowledge_chunks": 0,
                    "memory_objects": null,
                    "agents": 0,
                }),
                "the view counts the new tenant's own stores; the vector store only under a \
                 plan that caps it: {body}"
            );

            let made = calls(&node).await;
            assert_eq!(
                made,
                [
                    "PUT /v1/tenants/acme (admin)",
                    "POST /v1/tenants/acme/keys (admin)",
                    "GET /v1/tenants/acme/collections (tenant)",
                    "GET /v1/tenants/acme/collections/WorkforceMemory (tenant)",
                    "PUT /v1/tenants/acme/collections/WorkforceMemory (tenant)",
                    "POST /v1/tenants/acme/collections/WorkforceMemory/nearest (tenant)",
                ],
                "the store runs on the tenant's own key, probes its model, and is not counted uncapped"
            );

            let (status, body, _) = send(
                &state,
                api(
                    &state,
                    Method::PATCH,
                    "/platform/api/tenants/acme",
                    Some(json!({"plan": {"memory_objects": 100}})),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(
                body["usage"]["memory_objects"],
                3,
                "a plan that caps the store counts it: {body}"
            );
            assert!(
                calls(&node).await.contains(
                    &"POST /v1/tenants/acme/collections/WorkforceMemory/count (tenant)"
                        .to_string()
                ),
                "on the tenant's own key"
            );

            let acme = TenantId::parse("acme").unwrap();
            let raw: Vec<u8> = pool
                .get()
                .await
                .unwrap()
                .query_one(
                    "SELECT vellev_api_key FROM tenant_settings WHERE tenant_id = 'acme'",
                    &[],
                )
                .await
                .unwrap()
                .get(0);
            assert!(
                !raw.windows(TENANT_KEY.len()).any(|w| w == TENANT_KEY.as_bytes()),
                "the key is sealed at rest"
            );
            let store = TenantSettingsStore::new(TenantPool::new(pool.clone()), acme.clone(), test_master_key());
            assert_eq!(store.vellev_api_key().await.unwrap().as_deref(), Some(TENANT_KEY));
            assert!(matches!(state.resolve("acme.test"), Resolved::Tenant(_)));
            let (status, _, _) = send(
                &state,
                request(Method::GET, "acme.test", "/admin/api/me", None, None),
            )
            .await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "the admin surface is up, wanting a session");

            let (status, body, _) = send(
                &state,
                api(
                    &state,
                    Method::POST,
                    "/platform/api/tenants",
                    Some(json!({"id": "acme", "display_name": "Again"})),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::CONFLICT, "{body}");
            let (status, body, _) = send(
                &state,
                api(
                    &state,
                    Method::POST,
                    "/platform/api/tenants",
                    Some(json!({"id": "Not A Slug", "display_name": "x"})),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
            let (status, body, _) = send(
                &state,
                api(
                    &state,
                    Method::POST,
                    "/platform/api/tenants",
                    Some(json!({"id": "contoso", "display_name": "Contoso", "hostnames": ["acme.test"]})),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::CONFLICT, "a hostname names one tenant: {body}");
            assert!(state.infra().tenants.get(&TenantId::parse("contoso").unwrap()).await.unwrap().is_none());

            let (status, body, _) = send(&state, api(&state, Method::GET, "/platform/api/tenants", None)).await;
            assert_eq!(status, StatusCode::OK);
            let ids: Vec<&str> = body.as_array().unwrap().iter().map(|t| t["id"].as_str().unwrap()).collect();
            assert_eq!(ids, ["acme", "default"]);
            let (status, body, _) = send(&state, api(&state, Method::GET, "/platform/api/tenants/acme", None)).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body["id"], "acme");
            let (status, _, _) = send(&state, api(&state, Method::GET, "/platform/api/tenants/nobody", None)).await;
            assert_eq!(status, StatusCode::NOT_FOUND);
        })
        .await;
    }

    /// A vellev that refuses the key leaves no row, no settings, no
    /// hostname, no runtime — and the vellev tenant it made is dropped.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_vellev_failure_leaves_no_tenant_behind() {
        with_test_pool(PostgresSessionStore::build_pool, |pool| async move {
            let node = MockServer::start().await;
            Mock::given(method("PUT"))
                .and(path_regex(r"^/v1/tenants/acme$"))
                .respond_with(ResponseTemplate::new(200))
                .mount(&node)
                .await;
            Mock::given(method("POST"))
                .and(path_regex(r"^/v1/tenants/acme/keys$"))
                .respond_with(ResponseTemplate::new(403).set_body_json(json!({
                    "error": {"code": "forbidden", "message": "not an admin key"}
                })))
                .mount(&node)
                .await;
            Mock::given(method("DELETE"))
                .and(path_regex(r"^/v1/tenants/acme$"))
                .respond_with(ResponseTemplate::new(200))
                .expect(1)
                .mount(&node)
                .await;
            let tmp = tempfile::tempdir().unwrap();
            let state = cloud_state(pool.clone(), &node, tmp.path()).await;
            let (status, body, _) = send(
                &state,
                api(
                    &state,
                    Method::POST,
                    "/platform/api/tenants",
                    Some(json!({"id": "acme", "display_name": "Acme", "hostnames": ["acme.test"]})),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
            assert!(body["error"].as_str().unwrap().contains("vellev"), "{body}");
            let acme = TenantId::parse("acme").unwrap();
            assert!(
                state.infra().tenants.get(&acme).await.unwrap().is_none(),
                "no row"
            );
            assert_eq!(
                state
                    .infra()
                    .tenants
                    .resolve_hostname("acme.test")
                    .await
                    .unwrap(),
                None
            );
            assert!(matches!(state.resolve("acme.test"), Resolved::Unknown));
            assert_eq!(
                calls(&node).await,
                [
                    "PUT /v1/tenants/acme (admin)",
                    "POST /v1/tenants/acme/keys (admin)",
                    "DELETE /v1/tenants/acme (admin)"
                ]
            );
            let (status, _, _) = send(
                &state,
                api(&state, Method::GET, "/platform/api/tenants", None),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn suspend_and_resume_change_the_registry_and_what_the_host_answers() {
        with_test_pool(PostgresSessionStore::build_pool, |pool| async move {
            let node = vellev_mock().await;
            let tmp = tempfile::tempdir().unwrap();
            let state = cloud_state(pool.clone(), &node, tmp.path()).await;
            create_acme(&state).await;
            let acme = TenantId::parse("acme").unwrap();

            let (status, body, _) = send(
                &state,
                api(
                    &state,
                    Method::POST,
                    "/platform/api/tenants/acme/suspend",
                    Some(json!({"message": "Account on hold."})),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(body["status"], "suspended");
            assert_eq!(body["serving"], false);
            assert_eq!(body["unavailable"], "Account on hold.");
            let Resolved::Suspended(message) = state.resolve("acme.test") else {
                panic!("suspended");
            };
            assert_eq!(message, "Account on hold.");
            assert_eq!(
                state
                    .infra()
                    .tenants
                    .get(&acme)
                    .await
                    .unwrap()
                    .unwrap()
                    .status,
                TenantStatus::Suspended
            );
            let (status, body, _) = send(
                &state,
                request(
                    Method::GET,
                    "acme.test",
                    "/public/conversations/support",
                    None,
                    None,
                ),
            )
            .await;
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(body["message"], "Account on hold.");

            let (status, body, _) = send(
                &state,
                api(
                    &state,
                    Method::POST,
                    "/platform/api/tenants/acme/resume",
                    None,
                ),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(body["status"], "active");
            assert_eq!(body["serving"], true);
            assert!(matches!(state.resolve("acme.test"), Resolved::Tenant(_)));
            assert_eq!(
                state
                    .infra()
                    .tenants
                    .get(&acme)
                    .await
                    .unwrap()
                    .unwrap()
                    .status,
                TenantStatus::Active
            );

            let (status, _, _) = send(
                &state,
                api(
                    &state,
                    Method::POST,
                    "/platform/api/tenants/acme/suspend",
                    Some(json!({})),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "no body means the default message");
            let Resolved::Suspended(message) = state.resolve("acme.test") else {
                panic!("suspended");
            };
            assert_eq!(message, crate::server::SUSPENDED_MESSAGE);
            let (status, _, _) = send(
                &state,
                api(
                    &state,
                    Method::POST,
                    "/platform/api/tenants/nobody/suspend",
                    Some(json!({})),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::NOT_FOUND);
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn hostnames_reach_the_tenant_the_moment_they_change_without_a_reload() {
        with_test_pool(PostgresSessionStore::build_pool, |pool| async move {
            let node = vellev_mock().await;
            let tmp = tempfile::tempdir().unwrap();
            let state = cloud_state(pool.clone(), &node, tmp.path()).await;
            create_acme(&state).await;
            let Resolved::Tenant(before) = state.resolve("acme.test") else {
                panic!("serving");
            };

            let (status, body, _) = send(
                &state,
                api(
                    &state,
                    Method::POST,
                    "/platform/api/tenants/acme/hostnames",
                    Some(json!({"hostname": "Chat.Acme.example"})),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(body["hostnames"], json!(["acme.test", "chat.acme.example"]));
            let Resolved::Tenant(after) = state.resolve("chat.acme.example") else {
                panic!("the new hostname reaches the tenant at once");
            };
            assert!(Arc::ptr_eq(&before, &after), "the same runtime: no reload");

            for (host, status) in [
                ("platform.test", StatusCode::BAD_REQUEST),
                ("not a host", StatusCode::BAD_REQUEST),
                ("https://acme.example", StatusCode::BAD_REQUEST),
                ("default.test", StatusCode::CONFLICT),
            ] {
                let (got, body, _) = send(
                    &state,
                    api(
                        &state,
                        Method::POST,
                        "/platform/api/tenants/acme/hostnames",
                        Some(json!({"hostname": host})),
                    ),
                )
                .await;
                assert_eq!(got, status, "{host}: {body}");
            }

            let (status, body, _) = send(
                &state,
                api(
                    &state,
                    Method::DELETE,
                    "/platform/api/tenants/acme/hostnames/acme.test",
                    None,
                ),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(body["hostnames"], json!(["chat.acme.example"]));
            assert!(matches!(state.resolve("acme.test"), Resolved::Unknown));
            assert!(matches!(
                state.resolve("chat.acme.example"),
                Resolved::Tenant(_)
            ));
            let (status, _, _) = send(
                &state,
                api(
                    &state,
                    Method::DELETE,
                    "/platform/api/tenants/acme/hostnames/default.test",
                    None,
                ),
            )
            .await;
            assert_eq!(
                status,
                StatusCode::NOT_FOUND,
                "another tenant's hostname is not acme's to remove"
            );
            assert!(matches!(state.resolve("default.test"), Resolved::Tenant(_)));
        })
        .await;
    }

    /// Delete: confirmation required, `default` refused; otherwise the
    /// runtime is gone, vellev is asked to drop the tenant, the artifact
    /// directory is removed, and every row of the tenant's — registry,
    /// settings, hostnames, edge tables — is deleted.
    #[tokio::test(flavor = "multi_thread")]
    async fn deleting_a_tenant_drops_its_runtime_vellev_tenant_files_and_rows() {
        with_test_pool(PostgresSessionStore::build_pool, |pool| async move {
            let node = vellev_mock().await;
            let tmp = tempfile::tempdir().unwrap();
            let state = cloud_state(pool.clone(), &node, tmp.path()).await;
            create_acme(&state).await;
            let acme = TenantId::parse("acme").unwrap();
            let Resolved::Tenant(runtime) = state.resolve("acme.test") else {
                panic!("serving");
            };
            runtime.operators.add("email", "alice@acme.test", OPERATOR).await.unwrap();
            runtime.audit.record(OPERATOR, "test", "x", "").await;
            let conn = TenantPool::new(pool.clone()).conn(&acme).await.unwrap();
            conn.execute(
                "INSERT INTO edge_agents (tenant_id, id, config) VALUES ('acme', 'support', '{}')",
                &[],
            )
            .await
            .unwrap();
            conn.execute(
                "INSERT INTO edge_analytics.chat_identities (tenant_id, owner) VALUES ('acme', 'o')",
                &[],
            )
            .await
            .unwrap();
            conn.commit().await.unwrap();
            let files = tmp.path().join("acme/support");
            std::fs::create_dir_all(&files).unwrap();
            std::fs::write(files.join("a1.json"), "[]").unwrap();

            let (status, body, _) = send(
                &state,
                api(&state, Method::DELETE, "/platform/api/tenants/acme", None),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
            let (status, body, _) = send(
                &state,
                api(&state, Method::DELETE, "/platform/api/tenants/acme?confirm=contoso", None),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
            let (status, body, _) = send(
                &state,
                api(&state, Method::DELETE, "/platform/api/tenants/default?confirm=default", None),
            )
            .await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
            assert!(matches!(state.resolve("acme.test"), Resolved::Tenant(_)), "nothing happened yet");

            let (status, body, _) = send(
                &state,
                api(&state, Method::DELETE, "/platform/api/tenants/acme?confirm=acme", None),
            )
            .await;
            assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
            assert!(runtime.is_retired());
            assert!(matches!(state.resolve("acme.test"), Resolved::Unknown));
            assert!(calls(&node).await.contains(&"DELETE /v1/tenants/acme (admin)".to_string()));
            assert!(!tmp.path().join("acme").exists(), "the artifact root is gone");
            let conn = pool.get().await.unwrap();
            for (table, column) in [
                ("tenants", "id"),
                ("tenant_settings", "tenant_id"),
                ("tenant_hostnames", "tenant_id"),
                ("edge_agents", "tenant_id"),
                ("edge_operators", "tenant_id"),
                ("edge_audit", "tenant_id"),
                ("edge_analytics.chat_identities", "tenant_id"),
            ] {
                let n: i64 = conn
                    .query_one(
                        &format!("SELECT count(*) FROM {table} WHERE {column} = 'acme'"),
                        &[],
                    )
                    .await
                    .unwrap()
                    .get(0);
                assert_eq!(n, 0, "{table} still holds acme's rows");
            }
            let (status, _, _) = send(
                &state,
                api(&state, Method::GET, "/platform/api/tenants/acme", None),
            )
            .await;
            assert_eq!(status, StatusCode::NOT_FOUND);
        })
        .await;
    }

    /// A rescue mints the tenant's own admin session for the platform
    /// operator — for an hour, not the tenant's working day — recorded in
    /// the tenant's audit trail, behind a code that works once, on that
    /// tenant's host, within a minute.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_rescue_code_opens_the_tenants_admin_once_on_its_host_and_expires() {
        with_test_pool(PostgresSessionStore::build_pool, |pool| async move {
            let node = vellev_mock().await;
            let tmp = tempfile::tempdir().unwrap();
            let state = cloud_state(pool.clone(), &node, tmp.path()).await;
            create_acme(&state).await;

            let (status, body, _) = send(
                &state,
                api(
                    &state,
                    Method::POST,
                    "/platform/api/tenants/acme/rescue",
                    None,
                ),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let code = body["code"].as_str().unwrap().to_string();
            assert_eq!(
                body["url"],
                format!("https://acme.test/admin/api/login/rescue?code={code}")
            );
            assert_eq!(body["expires_in_secs"], 60);
            let Resolved::Tenant(runtime) = state.resolve("acme.test") else {
                panic!("serving");
            };
            let audit = runtime.audit.recent(10).await.unwrap();
            let row = audit
                .iter()
                .find(|e| e.action == "rescue")
                .expect("an audit row");
            assert_eq!(row.operator, OPERATOR);
            assert_eq!(row.detail, format!("rescued_by={OPERATOR}"));
            let minted = state
                .platform()
                .unwrap()
                .rescues
                .lock()
                .unwrap()
                .get(&code)
                .map(|rescue| rescue.token.clone())
                .expect("the code holds a session");
            let now = chrono::Utc::now().timestamp() as usize;
            let exp = runtime
                .admin
                .as_ref()
                .unwrap()
                .session_expiry(&minted)
                .expect("the tenant's own authenticator signed it");
            assert!(
                (now + 3500..=now + 3600).contains(&exp),
                "an hour, under the tenant's 12: exp {exp}, now {now}"
            );

            let exchange = |host: &str, code: &str| {
                request(
                    Method::GET,
                    host,
                    &format!("/admin/api/login/rescue?code={code}"),
                    None,
                    None,
                )
            };
            let (status, _, _) = send(&state, exchange("default.test", &code)).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "another tenant's host");
            let (status, _, _) = send(&state, exchange("acme.test", &code)).await;
            assert_eq!(
                status,
                StatusCode::UNAUTHORIZED,
                "and the code is spent by the attempt"
            );

            let (_, body, _) = send(
                &state,
                api(
                    &state,
                    Method::POST,
                    "/platform/api/tenants/acme/rescue",
                    None,
                ),
            )
            .await;
            let code = body["code"].as_str().unwrap().to_string();
            let (status, _, headers) = send(&state, exchange("acme.test", &code)).await;
            assert_eq!(status, StatusCode::FOUND);
            assert_eq!(headers.get(header::LOCATION).unwrap(), "/admin/");
            let cookie = headers
                .get(header::SET_COOKIE)
                .and_then(|v| v.to_str().ok())
                .expect("the tenant's admin cookie");
            assert!(cookie.starts_with(SESSION_COOKIE), "{cookie}");
            let session = cookie.split(';').next().unwrap();
            let (status, body, _) = send(
                &state,
                request(
                    Method::GET,
                    "acme.test",
                    "/admin/api/me",
                    Some(session),
                    None,
                ),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(body["operator"], OPERATOR);
            assert_eq!(body["tenant"]["id"], "acme");
            let (status, _, _) = send(&state, exchange("acme.test", &code)).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "single use");

            let (_, body, _) = send(
                &state,
                api(
                    &state,
                    Method::POST,
                    "/platform/api/tenants/acme/rescue",
                    None,
                ),
            )
            .await;
            let code = body["code"].as_str().unwrap().to_string();
            state.platform().unwrap().expire_rescues();
            let (status, _, _) = send(&state, exchange("acme.test", &code)).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "expired");
            let (status, _, _) = send(&state, exchange("acme.test", "")).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);

            let (status, _, _) = send(
                &state,
                api(
                    &state,
                    Method::POST,
                    "/platform/api/tenants/acme/suspend",
                    Some(json!({})),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            let (status, _, _) = send(
                &state,
                api(
                    &state,
                    Method::POST,
                    "/platform/api/tenants/acme/rescue",
                    None,
                ),
            )
            .await;
            assert_eq!(
                status,
                StatusCode::CONFLICT,
                "a suspended tenant has no admin to open"
            );
        })
        .await;
    }

    /// The TLS gate: 200 for a served tenant's registered name (case
    /// aside) and the platform's own, 404 for an unknown or a suspended
    /// tenant's name, 404 whatever the name when the request came through
    /// a public site, and a budget on refusals.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_tls_gate_answers_for_served_hostnames_from_the_loopback_only() {
        with_test_pool(PostgresSessionStore::build_pool, |pool| async move {
            let node = vellev_mock().await;
            let tmp = tempfile::tempdir().unwrap();
            let state = cloud_state(pool.clone(), &node, tmp.path()).await;
            create_acme(&state).await;
            let ask = |host: &str, domain: &str, ip: &str| {
                let mut req = request(
                    Method::GET,
                    host,
                    &format!("/internal/tls/ask?domain={domain}"),
                    None,
                    None,
                );
                req.extensions_mut().insert(peer(ip));
                req
            };
            let gate = "127.0.0.1:9080";
            for (domain, expected) in [
                ("acme.test", StatusCode::OK),
                ("ACME.test.", StatusCode::OK),
                ("default.test", StatusCode::OK),
                (PLATFORM_HOST, StatusCode::OK),
                ("nobody.test", StatusCode::NOT_FOUND),
                ("", StatusCode::NOT_FOUND),
            ] {
                let (status, _, _) = send(&state, ask(gate, domain, "127.0.0.1")).await;
                assert_eq!(status, expected, "{domain:?}");
            }
            let (status, _, _) = send(&state, ask("acme.test", "acme.test", "127.0.0.1")).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "through a public site");
            let (status, _, _) = send(&state, ask(PLATFORM_HOST, "acme.test", "127.0.0.1")).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "through the platform site");

            state
                .suspend(&TenantId::parse("acme").unwrap(), "hold".into())
                .await;
            let (status, _, _) = send(&state, ask(gate, "acme.test", "127.0.0.1")).await;
            assert_eq!(
                status,
                StatusCode::NOT_FOUND,
                "a suspended tenant gets no certificate"
            );

            let ip = "127.0.0.2";
            let mut last = StatusCode::OK;
            for _ in 0..=crate::server::UNRESOLVED_PER_MIN {
                let (status, _, _) = send(&state, ask(gate, "made-up.test", ip)).await;
                last = status;
            }
            assert_eq!(last, StatusCode::TOO_MANY_REQUESTS, "refusals are budgeted");
            let (status, _, _) = send(&state, ask(gate, "default.test", ip)).await;
            assert_eq!(status, StatusCode::OK, "a registered name still answers");
        })
        .await;
    }

    /// A rescue that cannot be recorded in the tenant's audit trail is not
    /// issued: no code is minted, and the response says why.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_rescue_is_refused_when_its_audit_entry_cannot_be_written() {
        with_test_pool(PostgresSessionStore::build_pool, |pool| async move {
            let node = vellev_mock().await;
            let tmp = tempfile::tempdir().unwrap();
            let state = cloud_state(pool.clone(), &node, tmp.path()).await;
            create_acme(&state).await;
            pool.get()
                .await
                .unwrap()
                .batch_execute("ALTER TABLE edge_audit RENAME TO edge_audit_offline")
                .await
                .unwrap();

            let (status, body, _) = send(
                &state,
                api(
                    &state,
                    Method::POST,
                    "/platform/api/tenants/acme/rescue",
                    None,
                ),
            )
            .await;
            assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
            assert!(body["error"].as_str().unwrap().contains("audit"), "{body}");
            assert!(body.get("code").is_none(), "no code was minted: {body}");
            assert!(
                state.platform().unwrap().rescues.lock().unwrap().is_empty(),
                "nothing is held for exchange"
            );
        })
        .await;
    }

    /// The platform's writes must come from the platform's own page: a
    /// foreign `Origin` is 403, so is a request carrying neither header,
    /// the platform's own `Origin` passes, and a read needs nothing. A
    /// session of someone taken off the staff list is 403 on every request.
    #[tokio::test]
    async fn platform_writes_need_the_platforms_origin_and_a_delisted_operator_is_refused() {
        let infra = Arc::new(SharedInfra::for_tests());
        let state = EdgeState::for_tests(TenantMode::Multi, vec![], infra, cloud_toml(), 1);
        let cookie = platform_cookie(&state, OPERATOR);
        let on_platform = |method: Method, headers: &[(&str, &str)]| {
            let mut req = Request::builder()
                .method(method)
                .uri("/platform/api/tenants/acme/suspend")
                .header(header::HOST, PLATFORM_HOST)
                .header(header::COOKIE, &cookie)
                .header(header::CONTENT_TYPE, "application/json");
            for (name, value) in headers {
                req = req.header(*name, *value);
            }
            req.extension(peer("203.0.113.1"))
                .body(Body::from("{}"))
                .unwrap()
        };
        let (status, _, _) = send(&state, on_platform(Method::POST, &[])).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "no header, no write");
        let (status, _, _) = send(
            &state,
            on_platform(Method::POST, &[("origin", "https://acme.test")]),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "a tenant's page");
        let (status, _, _) = send(
            &state,
            on_platform(Method::POST, &[("sec-fetch-site", "same-site")]),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "a sibling subdomain");
        let (status, _, _) = send(
            &state,
            on_platform(
                Method::POST,
                &[("origin", &format!("https://{PLATFORM_HOST}"))],
            ),
        )
        .await;
        assert_ne!(
            status,
            StatusCode::FORBIDDEN,
            "the platform's own page passes the guard"
        );
        let (status, _, _) = send(
            &state,
            on_platform(
                Method::POST,
                &[("origin", &format!("http://{PLATFORM_HOST}"))],
            ),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "the origin is exact, scheme included"
        );
        let mut bare = request(
            Method::GET,
            PLATFORM_HOST,
            "/platform/api/me",
            Some(&cookie),
            None,
        );
        bare.headers_mut().remove("sec-fetch-site");
        let (status, _, _) = send(&state, bare).await;
        assert_eq!(status, StatusCode::OK, "a read needs no proof");

        let delisted = platform_cookie(&state, "former@example.net");
        let (status, _, _) = send(
            &state,
            request(
                Method::GET,
                PLATFORM_HOST,
                "/platform/api/me",
                Some(&delisted),
                None,
            ),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "a valid session of someone not on the list"
        );
    }

    /// The outbound policy is the platform's to set: the PATCH writes the
    /// tenant's settings row and the rebuilt runtime follows it, while the
    /// tenant's own settings save cannot name it.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_platform_sets_a_tenants_outbound_policy_and_the_runtime_follows() {
        with_test_pool(PostgresSessionStore::build_pool, |pool| async move {
            let node = vellev_mock().await;
            let tmp = tempfile::tempdir().unwrap();
            let state = cloud_state(pool.clone(), &node, tmp.path()).await;
            create_acme(&state).await;
            let Resolved::Tenant(before) = state.resolve("acme.test") else {
                panic!("serving");
            };
            assert_eq!(
                before.outbound(),
                OutboundPolicy::Private,
                "the file's default"
            );

            let (status, body, _) = send(
                &state,
                api(
                    &state,
                    Method::PATCH,
                    "/platform/api/tenants/acme",
                    Some(json!({"outbound": "public"})),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let acme = TenantId::parse("acme").unwrap();
            let store = TenantSettingsStore::new(
                TenantPool::new(pool.clone()),
                acme.clone(),
                test_master_key(),
            );
            assert_eq!(
                store.load().await.unwrap().unwrap().outbound,
                OutboundPolicy::Public
            );
            let Resolved::Tenant(after) = state.resolve("acme.test") else {
                panic!("serving");
            };
            assert!(!Arc::ptr_eq(&before, &after), "rebuilt on the policy");
            assert_eq!(after.outbound(), OutboundPolicy::Public);

            let (status, body, _) = send(
                &state,
                api(
                    &state,
                    Method::PATCH,
                    "/platform/api/tenants/acme",
                    Some(json!({"outbound": "sideways"})),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
            let Resolved::Tenant(still) = state.resolve("acme.test") else {
                panic!("serving");
            };
            assert_eq!(still.outbound(), OutboundPolicy::Public, "unchanged");
        })
        .await;
    }

    /// A reload that races a deletion cannot leave a runtime behind: once a
    /// tenant is retired for deletion no runtime is installed for it, and
    /// `forget` leaves nothing serving.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_tenant_retired_for_deletion_takes_no_runtime_until_forgotten() {
        with_test_pool(PostgresSessionStore::build_pool, |pool| async move {
            let node = vellev_mock().await;
            let tmp = tempfile::tempdir().unwrap();
            let state = cloud_state(pool.clone(), &node, tmp.path()).await;
            create_acme(&state).await;
            let acme = TenantId::parse("acme").unwrap();
            let Resolved::Tenant(runtime) = state.resolve("acme.test") else {
                panic!("serving");
            };

            state.retire(&acme);
            assert!(runtime.is_retired());
            let err = state.reload(&acme).await.unwrap_err();
            assert!(
                format!("{err:#}").contains("being deleted"),
                "a reload after retirement is refused: {err:#}"
            );
            let err = state.start(&acme).await.unwrap_err();
            assert!(format!("{err:#}").contains("being deleted"), "{err:#}");
            assert!(
                matches!(state.resolve("acme.test"), Resolved::Suspended(_)),
                "still retired, nothing installed"
            );
            assert!(
                !state.active().iter().any(|r| r.id == acme),
                "no runtime serves the tenant"
            );

            state.forget(&acme);
            assert!(matches!(state.resolve("acme.test"), Resolved::Unknown));
            assert!(!state.active().iter().any(|r| r.id == acme));
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_plan_round_trips_and_caps_the_turn_semaphore() {
        with_test_pool(PostgresSessionStore::build_pool, |pool| async move {
            let node = vellev_mock().await;
            let tmp = tempfile::tempdir().unwrap();
            let state = cloud_state(pool.clone(), &node, tmp.path()).await;
            create_acme(&state).await;
            let Resolved::Tenant(before) = state.resolve("acme.test") else {
                panic!("serving");
            };
            assert_eq!(
                before.turn_limit.available_permits(),
                8,
                "the file's ceiling"
            );

            let (status, body, _) = send(
                &state,
                api(
                    &state,
                    Method::PATCH,
                    "/platform/api/tenants/acme",
                    Some(json!({
                        "display_name": "Acme Corporation",
                        "plan": {"turns_per_month": 5000, "max_concurrent_turns": 2},
                    })),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(body["display_name"], "Acme Corporation");
            assert_eq!(
                body["plan"],
                json!({"turns_per_month": 5000, "max_concurrent_turns": 2})
            );
            let (status, body, _) = send(
                &state,
                api(&state, Method::GET, "/platform/api/tenants/acme", None),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(
                body["plan"],
                json!({"turns_per_month": 5000, "max_concurrent_turns": 2})
            );
            let Resolved::Tenant(after) = state.resolve("acme.test") else {
                panic!("serving");
            };
            assert!(!Arc::ptr_eq(&before, &after), "rebuilt on the plan");
            assert_eq!(
                after.turn_limit.available_permits(),
                2,
                "the plan caps the semaphore"
            );
            assert_eq!(after.name, "Acme Corporation");
            assert_eq!(after.settings.plan.turns_per_month, Some(5000));

            let (status, body, _) = send(
                &state,
                api(
                    &state,
                    Method::PATCH,
                    "/platform/api/tenants/acme",
                    Some(json!({"plan": {"seats": 3}})),
                ),
            )
            .await;
            assert_eq!(
                status,
                StatusCode::UNPROCESSABLE_ENTITY,
                "an unknown cap is refused: {body}"
            );
            let (status, _, _) = send(
                &state,
                api(
                    &state,
                    Method::PATCH,
                    "/platform/api/tenants/acme",
                    Some(json!({"display_name": " "})),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
        })
        .await;
    }

    /// A file with no `[admin]`: a new tenant has no admin sign-in, so its
    /// admin API is 404 and a rescue has nothing to open. The platform
    /// provisions one through the PATCH — the section first checked as the
    /// rebuild would check it, then written, the runtime rebuilt on it and
    /// its allowlist seeded — and a rescue opens the admin on the tenant's
    /// host. The OIDC client secret is write-only: set and cleared through
    /// `admin_client_secret`, shown only as `has_admin_client_secret`, and
    /// never in the platform's view or the tenant's own settings.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_platform_provisions_a_tenants_admin_sign_in_so_a_rescue_can_open_it() {
        with_test_pool(PostgresSessionStore::build_pool, |pool| async move {
            let node = vellev_mock().await;
            let tmp = tempfile::tempdir().unwrap();
            let mut no_admin = cloud_toml();
            no_admin.admin = None;
            let state = cloud_state_on(pool.clone(), &node, tmp.path(), no_admin).await;
            let body = create_acme(&state).await;
            assert_eq!(body["has_admin"], false, "{body}");
            assert_eq!(body["has_admin_client_secret"], false);
            let (status, _, _) = send(
                &state,
                request(Method::GET, "acme.test", "/admin/api/me", None, None),
            )
            .await;
            assert_eq!(status, StatusCode::NOT_FOUND, "no admin surface at all");
            let rescue = |state: &Arc<EdgeState>| {
                api(state, Method::POST, "/platform/api/tenants/acme/rescue", None)
            };
            let (status, body, _) = send(&state, rescue(&state)).await;
            assert_eq!(status, StatusCode::CONFLICT, "{body}");
            assert!(body["error"].as_str().unwrap().contains("no admin"), "{body}");

            let patch = |state: &Arc<EdgeState>, body: Value| {
                api(state, Method::PATCH, "/platform/api/tenants/acme", Some(body))
            };
            let Resolved::Tenant(before) = state.resolve("acme.test") else {
                panic!("serving");
            };
            let secret = "oidc-secret-7f3a9c1d";
            for (body, refused) in [
                (
                    json!({"admin": {"oidc": {
                        "issuer": "https://kc.acme.test/realms/acme", "client_id": "edge-admin"
                    }}}),
                    "public_url",
                ),
                (
                    json!({"admin": {"oidc": {"issuer": "https://accounts.google.com"}}}),
                    "client_id",
                ),
                (json!({"admin": {"allowed_emails": ["alice@acme.test"]}}), "sign-in method"),
                (
                    json!({"admin": {
                        "oidc": {"issuer": "https://accounts.google.com", "client_id": "a", "client_secret": secret}
                    }}),
                    "admin_client_secret",
                ),
                (
                    json!({"admin": {"password_hash": "$argon2id$x", "oidc": {"issuer": "https://accounts.google.com", "client_id": "a"}}}),
                    "admin.password_hash",
                ),
                (json!({"admin": {"oidc": {"client_id": "a"}}}), "issuer"),
                (json!({"admins": {}}), "unknown field"),
            ] {
                let (status, response, _) = send(&state, patch(&state, body.clone())).await;
                assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}: {response}");
                assert!(
                    response["error"].as_str().unwrap().contains(refused),
                    "{body}: {response}"
                );
            }
            let Resolved::Tenant(still) = state.resolve("acme.test") else {
                panic!("serving");
            };
            assert!(Arc::ptr_eq(&before, &still), "nothing was written, nothing rebuilt");
            assert!(still.admin.is_none());
            let acme = TenantId::parse("acme").unwrap();
            let store = TenantSettingsStore::new(
                TenantPool::new(pool.clone()),
                acme.clone(),
                test_master_key(),
            );
            assert!(store.load().await.unwrap().unwrap().admin.is_none());

            let (status, body, _) = send(
                &state,
                patch(
                    &state,
                    json!({"admin": {
                        "oidc": {"issuer": "https://accounts.google.com", "client_id": "acme-admin"},
                        "allowed_emails": ["Alice@acme.test"],
                    }}),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(body["has_admin"], true, "{body}");
            assert_eq!(body["has_admin_client_secret"], false);
            let Resolved::Tenant(runtime) = state.resolve("acme.test") else {
                panic!("serving");
            };
            assert!(!Arc::ptr_eq(&before, &runtime), "rebuilt on the sign-in");
            let auth = runtime.admin.as_ref().expect("an admin surface now");
            assert_eq!(auth.oidc_issuer(), Some("https://accounts.google.com"));
            assert_eq!(auth.oidc_client_id(), Some("acme-admin"));
            let operators = runtime.operators.list().await.unwrap();
            assert_eq!(
                operators.iter().map(|o| o.value.as_str()).collect::<Vec<_>>(),
                ["alice@acme.test"],
                "the allowlist is seeded from the section"
            );
            let stored = store.load().await.unwrap().unwrap().admin.expect("the row holds it");
            assert_eq!(stored.allowed_emails, ["Alice@acme.test"]);
            assert!(stored.password_hash.is_none() && stored.session_secret.is_none());

            let (status, body, _) =
                send(&state, patch(&state, json!({"admin_client_secret": secret}))).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(body["has_admin"], true);
            assert_eq!(body["has_admin_client_secret"], true, "{body}");
            assert!(!body.to_string().contains(secret), "the view carries no secret");
            let (_, body, _) =
                send(&state, api(&state, Method::GET, "/platform/api/tenants", None)).await;
            assert!(!body.to_string().contains(secret), "nor does the list");
            let Resolved::Tenant(runtime) = state.resolve("acme.test") else {
                panic!("serving");
            };
            assert_eq!(
                runtime.admin.as_ref().unwrap().oidc_client_secret(),
                Some(secret),
                "the rebuilt tenant signs in with it"
            );

            let (status, body, _) = send(&state, rescue(&state)).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let code = body["code"].as_str().unwrap();
            assert_eq!(
                body["url"],
                format!("https://acme.test/admin/api/login/rescue?code={code}")
            );
            let (status, _, headers) = send(
                &state,
                request(
                    Method::GET,
                    "acme.test",
                    &format!("/admin/api/login/rescue?code={code}"),
                    None,
                    None,
                ),
            )
            .await;
            assert_eq!(status, StatusCode::FOUND);
            assert_eq!(headers.get(header::LOCATION).unwrap(), "/admin/");
            let cookie = headers
                .get(header::SET_COOKIE)
                .and_then(|v| v.to_str().ok())
                .expect("the tenant's admin cookie");
            assert!(cookie.starts_with(SESSION_COOKIE), "{cookie}");
            let session = cookie.split(';').next().unwrap();
            let (status, body, _) = send(
                &state,
                request(Method::GET, "acme.test", "/admin/api/me", Some(session), None),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(body["operator"], OPERATOR);
            let (status, body, _) = send(
                &state,
                request(Method::GET, "acme.test", "/admin/api/settings", Some(session), None),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(body["admin"]["oidc"]["client_id"], "acme-admin");
            assert_eq!(body["has_admin_client_secret"], true);
            assert!(
                !body.to_string().contains(secret),
                "the tenant's own settings show the secret as set, never as itself: {body}"
            );

            let (status, body, _) =
                send(&state, patch(&state, json!({"admin_client_secret": ""}))).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(body["has_admin"], true, "clearing the secret keeps the sign-in");
            assert_eq!(body["has_admin_client_secret"], false, "{body}");
            assert_eq!(store.holds_admin_client_secret().await.unwrap(), Some(false));
            let (status, body, _) =
                send(&state, api(&state, Method::GET, "/platform/api/tenants/acme", None)).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body["has_admin"], true);
            assert_eq!(body["has_admin_client_secret"], false);
        })
        .await;
    }

    /// The portal's last step: the platform puts the signer-up on the new
    /// tenant's allowlist. The entry lists, signs into that tenant and no
    /// other, needs no reload, is audited on the tenant, and goes again.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_platform_grants_and_revokes_a_tenants_operators_and_the_tenant_signs_them_in() {
        with_test_pool(PostgresSessionStore::build_pool, |pool| async move {
            let node = vellev_mock().await;
            let tmp = tempfile::tempdir().unwrap();
            let state = cloud_state(pool.clone(), &node, tmp.path()).await;
            create_acme(&state).await;
            let Resolved::Tenant(before) = state.resolve("acme.test") else {
                panic!("serving");
            };
            let operators = |state: &Arc<EdgeState>, method: Method, path: &str, body| {
                api(
                    state,
                    method,
                    &format!("/platform/api/tenants/acme/operators{path}"),
                    body,
                )
            };

            let (status, body, _) = send(&state, operators(&state, Method::GET, "", None)).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(
                body,
                json!([]),
                "a registered tenant starts with no operators"
            );
            let (status, _, _) = send(
                &state,
                api(
                    &state,
                    Method::GET,
                    "/platform/api/tenants/nobody/operators",
                    None,
                ),
            )
            .await;
            assert_eq!(status, StatusCode::NOT_FOUND);
            let (status, _, _) = send(
                &state,
                request(
                    Method::POST,
                    PLATFORM_HOST,
                    "/platform/api/tenants/acme/operators",
                    None,
                    Some(json!({"kind": "email", "value": "alice@acme.test"})),
                ),
            )
            .await;
            assert_eq!(
                status,
                StatusCode::UNAUTHORIZED,
                "guarded like every platform route"
            );

            let add = json!({"kind": "email", "value": "  Alice@Acme.test "});
            let (status, body, _) = send(
                &state,
                operators(&state, Method::POST, "", Some(add.clone())),
            )
            .await;
            assert_eq!(status, StatusCode::CREATED, "{body}");
            assert_eq!(body["kind"], "email");
            assert_eq!(body["value"], "alice@acme.test", "trimmed and lowercased");
            assert_eq!(body["added_by"], OPERATOR);
            assert!(
                body["added_at"].as_str().is_some_and(|at| !at.is_empty()),
                "{body}"
            );
            let (status, body, _) =
                send(&state, operators(&state, Method::POST, "", Some(add))).await;
            assert_eq!(status, StatusCode::CONFLICT, "{body}");
            let (status, body, _) = send(
                &state,
                operators(
                    &state,
                    Method::POST,
                    "",
                    Some(json!({"kind": "domain", "value": "@Acme.test"})),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::CREATED, "{body}");
            assert_eq!(body["value"], "acme.test");

            for (kind, value) in [
                ("email", "alice"),
                ("email", "alice@localhost"),
                ("email", "@acme.test"),
                ("email", "al ice@acme.test"),
                ("email", "a@b@acme.test"),
                ("domain", "not a domain"),
                ("domain", "acme"),
                ("domain", ""),
                ("owner", "alice@acme.test"),
            ] {
                let (status, body, _) = send(
                    &state,
                    operators(
                        &state,
                        Method::POST,
                        "",
                        Some(json!({"kind": kind, "value": value})),
                    ),
                )
                .await;
                assert_eq!(
                    status,
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "{kind} {value:?}: {body}"
                );
            }

            let (status, body, _) = send(&state, operators(&state, Method::GET, "", None)).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(
                body.as_array()
                    .unwrap()
                    .iter()
                    .map(|o| format!(
                        "{}:{}",
                        o["kind"].as_str().unwrap(),
                        o["value"].as_str().unwrap()
                    ))
                    .collect::<Vec<_>>(),
                ["domain:acme.test", "email:alice@acme.test"]
            );
            let Resolved::Tenant(runtime) = state.resolve("acme.test") else {
                panic!("serving");
            };
            assert!(
                Arc::ptr_eq(&before, &runtime),
                "the same runtime: no reload"
            );
            assert!(
                runtime
                    .operators
                    .is_allowed("alice@acme.test")
                    .await
                    .unwrap(),
                "the added email signs in"
            );
            assert!(
                runtime.operators.is_allowed("bob@acme.test").await.unwrap(),
                "so does the domain"
            );
            let default =
                EdgeOperatorStore::new(TenantPool::new(pool.clone()), TenantId::default_tenant());
            assert!(
                !default.is_allowed("alice@acme.test").await.unwrap(),
                "into acme, not into default"
            );
            assert!(!default.is_allowed("bob@acme.test").await.unwrap());

            let (status, body, _) = send(
                &state,
                operators(&state, Method::DELETE, "/email/Alice@acme.test", None),
            )
            .await;
            assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
            let (status, body, _) = send(
                &state,
                operators(&state, Method::DELETE, "/email/alice@acme.test", None),
            )
            .await;
            assert_eq!(status, StatusCode::NOT_FOUND, "gone already: {body}");
            let (status, body, _) = send(
                &state,
                operators(&state, Method::DELETE, "/owner/alice@acme.test", None),
            )
            .await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
            let (status, _, _) = send(
                &state,
                operators(&state, Method::DELETE, "/domain/acme.test", None),
            )
            .await;
            assert_eq!(
                status,
                StatusCode::NO_CONTENT,
                "the platform may empty the list"
            );
            assert!(!runtime
                .operators
                .is_allowed("alice@acme.test")
                .await
                .unwrap());
            assert!(!runtime.operators.is_allowed("bob@acme.test").await.unwrap());
            let (_, body, _) = send(&state, operators(&state, Method::GET, "", None)).await;
            assert_eq!(body, json!([]));

            let audit: Vec<(String, String, String, String)> = runtime
                .audit
                .recent(10)
                .await
                .unwrap()
                .into_iter()
                .rev()
                .map(|row| (row.operator, row.action, row.target, row.detail))
                .collect();
            let by = |action: &str, target: &str| {
                (
                    OPERATOR.to_string(),
                    action.to_string(),
                    target.to_string(),
                    "by platform".to_string(),
                )
            };
            assert_eq!(
                audit,
                [
                    by("operator.add", "email:alice@acme.test"),
                    by("operator.add", "domain:acme.test"),
                    by("operator.remove", "email:alice@acme.test"),
                    by("operator.remove", "domain:acme.test"),
                ],
                "each change is on the tenant's audit trail, and nothing refused is"
            );
        })
        .await;
    }

    /// The platform's rule on ids, over the slug grammar: nothing reserved
    /// and nothing under three characters becomes a tenant.
    #[tokio::test(flavor = "multi_thread")]
    async fn reserved_and_too_short_ids_are_refused_on_create() {
        with_test_pool(PostgresSessionStore::build_pool, |pool| async move {
            let node = vellev_mock().await;
            let tmp = tempfile::tempdir().unwrap();
            let state = cloud_state(pool.clone(), &node, tmp.path()).await;
            for id in ["api", "platform", "marrilo", "default", "ab", "a"] {
                let (status, body, _) = send(
                    &state,
                    api(
                        &state,
                        Method::POST,
                        "/platform/api/tenants",
                        Some(json!({"id": id, "display_name": "Taken"})),
                    ),
                )
                .await;
                assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{id}: {body}");
                assert!(
                    body["error"].as_str().unwrap().contains(&format!("{id:?}")),
                    "{id}: {body}"
                );
                let provisioned = format!("PUT /v1/tenants/{id} (admin)");
                assert!(
                    !calls(&node).await.contains(&provisioned),
                    "{id}: refused before anything is provisioned"
                );
            }
            let (status, body, _) = send(
                &state,
                api(
                    &state,
                    Method::POST,
                    "/platform/api/tenants",
                    Some(json!({"id": "abc", "display_name": "Abc"})),
                ),
            )
            .await;
            assert_eq!(
                status,
                StatusCode::CREATED,
                "three characters is enough: {body}"
            );
            let (_, body, _) = send(
                &state,
                api(&state, Method::GET, "/platform/api/tenants", None),
            )
            .await;
            let ids: Vec<&str> = body
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t["id"].as_str().unwrap())
                .collect();
            assert_eq!(ids, ["abc", "default"]);
        })
        .await;
    }

    /// The console is served on the platform host alone: the shell for
    /// `/platform/`, for a client route and for anything that is no asset,
    /// a hashed bundle with its immutable cache header, and nothing at all
    /// for a path under `/platform/api/` nothing routes — the API keeps its
    /// own answers, 401 for a guarded route without a session. Off the host,
    /// or without `[platform]`, every one is 404. Needs `ui-platform/dist`
    /// built, as CI builds it before the Rust tests.
    #[tokio::test]
    async fn the_console_is_served_on_the_platform_host_with_the_shell_for_client_routes() {
        let infra = Arc::new(SharedInfra::for_tests());
        let state = EdgeState::for_tests(TenantMode::Multi, vec![], infra, cloud_toml(), 1);

        for path in [
            "/platform",
            "/platform/",
            "/platform/tenants/acme",
            "/platform/audit",
        ] {
            let (status, body, headers) = send(
                &state,
                request(Method::GET, PLATFORM_HOST, path, None, None),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{path}: {body}");
            let content_type = headers[header::CONTENT_TYPE].to_str().unwrap();
            assert!(
                content_type.starts_with("text/html"),
                "{path}: {content_type}"
            );
            assert_eq!(headers[header::CACHE_CONTROL], "no-cache", "{path}");
            let csp = headers[header::CONTENT_SECURITY_POLICY].to_str().unwrap();
            assert!(csp.contains("frame-ancestors 'none'"), "{path}: {csp}");
            assert!(csp.contains("script-src 'self'"), "{path}: {csp}");
            assert!(
                body.as_str()
                    .is_some_and(|html| html.contains("<div id=\"root\">")),
                "{path}: the shell, not something else: {body}"
            );
        }

        let bundle = PlatformAssets::iter()
            .find(|p| p.starts_with("assets/") && p.ends_with(".js"))
            .expect("a JS bundle is embedded");
        let (status, _, headers) = send(
            &state,
            request(
                Method::GET,
                PLATFORM_HOST,
                &format!("/platform/{bundle}"),
                None,
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(headers[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .contains("javascript"));
        assert!(headers[header::CACHE_CONTROL]
            .to_str()
            .unwrap()
            .contains("immutable"));

        let (status, body, _) = send(
            &state,
            request(
                Method::GET,
                PLATFORM_HOST,
                "/platform/api/nothing",
                None,
                None,
            ),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "an API path is never the shell: {body}"
        );
        assert!(platform_asset("api/nothing").is_none());
        assert!(platform_asset("api").is_none());
        let (status, _, _) = send(
            &state,
            request(Method::GET, PLATFORM_HOST, "/platform/api/me", None, None),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "the guarded API keeps its own answer"
        );

        let (status, _, _) = send(
            &state,
            request(Method::GET, "acme.test", "/platform/", None, None),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "off the platform host");

        let mut without = cloud_toml();
        without.platform = None;
        let state = EdgeState::for_tests(
            TenantMode::Multi,
            vec![],
            Arc::new(SharedInfra::for_tests()),
            without,
            1,
        );
        let (status, _, _) = send(
            &state,
            request(Method::GET, PLATFORM_HOST, "/platform/", None, None),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "no [platform], no console");
    }

    /// Signing out clears the platform cookie — on the platform host; a
    /// tenant's host has no such route.
    #[tokio::test]
    async fn logout_clears_the_platform_cookie_on_the_platform_host_only() {
        let infra = Arc::new(SharedInfra::for_tests());
        let state = EdgeState::for_tests(TenantMode::Multi, vec![], infra, cloud_toml(), 1);
        let cookie = platform_cookie(&state, OPERATOR);

        let (status, body, headers) = send(
            &state,
            request(
                Method::POST,
                PLATFORM_HOST,
                "/platform/api/logout",
                Some(&cookie),
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["ok"], true);
        let set_cookie = headers[header::SET_COOKIE].to_str().unwrap();
        assert!(
            set_cookie.starts_with(&format!("{PLATFORM_SESSION_COOKIE}=;")),
            "{set_cookie}"
        );
        assert!(set_cookie.contains("Max-Age=0"), "{set_cookie}");
        assert!(set_cookie.contains("Path=/platform"), "{set_cookie}");

        let (status, _, _) = send(
            &state,
            request(
                Method::POST,
                "acme.test",
                "/platform/api/logout",
                Some(&cookie),
                None,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    /// The agent cap set through `PATCH` comes back from the usage endpoint
    /// and the tenant view beside the count of mounted agents, so the
    /// console can show one against the other.
    #[tokio::test]
    async fn the_usage_endpoint_reports_the_agent_count_beside_its_cap() {
        with_test_pool(PostgresSessionStore::build_pool, |pool| async move {
            let node = vellev_mock().await;
            let tmp = tempfile::tempdir().unwrap();
            let state = cloud_state(pool.clone(), &node, tmp.path()).await;
            create_acme(&state).await;

            let (status, body, _) = send(
                &state,
                api(
                    &state,
                    Method::PATCH,
                    "/platform/api/tenants/acme",
                    Some(json!({"plan": {"agents": 5}})),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(body["plan"]["agents"], 5, "the view carries the cap");
            assert_eq!(body["usage"]["agents"], 0, "and the count: {body}");

            let (status, body, _) = send(
                &state,
                api(
                    &state,
                    Method::GET,
                    "/platform/api/tenants/acme/usage",
                    None,
                ),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(body["serving"], true);
            assert_eq!(body["plan"]["agents"], 5);
            assert_eq!(body["usage"]["agents"], 0, "{body}");
        })
        .await;
    }

    /// The usage endpoint pairs the plan with a fresh count for a served
    /// tenant and says so for one that is not; every platform action lands
    /// in the audit log, newest first, under the principal that took it —
    /// an operator's email or `service:<name>` — paged by `limit` and
    /// `before`; and the tenant view carries the outbound posture and the
    /// admin section the console edits, secrets never.
    #[tokio::test]
    async fn usage_is_counted_against_the_plan_and_every_action_is_in_the_audit_log() {
        with_test_pool(PostgresSessionStore::build_pool, |pool| async move {
            let node = vellev_mock().await;
            let tmp = tempfile::tempdir().unwrap();
            let state = cloud_state(pool.clone(), &node, tmp.path()).await;

            let (status, body, _) = send(
                &state,
                api(&state, Method::GET, "/platform/api/audit", None),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(body["entries"], json!([]), "nothing done yet");

            let created = create_acme(&state).await;
            assert_eq!(
                created["outbound"], "private",
                "the file sets no default_outbound: {created}"
            );
            assert_eq!(
                created["admin"]["oidc"]["issuer"], "https://accounts.google.com",
                "the file's [admin] seeds the view: {created}"
            );

            let (status, body, _) = send(
                &state,
                api(
                    &state,
                    Method::PATCH,
                    "/platform/api/tenants/acme",
                    Some(json!({"plan": {"turns_per_month": 500, "memory_objects": 1000}})),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let (status, body, _) = send(
                &state,
                api(
                    &state,
                    Method::PATCH,
                    "/platform/api/tenants/acme",
                    Some(json!({
                        "admin": {
                            "oidc": {"issuer": "https://accounts.google.com", "client_id": "acme-admin", "mode": "google"},
                            "allowed_emails": ["ops@acme.test"],
                            "allowed_domains": [],
                            "session_hours": 8
                        },
                        "admin_client_secret": "oidc-client-secret-value"
                    })),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(body["admin"]["oidc"]["client_id"], "acme-admin");
            assert_eq!(body["admin"]["session_hours"], 8);
            assert_eq!(body["has_admin_client_secret"], true, "{body}");
            assert!(
                !body.to_string().contains("oidc-client-secret-value"),
                "the view never carries the secret: {body}"
            );

            let (status, body, _) = send(
                &state,
                api(
                    &state,
                    Method::GET,
                    "/platform/api/tenants/acme/usage",
                    None,
                ),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(body["serving"], true);
            assert_eq!(body["plan"]["turns_per_month"], 500);
            assert_eq!(body["usage"]["turns_this_month"], 0);
            assert_eq!(
                body["usage"]["memory_objects"], 3,
                "the vector store counted under its cap: {body}"
            );

            let (status, body, _) = send(
                &state,
                service_api(
                    Method::POST,
                    "/platform/api/tenants/acme/suspend",
                    BILLING_TOKEN,
                    Some(json!({"message": "Invoice overdue"})),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let (status, body, _) = send(
                &state,
                api(
                    &state,
                    Method::GET,
                    "/platform/api/tenants/acme/usage",
                    None,
                ),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(body["serving"], false);
            assert_eq!(body["usage"], Value::Null, "nothing counts a suspended tenant");
            assert_eq!(body["plan"]["turns_per_month"], 500, "the plan is the row's");

            let (status, _, _) = send(
                &state,
                api(
                    &state,
                    Method::GET,
                    "/platform/api/tenants/nobody/usage",
                    None,
                ),
            )
            .await;
            assert_eq!(status, StatusCode::NOT_FOUND);

            let (status, body, _) = send(
                &state,
                api(&state, Method::GET, "/platform/api/audit", None),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let entries = body["entries"].as_array().unwrap();
            let summary: Vec<(String, String, String)> = entries
                .iter()
                .map(|e| {
                    (
                        e["principal"].as_str().unwrap().to_string(),
                        e["action"].as_str().unwrap().to_string(),
                        e["tenant"].as_str().unwrap().to_string(),
                    )
                })
                .collect();
            let s = |a: &str, b: &str| (a.to_string(), b.to_string(), "acme".to_string());
            assert_eq!(
                summary,
                [
                    s("service:billing", "tenant.suspend"),
                    s(OPERATOR, "tenant.update"),
                    s(OPERATOR, "tenant.update"),
                    s(OPERATOR, "tenant.create"),
                ],
                "newest first: {body}"
            );
            assert_eq!(entries[0]["detail"], "Invoice overdue");
            assert_eq!(entries[1]["detail"], "admin,admin_client_secret");
            assert_eq!(entries[2]["detail"], "plan");
            assert!(
                entries[3]["detail"]
                    .as_str()
                    .unwrap()
                    .contains("hostnames=acme.test"),
                "{body}"
            );
            assert!(
                !body.to_string().contains("oidc-client-secret-value"),
                "the log names the field, never the secret: {body}"
            );
            let ids: Vec<i64> = entries.iter().map(|e| e["id"].as_i64().unwrap()).collect();
            assert!(ids.windows(2).all(|w| w[0] > w[1]), "{ids:?}");

            let (status, body, _) = send(
                &state,
                api(&state, Method::GET, "/platform/api/audit?limit=1", None),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(body["entries"].as_array().unwrap().len(), 1);
            assert_eq!(body["entries"][0]["id"], ids[0]);
            let (status, body, _) = send(
                &state,
                api(
                    &state,
                    Method::GET,
                    &format!("/platform/api/audit?limit=2&before={}", ids[0]),
                    None,
                ),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let page: Vec<i64> = body["entries"]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| e["id"].as_i64().unwrap())
                .collect();
            assert_eq!(page, ids[1..3], "the page after the first entry");

            let (status, _, _) = send(
                &state,
                request(Method::GET, PLATFORM_HOST, "/platform/api/audit", None, None),
            )
            .await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "guarded like the rest");
        })
        .await;
    }
}
