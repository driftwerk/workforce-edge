//! Customer file uploads.
//!
//! The admin knowledge upload is the shape this copies, not the route it
//! reuses: that one is operator-gated and writes to the training corpus. This
//! one is reached by **the customer's customers, over the public internet**, so
//! it is written to be hostile-input-first.
//!
//! Order of checks is deliberate — cheapest and most decisive first, so a
//! hostile caller is rejected before anything expensive happens:
//!
//! 1. the agent accepts uploads at all,
//! 2. the running total against the per-file cap, **while streaming** — a body
//!    is never buffered whole to discover it was too big,
//! 3. the SNIFFED type (magic bytes), because an extension and a declared
//!    `Content-Type` are claims by an untrusted client, not evidence,
//! 4. that type against the agent's allowlist,
//! 5. the per-conversation file count.
//!
//! Accepted bytes are staged in the artifact store owned by the verified
//! customer, and only the id travels onward: `TurnRequest` carries ids, never
//! bytes, and the turn re-checks ownership before reading one.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{ConnectInfo, Multipart, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;
use serde_json::json;
use workforce_agent::artifacts::store::ATTACHMENT_TTL;
use workforce_agent::artifacts::types::{ArtifactBody, ArtifactDescriptor, ContentType};
use workforce_agent::artifacts::ArtifactStore;

use crate::agent::{EmbedSettings, UploadKind};
use crate::tenant::ServedTenant;

/// Recorded on every staged upload, so a turn can tell a customer's own file
/// apart from any other artifact whose id it might have learned.
///
/// This is the SHARED session-upload producer, not an edge-specific name: the
/// by-reference resolver only admits artifacts from a closed producer list, so
/// a surface that invents its own value has every attachment rejected as "not
/// an uploaded attachment" — which is exactly what happened when it did.
pub const UPLOAD_PRODUCER: &str = workforce_agent::attachments::SESSION_UPLOAD_PRODUCER;

/// Files accepted in one request. A turn carrying more than a handful of
/// documents is a context problem long before it is a storage one.
pub const MAX_UPLOAD_FILES: usize = 5;

/// What the widget gets back for a staged file: enough to show a chip and to
/// name the file in the turn, never a URL it could share.
#[derive(Debug, Serialize, PartialEq)]
pub struct StagedFile {
    pub artifact_id: String,
    pub filename: String,
    pub media_type: String,
    pub bytes: usize,
}

/// Why an upload was refused. Each maps to a message the customer can act on —
/// "it failed" is not a usable answer when they are holding a file.
#[derive(Debug, PartialEq)]
pub enum UploadError {
    Disabled,
    /// Uploads are configured but there is nowhere to put the bytes — the
    /// install has no `[artifacts]` store. A DIFFERENT failure from "the
    /// operator turned uploads off", and conflating the two sends whoever hits
    /// it to check a setting that is already correct.
    NoStore,
    TooLarge {
        limit_mb: usize,
    },
    UnsupportedType {
        sniffed: String,
    },
    NotAllowed {
        kind: UploadKind,
    },
    TooMany {
        limit: usize,
    },
    Empty,
    Malformed(String),
}

impl UploadError {
    pub fn status(&self) -> StatusCode {
        match self {
            Self::Disabled | Self::NotAllowed { .. } => StatusCode::FORBIDDEN,
            Self::NoStore => StatusCode::SERVICE_UNAVAILABLE,
            Self::TooLarge { .. } => StatusCode::PAYLOAD_TOO_LARGE,
            Self::UnsupportedType { .. } | Self::Empty | Self::Malformed(_) => {
                StatusCode::BAD_REQUEST
            }
            Self::TooMany { .. } => StatusCode::TOO_MANY_REQUESTS,
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::Disabled => "This assistant doesn't accept file uploads.".into(),
            // The customer cannot act on a missing config block, so they get a
            // plain statement; the operator detail goes to the log beside it.
            Self::NoStore => "File uploads aren't available right now.".into(),
            Self::TooLarge { limit_mb } => {
                format!("That file is too big — the limit is {limit_mb} MB.")
            }
            Self::UnsupportedType { sniffed } => {
                format!("That file type isn't supported ({sniffed}).")
            }
            Self::NotAllowed { kind } => {
                format!("This assistant doesn't accept {} files.", kind.label())
            }
            Self::TooMany { limit } => {
                format!("You can attach at most {limit} files to one message.")
            }
            Self::Empty => "That file is empty.".into(),
            Self::Malformed(why) => format!("The upload couldn't be read: {why}"),
        }
    }
}

