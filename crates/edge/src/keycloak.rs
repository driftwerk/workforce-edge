//! End-customer Keycloak token verification for the customer-facing edge.
//!
//! A customer arrives already authenticated by the company's Keycloak; the
//! widget forwards their access token on every chat frame. Before we trust it
//! to identify the session owner — and forward it verbatim to the company MCP
//! server — we verify, per agent `[embed]` config:
//!
//! 1. signature against the realm's JWKS (RS/ES/PS only — never HMAC);
//! 2. `iss == <configured realm issuer>`;
//! 3. `aud == <configured audience>` when one is pinned;
//! 4. `exp` valid (industry-standard 5-minute clock skew).
//!
//! The JWKS cache, refresh throttle, and algorithm allowlist follow the
//! standard OIDC-validator pattern so the security-relevant bits are the same
//! as the rest of the platform.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, RwLock as StdRwLock};
use std::time::{Duration, Instant};

use eyre::{bail, eyre, Result, WrapErr};
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use tokio::sync::RwLock;
use tracing::warn;
use url::Url;

use crate::outbound::{self, OutboundPolicy, DISCOVERY_DOC_MAX, JWKS_MAX};

const CLOCK_SKEW: u64 = 300;

/// Bound on any call to the identity provider (discovery, JWKS). These run on
/// the socket task — a mid-turn `token` frame validates inline — so an untimed
/// client lets a slow Keycloak park that task, which the relay then reads as a
/// socket that has stopped draining and detaches from its own live turn.
const KEYCLOAK_HTTP_TIMEOUT: Duration = Duration::from_secs(5);

const JWKS_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const JWKS_REFRESH_THROTTLE: Duration = Duration::from_secs(30);
/// Asymmetric algorithms only — excluding HMAC defeats an `alg=HS256` confusion
/// attack that signs with the RSA public-key bytes as an HMAC secret.
const ACCEPTED_ALGS: &[Algorithm] = &[
    Algorithm::RS256,
    Algorithm::RS384,
    Algorithm::RS512,
    Algorithm::ES256,
    Algorithm::ES384,
    Algorithm::PS256,
    Algorithm::PS384,
    Algorithm::PS512,
];

/// GET `url` under `policy` and return its body, capped at `max` bytes under
/// `Public`. Each fetch builds its own client: under `Public` the connection
/// is pinned to the address checked for THIS url, which a shared client could
/// not carry for both the discovery document and the `jwks_uri` it names.
async fn fetch_json(policy: OutboundPolicy, url: &Url, max: usize) -> Result<serde_json::Value> {
    let client = outbound::client_for(policy, KEYCLOAK_HTTP_TIMEOUT, url)
        .wrap_err_with(|| format!("refusing to fetch {url}"))?;
    let resp = client.get(url.clone()).send().await?.error_for_status()?;
    let body = outbound::read_capped(policy, resp, max)
        .await
        .wrap_err_with(|| format!("reading {url}"))?;
    serde_json::from_slice(&body).wrap_err_with(|| format!("{url} is not JSON"))
}

/// The `jwks_uri` a discovery document names, parsed and — under `Public` —
/// checked before anything is fetched from it. The document is the issuer's
/// to write, and the issuer is the tenant's to name: this is the one URL in
/// the flow nobody configured.
fn jwks_uri_from(
    metadata: &serde_json::Value,
    issuer: &str,
    policy: OutboundPolicy,
) -> Result<Url> {
    let raw = metadata
        .get("jwks_uri")
        .and_then(|v| v.as_str())
        .ok_or_else(|| eyre!("Keycloak metadata for `{issuer}` has no jwks_uri"))?;
    let url = Url::parse(raw).wrap_err_with(|| format!("jwks_uri for `{issuer}` is not a URL"))?;
    outbound::check_url(policy, &url)
        .wrap_err_with(|| format!("refusing the jwks_uri `{issuer}` advertises"))?;
    Ok(url)
}

/// The verified identity of an end-customer, the only claims the ingress needs.
pub struct KeycloakClaims {
    pub sub: String,
    pub email: Option<String>,
    /// The IdP's assertion that it verified `email`. `Some(false)` means the
    /// address is explicitly unverified — an email allowlist must reject it,
    /// or anyone could register an account claiming an allowlisted address.
    pub email_verified: Option<bool>,
    /// The `nonce` claim, echoed from the auth request. The Authorization Code
    /// flow binds the returned id_token to its own login attempt by matching
    /// this against the nonce it generated (replay defence).
    pub nonce: Option<String>,
    /// Standard OIDC profile claims, used only to show a human name in the admin
    /// analytics/conversation views (never a trust input).
    pub name: Option<String>,
    pub preferred_username: Option<String>,
    pub given_name: Option<String>,
    pub family_name: Option<String>,
}

