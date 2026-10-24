//! Ingress primitives for the public surface: rate limiters checked at two
//! points, and trusted-proxy-aware client-IP resolution. Both are small and
//! self-contained — the internal hub's equivalents are not shared.

use std::collections::HashMap;
use std::hash::Hash;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::http::HeaderMap;
use ipnetwork::IpNetwork;

/// Hard cap on tracked source IPs, so an unauthenticated spray of distinct
/// addresses (e.g. an IPv6 /64) can't grow the map without bound. Once full,
/// a not-yet-seen IP is treated as over-limit — bounding memory without
/// affecting IPs already inside their window.
const MAX_TRACKED_IPS: usize = 100_000;

/// Cap on tracked signed-in users.
///
/// Deliberately far above any real concurrent-user count, because the
/// over-limit-when-full rule is conservative for IPs and hostile for users: an
/// address spraying the map is an attacker, whereas a user who cannot get a key
/// is a paying customer locked out of the product. Every key here has already
/// passed token verification, so filling the map requires that many real
/// accounts — not a spray.
const MAX_TRACKED_USERS: usize = 1_000_000;

struct Buckets<K> {
    hits: HashMap<K, (Instant, u32)>,
    /// Next time an aged-out sweep is allowed. Amortizes the O(n) eviction to at
    /// most once per window rather than on every request when the map is large.
    next_sweep: Instant,
}

/// Fixed-window rate limiter over any key.
///
/// Keyed by IP it is cheap enough to call before the registry lookup or Keycloak
/// verification, so an unauthenticated flood can't burn signature-verifies or
/// agent runs. Keyed by the verified owner it bounds one customer's own usage —
/// which an IP cannot do, because a whole office reaches us from one address.
pub struct RateLimiter<K = IpAddr> {
    max_per_window: u32,
    window: Duration,
    max_tracked: usize,
    inner: Mutex<Buckets<K>>,
}

impl<K: Eq + Hash> RateLimiter<K> {
    /// A limiter keyed by client IP, bounded by [`MAX_TRACKED_IPS`].
    pub fn new(max_per_window: u32, window: Duration) -> Self {
        Self::with_capacity(max_per_window, window, MAX_TRACKED_IPS)
    }

    /// A limiter keyed by verified user, bounded by [`MAX_TRACKED_USERS`].
    pub fn per_user(max_per_window: u32, window: Duration) -> Self {
        Self::with_capacity(max_per_window, window, MAX_TRACKED_USERS)
    }

    fn with_capacity(max_per_window: u32, window: Duration, max_tracked: usize) -> Self {
        Self {
            max_per_window,
            window,
            max_tracked,
            inner: Mutex::new(Buckets {
                hits: HashMap::new(),
                next_sweep: Instant::now() + window,
            }),
        }
    }

    /// Record a hit for `key`; returns `true` while within the window budget,
    /// `false` once it is exceeded (or once the tracked-key cap is reached).
    pub fn check(&self, key: K) -> bool {
        let now = Instant::now();
        let mut b = self.inner.lock().expect("rate-limiter mutex poisoned");
        if now >= b.next_sweep {
            let window = self.window;
            b.hits
                .retain(|_, (seen, _)| now.duration_since(*seen) < window);
            b.next_sweep = now + window;
        }
        if !b.hits.contains_key(&key) && b.hits.len() >= self.max_tracked {
            return false;
        }
        let entry = b.hits.entry(key).or_insert((now, 0));
        if now.duration_since(entry.0) >= self.window {
            *entry = (now, 0);
        }
        entry.1 += 1;
        entry.1 <= self.max_per_window
    }
}

/// The real client IP for `peer`, honouring `X-Forwarded-For` only when the
/// socket peer is itself a trusted proxy. Walks the header right-to-left and
/// returns the first hop that is not a trusted proxy — the address the
/// outermost trusted proxy actually accepted the connection from — so a client
/// can't spoof its way past the limiter by pre-seeding `X-Forwarded-For`.
pub fn client_ip(trusted: &[IpNetwork], headers: &HeaderMap, peer: Option<IpAddr>) -> IpAddr {
    let peer = peer.unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
    if !is_trusted(trusted, peer) {
        return peer;
    }
    if let Some(forwarded) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        for hop in forwarded.split(',').rev() {
            if let Ok(ip) = hop.trim().parse::<IpAddr>() {
                if !is_trusted(trusted, ip) {
                    return ip;
                }
            }
        }
    }
    peer
}