impl IntoResponse for UploadError {
    fn into_response(self) -> Response {
        (self.status(), Json(json!({ "error": self.message() }))).into_response()
    }
}

/// Identify a file from its leading bytes. An extension is a claim; this is
/// evidence. Anything not positively identified is refused — an allowlist can
/// only be enforced against a type we are sure of.
///
/// Text has no magic number, so a UTF-8 body with no control characters is
/// accepted as `text/plain` (CSV and Markdown included — they are text, and the
/// downstream extractor treats them as such).
pub fn sniff(bytes: &[u8], declared: &str) -> Option<String> {
    const ZIP: &[u8] = b"PK\x03\x04";
    let starts = |sig: &[u8]| bytes.starts_with(sig);

    if starts(b"\x89PNG\r\n\x1a\n") {
        return Some("image/png".into());
    }
    if starts(b"\xff\xd8\xff") {
        return Some("image/jpeg".into());
    }
    if starts(b"GIF87a") || starts(b"GIF89a") {
        return Some("image/gif".into());
    }
    if bytes.len() > 12 && starts(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some("image/webp".into());
    }
    if starts(b"%PDF-") {
        return Some("application/pdf".into());
    }
    if starts(ZIP) {
        // docx and xlsx are both zips; only the declared type distinguishes
        // them cheaply, so it is consulted ONLY to disambiguate a container we
        // have already positively identified — never to grant a type outright.
        let declared = declared.split(';').next().unwrap_or("").trim();
        return match declared {
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
            | "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => {
                Some(declared.to_string())
            }
            _ => None,
        };
    }
    // Text last: it is the only type identified by absence of evidence.
    let text = std::str::from_utf8(bytes).ok()?;
    text.chars()
        .all(|c| !c.is_control() || matches!(c, '\n' | '\r' | '\t'))
        .then(|| "text/plain".to_string())
}

impl UploadKind {
    /// Customer-facing name, for a refusal message.
    pub fn label(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Spreadsheet => "spreadsheet",
            Self::Document => "document",
        }
    }
}

/// Validate one uploaded file against the agent's policy. Split from the HTTP
/// handler so every rule is unit-testable without a server, a socket, or a
/// multipart body.
pub fn vet(
    embed: &EmbedSettings,
    bytes: &[u8],
    declared: &str,
    already: usize,
) -> Result<String, UploadError> {
    if !embed.uploads_enabled() {
        return Err(UploadError::Disabled);
    }
    if already >= MAX_UPLOAD_FILES {
        return Err(UploadError::TooMany {
            limit: MAX_UPLOAD_FILES,
        });
    }
    if bytes.is_empty() {
        return Err(UploadError::Empty);
    }
    if bytes.len() > embed.upload_max_bytes() {
        return Err(UploadError::TooLarge {
            limit_mb: embed.upload_max_bytes() / (1024 * 1024),
        });
    }
    let sniffed = sniff(bytes, declared).ok_or_else(|| UploadError::UnsupportedType {
        sniffed: "unrecognised".into(),
    })?;
    let kind = UploadKind::of_media_type(&sniffed).ok_or_else(|| UploadError::UnsupportedType {
        sniffed: sniffed.clone(),
    })?;
    if !embed.uploads.contains(&kind) {
        return Err(UploadError::NotAllowed { kind });
    }
    Ok(sniffed)
}

/// `POST /public/upload/{agent}` — stage one or more customer files.
///
/// Authenticated exactly as the download route is, so the staged artifact is
/// owned by the verified customer and the turn can later prove a submitted id
/// is theirs. The producer is recorded so a turn cannot pass off some other
/// tool's artifact as an upload.
pub async fn public_upload(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path(agent_id): Path<String>,
    headers: HeaderMap,
    State(state): State<Arc<crate::server::EdgeState>>,
    ServedTenant(tenant): ServedTenant,
    multipart: Multipart,
) -> Response {
    let caller = match crate::server::authenticate(
        &tenant,
        state.trusted_proxies(),
        &agent_id,
        &headers,
        None,
        Some(peer),
    )
    .await
    {
        Ok(c) => c,
        Err(status) => return status.into_response(),
    };
    let embed = &caller.host.agent().embed;
    if !embed.uploads_enabled() {
        return UploadError::Disabled.into_response();
    }
    let Some(artifacts) = caller.host.artifacts() else {
        tracing::warn!(
            agent = %agent_id,
            "uploads are enabled but this install has no [artifacts] store; refusing"
        );
        return UploadError::NoStore.into_response();
    };

    match stage_all(
        multipart,
        embed,
        artifacts.store.as_ref(),
        &agent_id,
        &caller.owner,
    )
    .await
    {
        Ok(files) => Json(json!({ "files": files })).into_response(),
        Err(e) => e.into_response(),
    }
}

