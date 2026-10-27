//! Outbound HTTP policy for URLs a tenant supplies.
//!
//! In the cloud, OIDC issuers, `jwks_uri`, MCP endpoints, permissions URLs and
//! token-exchange endpoints come from paying tenants and are attacker-
//! controlled. A self-hosted install points the same settings at private
//! in-cluster hosts (`http://cufflink-platform:8080/...`) and must keep
//! working unchanged. [`OutboundPolicy`] is that switch:
//!
//! - [`OutboundPolicy::Private`] (the default) builds the client each module
//!   has always built: a timeout and nothing else.
//! - [`OutboundPolicy::Public`] is the shape `image.fetch` already enforces,
//!   factored out so every module shares one implementation: `https` only,
//!   every resolved address checked (loopback, RFC1918, link-local and the
//!   metadata endpoint, CGNAT, multicast, unspecified, unique-local, and the
//!   IPv4 an IPv6 address may carry), the checked address pinned for the
//!   connection so a DNS rebind between check and connect has nothing to
//!   rebind, and every redirect hop re-checked before it is followed.
//!
//! [`workforce_agent::security::ssrf::ssrf_safe_client`] re-checks redirect
//! hops but does not pin DNS and admits `http`; the `Public` client here is
//! that policy plus the pin and the scheme rule.

use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderValue, USER_AGENT};
use serde::{Deserialize, Serialize};
use url::{Host, Url};
use workforce_agent::security::ssrf::BOT_USER_AGENT;

/// Whether the URLs a module calls are trusted (private, in-cluster) or
/// tenant-supplied (public internet, attacker-controlled).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutboundPolicy {
    /// Self-hosted: any scheme, any address, redirects followed. Today's
    /// behaviour.
    #[default]
    Private,
    /// Cloud: `https` only, no private/loopback/link-local/metadata address,
    /// address pinned, redirects re-checked per hop.
    Public,
}

/// Why a URL was refused under [`OutboundPolicy::Public`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutboundError {
    NotHttps,
    NoHost,
    Unresolvable(String),
    PrivateAddress(IpAddr),
}

impl std::fmt::Display for OutboundError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotHttps => write!(f, "only https URLs may be called"),
            Self::NoHost => write!(f, "the URL has no host"),
            Self::Unresolvable(host) => write!(f, "{host} could not be resolved"),
            Self::PrivateAddress(addr) => {
                write!(f, "the host resolves to a private address ({addr})")
            }
        }
    }
}

impl std::error::Error for OutboundError {}

impl OutboundError {
    /// Whether the host could not be reached at all, as opposed to the policy
    /// refusing it.
    pub fn is_unreachable(&self) -> bool {
        matches!(self, Self::Unresolvable(_))
    }
}

/// Connect bound on a `Public` client. The total timeout is the caller's; a
/// connect that stalls on an unreachable address must not eat all of it.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Most redirect hops a `Public` client follows, matching `ssrf_safe_client`.
const MAX_REDIRECTS: usize = 10;

/// Bodies larger than this are refused by [`read_capped`] under `Public`.
/// Named per use so each module's cap reads as a decision, not a magic number.
pub const DISCOVERY_DOC_MAX: usize = 64 * 1024;
pub const JWKS_MAX: usize = 256 * 1024;
pub const PERMISSIONS_MAX: usize = 64 * 1024;
pub const TOKEN_EXCHANGE_MAX: usize = 64 * 1024;

/// An address no `Public` call may reach. Loopback and private ranges are the
/// obvious ones; `169.254.0.0/16` is the cloud metadata endpoint and the whole
/// reason this check exists.
pub fn is_forbidden(addr: IpAddr) -> bool {
    match addr {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_unspecified()
                || v4.is_multicast()
                // 100.64.0.0/10, carrier-grade NAT — a VPC neighbour on some
                // platforms, and not covered by `is_private`.
                || (v4.octets()[0] == 100 && (64..128).contains(&v4.octets()[1]))
                // 0.0.0.0/8
                || v4.octets()[0] == 0
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                // fc00::/7 unique-local and fe80::/10 link-local.
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                // An IPv4-mapped address must be judged as the v4 it carries,
                // or ::ffff:127.0.0.1 walks straight through.
                || v6.to_ipv4_mapped().is_some_and(|v4| is_forbidden(v4.into()))
        }
    }
}

/// The system resolver, in the shape [`approve`] takes.
pub fn resolve_system(host: &str, port: u16) -> Vec<IpAddr> {
    (host, port)
        .to_socket_addrs()
        .map(|it| it.map(|s| s.ip()).collect())
        .unwrap_or_default()
}