impl KeycloakClaims {
    /// A human-readable name for this subject, in precedence order:
    /// `name` → `preferred_username` → `given_name`+`family_name` → `email`.
    /// `None` only when the token carries none of them.
    pub fn display_name(&self) -> Option<String> {
        if let Some(n) = non_empty(self.name.as_deref()) {
            return Some(n);
        }
        if let Some(u) = non_empty(self.preferred_username.as_deref()) {
            return Some(u);
        }
        let joined = [self.given_name.as_deref(), self.family_name.as_deref()]
            .into_iter()
            .flatten()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        if !joined.is_empty() {
            return Some(joined);
        }
        non_empty(self.email.as_deref())
    }
}

/// A trimmed, owned copy of `s` when it is non-empty after trimming.
fn non_empty(s: Option<&str>) -> Option<String> {
    s.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// The OIDC endpoints an install needs for the Authorization Code flow, read
/// from the issuer's discovery document.
#[derive(Debug, Clone)]
pub struct OidcEndpoints {
    pub authorization_endpoint: String,
    pub token_endpoint: String,
}

/// Process-wide cache of discovered endpoints, keyed by `(issuer, policy)`
/// like [`VALIDATORS`]. Endpoints are effectively static per realm; caching
/// them keeps a login (two lookups across `/start` + `/callback`) from
/// re-fetching the discovery doc each time.
static ENDPOINTS: LazyLock<StdRwLock<HashMap<String, (OidcEndpoints, Instant)>>> =
    LazyLock::new(|| StdRwLock::new(HashMap::new()));
const ENDPOINTS_TTL: Duration = Duration::from_secs(3600);

fn endpoints_key(issuer: &str, policy: OutboundPolicy) -> String {
    format!("{issuer}\u{1f}{policy:?}")
}

/// Fetch `{issuer}/.well-known/openid-configuration` and pull the authorize +
/// token endpoints, cached for an hour per issuer. [`OutboundPolicy::Private`];
/// a tenant-supplied issuer goes through [`discover_endpoints_with`].
pub async fn discover_endpoints(issuer: &str) -> Result<OidcEndpoints> {
    discover_endpoints_with(issuer, OutboundPolicy::Private).await
}

/// [`discover_endpoints`] under `policy`. The policy is part of the cache
/// key: a document fetched permissively for one caller is never handed to
/// one that asked for `Public`, even though the endpoints it names are only
/// ever CALLED through a policy-checked client of their own
/// (`oidc::exchange_code_with`).
pub async fn discover_endpoints_with(
    issuer: &str,
    policy: OutboundPolicy,
) -> Result<OidcEndpoints> {
    let key = endpoints_key(issuer, policy);
    if let Some((ep, at)) = ENDPOINTS.read().unwrap().get(&key) {
        if at.elapsed() < ENDPOINTS_TTL {
            return Ok(ep.clone());
        }
    }
    let metadata = fetch_json(policy, &discovery_url(issuer)?, DISCOVERY_DOC_MAX).await?;
    let field = |name: &str| -> Result<String> {
        metadata
            .get(name)
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .ok_or_else(|| eyre!("OIDC metadata for `{issuer}` has no {name}"))
    };
    let endpoints = OidcEndpoints {
        authorization_endpoint: field("authorization_endpoint")?,
        token_endpoint: field("token_endpoint")?,
    };
    ENDPOINTS
        .write()
        .unwrap()
        .insert(key, (endpoints.clone(), Instant::now()));
    Ok(endpoints)
}

/// What an issuer's discovery document and JWKS say, for an operator
/// checking an issuer before binding an agent to it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct IssuerReport {
    /// The `issuer` the document claims — a mismatch with the one asked for
    /// means every token would fail `iss`.
    pub issuer: Option<String>,
    pub jwks_uri: String,
    /// `id_token_signing_alg_values_supported`, as advertised.
    pub algs: Vec<String>,
    /// Signing keys in the JWKS that this edge can verify with (a `kid` and
    /// a key it can load); none means no token will verify.
    pub keys: usize,
}