/// Read, vet and stage every part. Separated from the handler so the whole
/// policy path is exercised without an HTTP stack.
async fn stage_all(
    mut multipart: Multipart,
    embed: &EmbedSettings,
    store: &dyn ArtifactStore,
    agent_id: &str,
    owner: &str,
) -> Result<Vec<StagedFile>, UploadError> {
    let mut staged: Vec<StagedFile> = Vec::new();
    loop {
        let field = match multipart.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => return Err(UploadError::Malformed(e.to_string())),
        };
        let filename = field
            .file_name()
            .map(sanitize_filename)
            .unwrap_or_else(|| "upload".to_string());
        let declared = field.content_type().unwrap_or_default().to_string();

        // Read with a running cap rather than `field.bytes()`: the point is to
        // stop reading a hostile body, not to measure it after it has landed.
        let bytes = read_capped(field, embed.upload_max_bytes()).await?;
        let media_type = vet(embed, &bytes, &declared, staged.len())?;
        let size = bytes.len();

        let descriptor = ArtifactDescriptor {
            user_id: Some(owner.to_string()),
            ttl_seconds: ATTACHMENT_TTL.as_secs(),
            ..ArtifactDescriptor::from_context(
                UPLOAD_PRODUCER,
                ContentType::OctetStream,
                agent_id.to_string(),
            )
            .with_file_meta(filename.clone(), &media_type)
        };
        let handle = store
            .write(agent_id, descriptor, ArtifactBody::Bytes(bytes))
            .map_err(|e| {
                tracing::warn!(error = %e, agent = %agent_id, "staging a customer upload failed");
                UploadError::Malformed("the file couldn't be stored".into())
            })?;
        staged.push(StagedFile {
            artifact_id: handle.id.as_str().to_string(),
            filename,
            media_type,
            bytes: size,
        });
    }
    if staged.is_empty() {
        return Err(UploadError::Empty);
    }
    Ok(staged)
}

/// Drain a multipart field, aborting as soon as it exceeds `limit`.
async fn read_capped(
    mut field: axum::extract::multipart::Field<'_>,
    limit: usize,
) -> Result<Vec<u8>, UploadError> {
    let mut out: Vec<u8> = Vec::new();
    loop {
        match field.chunk().await {
            Ok(Some(chunk)) => {
                if out.len() + chunk.len() > limit {
                    return Err(UploadError::TooLarge {
                        limit_mb: limit / (1024 * 1024),
                    });
                }
                out.extend_from_slice(&chunk);
            }
            Ok(None) => return Ok(out),
            Err(e) => return Err(UploadError::Malformed(e.to_string())),
        }
    }
}

