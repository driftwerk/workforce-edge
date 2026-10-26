//! OAuth2 Token Exchange (RFC 8693) against Keycloak.
//!
//! When an install configures `[token_exchange]`, the edge acts as a
//! confidential Keycloak client and exchanges the customer's access token
//! (the WebSocket subject token) for a NEW downstream token before each turn's
//! company-MCP calls — instead of forwarding the customer's raw SPA token.
//!
//! Why (deep-research backed):
//! - A token minted at turn start carries the realm's full access-token TTL
//!   (≥ the turn cap), so it cannot die mid-turn the way a browser-supplied
//!   token nearing expiry can.
//! - With `audience` set, the exchanged token is scoped to the MCP's own
//!   client — fixing the audience-confusion / confused-deputy smell of
//!   replaying a token minted for the host SPA against a different resource
//!   server.
//!
//! Keycloak notes: Standard Token Exchange is GA since 26.2; the requesting
//! client must be confidential with the "Standard token exchange" capability
//! enabled, and must belong to the SAME realm as the agents' `keycloak_issuer`.
//! The subject token must still be valid at exchange time — the host-push
//! token contract (Phase A) keeps it fresh.
//!
//! This is INSTALL-LEVEL config (like `[llm]`), deliberately not per-agent
//! DB config: the client secret lives only in env → config → process memory,
//! never in the DB-authoritative agent rows or the admin API.

use std::time::Duration;

use eyre::{eyre, Result};
use serde::Deserialize;
use url::Url;

use crate::config::TokenExchangeConfig;
use crate::outbound::{self, OutboundPolicy, TOKEN_EXCHANGE_MAX};

const GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:token-exchange";
const TOKEN_TYPE_ACCESS: &str = "urn:ietf:params:oauth:token-type:access_token";
/// Exchange must never stall a turn: fail fast and fall back.
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(10);

/// A ready-to-use exchanger: the install config plus the policy its issuer is
/// reached under. The client is built per exchange — under `Public` it is
/// pinned to the address checked for that call.
pub struct TokenExchanger {
    cfg: TokenExchangeConfig,
    policy: OutboundPolicy,
}

impl TokenExchanger {
    /// An exchanger that reaches the issuer under [`OutboundPolicy::Private`].
    pub fn new(cfg: TokenExchangeConfig) -> Self {
        Self::with_policy(cfg, OutboundPolicy::Private)
    }

    pub fn with_policy(cfg: TokenExchangeConfig, policy: OutboundPolicy) -> Self {
        Self { cfg, policy }
    }

    /// The pinned realm issuer this exchanger talks to. Hosts compare their
    /// agent's `keycloak_issuer` against this at build time.
    pub fn issuer(&self) -> &str {
        &self.cfg.issuer
    }

    pub fn policy(&self) -> OutboundPolicy {
        self.policy
    }

    /// The install's `[token_exchange].audience` — what a server that names
    /// no audience of its own is exchanged for.
    pub fn audience(&self) -> Option<&str> {
        self.cfg.audience.as_deref()
    }

    /// The issuer's token endpoint, as a URL.
    fn token_endpoint(&self) -> Result<Url> {
        Url::parse(&format!(
            "{}/protocol/openid-connect/token",
            self.cfg.issuer.trim_end_matches('/')
        ))
        .map_err(|e| eyre!("token exchange issuer is not a URL: {e}"))
    }