/// Fetch `issuer`'s discovery document and the JWKS it names, under
/// `policy` — the same two fetches a validator makes, with the same caps
/// and timeouts, reported rather than cached.
pub async fn inspect_issuer(issuer: &str, policy: OutboundPolicy) -> Result<IssuerReport> {
    let metadata = fetch_json(policy, &discovery_url(issuer)?, DISCOVERY_DOC_MAX).await?;
    let jwks_uri = jwks_uri_from(&metadata, issuer, policy)?;
    let jwks: jsonwebtoken::jwk::JwkSet =
        serde_json::from_value(fetch_json(policy, &jwks_uri, JWKS_MAX).await?)
            .wrap_err("JWKS document malformed")?;
    Ok(IssuerReport {
        issuer: metadata
            .get("issuer")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        jwks_uri: jwks_uri.to_string(),
        algs: metadata
            .get("id_token_signing_alg_values_supported")
            .and_then(|v| v.as_array())
            .map(|algs| {
                algs.iter()
                    .filter_map(|a| a.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        keys: jwks_to_keys(&jwks).len(),
    })
}

struct CachedKeys {
    keys: HashMap<String, DecodingKey>,
    fetched_at: Instant,
    last_attempt: Instant,
}

/// Verifies end-customer tokens for one Keycloak realm issuer. One per distinct
/// `(issuer, audience, policy)`; the gateway keeps a small registry keyed by
/// those.
pub struct KeycloakValidator {
    policy: OutboundPolicy,
    issuer: String,
    audience: Option<String>,
    cache: RwLock<Option<CachedKeys>>,
}

impl KeycloakValidator {
    /// A validator that reaches the issuer under [`OutboundPolicy::Private`].
    pub fn new(issuer: impl Into<String>, audience: Option<String>) -> Self {
        Self::with_policy(issuer, audience, OutboundPolicy::Private)
    }

    pub fn with_policy(
        issuer: impl Into<String>,
        audience: Option<String>,
        policy: OutboundPolicy,
    ) -> Self {
        Self {
            policy,
            issuer: issuer.into(),
            audience,
            cache: RwLock::new(None),
        }
    }

    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    pub fn policy(&self) -> OutboundPolicy {
        self.policy
    }

    /// Verify `token` and return its subject. Any `Err` ⇒ the ingress rejects.
    pub async fn verify(&self, token: &str) -> Result<KeycloakClaims> {
        let kid = decode_header(token)?
            .kid
            .ok_or_else(|| eyre!("token header missing kid"))?;
        let key = self
            .decoding_key(&kid)
            .await
            .ok_or_else(|| eyre!("no Keycloak signing key for kid {kid}"))?;
        verify_keycloak_token(token, &key, &self.issuer, self.audience.as_deref())
    }

    async fn decoding_key(&self, kid: &str) -> Option<DecodingKey> {
        {
            let cache = self.cache.read().await;
            if let Some(entry) = cache.as_ref() {
                if entry.fetched_at.elapsed() < JWKS_TTL {
                    if let Some(key) = entry.keys.get(kid) {
                        return Some(key.clone());
                    }
                    if entry.last_attempt.elapsed() < JWKS_REFRESH_THROTTLE {
                        return None;
                    }
                }
            }
        }
        self.refresh().await;
        self.cache.read().await.as_ref()?.keys.get(kid).cloned()
    }

    async fn refresh(&self) {
        let now = Instant::now();
        match self.fetch_keys().await {
            Ok(keys) => {
                *self.cache.write().await = Some(CachedKeys {
                    keys,
                    fetched_at: now,
                    last_attempt: now,
                });
            }
            Err(e) => {
                warn!(issuer = %self.issuer, error = %e, "Keycloak JWKS refresh failed");
                if let Some(entry) = self.cache.write().await.as_mut() {
                    entry.last_attempt = now;
                }
            }
        }
    }

    async fn fetch_keys(&self) -> Result<HashMap<String, DecodingKey>> {
        let metadata = fetch_json(
            self.policy,
            &discovery_url(&self.issuer)?,
            DISCOVERY_DOC_MAX,
        )
        .await?;
        let jwks_uri = jwks_uri_from(&metadata, &self.issuer, self.policy)?;
        let jwks: jsonwebtoken::jwk::JwkSet =
            serde_json::from_value(fetch_json(self.policy, &jwks_uri, JWKS_MAX).await?)
                .wrap_err("JWKS document malformed")?;
        Ok(jwks_to_keys(&jwks))
    }
}

/// Process-wide validator cache keyed by `(issuer, audience, policy)`. A single
/// edge install usually serves one realm, so this stays tiny; reusing the
/// validator keeps its JWKS cache warm across connections instead of refetching
/// per socket. Embed config is hot-reloadable, so a never-evicted entry for a
/// retired issuer is a negligible, bounded leak (one struct per realm ever
/// configured) — not worth an eviction policy.
static VALIDATORS: LazyLock<StdRwLock<HashMap<String, Arc<KeycloakValidator>>>> =
    LazyLock::new(|| StdRwLock::new(HashMap::new()));

/// The shared [`KeycloakValidator`] for `(issuer, audience)` under
/// [`OutboundPolicy::Private`]. The ingress calls this per connection; the
/// cost is one map lookup.
pub fn validator_for(issuer: &str, audience: Option<&str>) -> Arc<KeycloakValidator> {
    validator_for_policy(issuer, audience, OutboundPolicy::Private)
}

/// [`validator_for`] under `policy`. The policy is part of the key: a
/// validator that already reached a tenant's issuer permissively must not be
/// handed to a caller that asked for `Public`.
pub fn validator_for_policy(
    issuer: &str,
    audience: Option<&str>,
    policy: OutboundPolicy,
) -> Arc<KeycloakValidator> {
    let key = format!("{issuer}\u{1f}{}\u{1f}{policy:?}", audience.unwrap_or(""));
    if let Some(found) = VALIDATORS.read().unwrap().get(&key) {
        return Arc::clone(found);
    }
    Arc::clone(VALIDATORS.write().unwrap().entry(key).or_insert_with(|| {
        Arc::new(KeycloakValidator::with_policy(
            issuer,
            audience.map(str::to_string),
            policy,
        ))
    }))
}

fn discovery_url(issuer: &str) -> Result<Url> {
    Url::parse(&format!(
        "{}/.well-known/openid-configuration",
        issuer.trim_end_matches('/')
    ))
    .wrap_err_with(|| format!("issuer `{issuer}` is not a URL"))
}

fn jwks_to_keys(jwks: &jsonwebtoken::jwk::JwkSet) -> HashMap<String, DecodingKey> {
    jwks.keys
        .iter()
        .filter_map(|jwk| Some((jwk.common.key_id.clone()?, DecodingKey::from_jwk(jwk).ok()?)))
        .collect()
}

/// Pure verification: signature + `iss`/`exp` always, `aud` only when pinned.
/// No network — the caller supplies the resolved signing `key`, keeping this
/// unit-testable with a self-signed key.
fn verify_keycloak_token(
    token: &str,
    key: &DecodingKey,
    issuer: &str,
    audience: Option<&str>,
) -> Result<KeycloakClaims> {
    let header = decode_header(token)?;
    if !ACCEPTED_ALGS.contains(&header.alg) {
        bail!("rejected token algorithm {:?}", header.alg);
    }
    let mut validation = Validation::new(header.alg);
    validation.set_issuer(&[issuer]);
    validation.leeway = CLOCK_SKEW;
    // Audience is checked MANUALLY below (accepting `azp` too), so tell
    // jsonwebtoken not to reject on `aud`.
    validation.validate_aud = false;
    validation.set_required_spec_claims(&["exp", "iss"]);

    let claims = decode::<serde_json::Value>(token, key, &validation)?.claims;
    // When an audience is pinned, bind the token to THIS client: accept it only
    // if the expected value appears in `aud` (string or array) OR equals `azp`
    // (the authorized party). Keycloak commonly puts the client id in `azp`
    // rather than `aud` for id_tokens, so an `aud`-only check would reject valid
    // tokens; `azp` still proves the token was issued TO this client, preserving
    // the confused-deputy defence (a different client's token has a different azp).
    if let Some(expected) = audience {
        let aud_match = match claims.get("aud") {
            Some(serde_json::Value::String(s)) => s == expected,
            Some(serde_json::Value::Array(xs)) => xs.iter().any(|v| v.as_str() == Some(expected)),
            _ => false,
        };
        let azp_match = claims.get("azp").and_then(|v| v.as_str()) == Some(expected);
        if !aud_match && !azp_match {
            bail!("token audience/azp does not match `{expected}`");
        }
    }
    let sub = claims
        .get("sub")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| eyre!("token missing sub claim"))?
        .to_string();
    let email = claims
        .get("email")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let email_verified = claims.get("email_verified").and_then(|v| v.as_bool());
    let nonce = claims
        .get("nonce")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let string_claim = |name: &str| {
        claims
            .get(name)
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    Ok(KeycloakClaims {
        sub,
        email,
        email_verified,
        nonce,
        name: string_claim("name"),
        preferred_username: string_claim("preferred_username"),
        given_name: string_claim("given_name"),
        family_name: string_claim("family_name"),
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use jsonwebtoken::{encode, EncodingKey, Header};

    pub(crate) const PRIV_PEM: &str = "-----BEGIN RSA PRIVATE KEY-----
MIIEpAIBAAKCAQEAmZA1ivd3HPWJm+HT+pZAdIfEPrlFdX3ygMdc16QUFLUDC1P5
h3lKav5ZweBrP4Sqx2cuhlEARAAvx/eqPuwI3/DGAi6Dh0sBnUWgvhr6zGvXLUmz
VvJ/O2yFWjyFFkejLg7hp0Wezhq39JSkNGhRW1Y6f9IIGODE+L5NRKAneFvzee56
BnMHY9wDF2dQ8yPR8SUmVOTy/ss/F3QeebLknEDcCxubJIreRqYisLaz5EXB3GoW
5+tJjlpQbQIZ2KojXMyeN5sqWwZZhbY6EcG7BN8Z7ilJ9x6e7aTOtFWGhRXtuJDh
froLRx0kpEq4WIS/EATF0/8AYlxsNkrcGSBG7wIDAQABAoIBAHpHpPsllwnOU2d3
ae7OmkCwo4o2yaKHSgVVuw5Tr1HV2bi5g6dm4+VS6HJDm+MWZ08lUJzP/2AmreWT
x1MXbPG1rlaqZz3ickSj3x3CKheBf/SmDVKdAsd04LM8IdXd8YiJfRJPk+cWy4jM
m/zHXJFYlniV7LCyuuY/0xNkXEmv4s6YVI1NCvc5qpAes/EvH9CohUPlnWepHFgV
Y+wizcu59hSH0+rbtBcLC+iIF09u1q6sl64XOJhdOMwlhiGWx3kF2KepTFzlepEY
YFYhnAqRU4CViVL5BQSODK2uMGh2xDyzKQQJSj3KdMpPAT1s1zhUgOhtbHDSQ/m3
R4wcPSECgYEAy49lClZkg9hN+yshI/bstTuLfPwG6E9HBoEt8xycsTcgJzrZMQoC
XOTACs/m9rOTrsrDi7nA0iaAlxbnk8LazKhZjojo3BSyKOaJnWgnGIOLfXtLcFHB
esTB/tVFcOlrILGLKQURvRMUe/Xb/NkZuyzJyIVtOYoG19NJYDGC+/kCgYEAwR+R
40YM2Due+cbFIWskyj0ZYdyYnL8ViFrZjRWH9dj/YQtVECcIzMvfTyVcgkbyRIdl
iy1O0qaLR2JME/ymX09mHLvI4YIjS1MdQqxIYMUsVkzQcFxoN8204WAnCrbvqUnR
qh2jVHUY7DyQ/q6HxKtRjIEsWwUyYin80UscBCcCgYBc8Rz+KlH8jO+WBaw9Aooz
eXuH1n7E5SWVx7eWL/MvSu8oWks+g8ZuaZdxmFlpPLbqcPP1wRtUGHZQBgo4Qhiq
pxc06DgB/IUX/aLJtvtayv0OjxPztbB0gI/e7bumAT2DkOzsDr95p9OLgccu3GVO
O/Xp01n6kpXrRzJSdGjM6QKBgQCwco2yX0BBGEgp4igeXmxgsu5L8WcmX5fm20rF
kpSEVN0P53Cyr1dk9ihzgalCBlw3rJVkUeMgp7W9ZNwprr67einLuu7QyzxACBJF
5m7Cq6PUvZ376xH1WmcXO+iv63CQ9bcwfJ/L/KjmvnejmgbF89kwTJ0gFGFyOvaY
qs1QJQKBgQCZv+jfuKXTYamIoZLXrRWQh5/RDYGDS3ddm46AvaqpO8xfKKMLWVMM
0A7LdOl3XHADT6ryp+JhaMTqmAxngqxPuxDP3wIxtZoUZrqHCimFnncx22U6S6ys
ItKFbt4eprrwRUqnYSe7iDuORjgcuRlU6Ind4Yy+yJhh8YDudN6oPQ==
-----END RSA PRIVATE KEY-----";

    const PUB_PEM: &str = "-----BEGIN PUBLIC KEY-----
MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAmZA1ivd3HPWJm+HT+pZA
dIfEPrlFdX3ygMdc16QUFLUDC1P5h3lKav5ZweBrP4Sqx2cuhlEARAAvx/eqPuwI
3/DGAi6Dh0sBnUWgvhr6zGvXLUmzVvJ/O2yFWjyFFkejLg7hp0Wezhq39JSkNGhR
W1Y6f9IIGODE+L5NRKAneFvzee56BnMHY9wDF2dQ8yPR8SUmVOTy/ss/F3QeebLk
nEDcCxubJIreRqYisLaz5EXB3GoW5+tJjlpQbQIZ2KojXMyeN5sqWwZZhbY6EcG7
BN8Z7ilJ9x6e7aTOtFWGhRXtuJDhfroLRx0kpEq4WIS/EATF0/8AYlxsNkrcGSBG
7wIDAQAB
-----END PUBLIC KEY-----";

    const ISSUER: &str = "https://auth.harbor.example/realms/harbor";
    const AUDIENCE: &str = "harbor-hub";

    fn far_future() -> usize {
        4_070_908_800
    }

    fn pub_key() -> DecodingKey {
        DecodingKey::from_rsa_pem(PUB_PEM.as_bytes()).unwrap()
    }

    fn claims(extra: serde_json::Value) -> serde_json::Value {
        let mut base = serde_json::json!({
            "iss": ISSUER,
            "aud": AUDIENCE,
            "exp": far_future(),
            "sub": "cust-123",
            "email": "person@example.com",
        });
        let map = base.as_object_mut().unwrap();
        for (k, v) in extra.as_object().unwrap() {
            map.insert(k.clone(), v.clone());
        }
        base
    }

    pub(crate) fn sign(alg: Algorithm, claims: serde_json::Value) -> String {
        let key = EncodingKey::from_rsa_pem(PRIV_PEM.as_bytes()).unwrap();
        let mut header = Header::new(alg);
        header.kid = Some("test-key".to_string());
        encode(&header, &claims, &key).unwrap()
    }

    #[test]
    fn valid_token_passes_and_extracts_identity() {
        let token = sign(Algorithm::RS256, claims(serde_json::json!({})));
        let c = verify_keycloak_token(&token, &pub_key(), ISSUER, Some(AUDIENCE)).unwrap();
        assert_eq!(c.sub, "cust-123");
        assert_eq!(c.email.as_deref(), Some("person@example.com"));
        assert_eq!(c.email_verified, None, "absent claim stays None");
    }

    #[test]
    fn email_verified_claim_is_extracted_when_present() {
        let t = sign(
            Algorithm::RS256,
            claims(serde_json::json!({ "email_verified": true })),
        );
        let c = verify_keycloak_token(&t, &pub_key(), ISSUER, Some(AUDIENCE)).unwrap();
        assert_eq!(c.email_verified, Some(true));

        let t = sign(
            Algorithm::RS256,
            claims(serde_json::json!({ "email_verified": false })),
        );
        let c = verify_keycloak_token(&t, &pub_key(), ISSUER, Some(AUDIENCE)).unwrap();
        assert_eq!(c.email_verified, Some(false));
    }

    #[test]
    fn tampered_signature_rejected() {
        let token = sign(Algorithm::RS256, claims(serde_json::json!({})));
        let (head, _sig) = token.rsplit_once('.').unwrap();
        let tampered = format!("{head}.AAAAinvalidsignatureAAAA");
        assert!(verify_keycloak_token(&tampered, &pub_key(), ISSUER, Some(AUDIENCE)).is_err());
    }

    #[test]
    fn wrong_issuer_rejected() {
        let token = sign(
            Algorithm::RS256,
            claims(serde_json::json!({ "iss": "https://evil/realms/x" })),
        );
        assert!(verify_keycloak_token(&token, &pub_key(), ISSUER, Some(AUDIENCE)).is_err());
    }

    #[test]
    fn wrong_audience_rejected_when_pinned() {
        let token = sign(Algorithm::RS256, claims(serde_json::json!({})));
        assert!(verify_keycloak_token(&token, &pub_key(), ISSUER, Some("other-client")).is_err());
    }

    #[test]
    fn azp_satisfies_pinned_audience_when_aud_is_absent() {
        // Keycloak commonly emits `azp` (the client) but no `aud` on id_tokens.
        let mut c = claims(serde_json::json!({ "azp": AUDIENCE }));
        c.as_object_mut().unwrap().remove("aud");
        let token = sign(Algorithm::RS256, c);
        assert!(
            verify_keycloak_token(&token, &pub_key(), ISSUER, Some(AUDIENCE)).is_ok(),
            "azp matching the pinned audience is accepted"
        );
        // A DIFFERENT client's token (azp=other, no aud) is still rejected —
        // the confused-deputy defence holds.
        let mut c2 = claims(serde_json::json!({ "azp": "other-client" }));
        c2.as_object_mut().unwrap().remove("aud");
        let token2 = sign(Algorithm::RS256, c2);
        assert!(
            verify_keycloak_token(&token2, &pub_key(), ISSUER, Some(AUDIENCE)).is_err(),
            "a different client's azp is rejected"
        );
        // Neither aud nor azp ⇒ rejected when an audience is pinned.
        let mut c3 = claims(serde_json::json!({}));
        c3.as_object_mut().unwrap().remove("aud");
        let token3 = sign(Algorithm::RS256, c3);
        assert!(verify_keycloak_token(&token3, &pub_key(), ISSUER, Some(AUDIENCE)).is_err());
    }

    #[test]
    fn audience_skipped_when_unset() {
        // Pass-through: the webapp's token carries its own aud; with no pin we
        // verify only signature + issuer + expiry, so any realm token passes.
        let token = sign(
            Algorithm::RS256,
            claims(serde_json::json!({ "aud": "some-other-client" })),
        );
        let c = verify_keycloak_token(&token, &pub_key(), ISSUER, None).unwrap();
        assert_eq!(c.sub, "cust-123");
    }

    #[test]
    fn expired_token_rejected() {
        let token = sign(
            Algorithm::RS256,
            claims(serde_json::json!({ "exp": 1_000_000_000usize })),
        );
        assert!(verify_keycloak_token(&token, &pub_key(), ISSUER, Some(AUDIENCE)).is_err());
    }

    #[test]
    fn hs256_confusion_rejected() {
        let key = EncodingKey::from_secret(PUB_PEM.as_bytes());
        let mut header = Header::new(Algorithm::HS256);
        header.kid = Some("test-key".to_string());
        let token = encode(&header, &claims(serde_json::json!({})), &key).unwrap();
        assert!(verify_keycloak_token(&token, &pub_key(), ISSUER, Some(AUDIENCE)).is_err());
    }

    #[test]
    fn missing_sub_rejected() {
        let mut c = claims(serde_json::json!({}));
        c.as_object_mut().unwrap().remove("sub");
        let token = sign(Algorithm::RS256, c);
        assert!(verify_keycloak_token(&token, &pub_key(), ISSUER, None).is_err());
    }

    #[test]
    fn validator_for_caches_by_issuer_and_audience() {
        let a = validator_for("https://realm-a/realms/x", Some("client-1"));
        let again = validator_for("https://realm-a/realms/x", Some("client-1"));
        assert!(Arc::ptr_eq(&a, &again), "same key reuses the validator");

        let diff_aud = validator_for("https://realm-a/realms/x", Some("client-2"));
        assert!(!Arc::ptr_eq(&a, &diff_aud), "audience is part of the key");

        let diff_issuer = validator_for("https://realm-b/realms/y", Some("client-1"));
        assert!(!Arc::ptr_eq(&a, &diff_issuer), "issuer is part of the key");
    }

    #[test]
    fn profile_claims_are_extracted_and_name_wins() {
        let t = sign(
            Algorithm::RS256,
            claims(serde_json::json!({
                "name": "Ada Lovelace",
                "preferred_username": "ada",
                "given_name": "Ada",
                "family_name": "Lovelace",
            })),
        );
        let c = verify_keycloak_token(&t, &pub_key(), ISSUER, Some(AUDIENCE)).unwrap();
        assert_eq!(c.name.as_deref(), Some("Ada Lovelace"));
        assert_eq!(c.preferred_username.as_deref(), Some("ada"));
        assert_eq!(c.display_name().as_deref(), Some("Ada Lovelace"));
    }

    fn named_claims(
        name: Option<&str>,
        preferred: Option<&str>,
        given: Option<&str>,
        family: Option<&str>,
        email: Option<&str>,
    ) -> KeycloakClaims {
        KeycloakClaims {
            sub: "s".into(),
            email: email.map(str::to_string),
            email_verified: None,
            nonce: None,
            name: name.map(str::to_string),
            preferred_username: preferred.map(str::to_string),
            given_name: given.map(str::to_string),
            family_name: family.map(str::to_string),
        }
    }

    #[test]
    fn display_name_follows_precedence() {
        // name wins outright.
        assert_eq!(
            named_claims(
                Some("Full Name"),
                Some("uname"),
                Some("G"),
                Some("F"),
                Some("e@x")
            )
            .display_name()
            .as_deref(),
            Some("Full Name")
        );
        // preferred_username next.
        assert_eq!(
            named_claims(None, Some("uname"), Some("G"), Some("F"), Some("e@x"))
                .display_name()
                .as_deref(),
            Some("uname")
        );
        // given + family joined.
        assert_eq!(
            named_claims(None, None, Some("Grace"), Some("Hopper"), Some("e@x"))
                .display_name()
                .as_deref(),
            Some("Grace Hopper")
        );
        // A lone given (or family) name still resolves.
        assert_eq!(
            named_claims(None, None, Some("Grace"), None, Some("e@x"))
                .display_name()
                .as_deref(),
            Some("Grace")
        );
        // email is the final fallback.
        assert_eq!(
            named_claims(None, None, None, None, Some("e@x"))
                .display_name()
                .as_deref(),
            Some("e@x")
        );
        // Blank/whitespace claims are skipped, not returned.
        assert_eq!(
            named_claims(Some("  "), Some(""), None, None, Some("e@x"))
                .display_name()
                .as_deref(),
            Some("e@x")
        );
        // Nothing at all.
        assert_eq!(
            named_claims(None, None, None, None, None).display_name(),
            None
        );
    }

    #[test]
    fn discovery_url_appends_well_known_without_double_slash() {
        assert_eq!(
            discovery_url("https://host/realms/r/").unwrap().as_str(),
            "https://host/realms/r/.well-known/openid-configuration"
        );
        assert_eq!(
            discovery_url("https://host/realms/r").unwrap().as_str(),
            "https://host/realms/r/.well-known/openid-configuration"
        );
        assert!(discovery_url("not a url").is_err());
    }

    #[test]
    fn validator_cache_keys_on_policy_too() {
        let private = validator_for("https://realm-p/realms/x", None);
        let public = validator_for_policy("https://realm-p/realms/x", None, OutboundPolicy::Public);
        assert!(!Arc::ptr_eq(&private, &public), "policy is part of the key");
        assert_eq!(private.policy(), OutboundPolicy::Private);
        assert_eq!(public.policy(), OutboundPolicy::Public);
    }

    /// The `jwks_uri` is the one URL in the flow nobody configured: the
    /// issuer's document names it. Under `Public` it is checked like the
    /// issuer itself; under `Private` it is taken as-is, as always.
    #[test]
    fn a_jwks_uri_at_a_private_address_is_refused_only_under_public() {
        for inward in [
            "https://10.0.0.1/certs",
            "https://127.0.0.1/certs",
            "https://169.254.169.254/latest/meta-data/",
            "https://[fd00::1]/certs",
            "http://93.184.216.34/certs",
        ] {
            let doc = serde_json::json!({ "jwks_uri": inward });
            let err = jwks_uri_from(&doc, ISSUER, OutboundPolicy::Public)
                .unwrap_err()
                .to_string();
            assert!(err.contains("refusing the jwks_uri"), "{inward}: {err}");
            assert_eq!(
                jwks_uri_from(&doc, ISSUER, OutboundPolicy::Private)
                    .unwrap()
                    .as_str(),
                inward,
                "Private takes {inward} as-is"
            );
        }
        let doc = serde_json::json!({ "jwks_uri": "https://93.184.216.34/certs" });
        assert!(jwks_uri_from(&doc, ISSUER, OutboundPolicy::Public).is_ok());
        assert!(jwks_uri_from(&serde_json::json!({}), ISSUER, OutboundPolicy::Private).is_err());
    }

    /// The public modulus of [`PRIV_PEM`], as a JWK.
    const TEST_KEY_N: &str = "mZA1ivd3HPWJm-HT-pZAdIfEPrlFdX3ygMdc16QUFLUDC1P5h3lKav5ZweBrP4Sqx2cuhlEARAAvx_eqPuwI3_DGAi6Dh0sBnUWgvhr6zGvXLUmzVvJ_O2yFWjyFFkejLg7hp0Wezhq39JSkNGhRW1Y6f9IIGODE-L5NRKAneFvzee56BnMHY9wDF2dQ8yPR8SUmVOTy_ss_F3QeebLknEDcCxubJIreRqYisLaz5EXB3GoW5-tJjlpQbQIZ2KojXMyeN5sqWwZZhbY6EcG7BN8Z7ilJ9x6e7aTOtFWGhRXtuJDhfroLRx0kpEq4WIS_EATF0_8AYlxsNkrcGSBG7w";

    /// A loopback IdP whose discovery document names a loopback `jwks_uri`
    /// — exactly what a self-hosted install talks to, and exactly what a
    /// cloud tenant must not be able to point the edge at.
    async fn mock_idp() -> (wiremock::MockServer, String) {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        // A realm of its own per call: the caches are keyed by issuer and
        // a port one test's server gave back is another's to get.
        let realm = format!("realms/{}", uuid::Uuid::new_v4().simple());
        let issuer = format!("{}/{realm}", server.uri());
        Mock::given(method("GET"))
            .and(path(format!("/{realm}/.well-known/openid-configuration")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "jwks_uri": format!("{}/jwks", server.uri()),
                "authorization_endpoint": format!("{}/auth", server.uri()),
                "token_endpoint": format!("{}/token", server.uri()),
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

    #[tokio::test]
    async fn private_verifies_against_a_loopback_http_idp() {
        let (_server, issuer) = mock_idp().await;
        let validator = KeycloakValidator::new(issuer.clone(), None);
        let token = sign(
            Algorithm::RS256,
            serde_json::json!({ "iss": issuer, "sub": "cust-1", "exp": far_future() }),
        );
        let claims = validator.verify(&token).await.expect("verified");
        assert_eq!(claims.sub, "cust-1");
        let endpoints = discover_endpoints_with(&issuer, OutboundPolicy::Private)
            .await
            .expect("discovered");
        assert!(endpoints.token_endpoint.ends_with("/token"));
    }

    /// Same IdP, `Public`: `http://127.0.0.1` is refused before a byte is
    /// sent, so the token cannot verify and discovery fails loudly.
    #[tokio::test]
    async fn public_refuses_a_loopback_http_idp() {
        let (_server, issuer) = mock_idp().await;
        let validator =
            KeycloakValidator::with_policy(issuer.clone(), None, OutboundPolicy::Public);
        let token = sign(
            Algorithm::RS256,
            serde_json::json!({ "iss": issuer, "sub": "cust-1", "exp": far_future() }),
        );
        assert!(validator.verify(&token).await.is_err());
        let err = validator
            .fetch_keys()
            .await
            .expect_err("a loopback http idp is refused")
            .to_string();
        assert!(err.contains("refusing to fetch"), "{err}");
        let err = discover_endpoints_with(&issuer, OutboundPolicy::Public)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("refusing to fetch"), "{err}");
    }

    /// The operator's issuer check reports what the discovery document
    /// claims and how many keys the JWKS yields — and under `Public` a
    /// loopback issuer is refused before either is fetched.
    #[tokio::test]
    async fn inspecting_an_issuer_reports_its_document_and_usable_keys() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        let realm = format!("realms/{}", uuid::Uuid::new_v4().simple());
        let issuer = format!("{}/{realm}", server.uri());
        Mock::given(method("GET"))
            .and(path(format!("/{realm}/.well-known/openid-configuration")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "issuer": issuer,
                "jwks_uri": format!("{}/jwks", server.uri()),
                "id_token_signing_alg_values_supported": ["RS256", "ES256"],
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/jwks"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "keys": [
                    {"kty": "RSA", "kid": "test-key", "use": "sig", "alg": "RS256",
                     "n": TEST_KEY_N, "e": "AQAB"},
                    // No kid: unusable, so not counted.
                    {"kty": "RSA", "use": "sig", "alg": "RS256", "n": TEST_KEY_N, "e": "AQAB"}
                ],
            })))
            .mount(&server)
            .await;

        let report = inspect_issuer(&issuer, OutboundPolicy::Private)
            .await
            .expect("inspected");
        assert_eq!(report.issuer.as_deref(), Some(issuer.as_str()));
        assert_eq!(report.jwks_uri, format!("{}/jwks", server.uri()));
        assert_eq!(report.algs, ["RS256", "ES256"]);
        assert_eq!(report.keys, 1);

        let err = inspect_issuer(&issuer, OutboundPolicy::Public)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("refusing to fetch"), "{err}");
        assert!(inspect_issuer("not a url", OutboundPolicy::Private)
            .await
            .is_err());
    }
}
