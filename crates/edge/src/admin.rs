//! Operator (admin) authentication — the edge's second auth tier.
//!
//! Customers authenticate with the company IdP (see [`crate::keycloak`]);
//! operators authenticate here, with an install-local password, to reach the
//! admin surface (`/admin`). Absent `[admin]` config, every admin route is 404
//! — an existing install exposes nothing new until an operator opts in.
//!
//! Design (docs/ui-edge-design.md): argon2id password hash in config (never the
//! password), login rate-limited by the shared ingress limiter, sessions as
//! short-lived HS256 tokens in an HttpOnly cookie. The signing key comes from
//! `session_secret` when configured (sessions survive restarts) or is random
//! per boot (restart logs everyone out — safe default).

use std::sync::Arc;

use argon2::password_hash::PasswordHash;
use argon2::{Argon2, PasswordVerifier};
use axum::http::{header, HeaderMap, StatusCode};
use jsonwebtoken::{DecodingKey, EncodingKey, Validation};
use serde::{Deserialize, Serialize};

use crate::outbound::OutboundPolicy;
use crate::tenant::TenantId;

/// The `[admin]` config block. Its presence enables the admin surface. At least
/// one sign-in method — `oidc` or `password_hash` — must be set. Its secrets
/// are never serialised: a tenant's settings row keeps the OIDC client secret
/// sealed in its own column, and the break-glass hash and session secret stay
/// in the file.
#[derive(Clone, Serialize, Deserialize)]
pub struct AdminConfig {
    /// OIDC sign-in (the primary path): operators log in with their IdP account,
    /// gated by the operator allowlist. Reuses the customer OIDC validator.
    #[serde(default)]
    pub oidc: Option<AdminOidc>,
    /// Optional argon2id break-glass password (`workforce-edge --hash-password`),
    /// so a misconfigured allowlist can't lock everyone out.
    #[serde(default, skip_serializing)]
    pub password_hash: Option<String>,
    /// Emails seeded into the operator allowlist on first boot (then UI-managed).
    #[serde(default)]
    pub allowed_emails: Vec<String>,
    /// Email domains seeded into the operator allowlist on first boot, e.g.
    /// `harbor.example` — anyone with a verified `@harbor.example` address.
    #[serde(default)]
    pub allowed_domains: Vec<String>,
    /// Optional session-signing secret (≥32 chars recommended). Unset, the
    /// runtime fills it from the master key's session root when the install
    /// has one ([`crate::boot::SessionRoot`]); with neither it is a random
    /// per-boot key: restarts and reloads invalidate sessions.
    #[serde(default, skip_serializing)]
    pub session_secret: Option<String>,
    /// The install's public base URL (e.g. `https://workforce-edge.harbor.example`).
    /// Needed only for the OIDC redirect flow, to build the exact `redirect_uri`
    /// the IdP must have registered. When unset it is derived from the request's
    /// forwarded host — fine for a single-host install behind a proxy that sets
    /// `X-Forwarded-Proto`/`Host`.
    #[serde(default)]
    pub public_url: Option<String>,
    /// Session lifetime in hours.
    #[serde(default = "default_session_hours")]
    pub session_hours: u64,
}

/// Manual `Debug` so an accidental `{:?}` of the config can never print the
/// break-glass hash or the session secret.
impl std::fmt::Debug for AdminConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdminConfig")
            .field("oidc", &self.oidc)
            .field(
                "password_hash",
                &self.password_hash.as_ref().map(|_| "<redacted>"),
            )
            .field("allowed_emails", &self.allowed_emails)
            .field("allowed_domains", &self.allowed_domains)
            .field(
                "session_secret",
                &self.session_secret.as_ref().map(|_| "<redacted>"),
            )
            .field("public_url", &self.public_url)
            .field("session_hours", &self.session_hours)
            .finish()
    }
}

/// OIDC settings for operator sign-in.
#[derive(Clone, Serialize, Deserialize)]
pub struct AdminOidc {
    /// Issuer URL, e.g. `https://accounts.google.com` or
    /// `https://auth.harbor.example/realms/harbor`.
    pub issuer: String,
    /// Expected audience (usually the OAuth client id). Strongly recommended.
    #[serde(default)]
    pub audience: Option<String>,
    /// The OAuth client id (the Google button, or the Authorization Code flow).
    #[serde(default)]
    pub client_id: Option<String>,
    /// Client secret for a CONFIDENTIAL Authorization Code client. Omit for a
    /// public + PKCE client (the recommended Keycloak setup).
    #[serde(default, skip_serializing)]
    pub client_secret: Option<String>,
    /// Sign-in style: `google` (the Google Identity Services button) or
    /// `redirect` (the standard Authorization Code + PKCE browser flow, for
    /// Keycloak and every non-Google IdP). Defaults by issuer.
    #[serde(default)]
    pub mode: Option<String>,
}

/// Manual `Debug` so an accidental `{:?}` of the config can never print the
/// client secret.
impl std::fmt::Debug for AdminOidc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdminOidc")
            .field("issuer", &self.issuer)
            .field("audience", &self.audience)
            .field("client_id", &self.client_id)
            .field(
                "client_secret",
                &self.client_secret.as_ref().map(|_| "<redacted>"),
            )
            .field("mode", &self.mode)
            .finish()
    }
}