/// A display-safe filename. It only ever labels an attachment — it is never a
/// path — but it reaches a prompt and a UI, so path structure and control
/// characters are removed rather than merely escaped: separators become
/// boundaries, and any segment that is only dots (`.`, `..`) is dropped, so
/// `../../etc/passwd` reads as `etc-passwd` with no traversal left in it.
pub fn sanitize_filename(raw: &str) -> String {
    let flattened: String = raw
        .chars()
        .map(|c| match c {
            c if c.is_control() => '-',
            '/' | '\\' | ':' | '\0' => '-',
            c => c,
        })
        .collect();
    let name = flattened
        .split('-')
        .map(str::trim)
        .filter(|s| !s.is_empty() && !s.chars().all(|c| c == '.'))
        .collect::<Vec<_>>()
        .join("-");
    let capped: String = name.chars().take(120).collect();
    if capped.trim().is_empty() {
        "upload".to_string()
    } else {
        capped
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{Branding, McpSettings};

    fn embed(kinds: Vec<UploadKind>) -> EmbedSettings {
        EmbedSettings {
            allowed_origins: vec![],
            allowed_tools: vec![],
            allowed_emails: vec![],
            keycloak_issuer: "https://kc".into(),
            keycloak_audience: None,
            mcp: None::<McpSettings>,
            branding: Branding::default(),
            passive_memory: None,
            data_tables: None,
            charts: None,
            forms: None,
            maps: None,
            diagrams: None,
            excalidraw_export: None,
            inline_images: None,
            images: None,
            image_hosts: vec![],
            spill_skip_tools: vec![],
            uploads: kinds,
            upload_max_mb: None,
            drawing: None,
            tool_grants: vec![],
            permissions: None,
            policy: None,
        }
    }

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR";
    const XLSX: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
    const DOCX: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";

    #[test]
    fn sniffs_each_supported_type_from_its_bytes() {
        assert_eq!(sniff(PNG, ""), Some("image/png".into()));
        assert_eq!(sniff(b"\xff\xd8\xff\xe0", ""), Some("image/jpeg".into()));
        assert_eq!(sniff(b"GIF89a....", ""), Some("image/gif".into()));
        assert_eq!(
            sniff(b"RIFF\x00\x00\x00\x00WEBPVP8 ", ""),
            Some("image/webp".into())
        );
        assert_eq!(sniff(b"%PDF-1.7\n", ""), Some("application/pdf".into()));
        assert_eq!(sniff(b"name,qty\na,1\n", ""), Some("text/plain".into()));
    }

    /// The whole point of sniffing: a claim must not be able to grant a type.
    #[test]
    fn a_declared_type_cannot_override_the_bytes() {
        // An executable calling itself a PNG is not a PNG.
        assert_eq!(sniff(b"MZ\x90\x00\x03", "image/png"), None);
        // Nor can a lie promote text to a spreadsheet.
        assert_eq!(sniff(b"hello there", XLSX), Some("text/plain".into()));
    }

    /// docx and xlsx are the same container; the declared type only picks
    /// BETWEEN them, and only once the zip itself is confirmed.
    #[test]
    fn zip_needs_a_recognised_office_type_to_resolve() {
        assert_eq!(sniff(b"PK\x03\x04rest", XLSX), Some(XLSX.into()));
        assert_eq!(sniff(b"PK\x03\x04rest", DOCX), Some(DOCX.into()));
        assert_eq!(sniff(b"PK\x03\x04rest", "application/zip"), None);
        assert_eq!(sniff(b"PK\x03\x04rest", ""), None);
    }

    #[test]
    fn refuses_binary_that_is_not_a_known_format() {
        assert_eq!(sniff(b"\x7fELF\x02\x01\x01", ""), None);
        assert_eq!(sniff(&[0u8, 1, 2, 3, 4], ""), None);
    }

    #[test]
    fn vet_refuses_everything_when_uploads_are_off() {
        assert_eq!(vet(&embed(vec![]), PNG, "", 0), Err(UploadError::Disabled));
    }

    #[test]
    fn vet_enforces_the_allowlist_per_kind() {
        let e = embed(vec![UploadKind::Image]);
        assert_eq!(vet(&e, PNG, "", 0), Ok("image/png".into()));
        assert_eq!(
            vet(&e, b"%PDF-1.7\n", "", 0),
            Err(UploadError::NotAllowed {
                kind: UploadKind::Document
            })
        );
        let docs = embed(vec![UploadKind::Document]);
        assert_eq!(
            vet(&docs, b"%PDF-1.7\n", "", 0),
            Ok("application/pdf".into())
        );
    }

    #[test]
    fn vet_enforces_size_and_count_and_emptiness() {
        let e = embed(vec![UploadKind::Image]);
        assert_eq!(vet(&e, b"", "", 0), Err(UploadError::Empty));
        assert_eq!(
            vet(&e, PNG, "", MAX_UPLOAD_FILES),
            Err(UploadError::TooMany {
                limit: MAX_UPLOAD_FILES
            })
        );
        let mut big = PNG.to_vec();
        big.resize(e.upload_max_bytes() + 1, 0);
        assert!(matches!(
            vet(&e, &big, "", 0),
            Err(UploadError::TooLarge { .. })
        ));
    }

    /// An operator cannot configure the edge into buffering arbitrary bytes for
    /// an anonymous caller.
    #[test]
    fn the_configured_cap_is_clamped_both_ways() {
        let mut e = embed(vec![UploadKind::Image]);
        e.upload_max_mb = Some(9_999);
        assert_eq!(e.upload_max_bytes(), 25 * 1024 * 1024);
        e.upload_max_mb = Some(0);
        assert_eq!(e.upload_max_bytes(), 1024 * 1024);
        e.upload_max_mb = None;
        assert_eq!(e.upload_max_bytes(), 10 * 1024 * 1024);
    }

    #[test]
    fn drawing_needs_somewhere_to_put_its_png() {
        let mut e = embed(vec![UploadKind::Document]);
        e.drawing = Some(true);
        assert!(!e.drawing_enabled(), "no image kind ⇒ no canvas");
        e.uploads.push(UploadKind::Image);
        assert!(e.drawing_enabled());
    }

    #[test]
    fn filenames_are_stripped_of_paths_and_control_characters() {
        assert_eq!(sanitize_filename("../../etc/passwd"), "etc-passwd");
        assert_eq!(sanitize_filename("a\u{0}b\nc"), "a-b-c");
        assert_eq!(sanitize_filename("   "), "upload");
        assert_eq!(sanitize_filename(""), "upload");
        assert_eq!(sanitize_filename("report.xlsx"), "report.xlsx");
        assert_eq!(sanitize_filename(&"x".repeat(400)).len(), 120);
    }

    #[test]
    fn every_refusal_says_what_to_do_about_it() {
        for e in [
            UploadError::Disabled,
            UploadError::NoStore,
            UploadError::TooLarge { limit_mb: 10 },
            UploadError::UnsupportedType {
                sniffed: "x".into(),
            },
            UploadError::NotAllowed {
                kind: UploadKind::Image,
            },
            UploadError::TooMany { limit: 5 },
            UploadError::Empty,
        ] {
            let m = e.message();
            assert!(!m.is_empty() && m.ends_with('.'), "{m}");
            assert!(e.status().is_client_error() || e.status().is_server_error());
        }
    }
}

#[cfg(test)]
mod staging_tests {
    use super::*;
    use std::sync::Arc;

    use workforce_agent::artifacts::FsArtifactStore;

    fn store() -> Arc<FsArtifactStore> {
        FsArtifactStore::new(tempfile::TempDir::new().expect("tempdir").keep()).expect("store")
    }

    /// Staging records the owner and the producer. Both are what the turn checks
    /// before it will read a file, so if either stops being written the turn
    /// silently starts refusing every legitimate upload — or worse, stops being
    /// able to tell one customer's file from another's.
    #[test]
    fn a_staged_upload_records_its_owner_and_producer() {
        let store = store();
        let descriptor = ArtifactDescriptor {
            user_id: Some("ext:realm:alice".into()),
            ttl_seconds: ATTACHMENT_TTL.as_secs(),
            ..ArtifactDescriptor::from_context(
                UPLOAD_PRODUCER,
                ContentType::OctetStream,
                "agent-1".to_string(),
            )
            .with_file_meta("photo.png".to_string(), "image/png")
        };
        let handle = store
            .write(
                "agent-1",
                descriptor,
                ArtifactBody::Bytes(b"\x89PNG".to_vec()),
            )
            .expect("write");

        let stored = store
            .metadata("agent-1", &handle.id)
            .expect("metadata")
            .expect("present");
        assert_eq!(stored.user_id.as_deref(), Some("ext:realm:alice"));
        assert_eq!(stored.producer, UPLOAD_PRODUCER);
        assert_eq!(stored.filename.as_deref(), Some("photo.png"));
        assert_eq!(stored.media_type.as_deref(), Some("image/png"));
    }

    /// The turn's gate, stated as a test at the level it actually matters:
    /// another customer's id, and an artifact this agent never staged as an
    /// upload, must both be unusable.
    #[test]
    fn ownership_and_producer_together_gate_a_read() {
        let store = store();
        let mine = ArtifactDescriptor {
            user_id: Some("ext:realm:alice".into()),
            ..ArtifactDescriptor::from_context(
                UPLOAD_PRODUCER,
                ContentType::OctetStream,
                "agent-1".to_string(),
            )
        };
        let theirs = ArtifactDescriptor {
            user_id: Some("ext:realm:bob".into()),
            ..ArtifactDescriptor::from_context(
                UPLOAD_PRODUCER,
                ContentType::OctetStream,
                "agent-1".to_string(),
            )
        };
        let other_tool = ArtifactDescriptor {
            user_id: Some("ext:realm:alice".into()),
            ..ArtifactDescriptor::from_context(
                "export_xlsx",
                ContentType::OctetStream,
                "agent-1".to_string(),
            )
        };
        let a = store
            .write("agent-1", mine, ArtifactBody::Bytes(b"x".to_vec()))
            .expect("write");
        let b = store
            .write("agent-1", theirs, ArtifactBody::Bytes(b"x".to_vec()))
            .expect("write");
        let c = store
            .write("agent-1", other_tool, ArtifactBody::Bytes(b"x".to_vec()))
            .expect("write");

        let usable = |id: &workforce_agent::artifacts::types::ArtifactId| {
            let d = store.metadata("agent-1", id).unwrap().unwrap();
            d.user_id.as_deref() == Some("ext:realm:alice") && d.producer == UPLOAD_PRODUCER
        };
        assert!(usable(&a.id), "alice's own upload");
        assert!(!usable(&b.id), "another customer's upload");
        assert!(
            !usable(&c.id),
            "alice's own artifact from a different producer"
        );
    }
}
