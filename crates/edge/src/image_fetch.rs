//! `image.fetch` — turn a URL the agent was handed into a picture it can show.
//!
//! The motivating case: company photos live in private storage (contoso's
//! item media is an S3 key, not a URL), so a tool hands the model a short-lived
//! presigned URL. The model cannot render a URL — `wf-image` names artifacts —
//! and we deliberately do not let it try.
//!
//! # Why the edge fetches instead of the browser
//!
//! The URL arrives in TOOL OUTPUT, which is the canonical untrusted channel.
//! Putting an untrusted URL in an `<img src>` makes the customer's browser issue
//! a request to a host chosen by that data — an exfiltration primitive, and the
//! widget's CSP sets no `img-src`. Fetching here means the browser only ever
//! talks to the edge, and the bytes land in the same owner-scoped artifact store
//! everything else is served from.
//!
//! # What that buys the attacker, and what stops it
//!
//! Server-side fetch of a data-supplied URL is the textbook SSRF sink, and the
//! edge sits in a VPC next to a metadata endpoint. Every one of these is load-
//! bearing; none is decoration:
//!
//! 1. **Host allowlist**, per agent. Empty ⇒ the tool is never registered.
//! 2. **`https` only.**
//! 3. **Resolve, check the ADDRESS, then connect to that address.** Checking the
//!    hostname is useless — a DNS record can point at `127.0.0.1`. Private,
//!    loopback, link-local (`169.254.169.254`), unique-local and unspecified
//!    addresses are refused. The validated address is then pinned for the
//!    connection, so a rebind between check and connect cannot slip past.
//!    Steps 2 and 3 are [`crate::outbound`]'s `Public` policy, shared with
//!    every other tenant-supplied URL the edge calls.
//! 4. **No redirects.** Following one re-opens 1-3 for a URL nobody checked.
//! 5. **Size cap**, enforced while streaming, so a hostile body cannot be
//!    poured into the artifact store.
//! 6. **Magic-byte sniff**, not the `Content-Type` header — the same evidence
//!    rule uploads use, and it admits no scriptable type.
//! 7. **Timeouts** on connect and on the whole transfer.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use eyre::Result;
use serde_json::json;
use workforce_agent::artifacts::store::ATTACHMENT_TTL;
use workforce_agent::artifacts::types::{ArtifactBody, ArtifactDescriptor, ContentType};
use workforce_agent::artifacts::ArtifactStore;
use workforce_agent::tools::{Tool, ToolResult};

use crate::outbound::{self, OutboundError, Pinned, Redirects};
use crate::upload::sniff;

/// Producer stamped on a picture the AGENT fetched.
///
/// Deliberately NOT [`crate::upload::UPLOAD_PRODUCER`]: that constant means "the
/// customer uploaded this", and `host::vet_attachments` admits it as something
/// the customer may attach to a turn. A fetched image is owned by the customer
/// (so the render route serves it) but was never sent by them, and a turn frame
/// naming one is refused — an asymmetry that is the point, not an oversight.
pub const FETCH_PRODUCER: &str = "agent.image_fetch";

/// Most images one call may pull. Each is a full round trip and a stored blob;
/// a model looping over a catalogue would otherwise stall the turn.
pub const MAX_FETCH: usize = 8;

/// Ceiling per image. Independent of the customer upload cap — this is about
/// what the edge is willing to pull and store on the agent's say-so.
pub const MAX_IMAGE_BYTES: usize = 12 * 1024 * 1024;

/// Bound on the whole transfer; the connect bound is `outbound`'s.
const TOTAL_TIMEOUT: Duration = Duration::from_secs(20);

/// Types we will store and later render. Closed, and narrower than `sniff`
/// admits: a document fetched from a URL is not a picture, and SVG is a script
/// container we never render.
const RENDERABLE: &[&str] = &["image/png", "image/jpeg", "image/gif", "image/webp"];

/// Why a URL was refused. Each maps to a message the MODEL reads, so each says
/// what to do instead — a bare "failed" makes it retry the same URL.
#[derive(Debug, PartialEq)]
pub enum FetchRefusal {
    NotHttps,
    NoHost,
    HostNotAllowed(String),
    Unresolvable,
    PrivateAddress,
}