    /// Exchange `subject_token` (the customer's validated access token) for a
    /// downstream access token scoped to `audience`, at the PINNED issuer's
    /// token endpoint (never a per-agent, admin-editable URL). Returns only the
    /// new access token — per-turn re-exchange is the intended pattern, so no
    /// refresh token is requested or stored.
    pub async fn exchange(&self, subject_token: &str, audience: Option<&str>) -> Result<String> {
        let url = self.token_endpoint()?;
        let client = outbound::client_for(self.policy, EXCHANGE_TIMEOUT, &url)
            .map_err(|e| eyre!("refusing the token exchange endpoint: {e}"))?;
        let resp = client
            .post(url)
            .form(&form_params(&self.cfg, subject_token, audience))
            .send()
            .await
            .map_err(|e| eyre!("token exchange request failed: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            // Keycloak error bodies are {"error","error_description"} — no
            // tokens — but truncate hard anyway.
            let body: String = resp
                .text()
                .await
                .unwrap_or_default()
                .chars()
                .take(200)
                .collect();
            return Err(eyre!("token exchange rejected ({status}): {body}"));
        }
        #[derive(Deserialize)]
        struct ExchangeResponse {
            access_token: String,
        }
        let body = outbound::read_capped(self.policy, resp, TOKEN_EXCHANGE_MAX)
            .await
            .map_err(|e| eyre!("token exchange response unreadable: {e}"))?;
        Ok(serde_json::from_slice::<ExchangeResponse>(&body)
            .map_err(|e| eyre!("token exchange response malformed: {e}"))?
            .access_token)
    }
}

/// The RFC 8693 form body. Split out so the request shape is testable without
/// an HTTP mock. `audience` rides only when given (the MCP's Keycloak
/// clientId); without it the exchanged token still gains a fresh TTL, just not
/// the audience restriction. `scope` rides only when configured.
fn form_params<'a>(
    cfg: &'a TokenExchangeConfig,
    subject_token: &'a str,
    audience: Option<&'a str>,
) -> Vec<(&'static str, &'a str)> {
    let mut params = vec![
        ("grant_type", GRANT_TYPE),
        ("client_id", cfg.client_id.as_str()),
        ("client_secret", cfg.client_secret.as_str()),
        ("subject_token", subject_token),
        ("subject_token_type", TOKEN_TYPE_ACCESS),
        ("requested_token_type", TOKEN_TYPE_ACCESS),
    ];
    if let Some(aud) = audience {
        params.push(("audience", aud));
    }
    if let Some(scope) = cfg.scope.as_deref() {
        params.push(("scope", scope));
    }
    params
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(audience: Option<&str>) -> TokenExchangeConfig {
        TokenExchangeConfig {
            issuer: "https://kc.example/realms/r".into(),
            client_id: "workforce-edge-service".into(),
            client_secret: "s3cret".into(),
            audience: audience.map(String::from),
            scope: None,
        }
    }

    #[test]
    fn form_carries_rfc8693_fields_and_optional_audience() {
        let c = cfg(None);
        let params = form_params(&c, "subject-jwt", Some("harbor-mcp"));
        let get = |k: &str| params.iter().find(|(pk, _)| *pk == k).map(|(_, v)| *v);
        assert_eq!(get("grant_type"), Some(GRANT_TYPE));
        assert_eq!(get("subject_token"), Some("subject-jwt"));
        assert_eq!(get("subject_token_type"), Some(TOKEN_TYPE_ACCESS));
        assert_eq!(get("requested_token_type"), Some(TOKEN_TYPE_ACCESS));
        assert_eq!(get("audience"), Some("harbor-mcp"));

        let no_aud = form_params(&c, "t", None);
        assert!(no_aud.iter().all(|(k, _)| *k != "audience"));
    }

    #[test]
    fn form_carries_scope_only_when_configured() {
        let scoped = TokenExchangeConfig {
            scope: Some("openid".into()),
            ..cfg(None)
        };
        let params = form_params(&scoped, "t", None);
        assert!(params.contains(&("scope", "openid")));

        let plain = cfg(None);
        assert!(form_params(&plain, "t", None)
            .iter()
            .all(|(k, _)| *k != "scope"));
    }

    fn cfg_at(issuer: &str) -> TokenExchangeConfig {
        TokenExchangeConfig {
            issuer: issuer.into(),
            ..cfg(None)
        }
    }

    async fn mock_keycloak() -> wiremock::MockServer {
        use wiremock::matchers::{body_string_contains, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/realms/r/protocol/openid-connect/token"))
            .and(body_string_contains("subject_token=subject-jwt"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "access_token": "down.stream" })),
            )
            .mount(&server)
            .await;
        server
    }

    /// A self-hosted install's Keycloak is on a private, plain-http address.
    #[tokio::test]
    async fn private_exchanges_at_a_loopback_http_issuer() {
        let server = mock_keycloak().await;
        let exchanger = TokenExchanger::new(cfg_at(&format!("{}/realms/r", server.uri())));
        assert_eq!(exchanger.policy(), OutboundPolicy::Private);
        assert_eq!(
            exchanger.exchange("subject-jwt", None).await.unwrap(),
            "down.stream"
        );
    }

    /// The same issuer under `Public` is refused before the client secret
    /// and the customer's token are sent anywhere.
    #[tokio::test]
    async fn public_refuses_a_loopback_http_issuer() {
        let server = mock_keycloak().await;
        let exchanger = TokenExchanger::with_policy(
            cfg_at(&format!("{}/realms/r", server.uri())),
            OutboundPolicy::Public,
        );
        let err = exchanger
            .exchange("subject-jwt", None)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("refusing the token exchange endpoint"),
            "{err}"
        );
        assert!(server.received_requests().await.unwrap().is_empty());
    }
}
