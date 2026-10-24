//! Per-user capability resolution for tool tiers ([`crate::agent::ToolGrant`]).
//!
//! For `backend_permissions` grants the edge fetches the signed-in user's
//! resolved permission set from the install's own RBAC endpoint (the same source
//! its app uses), so the tools we offer can never widen beyond what the backend
//! will actually allow. The result is cached per subject for the configured TTL,
//! so we fetch once per session rather than once per turn.
//!
//! This is a UX / defence-in-depth layer only: the company backend still
//! enforces every write per-user on the forwarded token, so a fetch failure is
//! handled fail-closed (no write tools offered) without any loss of security.

use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, RwLock as StdRwLock};
use std::time::{Duration, Instant};

use base64::Engine as _;
use eyre::{Result, WrapErr};
use serde::Deserialize;

use crate::agent::{GrantSource, PermissionsConfig, ToolGrant};
use crate::outbound::{self, OutboundPolicy, PERMISSIONS_MAX};

/// Bound on one permissions fetch; the result is cached per session.
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// Extract the Keycloak roles carried in an ALREADY-VALIDATED token:
/// `realm_access.roles` plus every `resource_access.<client>.roles`. This only
/// base64-decodes the JWT payload (no signature check) — the caller passes the
/// token ingress already verified, so these are trusted claims. A malformed
/// token yields no roles (fail-closed). Used only for `keycloak_role` grants;
/// even a wrong result cannot widen access, since the backend enforces writes.
pub fn roles_from_token(token: &str) -> HashSet<String> {
    let mut roles = HashSet::new();
    let Some(payload_b64) = token.split('.').nth(1) else {
        return roles;
    };
    let Ok(bytes) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload_b64) else {
        return roles;
    };
    let Ok(claims) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return roles;
    };
    let mut collect = |arr: Option<&Vec<serde_json::Value>>| {
        if let Some(arr) = arr {
            for r in arr {
                if let Some(s) = r.as_str() {
                    roles.insert(s.to_string());
                }
            }
        }
    };
    collect(
        claims
            .get("realm_access")
            .and_then(|v| v.get("roles"))
            .and_then(|v| v.as_array()),
    );
    if let Some(map) = claims.get("resource_access").and_then(|v| v.as_object()) {
        for client in map.values() {
            collect(client.get("roles").and_then(|v| v.as_array()));
        }
    }
    roles
}

/// Extract the `sub` claim from an already-validated token (base64 payload
/// decode). Used to key the per-user permissions lookup and to fill the `{user}`
/// slot in the permissions URL. `None` on a malformed token.
pub fn sub_from_token(token: &str) -> Option<String> {
    let payload_b64 = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload_b64)
        .ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    claims
        .get("sub")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// A user's resolved capabilities, aggregated across the three grant sources.
#[derive(Debug, Clone, Default)]
pub struct UserCapabilities {
    /// Permission strings from the backend RBAC endpoint (e.g. `"items:edit"`).
    /// A superadmin marker (`"*:*"` or `"*"`) satisfies any requirement.
    pub permissions: HashSet<String>,
    /// Realm/client roles carried in the user's own token.
    pub roles: HashSet<String>,
    /// Tier hints asserted by the embedding host (UX only, never a boundary).
    pub hints: HashSet<String>,
}

impl UserCapabilities {
    fn is_superadmin(&self) -> bool {
        self.permissions.contains("*:*") || self.permissions.contains("*")
    }

    /// Whether the user satisfies a grant, checked against the grant's own
    /// `source`. Unknown/empty requirements are never satisfied (fail-closed).
    pub fn satisfies(&self, grant: &ToolGrant) -> bool {
        if grant.requires.is_empty() {
            return false;
        }
        match grant.source {
            GrantSource::BackendPermissions => {
                self.is_superadmin() || self.permissions.contains(&grant.requires)
            }
            GrantSource::KeycloakRole => self.roles.contains(&grant.requires),
            GrantSource::HostHint => self.hints.contains(&grant.requires),
        }
    }
}

/// The `{permissions, roles}` shape a backend permissions endpoint must return.
#[derive(Debug, Deserialize)]
struct PermissionsResponse {
    #[serde(default)]
    permissions: Vec<String>,
    #[serde(default)]
    roles: Vec<String>,
}

/// Process-wide cache: [`cache_key`] → (permissions, roles, fetched-at). Bounded
/// by the number of distinct (issuer, endpoint, user) triples seen; entries
/// refresh on TTL expiry.
struct CacheEntry {
    permissions: HashSet<String>,
    roles: HashSet<String>,
    at: Instant,
}

static CACHE: LazyLock<StdRwLock<HashMap<String, CacheEntry>>> =
    LazyLock::new(|| StdRwLock::new(HashMap::new()));