impl FetchRefusal {
    pub fn reason(&self) -> String {
        match self {
            Self::NotHttps => "only https URLs can be fetched".into(),
            Self::NoHost => "that URL has no host".into(),
            Self::HostNotAllowed(h) => {
                format!("{h} is not a host this assistant may fetch images from")
            }
            Self::Unresolvable => "that host could not be resolved".into(),
            Self::PrivateAddress => "that host resolves to a private address".into(),
        }
    }
}

/// Does `host` match an allowlist entry? An entry matches itself exactly, or —
/// written `.example.com` — any subdomain of it. A leading-dot entry never
/// matches the bare parent, so `.example.com` does not admit `example.com`:
/// widening an allowlist must be deliberate.
fn host_allowed(host: &str, allowlist: &[String]) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    allowlist.iter().any(|entry| {
        let entry = entry.trim().to_ascii_lowercase();
        match entry.strip_prefix('.') {
            Some(suffix) => host.ends_with(&format!(".{suffix}")),
            None => host == entry,
        }
    })
}

impl From<OutboundError> for FetchRefusal {
    fn from(e: OutboundError) -> Self {
        match e {
            OutboundError::NotHttps => Self::NotHttps,
            OutboundError::NoHost => Self::NoHost,
            OutboundError::Unresolvable(_) => Self::Unresolvable,
            OutboundError::PrivateAddress(_) => Self::PrivateAddress,
        }
    }
}

/// A URL that passed every static check, with the address it is pinned to.
#[derive(Debug)]
pub struct Approved {
    pub url: url::Url,
    pub host: String,
    pub addr: SocketAddr,
}

/// Check scheme, host and resolved address. Split from the fetch so the whole
/// policy is testable without a network — `resolve` is injected for the same
/// reason. The allowlist is this tool's own; scheme, address and pin are
/// [`outbound::approve`], shared with every other tenant-supplied URL.
pub fn approve(
    raw: &str,
    allowlist: &[String],
    resolve: impl Fn(&str, u16) -> Vec<IpAddr>,
) -> Result<Approved, FetchRefusal> {
    let url = url::Url::parse(raw).map_err(|_| FetchRefusal::NoHost)?;
    // Scheme first: an `http` URL is refused as such even when its host is
    // allowlisted, which is the message the model needs to see.
    if url.scheme() != "https" {
        return Err(FetchRefusal::NotHttps);
    }
    // Allowlist before resolving, so a host we would never fetch from is not
    // even looked up.
    let host = url.host_str().ok_or(FetchRefusal::NoHost)?.to_string();
    if !host_allowed(&host, allowlist) {
        return Err(FetchRefusal::HostNotAllowed(host));
    }
    let Pinned { host, addr } = outbound::approve(&url, resolve)?;
    Ok(Approved { url, host, addr })
}

/// Fetch pictures for one turn, on behalf of one customer.
///
/// Built per turn rather than registered once: the staged artifact must record
/// THIS caller as its owner, because `serve_owned_artifact` will serve it to
/// nobody else. A store-wide singleton would stage images owned by whoever
/// happened to construct it, and every `wf-image` would render "no longer
/// available" with no error logged anywhere.
pub struct EdgeImageFetchTool {
    store: Arc<dyn ArtifactStore>,
    agent_id: String,
    owner: String,
    allowlist: Vec<String>,
}

impl EdgeImageFetchTool {
    pub fn new(
        store: Arc<dyn ArtifactStore>,
        agent_id: String,
        owner: String,
        allowlist: Vec<String>,
    ) -> Self {
        Self {
            store,
            agent_id,
            owner,
            allowlist,
        }
    }