/// How the operator OIDC login is driven.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OidcMode {
    /// Google Identity Services button (client-side token → POST /login/oidc).
    Google,
    /// Authorization Code + PKCE redirect (/login/oidc/start → IdP → /callback).
    Redirect,
}

fn default_session_hours() -> u64 {
    12
}

/// Name of the session cookie.
pub const SESSION_COOKIE: &str = "wf_admin_session";

/// Name of the platform operators' session cookie, scoped to `/platform` so
/// a tenant's admin routes never see it.
pub const PLATFORM_SESSION_COOKIE: &str = "wf_platform_session";

/// What a session lets its holder do: a tenant's admin surface, or the
/// deployment's platform surface. Minted into the token, so a session of one
/// kind is refused by the other's guard even under a shared signing key.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionRole {
    #[default]
    Admin,
    Platform,
}

/// What an authenticator signs for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionScope {
    /// One tenant's admin surface.
    Tenant(TenantId),
    /// The deployment's platform surface.
    Platform,
}

impl SessionScope {
    fn role(&self) -> SessionRole {
        match self {
            Self::Tenant(_) => SessionRole::Admin,
            Self::Platform => SessionRole::Platform,
        }
    }

    fn tenant(&self) -> Option<&TenantId> {
        match self {
            Self::Tenant(tenant) => Some(tenant),
            Self::Platform => None,
        }
    }