/// A URL that passed every `Public` check, with the address it is pinned to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pinned {
    pub host: String,
    pub addr: SocketAddr,
}

/// The `Public` checks: `https`, a host, and every resolved address allowed.
/// `resolve` is injected so the whole policy is testable without a network.
pub fn approve(
    url: &Url,
    resolve: impl Fn(&str, u16) -> Vec<IpAddr>,
) -> Result<Pinned, OutboundError> {
    if url.scheme() != "https" {
        return Err(OutboundError::NotHttps);
    }
    let port = url.port_or_known_default().unwrap_or(443);
    // `host()` keeps an IPv6 literal as an address; `host_str()` would hand
    // back `[::1]`, which no resolver and no `IpAddr` parser accepts.
    let (host, addrs) = match url.host() {
        Some(Host::Ipv4(v4)) => (v4.to_string(), vec![IpAddr::V4(v4)]),
        Some(Host::Ipv6(v6)) => (v6.to_string(), vec![IpAddr::V6(v6)]),
        Some(Host::Domain(d)) => (d.to_string(), resolve(d, port)),
        None => return Err(OutboundError::NoHost),
    };
    if addrs.is_empty() {
        return Err(OutboundError::Unresolvable(host));
    }
    // EVERY address must be acceptable, not merely the first: a host that
    // resolves to one public and one loopback address is an attack, and picking
    // the public one would let a retry land on the other.
    if let Some(bad) = addrs.iter().copied().find(|a| is_forbidden(*a)) {
        return Err(OutboundError::PrivateAddress(bad));
    }
    Ok(Pinned {
        host,
        addr: SocketAddr::new(addrs[0], port),
    })
}

/// One-shot check of a URL a module built dynamically (a `jwks_uri` read from
/// a discovery document, say) before it is requested. `Private` accepts
/// anything that parsed.
pub fn check_url(policy: OutboundPolicy, url: &Url) -> Result<(), OutboundError> {
    match policy {
        OutboundPolicy::Private => Ok(()),
        OutboundPolicy::Public => approve(url, resolve_system).map(drop),
    }
}

/// What a `Public` client does with a redirect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Redirects {
    /// Never follow one — the caller checked exactly one URL and that is the
    /// only one it will talk to (`image.fetch`).
    Refuse,
    /// Follow up to [`MAX_REDIRECTS`] hops, each re-checked with [`approve`].
    /// The hop's address is checked but not pinned: `reqwest` fixes pins at
    /// build time, so a redirect target is as protected as `ssrf_safe_client`
    /// makes it, no less.
    Checked,
}

/// The decision a `Public` redirect policy makes for one hop. Split out so it
/// is testable: `reqwest` does not let a test construct an `Attempt`.
pub(crate) fn redirect_decision(next: &Url, previous_hops: usize) -> Result<(), String> {
    if previous_hops >= MAX_REDIRECTS {
        return Err("too many redirects".into());
    }
    approve(next, resolve_system)
        .map(drop)
        .map_err(|e| format!("redirect blocked: {e}"))
}

/// A client for `policy`, with no URL known yet. `Private` is the shape every
/// module built before this existed: a timeout and nothing else. `Public`
/// re-checks each redirect hop but cannot pin — use [`client_for`] when the
/// URL is known, which is almost always.
pub fn client(policy: OutboundPolicy, timeout: Duration) -> reqwest::Client {
    match policy {
        OutboundPolicy::Private => private_client(timeout),
        OutboundPolicy::Public => build_public(timeout, None, Redirects::Checked),
    }
}

/// A client for one request to `url` under `policy`. `Public` runs
/// [`approve`] and pins the connection to the address it checked; `Private`
/// is [`client`]. Build one per request rather than holding it: the pin is
/// to one host's address at one moment.
pub fn client_for(
    policy: OutboundPolicy,
    timeout: Duration,
    url: &Url,
) -> Result<reqwest::Client, OutboundError> {
    match policy {
        OutboundPolicy::Private => Ok(private_client(timeout)),
        OutboundPolicy::Public => {
            let pinned = approve(url, resolve_system)?;
            Ok(pinned_client(timeout, &pinned, Redirects::Checked))
        }
    }
}

/// A `Public`-shaped client pinned to an already-approved address.
pub fn pinned_client(timeout: Duration, pinned: &Pinned, redirects: Redirects) -> reqwest::Client {
    build_public(timeout, Some(pinned), redirects)
}