    /// Pull one approved URL, capping as it streams.
    ///
    /// The client is built per fetch rather than held: `resolve` pins a specific
    /// host to a specific validated address, so a shared client would either
    /// carry a stale pin or none at all.
    async fn pull(&self, ok: &Approved) -> Result<Vec<u8>, String> {
        // The connection is pinned to the address we validated while leaving
        // the URL's host intact — the Host header and SNI still match, which a
        // presigned URL's signature depends on, and a DNS rebind between check
        // and connect has nothing to rebind. No redirects: a redirect target is
        // a URL nobody allowlisted.
        let pinned = Pinned {
            host: ok.host.clone(),
            addr: ok.addr,
        };
        let client = outbound::pinned_client(TOTAL_TIMEOUT, &pinned, Redirects::Refuse);
        let response = client
            .get(ok.url.clone())
            .send()
            .await
            .map_err(|e| format!("the fetch failed: {e}"))?;
        if !response.status().is_success() {
            return Err(format!("the URL returned HTTP {}", response.status()));
        }
        // Trust the streamed length, never `Content-Length` — a lying header
        // would let an oversized body through.
        let mut body = Vec::new();
        let mut stream = response;
        while let Some(chunk) = stream
            .chunk()
            .await
            .map_err(|e| format!("the transfer failed: {e}"))?
        {
            if body.len() + chunk.len() > MAX_IMAGE_BYTES {
                return Err(format!(
                    "that image is larger than the {} MB limit",
                    MAX_IMAGE_BYTES / (1024 * 1024)
                ));
            }
            body.extend_from_slice(&chunk);
        }
        if body.is_empty() {
            return Err("that URL returned nothing".into());
        }
        Ok(body)
    }

    /// Stage fetched bytes as an artifact this customer owns.
    fn stage(&self, bytes: Vec<u8>, media_type: &str, filename: &str) -> Result<String, String> {
        let descriptor = ArtifactDescriptor {
            user_id: Some(self.owner.clone()),
            ttl_seconds: ATTACHMENT_TTL.as_secs(),
            ..ArtifactDescriptor::from_context(
                FETCH_PRODUCER,
                ContentType::OctetStream,
                self.agent_id.clone(),
            )
            .with_file_meta(filename.to_string(), media_type)
        };
        self.store
            .write(&self.agent_id, descriptor, ArtifactBody::Bytes(bytes))
            .map(|h| h.id.as_str().to_string())
            .map_err(|e| {
                tracing::warn!(error = %e, agent = %self.agent_id, "staging a fetched image failed");
                "that image could not be stored".to_string()
            })
    }

    /// Everything for one URL: approve, pull, sniff, stage.
    async fn one(&self, raw: &str, index: usize) -> serde_json::Value {
        let approved = match approve(raw, &self.allowlist, outbound::resolve_system) {
            Ok(a) => a,
            Err(refusal) => {
                // The URL is echoed back so the model can tell WHICH of several
                // failed, and a rejected HOST rides as its own field.
                //
                // That field is why it is here: an allowlist that does not match
                // reality refuses every image, and the first time it happened the
                // model paraphrased the reason and dropped the hostname — leaving
                // "it couldn't fetch them" and no way to tell WHICH host to allow
                // without reaching the logs. Structured, the model reports it.
                tracing::warn!(
                    agent = %self.agent_id,
                    refusal = ?refusal,
                    "image.fetch refused a URL"
                );
                let mut out = json!({"url": raw, "error": refusal.reason()});
                if let FetchRefusal::HostNotAllowed(host) = &refusal {
                    out["blocked_host"] = json!(host);
                    out["operator_note"] = json!(format!(
                        "an operator must add {host} to this agent's image_hosts; \
                         say which host was blocked so they know what to add"
                    ));
                }
                return out;
            }
        };
        let bytes = match self.pull(&approved).await {
            Ok(b) => b,
            Err(e) => return json!({"url": raw, "error": e}),
        };
        // Evidence, not the server's claim. A body that is not one of the four
        // renderable rasters is refused outright rather than stored and left to
        // fail at render time.
        let media_type = match sniff(&bytes, "") {
            Some(t) if RENDERABLE.contains(&t.as_str()) => t,
            _ => {
                return json!({
                    "url": raw,
                    "error": "that URL is not a PNG, JPEG, GIF or WebP image",
                })
            }
        };
        let ext = media_type.rsplit('/').next().unwrap_or("img");
        let filename = format!("image-{}.{ext}", index + 1);
        match self.stage(bytes, &media_type, &filename) {
            Ok(artifact_id) => json!({
                "url": raw,
                "artifact_id": artifact_id,
                "media_type": media_type,
            }),
            Err(e) => json!({"url": raw, "error": e}),
        }
    }
}