/// The cache key for one user's permissions at one endpoint. The resolved URL
/// already names the backend, its tenant and the user; the issuer is part of
/// the key because the backend answers for the bearer it was shown, and the
/// same `sub` minted by another issuer is another person.
fn cache_key(issuer: &str, resolved_url: &str) -> String {
    format!("{issuer}\u{1f}{resolved_url}")
}

/// Fetch (or return cached) backend permissions for `subject`, a user of
/// `issuer`, calling `config.url` with the user's `bearer`. On any error the
/// user is treated as having no backend permissions (fail-closed: write tools
/// stay hidden) — safe, because the backend still enforces every write
/// regardless. Reaches the endpoint under [`OutboundPolicy::Private`]; a
/// tenant-supplied endpoint goes through [`backend_permissions_with`].
pub async fn backend_permissions(
    config: &PermissionsConfig,
    issuer: &str,
    subject: &str,
    bearer: &str,
) -> (HashSet<String>, HashSet<String>) {
    backend_permissions_with(config, issuer, subject, bearer, OutboundPolicy::Private).await
}

/// [`backend_permissions`] under `policy`. The cache is unchanged and shared:
/// an entry is the backend's answer for one bearer at one URL, and the policy
/// only governs how that URL is reached.
pub async fn backend_permissions_with(
    config: &PermissionsConfig,
    issuer: &str,
    subject: &str,
    bearer: &str,
    policy: OutboundPolicy,
) -> (HashSet<String>, HashSet<String>) {
    let ttl = Duration::from_secs(config.cache_ttl_secs());
    // The endpoint identifies the user by id in the path; substitute the caller's
    // subject into the `{user}` placeholder. (Other envs vary the tenant, so the
    // rest of the URL is bound per-env via ${EDGE_PERMISSIONS_URL}.)
    let url = config.url.replace("{user}", subject);
    let key = cache_key(issuer, &url);
    if let Some(e) = CACHE.read().unwrap().get(&key) {
        if e.at.elapsed() < ttl {
            return (e.permissions.clone(), e.roles.clone());
        }
    }
    match fetch(policy, &url, bearer).await {
        Ok((permissions, roles)) => {
            CACHE.write().unwrap().insert(
                key,
                CacheEntry {
                    permissions: permissions.clone(),
                    roles: roles.clone(),
                    at: Instant::now(),
                },
            );
            (permissions, roles)
        }
        Err(e) => {
            tracing::warn!(
                url = %config.url,
                error = %e,
                "backend permissions fetch failed; treating user as read-only"
            );
            (HashSet::new(), HashSet::new())
        }
    }
}