fn private_client(timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

fn build_public(
    timeout: Duration,
    pinned: Option<&Pinned>,
    redirects: Redirects,
) -> reqwest::Client {
    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, HeaderValue::from_static(BOT_USER_AGENT));
    let redirect =
        match redirects {
            Redirects::Refuse => reqwest::redirect::Policy::none(),
            Redirects::Checked => reqwest::redirect::Policy::custom(|attempt| {
                match redirect_decision(attempt.url(), attempt.previous().len()) {
                    Ok(()) => attempt.follow(),
                    Err(reason) => attempt.error(reason),
                }
            }),
        };
    let mut builder = reqwest::Client::builder()
        .default_headers(headers)
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(timeout)
        .redirect(redirect);
    if let Some(p) = pinned {
        // Pin the connection to the address that was checked while leaving the
        // URL's host intact — the Host header and SNI still match, and a DNS
        // rebind between check and connect has nothing to rebind.
        builder = builder.resolve(&p.host, p.addr);
    }
    builder
        .build()
        .unwrap_or_else(|_| build_public_fallback(timeout))
}

/// `reqwest`'s builder fails only for a broken TLS backend; even then a
/// `Public` client must never fall back to one that follows redirects freely.
fn build_public_fallback(timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// Why a body could not be read whole.
#[derive(Debug)]
pub enum ReadError {
    Transport(reqwest::Error),
    /// The body passed `limit` bytes while streaming.
    TooLarge {
        limit: usize,
    },
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(e) => write!(f, "{e}"),
            Self::TooLarge { limit } => {
                write!(f, "response body exceeds the {limit} byte cap")
            }
        }
    }
}

impl std::error::Error for ReadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Transport(e) => Some(e),
            Self::TooLarge { .. } => None,
        }
    }
}

impl From<reqwest::Error> for ReadError {
    fn from(e: reqwest::Error) -> Self {
        Self::Transport(e)
    }
}