fn is_trusted(trusted: &[IpNetwork], ip: IpAddr) -> bool {
    trusted.iter().any(|net| net.contains(ip))
}

/// The hostname a request was addressed to, as the tenant registry keys it:
/// lowercase, without a port. `X-Forwarded-Host` is honoured only when the
/// socket peer is a trusted proxy — the same rule as [`client_ip`], so a
/// direct client cannot pick its tenant by header. `None` when the request
/// names no host at all.
pub fn request_host(
    trusted: &[IpNetwork],
    headers: &HeaderMap,
    peer: Option<IpAddr>,
) -> Option<String> {
    let forwarded = peer
        .filter(|peer| is_trusted(trusted, *peer))
        .and_then(|_| headers.get("x-forwarded-host"))
        .and_then(|v| v.to_str().ok())
        // A proxy chain appends; the first entry is the host the client asked for.
        .and_then(|v| v.split(',').next())
        .map(str::trim)
        .filter(|h| !h.is_empty());
    let raw = forwarded.or_else(|| {
        headers
            .get(axum::http::header::HOST)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|h| !h.is_empty())
    })?;
    let without_port = match raw.strip_prefix('[') {
        // An IPv6 literal keeps its brackets; only a port after them goes.
        Some(_) => raw.find(']').map_or(raw, |end| &raw[..=end]),
        None => raw.rsplit_once(':').map_or(raw, |(host, _port)| host),
    };
    Some(without_port.to_ascii_lowercase())
}

/// The bearer from `Authorization: Bearer …`, falling back to a query-string
/// token (browsers can't set headers on some navigations). Empty ⇒ `None`.
pub fn bearer_token(headers: &HeaderMap, query_token: Option<&str>) -> Option<String> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .map(str::to_owned)
        .or_else(|| query_token.map(str::to_owned))
        .filter(|t| !t.is_empty())
}

/// The collision-free session owner for a verified customer: `ext:` + the realm
/// issuer + the subject. Distinct from any internal UUID owner, and unique
/// across realms, so a customer can only ever address their own sessions.
pub fn external_owner(issuer: &str, sub: &str) -> String {
    format!("{CUSTOMER_PREFIX}{issuer}\u{1f}{sub}")
}

/// What every customer's owner key starts with.
const CUSTOMER_PREFIX: &str = "ext:";

/// Whether `key` is a customer's owner key, as [`external_owner`] makes it: an
/// issuer and a subject, neither empty. Lessons, training data and any other
/// identity the edge keys by have another shape, so erasure that reaches only
/// these reaches only customers.
pub fn is_customer_owner(key: &str) -> bool {
    key.strip_prefix(CUSTOMER_PREFIX)
        .and_then(|rest| rest.split_once('\u{1f}'))
        .is_some_and(|(issuer, sub)| !issuer.is_empty() && !sub.is_empty())
}