#[async_trait]
impl Tool for EdgeImageFetchTool {
    fn name(&self) -> &str {
        "image.fetch"
    }

    fn description(&self) -> &str {
        "Fetch pictures from URLs another tool gave you, so you can SHOW them. Returns an \
         artifact_id per image; put those in a wf-image block to display them. Use this \
         whenever the customer wants to see photos (an item's photos, for example) and a \
         tool handed you their URLs — do not paste a URL into your reply and do not claim \
         you cannot show pictures. Only URLs a tool returned, never one you composed \
         yourself; up to 8 at a time. An entry that comes back with `error` instead of \
         `artifact_id` could not be fetched — say so briefly and show the rest."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "urls": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Image URLs from a tool result, in the order to show them.",
                    "maxItems": MAX_FETCH,
                }
            },
            "required": ["urls"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let urls: Vec<String> = input
            .get("urls")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        if urls.is_empty() {
            return Ok(ToolResult::failure(
                "no urls given — pass the image URLs a tool returned".to_string(),
                start.elapsed().as_millis() as u64,
            ));
        }
        let dropped = urls.len().saturating_sub(MAX_FETCH);
        let mut images = Vec::new();
        for (i, url) in urls.iter().take(MAX_FETCH).enumerate() {
            images.push(self.one(url, i).await);
        }
        let mut out = json!({ "images": images });
        // Say what was dropped. Silent truncation reads to the model as "that
        // was all of them", and it would tell the customer the same.
        if dropped > 0 {
            out["not_fetched"] = json!(dropped);
            out["note"] = json!(format!(
                "only the first {MAX_FETCH} URLs were fetched; {dropped} were not"
            ));
        }
        Ok(ToolResult::success(out, start.elapsed().as_millis() as u64))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PUBLIC: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(93, 184, 216, 34));
    const ALLOW: fn() -> Vec<String> =
        || vec!["media.example.com".into(), ".s3.amazonaws.com".into()];

    fn to(addr: IpAddr) -> impl Fn(&str, u16) -> Vec<IpAddr> {
        move |_, _| vec![addr]
    }

    #[test]
    fn only_https_is_fetchable() {
        let e = approve("http://media.example.com/a.png", &ALLOW(), to(PUBLIC)).unwrap_err();
        assert_eq!(e, FetchRefusal::NotHttps);
        // …including the schemes that read a local file or a socket.
        for raw in [
            "file:///etc/passwd",
            "gopher://media.example.com/",
            "ftp://x/",
        ] {
            assert!(approve(raw, &ALLOW(), to(PUBLIC)).is_err(), "{raw}");
        }
    }

    #[test]
    fn a_host_off_the_allowlist_is_refused() {
        let e = approve("https://evil.test/a.png", &ALLOW(), to(PUBLIC)).unwrap_err();
        assert_eq!(e, FetchRefusal::HostNotAllowed("evil.test".into()));
        assert!(approve("https://media.example.com/a.png", &ALLOW(), to(PUBLIC)).is_ok());
    }

    /// A suffix entry admits subdomains and nothing else — in particular not a
    /// lookalike that merely ENDS with the same characters.
    #[test]
    fn a_dotted_entry_matches_subdomains_only() {
        let allow = ALLOW();
        assert!(approve("https://bucket.s3.amazonaws.com/k", &allow, to(PUBLIC)).is_ok());
        assert!(approve("https://s3.amazonaws.com/k", &allow, to(PUBLIC)).is_err());
        assert!(approve(
            "https://evils3.amazonaws.com.evil.test/k",
            &allow,
            to(PUBLIC)
        )
        .is_err());
        assert!(approve("https://notmedia.example.com/k", &allow, to(PUBLIC)).is_err());
    }

    #[test]
    fn a_trailing_dot_or_odd_case_cannot_dodge_the_allowlist() {
        let allow = ALLOW();
        assert!(approve("https://MEDIA.example.COM/a.png", &allow, to(PUBLIC)).is_ok());
        assert!(approve("https://media.example.com./a.png", &allow, to(PUBLIC)).is_ok());
    }

    /// The check that matters: an allowlisted host whose DNS points inward.
    /// Nothing about the URL looks wrong, which is exactly the attack.
    #[test]
    fn an_allowlisted_host_resolving_inward_is_refused() {
        for addr in [
            "127.0.0.1",
            "169.254.169.254",
            "10.20.0.5",
            "192.168.1.1",
            "172.16.0.1",
            "100.64.0.1",
            "0.0.0.0",
        ] {
            let e = approve(
                "https://media.example.com/a.png",
                &ALLOW(),
                to(addr.parse().unwrap()),
            )
            .unwrap_err();
            assert_eq!(e, FetchRefusal::PrivateAddress, "{addr} must be refused");
        }
    }

    #[test]
    fn ipv6_loopback_local_and_mapped_v4_are_refused() {
        for addr in [
            "::1",
            "fe80::1",
            "fc00::1",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
        ] {
            let e = approve(
                "https://media.example.com/a.png",
                &ALLOW(),
                to(addr.parse().unwrap()),
            )
            .unwrap_err();
            assert_eq!(e, FetchRefusal::PrivateAddress, "{addr} must be refused");
        }
    }

    /// One public address does not make a host safe when it also resolves
    /// inward — a retry would land on the other one.
    #[test]
    fn a_host_with_any_forbidden_address_is_refused() {
        let mixed = |_: &str, _: u16| vec![PUBLIC, "127.0.0.1".parse().unwrap()];
        let e = approve("https://media.example.com/a.png", &ALLOW(), mixed).unwrap_err();
        assert_eq!(e, FetchRefusal::PrivateAddress);
    }

    #[test]
    fn a_host_that_does_not_resolve_is_refused() {
        let none = |_: &str, _: u16| Vec::new();
        let e = approve("https://media.example.com/a.png", &ALLOW(), none).unwrap_err();
        assert_eq!(e, FetchRefusal::Unresolvable);
    }

    /// The connection is pinned to the address that was checked, not re-resolved.
    #[test]
    fn the_approved_address_is_the_one_that_was_checked() {
        let ok = approve("https://media.example.com/a.png", &ALLOW(), to(PUBLIC)).unwrap();
        assert_eq!(ok.addr, SocketAddr::new(PUBLIC, 443));
        assert_eq!(ok.host, "media.example.com");
    }

    #[test]
    fn an_empty_allowlist_admits_nothing() {
        assert!(approve("https://media.example.com/a.png", &[], to(PUBLIC)).is_err());
    }

    /// A fetched picture must NOT claim the customer uploaded it — that
    /// producer is what `vet_attachments` admits on a turn frame.
    #[test]
    fn a_fetched_image_is_not_stamped_as_a_customer_upload() {
        assert_ne!(FETCH_PRODUCER, crate::upload::UPLOAD_PRODUCER);
    }
}