/// Read a response body, refusing one past `limit` bytes under `Public`.
/// Counted as it streams, never from `Content-Length` — a lying header would
/// let an oversized body through. `Private` reads the body whole, as before.
pub async fn read_capped(
    policy: OutboundPolicy,
    resp: reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>, ReadError> {
    if policy == OutboundPolicy::Private {
        return Ok(resp.bytes().await?.to_vec());
    }
    let mut body = Vec::new();
    let mut resp = resp;
    while let Some(chunk) = resp.chunk().await? {
        if body.len() + chunk.len() > limit {
            return Err(ReadError::TooLarge { limit });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PUBLIC: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(93, 184, 216, 34));

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    /// Every address a `Public` call must refuse, written as a literal so no
    /// test depends on DNS.
    const REFUSED: &[&str] = &[
        "https://127.0.0.1/",
        "https://10.0.0.1/",
        "https://192.168.1.1/",
        "https://172.16.0.1/",
        "https://169.254.169.254/latest/meta-data/",
        "https://100.64.0.1/",
        "https://0.0.0.0/",
        "https://224.0.0.1/",
        "https://[::1]/",
        "https://[::]/",
        "https://[fd00::1]/",
        "https://[fe80::1]/",
        "https://[ff02::1]/",
        "https://[::ffff:127.0.0.1]/",
        "https://[::ffff:169.254.169.254]/",
    ];

    #[test]
    fn public_refuses_http() {
        assert_eq!(
            check_url(OutboundPolicy::Public, &url("http://example.com/")),
            Err(OutboundError::NotHttps)
        );
        for raw in ["file:///etc/passwd", "ftp://example.com/", "gopher://x/"] {
            assert!(
                check_url(OutboundPolicy::Public, &url(raw)).is_err(),
                "{raw}"
            );
        }
    }

    #[test]
    fn public_refuses_every_private_literal() {
        for raw in REFUSED {
            match check_url(OutboundPolicy::Public, &url(raw)) {
                Err(OutboundError::PrivateAddress(_)) => {}
                other => panic!("{raw} must be refused as private, got {other:?}"),
            }
        }
    }

    #[test]
    fn private_accepts_all_of_them() {
        assert!(check_url(OutboundPolicy::Private, &url("http://example.com/")).is_ok());
        for raw in REFUSED {
            assert!(
                check_url(OutboundPolicy::Private, &url(raw)).is_ok(),
                "{raw}"
            );
        }
        // Shape only: the client builds, no call is made.
        let _ = client(OutboundPolicy::Private, Duration::from_secs(1));
        let _ = client_for(
            OutboundPolicy::Private,
            Duration::from_secs(1),
            &url("http://10.0.0.1/"),
        )
        .unwrap();
    }

    #[test]
    fn a_hostname_resolving_inward_is_refused_and_outward_is_pinned() {
        let u = url("https://idp.example.com:8443/realms/x");
        let inward = |_: &str, _: u16| vec!["10.20.0.5".parse().unwrap()];
        assert_eq!(
            approve(&u, inward),
            Err(OutboundError::PrivateAddress("10.20.0.5".parse().unwrap()))
        );
        // One public address does not make a host safe when it also resolves
        // inward — a retry would land on the other one.
        let mixed = |_: &str, _: u16| vec![PUBLIC, "127.0.0.1".parse().unwrap()];
        assert!(matches!(
            approve(&u, mixed),
            Err(OutboundError::PrivateAddress(_))
        ));
        let none = |_: &str, _: u16| Vec::new();
        assert_eq!(
            approve(&u, none),
            Err(OutboundError::Unresolvable("idp.example.com".into()))
        );
        let ok = approve(&u, |_, _| vec![PUBLIC]).unwrap();
        assert_eq!(
            ok,
            Pinned {
                host: "idp.example.com".into(),
                addr: SocketAddr::new(PUBLIC, 8443),
            }
        );
    }

    #[test]
    fn a_public_literal_is_accepted_and_the_client_builds() {
        let u = url("https://93.184.216.34/");
        assert!(check_url(OutboundPolicy::Public, &u).is_ok());
        let _ = client_for(OutboundPolicy::Public, Duration::from_secs(1), &u).unwrap();
        let _ = client(OutboundPolicy::Public, Duration::from_secs(1));
    }

    #[test]
    fn public_client_for_refuses_before_building() {
        for raw in REFUSED {
            assert!(
                client_for(OutboundPolicy::Public, Duration::from_secs(1), &url(raw)).is_err(),
                "{raw}"
            );
        }
        assert_eq!(
            client_for(
                OutboundPolicy::Public,
                Duration::from_secs(1),
                &url("http://93.184.216.34/")
            )
            .err(),
            Some(OutboundError::NotHttps)
        );
    }

    /// The per-hop decision a `Public` client makes: a redirect to a private
    /// address, to `http`, or past the hop cap is refused.
    #[test]
    fn a_redirect_to_a_private_address_is_refused() {
        assert!(redirect_decision(&url("https://127.0.0.1/next"), 1).is_err());
        assert!(redirect_decision(&url("https://169.254.169.254/"), 1).is_err());
        assert!(redirect_decision(&url("https://[fd00::1]/"), 1).is_err());
        assert!(redirect_decision(&url("http://93.184.216.34/"), 1).is_err());
        assert!(redirect_decision(&url("https://93.184.216.34/"), MAX_REDIRECTS).is_err());
        assert!(redirect_decision(&url("https://93.184.216.34/"), 1).is_ok());
    }

    #[test]
    fn only_an_unresolvable_host_is_unreachable() {
        assert!(OutboundError::Unresolvable("docs.example.com".into()).is_unreachable());
        [
            OutboundError::NotHttps,
            OutboundError::NoHost,
            OutboundError::PrivateAddress(IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 1))),
        ]
        .iter()
        .for_each(|refusal| assert!(!refusal.is_unreachable(), "{refusal}"));
    }

    #[test]
    fn policy_serialises_lowercase_and_defaults_private() {
        assert_eq!(OutboundPolicy::default(), OutboundPolicy::Private);
        assert_eq!(
            serde_json::to_string(&OutboundPolicy::Public).unwrap(),
            "\"public\""
        );
        assert_eq!(
            serde_json::from_str::<OutboundPolicy>("\"private\"").unwrap(),
            OutboundPolicy::Private
        );
        assert!(serde_json::from_str::<OutboundPolicy>("\"Public\"").is_err());
    }

    #[tokio::test]
    async fn public_caps_a_body_and_private_does_not() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![b'x'; 100]))
            .mount(&server)
            .await;
        let get = || reqwest::Client::new().get(server.uri()).send();
        let err = read_capped(OutboundPolicy::Public, get().await.unwrap(), 50)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("cap"), "{err}");
        let body = read_capped(OutboundPolicy::Public, get().await.unwrap(), 100)
            .await
            .unwrap();
        assert_eq!(body.len(), 100);
        let body = read_capped(OutboundPolicy::Private, get().await.unwrap(), 50)
            .await
            .unwrap();
        assert_eq!(body.len(), 100, "Private reads whole, as before");
    }
}