async fn fetch(
    policy: OutboundPolicy,
    url: &str,
    bearer: &str,
) -> Result<(HashSet<String>, HashSet<String>)> {
    let url = url::Url::parse(url).wrap_err("permissions url is not a URL")?;
    let resp = outbound::client_for(policy, FETCH_TIMEOUT, &url)
        .wrap_err("refusing the permissions url")?
        .get(url)
        .bearer_auth(bearer)
        .send()
        .await
        .wrap_err("permissions request failed")?
        .error_for_status()
        .wrap_err("permissions endpoint returned an error status")?;
    let body = outbound::read_capped(policy, resp, PERMISSIONS_MAX)
        .await
        .wrap_err("permissions response unreadable")?;
    let resp: PermissionsResponse = serde_json::from_slice(&body)
        .wrap_err("permissions response was not the expected {permissions, roles} JSON")?;
    Ok((
        resp.permissions.into_iter().collect(),
        resp.roles.into_iter().collect(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(perms: &[&str], roles: &[&str], hints: &[&str]) -> UserCapabilities {
        UserCapabilities {
            permissions: perms.iter().map(|s| s.to_string()).collect(),
            roles: roles.iter().map(|s| s.to_string()).collect(),
            hints: hints.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn grant(source: GrantSource, requires: &str) -> ToolGrant {
        ToolGrant {
            source,
            requires: requires.into(),
            tools: vec!["w".into()],
        }
    }

    #[test]
    fn backend_permission_grant_matches_the_permission_set() {
        let c = caps(&["items:edit"], &[], &[]);
        assert!(c.satisfies(&grant(GrantSource::BackendPermissions, "items:edit")));
        assert!(!c.satisfies(&grant(GrantSource::BackendPermissions, "pickups:edit")));
    }

    #[test]
    fn superadmin_satisfies_any_backend_grant() {
        assert!(caps(&["*:*"], &[], &[])
            .satisfies(&grant(GrantSource::BackendPermissions, "anything:edit")));
        assert!(caps(&["*"], &[], &[]).satisfies(&grant(GrantSource::BackendPermissions, "x:y")));
    }

    #[test]
    fn each_source_checks_its_own_bucket() {
        let c = caps(&["items:edit"], &["editor"], &["writer"]);
        // role grant checks roles, not permissions
        assert!(c.satisfies(&grant(GrantSource::KeycloakRole, "editor")));
        assert!(!c.satisfies(&grant(GrantSource::KeycloakRole, "items:edit")));
        // host hint checks hints
        assert!(c.satisfies(&grant(GrantSource::HostHint, "writer")));
        assert!(!c.satisfies(&grant(GrantSource::HostHint, "editor")));
    }

    #[test]
    fn empty_requirement_is_never_satisfied() {
        assert!(!caps(&["*:*"], &[], &[]).satisfies(&grant(GrantSource::BackendPermissions, "")));
    }

    #[test]
    fn cache_key_tells_apart_issuers_endpoints_and_users() {
        let url = |user: &str| format!("https://api.example/t/1/users/{user}/permissions");
        let a = cache_key("https://idp-a.example/realms/x", &url("sub-1"));
        assert_eq!(
            a,
            cache_key("https://idp-a.example/realms/x", &url("sub-1"))
        );
        assert_ne!(
            a,
            cache_key("https://idp-b.example/realms/x", &url("sub-1"))
        );
        assert_ne!(
            a,
            cache_key("https://idp-a.example/realms/x", &url("sub-2"))
        );
        assert_ne!(
            a,
            cache_key(
                "https://idp-a.example/realms/x",
                "https://api.example/t/2/users/sub-1/permissions"
            )
        );
    }

    async fn mock_backend() -> wiremock::MockServer {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/users/sub-1/permissions"))
            .and(header("authorization", "Bearer tok"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "permissions": ["items:edit"],
                "roles": ["editor"],
            })))
            .mount(&server)
            .await;
        server
    }

    /// A self-hosted install's RBAC endpoint is on a private, plain-http
    /// address and keeps answering under the default policy.
    #[tokio::test]
    async fn private_fetches_from_a_loopback_http_backend() {
        let server = mock_backend().await;
        let url = format!("{}/users/{{user}}/permissions", server.uri());
        let (perms, roles) = fetch(
            OutboundPolicy::Private,
            &url.replace("{user}", "sub-1"),
            "tok",
        )
        .await
        .expect("fetched");
        assert!(perms.contains("items:edit"));
        assert!(roles.contains("editor"));
    }

    /// The same endpoint under `Public` is refused before the bearer is
    /// sent, and the user is read-only (fail-closed), not errored.
    #[tokio::test]
    async fn public_refuses_a_loopback_http_backend_and_fails_closed() {
        let server = mock_backend().await;
        let url = format!("{}/users/sub-1/permissions", server.uri());
        let err = fetch(OutboundPolicy::Public, &url, "tok")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("refusing the permissions url"), "{err}");
        assert!(server.received_requests().await.unwrap().is_empty());

        let config = PermissionsConfig {
            url: format!("{}/users/{{user}}/permissions", server.uri()),
            cache_ttl_secs: None,
        };
        let (perms, roles) = backend_permissions_with(
            &config,
            "https://idp.example/realms/x",
            "sub-public",
            "tok",
            OutboundPolicy::Public,
        )
        .await;
        assert!(perms.is_empty() && roles.is_empty(), "read-only on refusal");
    }

    #[test]
    fn permissions_response_parses_and_tolerates_missing_roles() {
        let r: PermissionsResponse =
            serde_json::from_str(r#"{"permissions":["items:edit","pickups:view"]}"#).unwrap();
        assert_eq!(r.permissions.len(), 2);
        assert!(r.roles.is_empty());
    }

    #[test]
    fn roles_from_token_reads_realm_and_resource_roles() {
        let claims = serde_json::json!({
            "realm_access": {"roles": ["editor", "staff"]},
            "resource_access": {
                "backoffice": {"roles": ["items-writer"]},
                "other": {"roles": ["x"]}
            }
        });
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&claims).unwrap());
        let token = format!("header.{payload}.sig");
        let roles = roles_from_token(&token);
        assert!(roles.contains("editor"));
        assert!(roles.contains("staff"));
        assert!(roles.contains("items-writer"));
        assert!(roles.contains("x"));
    }

    #[test]
    fn roles_from_token_is_fail_closed_on_garbage() {
        assert!(roles_from_token("not-a-jwt").is_empty());
        assert!(roles_from_token("h.@@@not-base64@@@.sig").is_empty());
        assert!(roles_from_token("").is_empty());
    }

    #[test]
    fn sub_from_token_reads_the_sub_claim() {
        let claims = serde_json::json!({"sub": "55b43b66-abc", "realm_access": {"roles": []}});
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&claims).unwrap());
        assert_eq!(
            sub_from_token(&format!("h.{payload}.s")).as_deref(),
            Some("55b43b66-abc")
        );
        assert!(sub_from_token("garbage").is_none());
        assert!(sub_from_token("").is_none());
    }
}