/// Parse the configured trusted-proxy CIDRs, skipping (with a warning) any that
/// don't parse so one typo can't silently disable proxy handling.
pub fn parse_trusted_proxies(values: &[String]) -> Vec<IpNetwork> {
    values
        .iter()
        .filter_map(|raw| match raw.parse::<IpNetwork>() {
            Ok(net) if net.prefix() == 0 => {
                tracing::warn!(
                    value = %raw,
                    "refusing a /0 trusted_proxy — it would trust every peer's X-Forwarded-For and defeat the rate limiter"
                );
                None
            }
            Ok(net) => Some(net),
            Err(e) => {
                tracing::warn!(value = %raw, error = %e, "ignoring invalid trusted_proxy CIDR");
                None
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn only_a_key_made_for_a_customer_is_a_customers() {
        assert!(is_customer_owner(&external_owner(
            "https://id.example",
            "sub-1"
        )));
        for other in [
            "ext:cust-9",
            "ext:\u{1f}sub-1",
            "ext:https://id.example\u{1f}",
            "hub:ext:https://id.example\u{1f}sub-1",
            "edge:lessons",
            "6f1c0d2e-9a5b-4c1d-8e7f-0a1b2c3d4e5f",
            "",
        ] {
            assert!(!is_customer_owner(other), "{other:?}");
        }
    }

    #[test]
    fn rate_limiter_allows_up_to_the_budget_then_denies() {
        let rl = RateLimiter::new(2, Duration::from_secs(60));
        let addr = ip("203.0.113.7");
        assert!(rl.check(addr));
        assert!(rl.check(addr));
        assert!(!rl.check(addr), "third hit exceeds the budget of 2");
        // A different IP has its own independent budget.
        assert!(rl.check(ip("203.0.113.8")));
    }

    #[test]
    fn direct_peer_is_used_when_untrusted() {
        let trusted = parse_trusted_proxies(&["10.0.0.0/8".into()]);
        let headers = HeaderMap::new();
        // A direct (untrusted) peer is taken from the socket, XFF ignored.
        assert_eq!(
            client_ip(&trusted, &headers, Some(ip("203.0.113.9"))),
            ip("203.0.113.9")
        );
    }

    #[test]
    fn trusted_proxy_uses_rightmost_untrusted_forwarded_hop() {
        let trusted = parse_trusted_proxies(&["10.0.0.0/8".into()]);
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-forwarded-for",
            "203.0.113.1, 198.51.100.2, 10.0.0.5".parse().unwrap(),
        );
        // Peer is a trusted proxy (10.x); the rightmost non-trusted hop wins.
        assert_eq!(
            client_ip(&trusted, &headers, Some(ip("10.0.0.9"))),
            ip("198.51.100.2")
        );
    }

    #[test]
    fn request_host_is_lowercase_without_a_port() {
        let trusted = parse_trusted_proxies(&["10.0.0.0/8".into()]);
        let host = |value: &str| {
            let mut headers = HeaderMap::new();
            headers.insert("host", value.parse().unwrap());
            request_host(&trusted, &headers, Some(ip("203.0.113.9")))
        };
        assert_eq!(
            host("Acme.Marrilo.com:443").as_deref(),
            Some("acme.marrilo.com")
        );
        assert_eq!(
            host("acme.marrilo.com").as_deref(),
            Some("acme.marrilo.com")
        );
        assert_eq!(host("[::1]:8080").as_deref(), Some("[::1]"));
        assert_eq!(host("   ").as_deref(), None);
        assert_eq!(
            request_host(&trusted, &HeaderMap::new(), Some(ip("203.0.113.9"))),
            None,
            "no host header, no host"
        );
    }

    #[test]
    fn forwarded_host_is_honoured_only_from_a_trusted_proxy() {
        let trusted = parse_trusted_proxies(&["10.0.0.0/8".into()]);
        let mut headers = HeaderMap::new();
        headers.insert("host", "edge.internal:8080".parse().unwrap());
        headers.insert(
            "x-forwarded-host",
            "Acme.Marrilo.com, edge.internal".parse().unwrap(),
        );
        assert_eq!(
            request_host(&trusted, &headers, Some(ip("10.0.0.5"))).as_deref(),
            Some("acme.marrilo.com"),
            "behind the proxy, the client's host is the first forwarded entry"
        );
        assert_eq!(
            request_host(&trusted, &headers, Some(ip("203.0.113.9"))).as_deref(),
            Some("edge.internal"),
            "a direct client cannot pick its tenant by header"
        );
    }

    #[test]
    fn spoofed_forwarded_header_from_direct_client_is_ignored() {
        let trusted = parse_trusted_proxies(&["10.0.0.0/8".into()]);
        let mut headers = HeaderMap::new();
        // An untrusted client pre-seeds XFF to look like someone else.
        headers.insert("x-forwarded-for", "1.2.3.4".parse().unwrap());
        assert_eq!(
            client_ip(&trusted, &headers, Some(ip("203.0.113.9"))),
            ip("203.0.113.9")
        );
    }
}

#[cfg(test)]
mod rate_limit_tests {
    use super::*;

    #[test]
    fn a_user_key_gets_its_own_budget() {
        let limiter = RateLimiter::per_user(2, Duration::from_secs(60));
        assert!(limiter.check("alice".to_string()));
        assert!(limiter.check("alice".to_string()));
        assert!(
            !limiter.check("alice".to_string()),
            "alice is over her budget"
        );
        assert!(
            limiter.check("bob".to_string()),
            "bob must not be throttled by alice — that is the whole point of \
             keying on the user rather than a shared office address"
        );
    }

    #[test]
    fn the_ip_tier_still_bounds_what_it_tracks() {
        let limiter: RateLimiter<IpAddr> = RateLimiter::new(1, Duration::from_secs(60));
        let ip: IpAddr = "203.0.113.1".parse().unwrap();
        assert!(limiter.check(ip));
        assert!(!limiter.check(ip));
    }
}