#[cfg(test)]
mod refusal_reporting_tests {
    use super::*;

    /// The first live failure refused every photo and the model answered "the
    /// image host isn't one I'm allowed to fetch from" — without naming it. The
    /// host was in the prose, so the model dropped it; as its own field it
    /// survives, and the fix is a one-line config change instead of a hunt
    /// through logs on a box you may not be able to reach.
    #[tokio::test]
    async fn a_blocked_host_is_named_in_its_own_field() {
        let store = workforce_agent::artifacts::FsArtifactStore::new(
            tempfile::TempDir::new().unwrap().keep(),
        )
        .unwrap();
        let tool = EdgeImageFetchTool::new(
            store,
            "agent-1".into(),
            "ext:realm:alice".into(),
            vec!["allowed.example.com".into()],
        );
        let out = tool
            .execute(json!({"urls": ["https://hel1.your-objectstorage.com/b/k.jpg"]}))
            .await
            .unwrap();
        let image = &out.output["images"][0];
        assert_eq!(image["blocked_host"], "hel1.your-objectstorage.com");
        assert!(
            image["operator_note"]
                .as_str()
                .unwrap()
                .contains("hel1.your-objectstorage.com"),
            "the note must name the host an operator has to add"
        );
    }

    /// A failure that is NOT about the allowlist must not invent a blocked host
    /// — an operator told to allowlist something irrelevant chases the wrong fix.
    #[tokio::test]
    async fn a_non_allowlist_refusal_carries_no_blocked_host() {
        let store = workforce_agent::artifacts::FsArtifactStore::new(
            tempfile::TempDir::new().unwrap().keep(),
        )
        .unwrap();
        let tool = EdgeImageFetchTool::new(
            store,
            "agent-1".into(),
            "ext:realm:alice".into(),
            vec!["allowed.example.com".into()],
        );
        let out = tool
            .execute(json!({"urls": ["http://allowed.example.com/k.jpg"]}))
            .await
            .unwrap();
        let image = &out.output["images"][0];
        assert!(image["error"].as_str().unwrap().contains("https"));
        assert!(image.get("blocked_host").is_none());
    }
}

