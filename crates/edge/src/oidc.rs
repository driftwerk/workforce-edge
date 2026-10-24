//! The OIDC Authorization Code + PKCE flow for operator sign-in.
//!
//! The Google GSI button (see [`crate::admin`]) is a Google-only shortcut; every
//! other IdP — Keycloak in particular — uses the standard browser redirect flow:
//!
//! 1. `/login/oidc/start` → mint `state`/`nonce`/PKCE, stash them in a signed,
//!    short-lived cookie, and 302 to the IdP's `authorization_endpoint`.
//! 2. the IdP authenticates the operator and redirects back to
//!    `/login/oidc/callback?code=…&state=…`.
//! 3. the callback matches `state` against the cookie, exchanges `code` at the
//!    `token_endpoint` (proving possession of the PKCE verifier), then verifies
//!    the returned `id_token` (signature/issuer/audience via the existing
//!    Keycloak validator) and its `nonce` before the allowlist + session.
//!
//! Token *verification* is unchanged and IdP-agnostic; this module only adds the
//! browser dance that gets an `id_token` in the first place.

use std::time::Duration;

use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::outbound::{self, OutboundPolicy, TOKEN_EXCHANGE_MAX};

const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// Bound on the code exchange. An operator is waiting on the callback; a
/// token endpoint that stalls must fail the login, not park the request.
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(10);

/// A PKCE verifier and its S256 challenge.
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

/// Fresh PKCE material: a high-entropy verifier and `base64url(sha256(verifier))`.
pub fn pkce() -> Pkce {
    let verifier = random_token();
    let digest = Sha256::digest(verifier.as_bytes());
    Pkce {
        challenge: B64.encode(digest),
        verifier,
    }
}

/// A URL-safe random token (~43 chars from 32 bytes) for `state`, `nonce`, and
/// the PKCE verifier.
pub fn random_token() -> String {
    let mut buf = [0u8; 32];
    getrandom::getrandom(&mut buf).expect("os rng");
    B64.encode(buf)
}

/// What the signed flow cookie carries between `/start` and `/callback`. Signed
/// with the admin session key so the browser can't tamper with it.
#[derive(Debug, Serialize, Deserialize)]
pub struct FlowState {
    pub state: String,
    pub verifier: String,
    pub nonce: String,
    pub exp: usize,
}

/// Build the IdP `authorization_endpoint` redirect URL for the code flow.
pub fn authorize_url(
    authorization_endpoint: &str,
    client_id: &str,
    redirect_uri: &str,
    state: &str,
    nonce: &str,
    code_challenge: &str,
) -> Result<String, url::ParseError> {
    let mut url = url::Url::parse(authorization_endpoint)?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", "openid email")
        .append_pair("state", state)
        .append_pair("nonce", nonce)
        .append_pair("code_challenge", code_challenge)
        .append_pair("code_challenge_method", "S256");
    Ok(url.to_string())
}

/// Exchange an authorization `code` for tokens at the `token_endpoint`, and
/// return the raw `id_token`. Sends the PKCE `verifier`; includes the client
/// secret only for confidential clients (public + PKCE clients omit it).
/// Reaches the endpoint under [`OutboundPolicy::Private`]; a tenant-supplied
/// endpoint goes through [`exchange_code_with`].
pub async fn exchange_code(
    token_endpoint: &str,
    client_id: &str,
    client_secret: Option<&str>,
    redirect_uri: &str,
    code: &str,
    verifier: &str,
) -> eyre::Result<String> {
    exchange_code_with(
        OutboundPolicy::Private,
        token_endpoint,
        client_id,
        client_secret,
        redirect_uri,
        code,
        verifier,
    )
    .await
}