    fn cookie_name(&self) -> &'static str {
        match self {
            Self::Tenant(_) => SESSION_COOKIE,
            Self::Platform => PLATFORM_SESSION_COOKIE,
        }
    }

    fn cookie_path(&self) -> &'static str {
        match self {
            Self::Tenant(_) => "/",
            Self::Platform => "/platform",
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct SessionClaims {
    /// The operator this session belongs to — a verified OIDC email, or the
    /// break-glass sentinel. Carried into audit records.
    sub: String,
    /// The tenant the session was minted for; absent on a platform session.
    /// Checked against the tenant a request resolves to, so a session is
    /// worthless on another tenant's hostname even when the two share a
    /// signing key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tenant_id: Option<TenantId>,
    /// A token without the claim is from before platform sessions existed
    /// and was minted for an admin.
    #[serde(default)]
    role: SessionRole,
    exp: usize,
    iat: usize,
}

/// The subject recorded for a break-glass (password) session.
pub const BREAK_GLASS_OPERATOR: &str = "break-glass (password)";

/// Resolved admin authenticator, built once per tenant — or once for the
/// platform surface, which signs with its own key and role.
pub struct AdminAuth {
    /// What this authenticator signs for.
    scope: SessionScope,
    /// Break-glass password hash, if configured.
    password_hash: Option<String>,
    /// OIDC validator, if configured (verifies id_tokens for both sign-in modes).
    oidc: Option<Arc<crate::keycloak::KeycloakValidator>>,
    /// The OAuth client id (Google button + Authorization Code flow).
    oidc_client_id: Option<String>,
    /// Confidential-client secret for the Authorization Code flow, if any.
    oidc_client_secret: Option<String>,
    /// The issuer, for OIDC endpoint discovery in the redirect flow.
    oidc_issuer: Option<String>,
    /// Which sign-in style is active (`None` ⇒ OIDC disabled).
    oidc_mode: Option<OidcMode>,
    /// Configured public base URL for building the redirect_uri.
    public_url: Option<String>,
    encoding: EncodingKey,
    decoding: DecodingKey,
    session_hours: u64,
}

/// A verified operator identity from an id_token.
pub struct VerifiedOidc {
    /// The verified, lowercased email.
    pub email: String,
    /// The token's `nonce`, for the redirect flow's replay check.
    pub nonce: Option<String>,
}

/// Upper bound on a session's lifetime (30 days) — clamps a pathological
/// config away from the `i64`/Max-Age overflow at issue time.
const MAX_SESSION_HOURS: u64 = 720;

impl AdminAuth {
    /// Build the tenant's admin auth. `outbound` is how its OIDC issuer is
    /// reached: a cloud tenant's admin IdP is tenant-supplied, so its
    /// discovery and JWKS fetches follow the same policy as every other URL
    /// the tenant names.
    pub fn new(
        config: &AdminConfig,
        tenant: TenantId,
        outbound: OutboundPolicy,
    ) -> eyre::Result<Self> {
        Self::build(config, SessionScope::Tenant(tenant), outbound)
    }

    /// The platform surface's authenticator: the same OIDC redirect flow and
    /// signed sessions, minted with the platform role under the platform
    /// cookie, so no tenant's admin guard honours them.
    pub fn platform(config: &AdminConfig, outbound: OutboundPolicy) -> eyre::Result<Self> {
        Self::build(config, SessionScope::Platform, outbound)
    }

    fn build(
        config: &AdminConfig,
        scope: SessionScope,
        outbound: OutboundPolicy,
    ) -> eyre::Result<Self> {
        if config.oidc.is_none() && config.password_hash.is_none() {
            eyre::bail!(
                "[admin] needs a sign-in method: set `oidc` (recommended) and/or `password_hash`"
            );
        }
        // Validate the break-glass hash at boot (argon2 family) if present.
        if let Some(hash) = &config.password_hash {
            let parsed = PasswordHash::new(hash)
                .map_err(|e| eyre::eyre!("admin.password_hash is not a valid PHC string: {e}"))?;
            if !parsed.algorithm.as_str().starts_with("argon2") {
                eyre::bail!(
                    "admin.password_hash must be argon2 (got {}); regenerate with `workforce-edge --hash-password`",
                    parsed.algorithm
                );
            }
        }
        // Audience binding is the defense against a token minted for ANOTHER
        // app on the same issuer (e.g. a customer's own id_token) being replayed
        // to mint an operator session. Require it: default to the OAuth
        // client_id (which equals the Google id_token `aud`) when `audience` is
        // unset, and fail boot if neither is present.
        let oidc = match config.oidc.as_ref() {
            Some(o) => {
                let audience = o
                    .audience
                    .clone()
                    .or_else(|| o.client_id.clone())
                    .map(|a| a.trim().to_string())
                    .filter(|a| !a.is_empty());
                if audience.is_none() {
                    eyre::bail!(
                        "[admin.oidc] needs `audience` (or `client_id`) so tokens are bound to \
                         this app — without it any token from the issuer would be accepted"
                    );
                }
                Some(crate::keycloak::validator_for_policy(
                    &o.issuer,
                    audience.as_deref(),
                    outbound,
                ))
            }
            None => None,
        };
        // Sign-in mode: honour an explicit `mode`, else default by issuer
        // (Google → the GSI button; anything else → the redirect flow).
        let oidc_mode = config
            .oidc
            .as_ref()
            .map(|o| match o.mode.as_deref().map(str::trim) {
                Some("google") => OidcMode::Google,
                Some("redirect") => OidcMode::Redirect,
                _ if o.issuer.starts_with("https://accounts.google.com") => OidcMode::Google,
                _ => OidcMode::Redirect,
            });
        // The redirect flow needs a client_id to send to the authorize endpoint.
        if oidc_mode == Some(OidcMode::Redirect)
            && config
                .oidc
                .as_ref()
                .and_then(|o| o.client_id.as_ref())
                .map(|c| c.trim().is_empty())
                .unwrap_or(true)
        {
            eyre::bail!(
                "[admin.oidc] redirect sign-in needs `client_id` (the OAuth client the IdP registered)"
            );
        }
        // The redirect flow's `redirect_uri` must be a stable, registered URL —
        // require `public_url` rather than deriving it from a client `Host`
        // header (which a proxy might not pin, and which shouldn't drive a
        // security-relevant URL).
        if oidc_mode == Some(OidcMode::Redirect)
            && config
                .public_url
                .as_ref()
                .map(|u| u.trim().is_empty())
                .unwrap_or(true)
        {
            eyre::bail!(
                "[admin] redirect sign-in needs `public_url` (this install's base URL, e.g. \
                 https://workforce-edge.example.com) to build the OIDC redirect_uri"
            );
        }
        let key: Vec<u8> = match &config.session_secret {
            Some(secret) if !secret.trim().is_empty() => {
                if secret.len() < 32 {
                    eyre::bail!("admin.session_secret must be at least 32 bytes (or omit it for a random per-boot key)");
                }
                secret.as_bytes().to_vec()
            }
            _ => {
                let mut buf = [0u8; 32];
                getrandom::getrandom(&mut buf)
                    .map_err(|e| eyre::eyre!("generating session key: {e}"))?;
                buf.to_vec()
            }
        };
        let clean = |s: &Option<String>| {
            s.clone()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        Ok(Self {
            scope,
            password_hash: config.password_hash.clone(),
            oidc,
            oidc_client_id: config.oidc.as_ref().and_then(|o| clean(&o.client_id)),
            oidc_client_secret: config.oidc.as_ref().and_then(|o| clean(&o.client_secret)),
            oidc_issuer: config.oidc.as_ref().map(|o| o.issuer.trim().to_string()),
            oidc_mode,
            public_url: clean(&config.public_url).map(|u| u.trim_end_matches('/').to_string()),
            encoding: EncodingKey::from_secret(&key),
            decoding: DecodingKey::from_secret(&key),
            session_hours: config.session_hours.clamp(1, MAX_SESSION_HOURS),
        })
    }

    /// The active OIDC sign-in mode, or `None` when OIDC is disabled.
    pub fn oidc_mode(&self) -> Option<OidcMode> {
        self.oidc_mode
    }

    /// The OIDC issuer (for endpoint discovery in the redirect flow).
    pub fn oidc_issuer(&self) -> Option<&str> {
        self.oidc_issuer.as_deref()
    }

    /// The confidential-client secret for the redirect flow, if configured.
    pub fn oidc_client_secret(&self) -> Option<&str> {
        self.oidc_client_secret.as_deref()
    }

    /// The configured public base URL (for the redirect_uri), if set.
    pub fn public_url(&self) -> Option<&str> {
        self.public_url.as_deref()
    }

    /// Sign the short-lived flow-state cookie (state + PKCE verifier + nonce)
    /// with the session key so the browser can't forge it.
    pub fn issue_flow(&self, state: &crate::oidc::FlowState) -> eyre::Result<String> {
        Ok(jsonwebtoken::encode(
            &jsonwebtoken::Header::default(),
            state,
            &self.encoding,
        )?)
    }

    /// Verify + decode a flow-state cookie. `None` on tamper/expiry.
    pub fn verify_flow(&self, token: &str) -> Option<crate::oidc::FlowState> {
        let mut v = Validation::default();
        v.validate_exp = true;
        v.required_spec_claims.clear();
        jsonwebtoken::decode::<crate::oidc::FlowState>(token, &self.decoding, &v)
            .ok()
            .map(|d| d.claims)
    }

    /// Whether a break-glass password is configured.
    pub fn has_password(&self) -> bool {
        self.password_hash.is_some()
    }

    /// The public OIDC client id for the login page's Google button (`None`
    /// disables the button).
    pub fn oidc_client_id(&self) -> Option<&str> {
        self.oidc_client_id.as_deref()
    }

    /// Verify the break-glass password. `false` when no password is configured.
    pub fn verify_password(&self, password: &str) -> bool {
        let Some(hash) = &self.password_hash else {
            return false;
        };
        let Ok(hash) = PasswordHash::new(hash) else {
            return false;
        };
        Argon2::default()
            .verify_password(password.as_bytes(), &hash)
            .is_ok()
    }

    /// Verify an operator id_token and return its verified email + nonce. `Err`
    /// on any failure (bad token, no email, unverified email, OIDC not configured).
    pub async fn verify_oidc_claims(&self, token: &str) -> eyre::Result<VerifiedOidc> {
        let validator = self
            .oidc
            .as_ref()
            .ok_or_else(|| eyre::eyre!("OIDC sign-in is not configured"))?;
        let claims = validator.verify(token).await?;
        let email = claims
            .email
            .filter(|e| !e.is_empty())
            .ok_or_else(|| eyre::eyre!("token carries no email"))?;
        // Operators can be allowlisted by DOMAIN, so REQUIRE an explicit
        // `email_verified: true` — not merely "not false". Otherwise an IdP that
        // omits the claim would let anyone assert `ceo@allowed-domain.com`.
        // Stricter than the customer path (exact-email only) by design, since an
        // operator holds far more privilege; Google always sets this true.
        if claims.email_verified != Some(true) {
            eyre::bail!("identity provider did not assert the email is verified");
        }
        Ok(VerifiedOidc {
            email: email.to_lowercase(),
            nonce: claims.nonce,
        })
    }

    /// Verify an operator id_token and return only the verified email (the
    /// Google GSI path, which carries no nonce to check).
    pub async fn verify_oidc_email(&self, token: &str) -> eyre::Result<String> {
        Ok(self.verify_oidc_claims(token).await?.email)
    }

    /// Mint a session token for `operator` (a verified email or the break-glass
    /// sentinel), living the configured `session_hours`.
    pub fn issue_session(&self, operator: &str) -> eyre::Result<String> {
        self.issue_session_for(
            operator,
            std::time::Duration::from_secs(self.session_hours * 3600),
        )
    }

    /// Mint a session token for `operator` living `lifetime` at most: the
    /// configured `session_hours` still bound it, so a caller can shorten a
    /// session (a platform rescue) but never lengthen one.
    pub fn issue_session_for(
        &self,
        operator: &str,
        lifetime: std::time::Duration,
    ) -> eyre::Result<String> {
        let configured = std::time::Duration::from_secs(self.session_hours * 3600);
        let lifetime = chrono::Duration::from_std(lifetime.min(configured))
            .map_err(|e| eyre::eyre!("session lifetime out of range: {e}"))?;
        let now = chrono::Utc::now();
        let claims = SessionClaims {
            sub: operator.to_string(),
            tenant_id: self.scope.tenant().cloned(),
            role: self.scope.role(),
            iat: now.timestamp() as usize,
            exp: (now + lifetime).timestamp() as usize,
        };
        jsonwebtoken::encode(&jsonwebtoken::Header::default(), &claims, &self.encoding)
            .map_err(|e| eyre::eyre!("signing admin session: {e}"))
    }

    fn decode_session(&self, token: &str) -> Option<SessionClaims> {
        jsonwebtoken::decode::<SessionClaims>(token, &self.decoding, &Validation::default())
            .ok()
            .map(|d| d.claims)
    }

    /// When a valid session under this key expires, as a Unix timestamp.
    #[cfg(test)]
    pub(crate) fn session_expiry(&self, token: &str) -> Option<usize> {
        self.decode_session(token).map(|claims| claims.exp)
    }

    /// The operator and tenant a valid, unexpired admin session was minted
    /// for (`None` if invalid, or not an admin session). The tenant is the
    /// token's claim, not this authenticator's: [`require_admin`] is what
    /// compares the two.
    pub fn verify_session(&self, token: &str) -> Option<(String, TenantId)> {
        let claims = self.decode_session(token)?;
        if claims.role != SessionRole::Admin {
            return None;
        }
        Some((claims.sub, claims.tenant_id?))
    }

    /// The role of a valid, unexpired session under this key, whatever it
    /// was minted for: what tells a guard whether to refuse a session as
    /// the wrong kind (403) rather than as no session (401).
    pub fn session_role(&self, token: &str) -> Option<SessionRole> {
        self.decode_session(token).map(|claims| claims.role)
    }

    /// The operator a valid, unexpired platform session belongs to (`None`
    /// if invalid, or not a platform session).
    pub fn verify_platform_session(&self, token: &str) -> Option<String> {
        let claims = self.decode_session(token)?;
        (claims.role == SessionRole::Platform).then_some(claims.sub)
    }

    /// The `Set-Cookie` value for a fresh session. HttpOnly + Secure +
    /// SameSite=Strict: unreadable from JS, never sent cross-site.
    pub fn session_cookie(&self, token: &str) -> String {
        format!(
            "{}={token}; Path={}; HttpOnly; Secure; SameSite=Strict; Max-Age={}",
            self.scope.cookie_name(),
            self.scope.cookie_path(),
            self.session_hours * 3600
        )
    }

    /// The `Set-Cookie` value that clears the session (logout).
    pub fn clear_cookie(&self) -> String {
        format!(
            "{}=; Path={}; HttpOnly; Secure; SameSite=Strict; Max-Age=0",
            self.scope.cookie_name(),
            self.scope.cookie_path()
        )
    }
}

/// The values of every `name=` cookie in the request, in order.
fn cookie_values<'a>(headers: &'a HeaderMap, name: &'a str) -> impl Iterator<Item = &'a str> {
    headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .into_iter()
        .flat_map(|cookies| cookies.split(';'))
        .filter_map(move |c| {
            c.trim()
                .strip_prefix(name)
                .and_then(|rest| rest.strip_prefix('='))
        })
}

/// Whether a request may change state under a cookie session, as a
/// cross-site request forgery check. Reads only; writes need proof that the
/// page that sent them is this surface's own: a `Sec-Fetch-Site` of
/// `same-origin` or `none` (a browser's own navigation), or an `Origin` that
/// `own_origin` accepts. A tenant's hosts are same-site with the platform's
/// and with each other, so `SameSite=Strict` on the cookie is not enough on
/// its own, and a bodiless `POST` is a simple request no preflight guards.
///
/// A request carrying neither header is refused: a non-browser API client
/// presenting a cookie sends `Sec-Fetch-Site: same-origin` (or an `Origin`
/// of the surface's own) with every state-changing request.
pub fn csrf_allows(
    method: &axum::http::Method,
    headers: &HeaderMap,
    own_origin: impl Fn(&str) -> bool,
) -> bool {
    use axum::http::Method;
    if matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS) {
        return true;
    }
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    match header("sec-fetch-site").map(str::trim) {
        Some("same-origin" | "none") => return true,
        Some(_) => {
            // A browser said the request is cross-site; no Origin overrules it.
            return false;
        }
        None => {}
    }
    header("origin").map(str::trim).is_some_and(own_origin)
}

/// Whether `origin` (an `Origin` header value) names `host`: the same
/// hostname, whatever the scheme or port — the shape the tenant registry
/// keys hosts by (`ingress::request_host`) — so an admin page served on the
/// tenant's own hostname passes and any other site does not.
pub fn origin_is_host(origin: &str, host: &str) -> bool {
    url::Url::parse(origin)
        .ok()
        .and_then(|url| url.host_str().map(|h| h.eq_ignore_ascii_case(host)))
        .unwrap_or(false)
}

/// Extract and verify the admin session from request cookies, returning the
/// operator it belongs to. `Err` carries the status the caller should return:
/// 404 when the admin surface is disabled (indistinguishable from a non-existent
/// route), 401 otherwise — including a session minted for another tenant,
/// however it was signed, and a platform session.
pub fn require_admin(auth: Option<&AdminAuth>, headers: &HeaderMap) -> Result<String, StatusCode> {
    let auth = auth.ok_or(StatusCode::NOT_FOUND)?;
    cookie_values(headers, SESSION_COOKIE)
        .find_map(|token| auth.verify_session(token))
        .filter(|(_, tenant)| Some(tenant) == auth.scope.tenant())
        .map(|(operator, _)| operator)
        .ok_or(StatusCode::UNAUTHORIZED)
}

/// Extract and verify the platform session from request cookies, returning
/// the platform operator it belongs to. 404 when the platform surface is
/// disabled, 401 without a valid session, 403 for a session that verifies
/// under the platform key but was minted for an admin.
pub fn require_platform(
    auth: Option<&AdminAuth>,
    headers: &HeaderMap,
) -> Result<String, StatusCode> {
    let auth = auth.ok_or(StatusCode::NOT_FOUND)?;
    let mut wrong_kind = false;
    for token in cookie_values(headers, PLATFORM_SESSION_COOKIE) {
        match auth.session_role(token) {
            Some(SessionRole::Platform) => {
                if let Some(operator) = auth.verify_platform_session(token) {
                    return Ok(operator);
                }
            }
            Some(SessionRole::Admin) => wrong_kind = true,
            None => {}
        }
    }
    Err(if wrong_kind {
        StatusCode::FORBIDDEN
    } else {
        StatusCode::UNAUTHORIZED
    })
}

/// Interactive helper for `workforce-edge --hash-password`: reads the password
/// from stdin and prints the PHC hash to paste into `[admin] password_hash`.
pub fn hash_password_cli() -> eyre::Result<()> {
    use argon2::password_hash::{rand_core::OsRng, SaltString};
    use argon2::PasswordHasher;
    eprintln!("Enter admin password (input is echoed; run in a private terminal):");
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let password = line.trim_end_matches(['\r', '\n']);
    if password.len() < 12 {
        eyre::bail!("password must be at least 12 characters");
    }
    let salt = SaltString::generate(&mut OsRng);
    let hash = Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| eyre::eyre!("hashing password: {e}"))?;
    println!("{hash}");
    eprintln!("Paste into edge.toml:  [admin]\\n  password_hash = \"<the line above>\"");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outbound::OutboundPolicy;
    use argon2::password_hash::{rand_core::OsRng, SaltString};
    use argon2::PasswordHasher;

    fn argon2_hash(password: &str) -> String {
        let salt = SaltString::generate(&mut OsRng);
        Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .unwrap()
            .to_string()
    }

    fn cfg(password_hash: Option<String>, secret: Option<&str>, hours: u64) -> AdminConfig {
        AdminConfig {
            oidc: None,
            password_hash,
            allowed_emails: vec![],
            allowed_domains: vec![],
            session_secret: secret.map(str::to_string),
            public_url: None,
            session_hours: hours,
        }
    }

    fn auth_with(password: &str, secret: Option<&str>) -> AdminAuth {
        auth_for(TenantId::default_tenant(), password, secret)
    }

    fn auth_for(tenant: TenantId, password: &str, secret: Option<&str>) -> AdminAuth {
        AdminAuth::new(
            &cfg(Some(argon2_hash(password)), secret, 1),
            tenant,
            OutboundPolicy::Private,
        )
        .unwrap()
    }

    fn headers_with_cookie(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::COOKIE, value.parse().unwrap());
        h
    }

    #[test]
    fn password_verifies_and_rejects() {
        let auth = auth_with("correct horse battery staple", None);
        assert!(auth.verify_password("correct horse battery staple"));
        assert!(!auth.verify_password("wrong"));
        assert!(!auth.verify_password(""));
    }

    #[test]
    fn invalid_hash_fails_at_boot_not_first_login() {
        let err = AdminAuth::new(
            &cfg(Some("not-a-phc-hash".into()), None, 1),
            TenantId::default_tenant(),
            OutboundPolicy::Private,
        );
        assert!(err.is_err());
    }

    #[test]
    fn boot_rejects_non_argon2_hash_and_short_secret() {
        // A syntactically-valid but non-argon2 PHC hash would lock the operator
        // out on every login; reject it at boot instead.
        let pbkdf2 = "$pbkdf2-sha256$i=1000$c2FsdA$aGFzaA";
        let default = TenantId::default_tenant;
        assert!(AdminAuth::new(
            &cfg(Some(pbkdf2.into()), None, 1),
            default(),
            OutboundPolicy::Private
        )
        .is_err());
        // A short explicit secret is a footgun — require ≥32 bytes.
        let good = argon2_hash("a strong password");
        assert!(AdminAuth::new(
            &cfg(Some(good.clone()), Some("short"), 1),
            default(),
            OutboundPolicy::Private
        )
        .is_err());
        // Empty/whitespace still falls back to the safe random key.
        assert!(AdminAuth::new(
            &cfg(Some(good), Some("   "), 999_999),
            default(),
            OutboundPolicy::Private
        )
        .is_ok());
        // A config with neither sign-in method is rejected.
        assert!(AdminAuth::new(&cfg(None, None, 1), default(), OutboundPolicy::Private).is_err());
    }

    #[test]
    fn session_round_trips_and_garbage_rejects() {
        let auth = auth_with(
            "a strong password",
            Some("0123456789abcdef0123456789abcdef"),
        );
        let token = auth.issue_session("alice@x.com").unwrap();
        assert_eq!(
            auth.verify_session(&token),
            Some(("alice@x.com".to_string(), TenantId::default_tenant()))
        );
        assert!(auth.verify_session("garbage").is_none());
        assert!(auth.verify_session(&format!("{token}x")).is_none());
        // A token signed under a DIFFERENT key never validates.
        let other = auth_with(
            "a strong password",
            Some("ffffffffffffffffffffffffffffffff"),
        );
        assert!(other.verify_session(&token).is_none());
    }

    #[test]
    fn require_admin_is_404_when_disabled_and_401_without_session() {
        assert_eq!(
            require_admin(None, &HeaderMap::new()),
            Err(StatusCode::NOT_FOUND),
            "disabled surface must be indistinguishable from absent routes"
        );
        let auth = auth_with("a strong password", None);
        assert_eq!(
            require_admin(Some(&auth), &HeaderMap::new()),
            Err(StatusCode::UNAUTHORIZED)
        );
        let token = auth.issue_session(BREAK_GLASS_OPERATOR).unwrap();
        let h = headers_with_cookie(&format!("other=1; {SESSION_COOKIE}={token}"));
        assert_eq!(
            require_admin(Some(&auth), &h).as_deref(),
            Ok(BREAK_GLASS_OPERATOR)
        );
        let h = headers_with_cookie(&format!("{SESSION_COOKIE}=tampered"));
        assert_eq!(
            require_admin(Some(&auth), &h),
            Err(StatusCode::UNAUTHORIZED)
        );
    }

    /// Two tenants sharing a signing key (today's multi-tenant boot copies the
    /// file's `[admin]` to every tenant) must still not honour each other's
    /// sessions: the tenant claim, not the signature, draws that line.
    #[test]
    fn a_session_minted_for_one_tenant_is_refused_by_another() {
        let secret = Some("0123456789abcdef0123456789abcdef");
        let acme = auth_for(
            TenantId::parse("acme").unwrap(),
            "a strong password",
            secret,
        );
        let contoso = auth_for(
            TenantId::parse("contoso").unwrap(),
            "a strong password",
            secret,
        );
        let token = acme.issue_session("alice@acme.test").unwrap();
        let h = headers_with_cookie(&format!("{SESSION_COOKIE}={token}"));
        assert_eq!(
            require_admin(Some(&acme), &h).as_deref(),
            Ok("alice@acme.test")
        );
        assert_eq!(
            contoso.verify_session(&token).map(|(_, t)| t),
            Some(TenantId::parse("acme").unwrap()),
            "the signature checks out under the shared key"
        );
        assert_eq!(
            require_admin(Some(&contoso), &h),
            Err(StatusCode::UNAUTHORIZED),
            "but the session names acme, not contoso"
        );
    }

    #[tokio::test]
    async fn oidc_only_config_is_valid_and_password_is_absent() {
        // A config with only OIDC (no break-glass password) must build, expose
        // no password path, and reject OIDC verification for a garbage token.
        let auth = AdminAuth::new(
            &AdminConfig {
                oidc: Some(AdminOidc {
                    issuer: "https://accounts.google.com".into(),
                    audience: Some("client-x".into()),
                    client_id: Some("client-x".into()),
                    client_secret: None,
                    mode: None,
                }),
                password_hash: None,
                allowed_emails: vec![],
                allowed_domains: vec![],
                session_secret: None,
                public_url: None,
                session_hours: 1,
            },
            TenantId::default_tenant(),
            OutboundPolicy::Private,
        )
        .unwrap();
        assert!(!auth.has_password());
        assert!(!auth.verify_password("anything"), "no password configured");
        assert_eq!(auth.oidc_client_id(), Some("client-x"));
        // Garbage token can't verify (no network needed to reject a non-JWT).
        assert!(auth.verify_oidc_email("not-a-token").await.is_err());
    }

    #[test]
    fn oidc_without_audience_or_client_id_fails_boot() {
        // The audience-binding blocker: OIDC configured with neither audience
        // nor client_id must fail closed at boot, not accept any issuer token.
        let bad = AdminConfig {
            oidc: Some(AdminOidc {
                issuer: "https://accounts.google.com".into(),
                audience: None,
                client_id: None,
                client_secret: None,
                mode: None,
            }),
            password_hash: None,
            allowed_emails: vec![],
            allowed_domains: vec![],
            session_secret: None,
            public_url: None,
            session_hours: 1,
        };
        assert!(AdminAuth::new(&bad, TenantId::default_tenant(), OutboundPolicy::Private).is_err());
    }

    fn oidc_cfg(issuer: &str, client_id: Option<&str>, mode: Option<&str>) -> AdminConfig {
        AdminConfig {
            oidc: Some(AdminOidc {
                issuer: issuer.into(),
                audience: Some("edge-admin".into()),
                client_id: client_id.map(str::to_string),
                client_secret: None,
                mode: mode.map(str::to_string),
            }),
            password_hash: None,
            allowed_emails: vec![],
            allowed_domains: vec![],
            session_secret: None,
            // Redirect mode requires a public_url; harmless for the Google case.
            public_url: Some("https://edge.example.com".into()),
            session_hours: 1,
        }
    }

    #[test]
    fn oidc_mode_defaults_by_issuer_and_redirect_needs_client_id() {
        // Google issuer → the GSI button.
        let g = AdminAuth::new(
            &oidc_cfg("https://accounts.google.com", Some("cid"), None),
            TenantId::default_tenant(),
            OutboundPolicy::Private,
        )
        .unwrap();
        assert_eq!(g.oidc_mode(), Some(OidcMode::Google));
        // Any other issuer (Keycloak) → the redirect flow.
        let k = AdminAuth::new(
            &oidc_cfg(
                "https://auth.example.com/realms/x",
                Some("edge-admin"),
                None,
            ),
            TenantId::default_tenant(),
            OutboundPolicy::Private,
        )
        .unwrap();
        assert_eq!(k.oidc_mode(), Some(OidcMode::Redirect));
        assert_eq!(k.oidc_client_secret(), None);
        // Redirect mode with NO client_id fails closed at boot.
        assert!(
            AdminAuth::new(
                &oidc_cfg("https://auth.example.com/realms/x", None, None),
                TenantId::default_tenant(),
                OutboundPolicy::Private
            )
            .is_err(),
            "redirect flow needs a client_id"
        );
        // Redirect mode also boot-requires public_url (for a stable redirect_uri).
        let mut no_public = oidc_cfg(
            "https://auth.example.com/realms/x",
            Some("edge-admin"),
            None,
        );
        no_public.public_url = None;
        assert!(
            AdminAuth::new(
                &no_public,
                TenantId::default_tenant(),
                OutboundPolicy::Private
            )
            .is_err(),
            "redirect flow needs public_url"
        );
        // Explicit mode override is honoured.
        let forced = AdminAuth::new(
            &oidc_cfg("https://accounts.google.com", Some("cid"), Some("redirect")),
            TenantId::default_tenant(),
            OutboundPolicy::Private,
        )
        .unwrap();
        assert_eq!(forced.oidc_mode(), Some(OidcMode::Redirect));
    }

    #[test]
    fn flow_state_round_trips_and_rejects_tamper() {
        let auth = auth_with(
            "a strong password",
            Some("0123456789abcdef0123456789abcdef"),
        );
        let fs = crate::oidc::FlowState {
            state: "st".into(),
            verifier: "ver".into(),
            nonce: "n0nce".into(),
            exp: 4_000_000_000, // far future
        };
        let tok = auth.issue_flow(&fs).unwrap();
        let back = auth.verify_flow(&tok).expect("valid flow token");
        assert_eq!(back.state, "st");
        assert_eq!(back.verifier, "ver");
        assert_eq!(back.nonce, "n0nce");
        assert!(auth.verify_flow("garbage").is_none());
        // A token signed under a DIFFERENT key never validates.
        let other = auth_with(
            "a strong password",
            Some("ffffffffffffffffffffffffffffffff"),
        );
        assert!(other.verify_flow(&tok).is_none());
        // An expired flow token is rejected.
        let expired = auth
            .issue_flow(&crate::oidc::FlowState {
                state: "s".into(),
                verifier: "v".into(),
                nonce: "n".into(),
                exp: 1,
            })
            .unwrap();
        assert!(auth.verify_flow(&expired).is_none());
    }

    #[tokio::test]
    async fn oidc_verify_errors_when_not_configured() {
        let auth = auth_with("a strong password", None);
        assert!(auth.verify_oidc_email("x").await.is_err());
    }

    /// A caller may shorten a session's life, never lengthen it past the
    /// configured hours.
    #[test]
    fn a_bounded_session_expires_when_asked_and_never_past_the_configured_hours() {
        let auth = auth_with(
            "a strong password",
            Some("0123456789abcdef0123456789abcdef"),
        );
        let now = chrono::Utc::now().timestamp() as usize;
        let short = auth
            .issue_session_for("alice@x.com", std::time::Duration::from_secs(600))
            .unwrap();
        let exp = auth.session_expiry(&short).expect("a valid session");
        assert!(
            (now + 590..=now + 610).contains(&exp),
            "exp {exp} vs now {now}"
        );
        assert_eq!(
            auth.verify_session(&short),
            Some(("alice@x.com".to_string(), TenantId::default_tenant()))
        );
        // The authenticator is configured for one hour; asking for a day
        // still yields an hour.
        let long = auth
            .issue_session_for("alice@x.com", std::time::Duration::from_secs(86_400))
            .unwrap();
        let exp = auth.session_expiry(&long).unwrap();
        assert!(
            (now + 3590..=now + 3610).contains(&exp),
            "exp {exp} vs now {now}"
        );
    }

    /// Reads always pass; a write needs the browser's same-origin word or
    /// an Origin the surface owns, and a request with neither is refused.
    #[test]
    fn csrf_allows_reads_and_same_origin_writes_only() {
        use axum::http::Method;
        let own = |origin: &str| origin_is_host(origin, "acme.test");
        let with = |pairs: &[(&'static str, &str)]| {
            let mut h = HeaderMap::new();
            for (name, value) in pairs {
                h.insert(*name, value.parse().unwrap());
            }
            h
        };
        assert!(csrf_allows(&Method::GET, &HeaderMap::new(), own));
        assert!(csrf_allows(&Method::HEAD, &HeaderMap::new(), own));
        assert!(
            !csrf_allows(&Method::POST, &HeaderMap::new(), own),
            "no header, no proof"
        );
        assert!(csrf_allows(
            &Method::POST,
            &with(&[("sec-fetch-site", "same-origin")]),
            own
        ));
        assert!(csrf_allows(
            &Method::DELETE,
            &with(&[("sec-fetch-site", "none")]),
            own
        ));
        assert!(!csrf_allows(
            &Method::POST,
            &with(&[("sec-fetch-site", "same-site")]),
            own
        ));
        assert!(
            !csrf_allows(
                &Method::POST,
                &with(&[
                    ("sec-fetch-site", "cross-site"),
                    ("origin", "https://acme.test")
                ]),
                own
            ),
            "the browser's verdict stands over a forged Origin"
        );
        assert!(csrf_allows(
            &Method::PUT,
            &with(&[("origin", "https://acme.test")]),
            own
        ));
        assert!(csrf_allows(
            &Method::PUT,
            &with(&[("origin", "https://ACME.test")]),
            own
        ));
        assert!(!csrf_allows(
            &Method::PUT,
            &with(&[("origin", "https://evil.test")]),
            own
        ));
        assert!(!csrf_allows(
            &Method::PUT,
            &with(&[("origin", "https://acme.test.evil")]),
            own
        ));
        assert!(!csrf_allows(
            &Method::PUT,
            &with(&[("origin", "null")]),
            own
        ));
        assert!(
            origin_is_host("https://acme.test:8443", "acme.test"),
            "the port is not part of the host, as the registry keys it"
        );
        assert!(!origin_is_host("not a url", "acme.test"));
    }

    #[test]
    fn cookies_carry_the_security_flags() {
        let auth = auth_with("a strong password", None);
        let c = auth.session_cookie("tok");
        for flag in ["HttpOnly", "Secure", "SameSite=Strict", "Path=/"] {
            assert!(c.contains(flag), "missing {flag}");
        }
        assert!(auth.clear_cookie().contains("Max-Age=0"));
    }
}