/// Staging is the half that fails SILENTLY. `serve_owned_artifact` will hand a
/// file to nobody but its owner, so a descriptor missing this caller turns every
/// fetched image into "no longer available" with nothing logged anywhere —
/// which is why the descriptor gets its own tests against a real store rather
/// than an assertion that two constants differ.
#[cfg(test)]
mod staging_tests {
    use super::*;
    use workforce_agent::artifacts::FsArtifactStore;

    fn tool(owner: &str) -> EdgeImageFetchTool {
        let store =
            FsArtifactStore::new(tempfile::TempDir::new().expect("tempdir").keep()).expect("store");
        EdgeImageFetchTool::new(
            store,
            "agent-1".to_string(),
            owner.to_string(),
            vec!["media.example.com".into()],
        )
    }

    #[test]
    fn a_fetched_image_is_owned_by_the_caller_it_was_fetched_for() {
        let tool = tool("ext:realm:alice");
        let id = tool
            .stage(vec![1, 2, 3], "image/png", "image-1.png")
            .expect("staged");
        let descriptor = tool
            .store
            .metadata(
                "agent-1",
                &workforce_agent::artifacts::ArtifactId::parse(&id).unwrap(),
            )
            .expect("readable")
            .expect("present");
        assert_eq!(
            descriptor.user_id.as_deref(),
            Some("ext:realm:alice"),
            "without this the render route serves it to nobody"
        );
        assert_eq!(descriptor.producer, FETCH_PRODUCER);
        assert_eq!(descriptor.filename.as_deref(), Some("image-1.png"));
        assert_eq!(descriptor.media_type.as_deref(), Some("image/png"));
    }

    /// The asymmetry, asserted on a real descriptor rather than on constants: a
    /// fetched image is owned by the customer (so it renders) but is NOT what
    /// `host::vet_attachments` admits when a turn frame names an id.
    #[test]
    fn a_fetched_image_is_not_admissible_as_a_customer_attachment() {
        let tool = tool("ext:realm:alice");
        let id = tool
            .stage(vec![1], "image/jpeg", "image-1.jpeg")
            .expect("staged");
        let descriptor = tool
            .store
            .metadata(
                "agent-1",
                &workforce_agent::artifacts::ArtifactId::parse(&id).unwrap(),
            )
            .unwrap()
            .unwrap();
        assert_ne!(
            descriptor.producer,
            crate::upload::UPLOAD_PRODUCER,
            "a turn frame naming this id must be refused"
        );
    }

    #[test]
    fn a_staged_image_expires_like_an_attachment() {
        let tool = tool("ext:realm:alice");
        let id = tool
            .stage(vec![1], "image/png", "image-1.png")
            .expect("staged");
        let descriptor = tool
            .store
            .metadata(
                "agent-1",
                &workforce_agent::artifacts::ArtifactId::parse(&id).unwrap(),
            )
            .unwrap()
            .unwrap();
        assert_eq!(descriptor.ttl_seconds, ATTACHMENT_TTL.as_secs());
    }
}