/// [`exchange_code`] under `policy`. The `token_endpoint` came from the
/// issuer's discovery document, so under `Public` it is checked and pinned
/// here, where it is called, not where it was read.
pub async fn exchange_code_with(
    policy: OutboundPolicy,
    token_endpoint: &str,
    client_id: &str,
    client_secret: Option<&str>,
    redirect_uri: &str,
    code: &str,
    verifier: &str,
) -> eyre::Result<String> {
    let mut form = vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", client_id),
        ("code_verifier", verifier),
    ];
    if let Some(secret) = client_secret {
        form.push(("client_secret", secret));
    }
    let endpoint = url::Url::parse(token_endpoint)
        .map_err(|e| eyre::eyre!("token endpoint is not a URL: {e}"))?;
    let client = outbound::client_for(policy, EXCHANGE_TIMEOUT, &endpoint)
        .map_err(|e| eyre::eyre!("refusing the token endpoint: {e}"))?;
    let resp = client
        .post(endpoint)
        .form(&form)
        .send()
        .await
        .map_err(|e| eyre::eyre!("token endpoint request failed: {e}"))?;
    if !resp.status().is_success() {
        // Don't surface the IdP's raw error body (may echo the code); log-worthy
        // detail stays with the caller.
        eyre::bail!("token exchange rejected ({})", resp.status());
    }
    let body = outbound::read_capped(policy, resp, TOKEN_EXCHANGE_MAX)
        .await
        .map_err(|e| eyre::eyre!("token endpoint response unreadable: {e}"))?;
    let body: serde_json::Value = serde_json::from_slice(&body)
        .map_err(|e| eyre::eyre!("token endpoint returned non-JSON: {e}"))?;
    body.get("id_token")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| eyre::eyre!("token response carried no id_token"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_is_s256_of_verifier() {
        let p = pkce();
        // Recompute the challenge independently.
        let expect = B64.encode(Sha256::digest(p.verifier.as_bytes()));
        assert_eq!(p.challenge, expect);
        // URL-safe, no padding, non-trivial length.
        assert!(p.verifier.len() >= 43 && !p.verifier.contains('='));
        assert!(!p.challenge.contains('+') && !p.challenge.contains('/'));
    }

    #[test]
    fn random_tokens_are_distinct() {
        assert_ne!(random_token(), random_token());
    }

    async fn mock_token_endpoint() -> wiremock::MockServer {
        use wiremock::matchers::{body_string_contains, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("grant_type=authorization_code"))
            .and(body_string_contains("code_verifier=v3rifier"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "id_token": "id.tok.en" })),
            )
            .mount(&server)
            .await;
        server
    }

    /// A self-hosted install's IdP is on a private, plain-http address.
    #[tokio::test]
    async fn private_exchanges_at_a_loopback_http_endpoint() {
        let server = mock_token_endpoint().await;
        let id_token = exchange_code(
            &format!("{}/token", server.uri()),
            "edge-admin",
            Some("s3cret"),
            "https://edge.example.com/cb",
            "c0de",
            "v3rifier",
        )
        .await
        .expect("exchanged");
        assert_eq!(id_token, "id.tok.en");
    }

    /// The same endpoint under `Public` is refused before the code and
    /// secret are sent anywhere.
    #[tokio::test]
    async fn public_refuses_a_loopback_http_endpoint() {
        let server = mock_token_endpoint().await;
        let err = exchange_code_with(
            OutboundPolicy::Public,
            &format!("{}/token", server.uri()),
            "edge-admin",
            Some("s3cret"),
            "https://edge.example.com/cb",
            "c0de",
            "v3rifier",
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("refusing the token endpoint"), "{err}");
        assert!(
            server.received_requests().await.unwrap().is_empty(),
            "nothing may reach the endpoint"
        );
    }

    #[test]
    fn authorize_url_has_the_required_params() {
        let u = authorize_url(
            "https://auth.example.com/realms/x/protocol/openid-connect/auth",
            "edge-admin",
            "https://edge.example.com/admin/api/login/oidc/callback",
            "st8",
            "n0nce",
            "chal",
        )
        .unwrap();
        for needle in [
            "response_type=code",
            "client_id=edge-admin",
            "code_challenge=chal",
            "code_challenge_method=S256",
            "scope=openid+email",
            "state=st8",
            "nonce=n0nce",
        ] {
            assert!(u.contains(needle), "missing {needle} in {u}");
        }
    }
}
