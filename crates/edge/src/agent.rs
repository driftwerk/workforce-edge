//! Customer-facing agent definitions and the registry that resolves them.
//!
//! An edge agent is deliberately cut down versus an internal hub agent: it has
//! a persona, a welcome, a model, and an `[embed]` block that gates the public
//! surface (which origins may load it, which tools it may call, which Keycloak
//! realm identifies its customers, and which company MCP server it acts
//! against). Everything an internal agent carries — Slack, workflows, vault,
//! scheduler — is absent by construction.

use serde::{Deserialize, Serialize};

/// A single customer-facing agent.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EdgeAgent {
    /// Stable identifier — also the session namespace and the Weaviate
    /// memory/training owner scope. Must be unique within the install.
    pub id: String,
    /// Human-readable name shown in the widget header and admin UI.
    pub display_name: String,
    /// LEGACY persona string. Superseded by [`EdgeAgent::identity`]; kept as the
    /// fallback for agents authored before the identity-file split, and cleared
    /// once identity files are saved. `#[serde(default)]` so identity-only agents
    /// (created via the admin UI) deserialize without it.
    #[serde(default)]
    pub system_prompt: String,
    /// The multi-file persona (SOUL/IDENTITY/AGENTS/TOOLS), the customer-facing
    /// analogue of a hub agent's workspace identity files. When any file is set,
    /// the system prompt is assembled from these instead of `system_prompt`.
    #[serde(default)]
    pub identity: IdentityFiles,
    /// Greeting shown to a customer when a new conversation opens.
    #[serde(default)]
    pub welcome: String,
    /// Model override; falls back to the install's default LLM model.
    #[serde(default)]
    pub model: Option<String>,
    /// Guided flow auto-started for each new conversation: the id of a
    /// playbook loaded from `playbooks_dir`. Boot fails if it doesn't resolve.
    #[serde(default)]
    pub playbook: Option<String>,
    /// Catalog of guided flows this agent may start when it detects the user
    /// wants something a flow covers. The auto-start flow (if any) is always
    /// included. Boot fails on ids that don't resolve against `playbooks_dir`
    /// (runtime-created defs will relax this when the admin UI lands).
    #[serde(default)]
    pub playbooks: Vec<String>,
    /// The public-surface gate.
    pub embed: EmbedSettings,
    /// This agent's own words for the "Looking up …" label a customer sees
    /// while a company-MCP operation or a direct MCP tool runs, checked in
    /// order before the generic defaults — so a deployment's domain vocabulary
    /// lives in its config.
    #[serde(default)]
    pub progress_labels: Vec<ProgressLabel>,
    /// This agent's domain words for refusing derived figures ("23 unfilled
    /// shifts") as memories, on top of the generic ones.
    #[serde(default)]
    pub derived_figures: workforce_common::settings::DerivedFigureVocabulary,
}

/// One "Looking up …" label: an invoked operation or MCP tool whose name
/// contains `needle` (case-insensitively) reads as "Looking up {label}" — or
/// "Updating {label}" for a tool named as a write (`create_lead`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgressLabel {
    pub needle: String,
    pub label: String,
}

/// The agent's persona as separate markdown files, mirroring the platform's
/// SOUL/IDENTITY/AGENTS/TOOLS identity model (see `workforce_common::identity`).
/// WELCOME.md is the [`EdgeAgent::welcome`] greeting — kept separate because it
/// is shown to the customer, not fed to the model.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IdentityFiles {
    /// SOUL.md — personality, values, voice.
    #[serde(default)]
    pub soul: String,
    /// IDENTITY.md — who the agent is (name, role, remit).
    #[serde(default)]
    pub identity: String,
    /// AGENTS.md — operational rules / guidelines.
    #[serde(default)]
    pub agents: String,
    /// TOOLS.md — notes on the tools available.
    #[serde(default)]
    pub tools: String,
}

impl IdentityFiles {
    /// Whether every identity file is blank (⇒ fall back to `system_prompt`).
    pub fn is_empty(&self) -> bool {
        self.soul.trim().is_empty()
            && self.identity.trim().is_empty()
            && self.agents.trim().is_empty()
            && self.tools.trim().is_empty()
    }
}

impl EdgeAgent {
    /// The system prompt for a turn: assembled from the identity files (mirroring
    /// the hub's section-headered layout in `build_identity_prompt`) when any is
    /// set, else the legacy single `system_prompt`.
    pub fn assembled_prompt(&self) -> String {
        if self.identity.is_empty() {
            return self.system_prompt.clone();
        }
        let id = &self.identity;
        let mut parts = Vec::new();
        for (header, body) in [
            ("## Your Identity", &id.soul),
            ("## Who You Are", &id.identity),
            ("## Operational Guidelines", &id.agents),
            ("## Tool Notes", &id.tools),
        ] {
            let b = body.trim();
            if !b.is_empty() {
                parts.push(format!("{header}\n{b}"));
            }
        }
        parts.join("\n\n")
    }
}

/// The `[embed]` block: everything that gates and shapes the public surface.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EmbedSettings {
    /// Web origins permitted to load the widget and open a socket. Exact match,
    /// scheme + host + port — never a wildcard.
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    /// Closed tool allowlist. A tool is callable only if it matches an entry
    /// exactly or via a `prefix.*` glob. Empty ⇒ the agent calls no tools.
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    /// Keycloak realm issuer that must have signed a customer's token.
    pub keycloak_issuer: String,
    /// Audience to pin on customer tokens; `None` verifies signature + issuer
    /// + expiry only (pass-through of the webapp's own token).
    #[serde(default)]
    pub keycloak_audience: Option<String>,
    /// Optional closed allowlist of customer email addresses. Empty ⇒ any
    /// valid token from the realm is accepted (a company IdP already bounds
    /// who exists). Non-empty ⇒ the token must carry a matching, verified
    /// `email` claim — for installs whose IdP is open to the world (e.g. a
    /// test install on Google OIDC, where anyone has an account).
    #[serde(default)]
    pub allowed_emails: Vec<String>,
    /// The primary company MCP server this agent acts against, as the customer;
    /// `allowed_tools` and `tool_grants` are its allowlist and write tiers.
    /// Optional: an agent with no MCP is a pure-chat agent (no tools), so the
    /// edge can run on installs that have no MCP server at all.
    #[serde(default)]
    pub mcp: Option<McpSettings>,
    /// MCP servers used alongside the primary, each with its own connection,
    /// allowlist and write tiers. Needs the primary. Every server's tool names
    /// must be distinct from every other's on the wire.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_servers: Vec<McpServerSettings>,
    /// Cosmetic widget branding.
    #[serde(default)]
    pub branding: Branding,
    /// Whether to passively learn durable facts about each customer from their
    /// turns (identity, role, preferences) for continuity. `None` ⇒ on. A
    /// per-agent kill switch — set `false` to disable without a redeploy.
    #[serde(default)]
    pub passive_memory: Option<bool>,
    /// Opt-in rendering of interactive data tables from `wf-table` fenced
    /// blocks in the widget, with matching system-prompt guidance. `None` ⇒ off.
    #[serde(default)]
    pub data_tables: Option<bool>,
    /// Opt-in rendering of interactive charts from `wf-chart` fenced blocks in
    /// the widget, with matching system-prompt guidance. `None` ⇒ off.
    #[serde(default)]
    pub charts: Option<bool>,
    /// Opt-in rendering of interactive forms from `wf-form` fenced blocks in the
    /// widget, with matching system-prompt guidance. `None` ⇒ off.
    #[serde(default)]
    pub forms: Option<bool>,
    /// Opt-in rendering of choropleth maps from `wf-map` fenced blocks in the
    /// widget, with matching system-prompt guidance. `None` ⇒ off.
    #[serde(default)]
    pub maps: Option<bool>,
    /// Opt-in rendering of node-and-arrow diagrams from `wf-diagram` fenced
    /// blocks in the widget, with matching system-prompt guidance. `None` ⇒ off.
    #[serde(default)]
    pub diagrams: Option<bool>,
    /// Which kinds of file a customer may attach. Empty ⇒ uploads are off,
    /// which is the default.
    #[serde(default)]
    pub uploads: Vec<UploadKind>,
    /// Per-file cap in MiB. `None` ⇒ [`DEFAULT_UPLOAD_MAX_MB`], clamped to
    /// [`MAX_UPLOAD_MAX_MB`].
    #[serde(default)]
    pub upload_max_mb: Option<u32>,
    /// Opt-in drawing canvas in the composer. Its output is a PNG, so it needs
    /// [`UploadKind::Image`] in `uploads` to have anywhere to go.
    #[serde(default)]
    pub drawing: Option<bool>,
    /// Opt-in inline rendering of image attachments. Off ⇒ every attachment is
    /// a download button, which is what the widget did before this existed.
    #[serde(default)]
    pub inline_images: Option<bool>,
    /// Opt-in rendering of `wf-image` blocks — the agent showing a picture back
    /// in its reply, naming an artifact the customer owns. DISTINCT from
    /// [`Self::inline_images`], which only decides how an *attachment* is drawn:
    /// an operator can want uploads shown as pictures without wanting the agent
    /// to compose images into answers, or the reverse. `None` ⇒ off.
    #[serde(default)]
    pub images: Option<bool>,
    /// Hosts `image.fetch` may pull pictures from. EMPTY ⇒ the tool is never
    /// registered, which is the default and the safe posture: a server-side
    /// fetch of a data-supplied URL is an SSRF sink, and an allowlist is what
    /// bounds it. An entry matches a host exactly, or — written `.example.com`
    /// — any subdomain of it (never the bare parent).
    #[serde(default)]
    pub image_hosts: Vec<String>,
    /// Tools whose result the model must read VERBATIM, on top of the shared
    /// content namespaces — they are exempted from spilling.
    ///
    /// The edge spills any tool result over 8 KB to an artifact handle, which is
    /// right for record sets (the widget browses them) and wrong for a result the
    /// model has to copy values out of. A tool returning signed URLs is the
    /// motivating case: the URLs are long, so a handful crosses the threshold,
    /// and a handle cannot be turned back into URLs — the model simply cannot do
    /// the thing it was asked to. Anything else the model must quote exactly
    /// (share links, upload URLs, tokens) has the same shape.
    ///
    /// Matched like the shared list: an exact name, or a prefix followed by `.`
    /// or `_`.
    #[serde(default)]
    pub spill_skip_tools: Vec<String>,
    /// Opt-in "download for Excalidraw" affordance on a rendered diagram. Purely
    /// a widget control — the model is never told about it — so an operator can
    /// keep a third-party tool's name off their customer surface. Downloading a
    /// diagram as an image is always available and needs no flag. `None` ⇒ off.
    #[serde(default)]
    pub excalidraw_export: Option<bool>,
    /// Per-user write tiers: tools unlocked only for a signed-in user who holds
    /// the required capability. `allowed_tools` is the base every user gets; each
    /// grant adds its `tools` on top for users who satisfy `requires`. Empty ⇒
    /// everyone gets exactly `allowed_tools` (the historical behaviour). This is
    /// a UX / defence-in-depth layer — the company backend still enforces every
    /// write per-user on the forwarded token, so grants never widen real access.
    #[serde(default)]
    pub tool_grants: Vec<ToolGrant>,
    /// How the edge resolves the signed-in user's permission set for
    /// `backend_permissions` grants: it calls this endpoint once per session with
    /// the user's forwarded bearer and caches the result. Absent ⇒
    /// `backend_permissions` grants can never be satisfied (fail-closed).
    #[serde(default)]
    pub permissions: Option<PermissionsConfig>,
    /// Optional policy over the granted write tiers (e.g. an enforced
    /// confirmation before any write). Absent ⇒ no extra policy.
    #[serde(default)]
    pub policy: Option<EmbedPolicy>,
}

impl EmbedSettings {
    /// The primary's tools, then each additional server's.
    fn tool_sets(&self) -> impl Iterator<Item = ToolSet<'_>> {
        std::iter::once(ToolSet {
            allowed_tools: &self.allowed_tools,
            tool_grants: &self.tool_grants,
        })
        .chain(self.mcp_servers.iter().map(McpServerSettings::tools))
    }

    /// The tools this user may be offered: every server's base allowlist plus
    /// the tools from every grant the user satisfies. `is_granted` is supplied
    /// by the session layer (which knows the user's resolved capabilities per
    /// source), keeping this pure and unit-testable.
    pub fn effective_allowed_tools(&self, is_granted: impl Fn(&ToolGrant) -> bool) -> Vec<String> {
        self.tool_sets()
            .flat_map(|set| set.effective(&is_granted))
            .collect()
    }

    /// Every server's grants, the primary's first.
    pub fn all_tool_grants(&self) -> impl Iterator<Item = &ToolGrant> {
        self.tool_sets().flat_map(|set| set.tool_grants.iter())
    }

    /// Whether `tool` is a write-tier tool on some server (unlocked only by a
    /// grant there, not part of that server's base allowlist) — used to decide
    /// whether the confirmation policy and per-user gating apply to it.
    pub fn is_granted_tool(&self, tool: &str) -> bool {
        self.tool_sets().any(|set| set.grants_tool(tool))
    }

    /// Whether any server's base allowlist admits `tool`.
    pub fn any_server_admits(&self, tool: &str) -> bool {
        self.tool_sets()
            .any(|set| allowlist_admits(set.allowed_tools, tool))
    }

    /// The primary server first, then the additional ones in declared order.
    pub fn mcp_server_specs(&self) -> Vec<McpServerSpec<'_>> {
        let primary_tools = ToolSet {
            allowed_tools: &self.allowed_tools,
            tool_grants: &self.tool_grants,
        };
        self.mcp
            .iter()
            .map(|mcp| McpServerSpec::new(PRIMARY_MCP_ID, mcp, None, primary_tools, true))
            .chain(self.mcp_servers.iter().map(|s| {
                McpServerSpec::new(
                    &s.id,
                    &s.connection,
                    s.tool_prefix.as_deref(),
                    s.tools(),
                    false,
                )
            }))
            .collect()
    }

    /// Every static rule over the agent's servers; `Err` names the field to
    /// fix. Runs on every build path, so it checks only what a config written
    /// before additional servers existed cannot contain: an existing
    /// single-server config never starts failing here.
    pub fn check_mcp_servers(&self) -> Result<(), McpConfigError> {
        let refuse = |reason, message| Err(McpConfigError::new(reason, message));
        if self.mcp.is_none() && !self.mcp_servers.is_empty() {
            return refuse(
                "missing_primary",
                "mcp_servers need the primary server: set mcp before adding more servers"
                    .to_string(),
            );
        }
        let mut seen = std::collections::HashSet::new();
        for server in &self.mcp_servers {
            let field = format!("mcp_servers[{}]", server.id);
            if !valid_id(&server.id) {
                return refuse(
                    "invalid_id",
                    format!(
                        "{field}.id must be 1–64 chars of lowercase letters, digits, or dashes"
                    ),
                );
            }
            if server.id == PRIMARY_MCP_ID {
                return refuse(
                    "reserved_id",
                    format!("{field}.id: {PRIMARY_MCP_ID:?} names the primary server"),
                );
            }
            if !seen.insert(server.id.as_str()) {
                return refuse(
                    "duplicate_id",
                    format!("{field}.id is used by more than one server"),
                );
            }
            if let Some(prefix) = server.tool_prefix.as_deref() {
                if !valid_tool_prefix(prefix) {
                    return refuse(
                        "invalid_prefix",
                        format!(
                            "{field}.tool_prefix must be 1–32 chars of lowercase letters, digits, '_' or '-'"
                        ),
                    );
                }
            }
            if server.allowed_tools.is_empty() {
                return refuse(
                    "empty_allowlist",
                    format!("{field}.allowed_tools must name at least one tool"),
                );
            }
            check_tool_grants(&format!("{field}.tool_grants"), &server.tool_grants)
                .map_err(|message| McpConfigError::new("invalid_grant", message))?;
            if let Some(prefix) = server.tool_prefix.as_deref() {
                let inside = format!("{prefix}.");
                if let Some(entry) = server
                    .tools()
                    .catalogue()
                    .into_iter()
                    .find(|e| !e.starts_with(&inside))
                {
                    return refuse(
                        "outside_prefix",
                        format!(
                            "{field}: {entry:?} can never match; this server's tools are named {prefix}.<tool>"
                        ),
                    );
                }
            }
        }
        let specs = self.mcp_server_specs();
        specs.iter().try_for_each(McpServerSpec::check_connection)?;
        specs
            .iter()
            .enumerate()
            .flat_map(|(i, a)| specs[i + 1..].iter().map(move |b| (a, b)))
            .find_map(|(a, b)| a.overlap_with(b))
            .map_or(Ok(()), |message| refuse("name_collision", message))
    }

    /// Drop every server's discovery credential, for a revision snapshot.
    pub fn strip_credentials(&mut self) {
        self.mcp
            .iter_mut()
            .chain(self.mcp_servers.iter_mut().map(|s| &mut s.connection))
            .for_each(|c| c.discovery_credential = None);
    }

    /// Re-attach `live`'s credentials to the servers that still point where
    /// they did: the primary by URL, each additional server by id and URL.
    pub fn adopt_credentials(&mut self, live: &EmbedSettings) {
        if let Some(mcp) = self.mcp.as_mut() {
            mcp.adopt_credential(live.mcp.as_ref());
        }
        for server in &mut self.mcp_servers {
            server.connection.adopt_credential(
                live.mcp_servers
                    .iter()
                    .find(|l| l.id == server.id)
                    .map(|l| &l.connection),
            );
        }
    }

    /// Whether granted write tiers require an enforced in-chat confirmation.
    pub fn confirm_writes_enabled(&self) -> bool {
        self.policy
            .as_ref()
            .is_some_and(EmbedPolicy::confirm_writes_enabled)
    }
    /// Passive personal-memory capture is on unless explicitly disabled.
    pub fn passive_memory_enabled(&self) -> bool {
        self.passive_memory != Some(false)
    }

    /// Interactive data tables are off unless explicitly enabled.
    pub fn data_tables_enabled(&self) -> bool {
        self.data_tables == Some(true)
    }

    /// Interactive charts are off unless explicitly enabled.
    pub fn charts_enabled(&self) -> bool {
        self.charts == Some(true)
    }

    /// Interactive forms are off unless explicitly enabled.
    pub fn forms_enabled(&self) -> bool {
        self.forms == Some(true)
    }

    /// Choropleth maps are off unless explicitly enabled.
    pub fn maps_enabled(&self) -> bool {
        self.maps == Some(true)
    }

    /// Node-and-arrow diagrams are off unless explicitly enabled.
    pub fn diagrams_enabled(&self) -> bool {
        self.diagrams == Some(true)
    }

    /// The Excalidraw download is off unless explicitly enabled.
    pub fn excalidraw_export_enabled(&self) -> bool {
        self.excalidraw_export == Some(true)
    }

    /// Inline image rendering is off unless explicitly enabled.
    pub fn inline_images_enabled(&self) -> bool {
        self.inline_images == Some(true)
    }

    /// Every tool exempted from spilling for this agent: the shared content
    /// namespaces plus whatever the install named. Built here so the two lists
    /// can never drift apart at the call site.
    pub fn spill_skip_tools(&self) -> Vec<String> {
        let mut skip = workforce_common::session::SpillConfig::default().skip_tools;
        skip.extend(self.spill_skip_tools.iter().cloned());
        skip
    }

    /// Whether the agent may fetch pictures from company storage. Needs BOTH a
    /// renderer ([`Self::images_enabled`]) and somewhere it is allowed to fetch
    /// from: a fetch tool with no renderer stages images nothing can show, and
    /// an allowlist-less one is the SSRF surface with no bound on it.
    pub fn image_fetch_enabled(&self) -> bool {
        self.images_enabled() && !self.image_hosts.is_empty()
    }

    /// The agent showing pictures in its replies is off unless explicitly
    /// enabled. Gates the system-prompt section, the artifact ids the model is
    /// told about, and the widget's renderer together — telling a model about
    /// ids it cannot render with produces fences drawn as code blocks.
    pub fn images_enabled(&self) -> bool {
        self.images == Some(true)
    }

    /// Whether the customer may attach files at all.
    pub fn uploads_enabled(&self) -> bool {
        !self.uploads.is_empty()
    }

    /// Whether this agent accepts a file of `media_type`. The caller passes the
    /// SNIFFED type; a client-declared one is a claim, not evidence.
    pub fn accepts_upload(&self, media_type: &str) -> bool {
        UploadKind::of_media_type(media_type).is_some_and(|k| self.uploads.contains(&k))
    }

    /// The per-file cap in bytes, defaulted and clamped.
    pub fn upload_max_bytes(&self) -> usize {
        let mb = self
            .upload_max_mb
            .unwrap_or(DEFAULT_UPLOAD_MAX_MB)
            .clamp(1, MAX_UPLOAD_MAX_MB);
        mb as usize * 1024 * 1024
    }

    /// The drawing canvas is off unless enabled AND images are accepted — its
    /// output is a PNG, so without that it would be a control that cannot
    /// deliver.
    pub fn drawing_enabled(&self) -> bool {
        self.drawing == Some(true) && self.uploads.contains(&UploadKind::Image)
    }
}

/// A category of file a customer may attach. An allowlist rather than a single
/// boolean because operators genuinely differ — an events company wants images,
/// a finance one wants spreadsheets and PDFs — and one flag would force
/// all-or-nothing.
///
/// A kind is resolved from the file's SNIFFED type, never its extension: an
/// extension is a claim by an untrusted client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UploadKind {
    Image,
    Spreadsheet,
    Document,
}

impl UploadKind {
    /// The media types this kind admits. Deliberately closed: `image/*` would
    /// include SVG, which is a script container.
    pub fn media_types(self) -> &'static [&'static str] {
        match self {
            Self::Image => &["image/png", "image/jpeg", "image/gif", "image/webp"],
            Self::Spreadsheet => &[
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                "text/csv",
            ],
            Self::Document => &[
                "application/pdf",
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
                "text/plain",
                "text/markdown",
            ],
        }
    }

    /// The kind a resolved media type belongs to, or `None` when nothing accepts
    /// it. Parameters (`; charset=…`) are ignored.
    pub fn of_media_type(media_type: &str) -> Option<Self> {
        let base = media_type
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        [Self::Image, Self::Spreadsheet, Self::Document]
            .into_iter()
            .find(|k| k.media_types().contains(&base.as_str()))
    }
}

/// Per-file upload cap when the agent sets none.
pub const DEFAULT_UPLOAD_MAX_MB: u32 = 10;
/// Ceiling on what an operator may configure, so a misconfiguration cannot make
/// the edge buffer arbitrary bytes for an anonymous caller.
pub const MAX_UPLOAD_MAX_MB: u32 = 25;

/// Where a [`ToolGrant`]'s `requires` value is checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantSource {
    /// The user's resolved permission set, fetched per session from
    /// [`EmbedSettings::permissions`] (the company's own RBAC — cannot drift from
    /// what the backend actually enforces). The default and recommended source.
    #[default]
    BackendPermissions,
    /// A realm/client role or group carried directly in the user's Keycloak
    /// token. Simplest, but can drift from the backend's real permissions.
    KeycloakRole,
    /// A tier asserted by the embedding host app over postMessage. A UX hint
    /// only — never a security boundary (the backend still enforces every write).
    HostHint,
}

/// A per-user write tier: `tools` become callable only for a user who holds
/// `requires`, resolved against `source`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolGrant {
    /// Where `requires` is checked (default: the backend permission set).
    #[serde(default)]
    pub source: GrantSource,
    /// The capability that unlocks this tier — a permission string
    /// (e.g. `"items:edit"`), a role, or a host hint, per `source`.
    pub requires: String,
    /// The tools unlocked when `requires` is satisfied.
    #[serde(default)]
    pub tools: Vec<String>,
}

/// How the edge resolves a user's permission set for `backend_permissions` grants.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionsConfig {
    /// Endpoint called once per session with the user's forwarded bearer token.
    /// Must return `{"permissions": ["items:edit", ...], "roles": [...]}`.
    pub url: String,
    /// Per-session cache TTL for the fetched set, in seconds. `None` ⇒ 300.
    #[serde(default)]
    pub cache_ttl_secs: Option<u64>,
}

impl PermissionsConfig {
    /// Cache TTL in seconds (default 300).
    pub fn cache_ttl_secs(&self) -> u64 {
        self.cache_ttl_secs.unwrap_or(300)
    }
}

/// Policy applied to the granted write tiers.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EmbedPolicy {
    /// Require an enforced in-chat confirmation before any tool a grant unlocked
    /// (i.e. any write tier). `None`/`false` ⇒ no confirmation.
    #[serde(default)]
    pub confirm_writes: Option<bool>,
}

impl EmbedPolicy {
    /// Whether writes require an enforced confirmation (off unless explicitly on).
    pub fn confirm_writes_enabled(&self) -> bool {
        self.confirm_writes == Some(true)
    }
}

/// Whether a closed tool allowlist admits `tool`: an entry naming it exactly,
/// or a `prefix.*` glob matching `prefix` itself or anything in its dotted
/// namespace. The one matcher every allowlist in the edge goes through.
pub fn allowlist_admits(entries: &[String], tool: &str) -> bool {
    entries.iter().any(|entry| match entry.strip_suffix(".*") {
        Some(prefix) => {
            tool == prefix
                || tool
                    .strip_prefix(prefix)
                    .is_some_and(|rest| rest.starts_with('.'))
        }
        None => tool == entry,
    })
}

/// Whether a closed allowlist could admit `tool` as the model names it, where
/// `.` and `_` are one character — so a tool's dotted and wire forms answer
/// alike.
pub fn allowlist_admits_on_the_wire(entries: &[String], tool: &str) -> bool {
    let tool = ToolPattern::Exact(workforce_agent::runner::to_llm_name(tool));
    entries
        .iter()
        .any(|entry| ToolPattern::parse(entry).overlaps(&tool))
}

/// The id the primary `[embed.mcp]` server goes by in logs and messages, and
/// which no additional server may take.
pub const PRIMARY_MCP_ID: &str = "default";

/// How long a server that is not required is skipped after its turn start
/// timed out, when it names no window of its own.
pub const DEFAULT_UNAVAILABLE_SKIP_SECS: u64 = 60;

/// How a server's tool calls authenticate as the customer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpAuth {
    /// The turn's `[token_exchange]` token for this server's audience, or the
    /// customer's own token when exchange is off or fails.
    #[default]
    Exchanged,
    /// Always the customer's own token.
    Forwarded,
}

impl McpAuth {
    pub fn is_exchanged(&self) -> bool {
        *self == Self::Exchanged
    }
}

/// A company MCP server's connection: the primary `[embed.mcp]` block, and the
/// connection half of every `[[embed.mcp_servers]]` entry. Every field past the
/// credential is skipped at its default, so a config that sets none of them
/// serialises exactly as before they existed.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct McpSettings {
    /// Base URL of the company MCP server.
    pub url: String,
    /// Optional service-account credential used only to discover the server's
    /// tool list at startup; per-customer calls carry the customer's own token
    /// instead.
    #[serde(default)]
    pub discovery_credential: Option<String>,
    #[serde(default, skip_serializing_if = "McpAuth::is_exchanged")]
    pub auth: McpAuth,
    /// The audience the customer's token is exchanged for, for this server
    /// alone, so a token minted for it is not accepted by another server.
    /// `None` ⇒ `[token_exchange].audience`. Only with `auth = "exchanged"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audience: Option<String>,
    /// Whether this server being unavailable fails startup and the turn
    /// (`true`), or only removes its tools for the turn (`false`). `None` ⇒
    /// `true` for the primary server, `false` for an additional one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
    /// How long a server that is not required is skipped after its turn start
    /// timed out, so a stalled server does not slow every turn. `None` ⇒
    /// [`DEFAULT_UNAVAILABLE_SKIP_SECS`]; `0` ⇒ never skipped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable_skip_secs: Option<u64>,
}

impl McpSettings {
    /// Whether a non-empty discovery credential is configured — what logs and
    /// the admin are told in place of the credential itself.
    pub fn has_discovery_credential(&self) -> bool {
        self.discovery_credential
            .as_deref()
            .is_some_and(|c| !c.is_empty())
    }

    /// Take the live credential when `live` still points where this does, so
    /// a secret never follows a server to another URL.
    fn adopt_credential(&mut self, live: Option<&McpSettings>) {
        self.discovery_credential = live
            .filter(|l| l.url == self.url)
            .and_then(|l| l.discovery_credential.clone());
    }
}

// Manual Debug so the discovery credential never lands in a debug log.
impl std::fmt::Debug for McpSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpSettings")
            .field("url", &self.url)
            .field("has_discovery_credential", &self.has_discovery_credential())
            .field("auth", &self.auth)
            .field("audience", &self.audience)
            .field("required", &self.required)
            .field("unavailable_skip_secs", &self.unavailable_skip_secs)
            .finish()
    }
}

/// An MCP server an agent uses alongside its primary `[embed.mcp]`, with its
/// own allowlist and write tiers. `Debug` goes through [`McpSettings`]'s, so it
/// never prints the credential either.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct McpServerSettings {
    /// Stable id: names the server in logs, the admin API and the sealed
    /// credential. Same grammar as an agent id; `default` is the primary's.
    pub id: String,
    #[serde(flatten)]
    pub connection: McpSettings,
    /// Exposes every tool as `<prefix>.<tool>`, so two servers that both have a
    /// `search` tool can both be used. `tools/call` still sends the raw name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_prefix: Option<String>,
    /// This server's closed allowlist, in the names the agent sees (prefix
    /// included). Must name at least one tool.
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    /// Per-user write tiers on this server, exactly like
    /// [`EmbedSettings::tool_grants`] for the primary.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_grants: Vec<ToolGrant>,
}

impl McpServerSettings {
    pub fn tools(&self) -> ToolSet<'_> {
        ToolSet {
            allowed_tools: &self.allowed_tools,
            tool_grants: &self.tool_grants,
        }
    }
}

/// One server's tools: its base allowlist and the write tiers on top. The one
/// place that knows how a server's tools resolve for a user.
#[derive(Debug, Clone, Copy)]
pub struct ToolSet<'a> {
    pub allowed_tools: &'a [String],
    pub tool_grants: &'a [ToolGrant],
}

impl ToolSet<'_> {
    /// The base allowlist plus the tools of every grant `is_granted` admits,
    /// without duplicates.
    pub fn effective(&self, is_granted: impl Fn(&ToolGrant) -> bool) -> Vec<String> {
        self.tool_grants
            .iter()
            .filter(|g| is_granted(g))
            .flat_map(|g| &g.tools)
            .fold(self.allowed_tools.to_vec(), |mut tools, t| {
                if !tools.contains(t) {
                    tools.push(t.clone());
                }
                tools
            })
    }

    /// Everything this server must DISCOVER: the base plus every tool any grant
    /// could unlock. The per-turn layer narrows it to one user's effective set.
    pub fn catalogue(&self) -> Vec<String> {
        self.effective(|_| true)
    }

    /// Whether `tool` is a write-tier tool here: named by a grant and not
    /// admitted by the base allowlist.
    pub fn grants_tool(&self, tool: &str) -> bool {
        !allowlist_admits(self.allowed_tools, tool)
            && self
                .tool_grants
                .iter()
                .any(|g| g.tools.iter().any(|t| t == tool))
    }
}

/// One MCP server as every consumer sees it, primary and additional alike.
#[derive(Debug, Clone)]
pub struct McpServerSpec<'a> {
    pub id: &'a str,
    pub connection: &'a McpSettings,
    pub tool_prefix: Option<&'a str>,
    pub tools: ToolSet<'a>,
    /// Whether this server being unavailable fails startup and the turn.
    pub required: bool,
    primary: bool,
}

impl<'a> McpServerSpec<'a> {
    fn new(
        id: &'a str,
        connection: &'a McpSettings,
        tool_prefix: Option<&'a str>,
        tools: ToolSet<'a>,
        primary: bool,
    ) -> Self {
        Self {
            id,
            connection,
            tool_prefix,
            tools,
            required: connection.required.unwrap_or(primary),
            primary,
        }
    }

    /// How long this server is skipped after a timed-out turn start.
    pub fn unavailable_skip(&self) -> std::time::Duration {
        std::time::Duration::from_secs(
            self.connection
                .unavailable_skip_secs
                .unwrap_or(DEFAULT_UNAVAILABLE_SKIP_SECS),
        )
    }

    /// The config path this server's fields live under, for error messages.
    fn field(&self) -> String {
        if self.primary {
            "mcp".to_string()
        } else {
            format!("mcp_servers[{}]", self.id)
        }
    }

    fn check_connection(&self) -> Result<(), McpConfigError> {
        let field = self.field();
        match (self.connection.audience.as_deref(), self.connection.auth) {
            (Some(a), _) if a.trim().is_empty() => {
                return Err(McpConfigError::new(
                    "invalid_audience",
                    format!("{field}.audience must not be blank"),
                ))
            }
            (Some(_), McpAuth::Forwarded) => {
                return Err(McpConfigError::new(
                    "invalid_audience",
                    format!(
                        "{field}.audience applies only to auth = \"exchanged\"; a forwarded server gets the customer's own token"
                    ),
                ))
            }
            _ => {}
        }
        if self.required && self.connection.unavailable_skip_secs.is_some() {
            return Err(McpConfigError::new(
                "skip_on_required",
                format!(
                    "{field}.unavailable_skip_secs applies only to a server that is not required; a required server is never skipped"
                ),
            ));
        }
        Ok(())
    }

    /// The first pair of entries by which `other`'s catalogue could collide
    /// with this one's on the wire, as the message naming both.
    fn overlap_with(&self, other: &Self) -> Option<String> {
        let (mine, theirs) = (self.tools.catalogue(), other.tools.catalogue());
        mine.iter()
            .flat_map(|a| theirs.iter().map(move |b| (a, b)))
            .find(|(a, b)| ToolPattern::parse(a).overlaps(&ToolPattern::parse(b)))
            .map(|(a, b)| {
                format!(
                    "{}: {b:?} overlaps {a:?} on server {}; tool names must be distinct across servers (a tool_prefix separates them)",
                    other.field(),
                    self.id
                )
            })
    }
}

/// Why [`EmbedSettings::check_mcp_servers`] refused a config: a stable
/// `reason` to count refusals by, and the message naming the field to fix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpConfigError {
    pub reason: &'static str,
    pub message: String,
}

impl McpConfigError {
    fn new(reason: &'static str, message: String) -> Self {
        Self { reason, message }
    }
}

impl std::fmt::Display for McpConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for McpConfigError {}

/// An allowlist entry in LLM-name space (`.` → `_`), where two servers' tools
/// collide: an exact name, or a `prefix.*` namespace.
enum ToolPattern {
    Exact(String),
    Namespace(String),
}

impl ToolPattern {
    fn parse(entry: &str) -> Self {
        let to_llm = workforce_agent::runner::to_llm_name;
        match entry.strip_suffix(".*") {
            Some(prefix) => Self::Namespace(to_llm(prefix)),
            None => Self::Exact(to_llm(entry)),
        }
    }

    /// Conservative: whether any name could match both. `_` stands in for the
    /// `.` a namespace match needs, because on the wire the two are one.
    fn overlaps(&self, other: &Self) -> bool {
        let within = |name: &str, namespace: &str| {
            name == namespace
                || name
                    .strip_prefix(namespace)
                    .is_some_and(|rest| rest.starts_with('_'))
        };
        match (self, other) {
            (Self::Exact(a), Self::Exact(b)) => a == b,
            (Self::Exact(name), Self::Namespace(ns)) | (Self::Namespace(ns), Self::Exact(name)) => {
                within(name, ns)
            }
            (Self::Namespace(a), Self::Namespace(b)) => within(a, b) || within(b, a),
        }
    }
}

/// A slug-safe id — lowercase letters, digits and dashes, 1..64 chars — for an
/// agent (a session namespace and memory owner scope) or an MCP server.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn valid_tool_prefix(prefix: &str) -> bool {
    !prefix.is_empty()
        && prefix.len() <= 32
        && prefix
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// Shape checks on a list of grants found at `field`: each needs the
/// capability it requires and at least one tool to unlock.
pub fn check_tool_grants(field: &str, grants: &[ToolGrant]) -> Result<(), String> {
    grants.iter().enumerate().try_for_each(|(i, grant)| {
        if grant.requires.trim().is_empty() {
            Err(format!("{field}[{i}] needs the capability it requires"))
        } else if grant.tools.is_empty() {
            Err(format!("{field}[{i}] must unlock at least one tool"))
        } else {
            Ok(())
        }
    })
}

/// Per-install widget theme. Every field is optional; the loader applies safe
/// defaults and validates each value (colours must match a strict hex pattern,
/// `logo_url` must be http(s)/data, enums fall back) before it reaches the page,
/// so a config typo degrades to the default rather than breaking the widget.
/// Admin-controlled (config today, data-plane-editable later) — never customer
/// input — but validated regardless as defence in depth.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Branding {
    /// Header title, and the label of a `pill` launcher.
    #[serde(default)]
    pub title: Option<String>,
    /// A short line under the title in the header.
    #[serde(default)]
    pub subtitle: Option<String>,
    /// Logo shown in the header and bubble launcher. `https://…`, `http://…`, or
    /// a `data:image/…` URI.
    #[serde(default)]
    pub logo_url: Option<String>,
    /// Launcher shape: `bubble` (circular icon), `pill` (permanently labelled), or
    /// `expand` (circular, growing to a labelled pill on hover/focus). Default
    /// `bubble`.
    #[serde(default)]
    pub launcher: Option<String>,
    /// Whether each agent reply is preceded by a small round brand avatar (the
    /// `logo_url` mark on the brand colour or gradient). Default `false`.
    #[serde(default)]
    pub avatar: Option<bool>,
    /// Paint for the agent avatar, independent of the brand gradient. Unset ⇒ it
    /// inherits the shared brand paint, which is what it did before this existed.
    #[serde(default)]
    pub avatar_paint: Option<SurfacePaint>,
    /// Paint for the send button, independent of both the brand gradient and the
    /// avatar. Unset ⇒ inherits the shared brand paint.
    #[serde(default)]
    pub send_paint: Option<SurfacePaint>,
    /// Framed presentation: the brand paint fills the panel and the conversation
    /// is inset inside it, so the gradient runs behind the header and around the
    /// body as a frame. Default `false` — the panel keeps its flat fill and
    /// hairline border. Independent of `gradient`: a brand may want a gradient
    /// without the frame, or a framed panel in a flat colour.
    #[serde(default)]
    pub frame: Option<bool>,
    /// Thickness in px of the brand paint showing around the inset conversation,
    /// when `frame` is on. Default 2. Clamped to a range that leaves the panel
    /// usable; 0 sits the body flush and leaves only the header painted.
    #[serde(default)]
    pub frame_width: Option<u32>,
    /// Send button shape: `rounded` (default) or `circle`.
    #[serde(default)]
    pub send_shape: Option<String>,
    /// Send button glyph: `arrow` (default) or `plane` (a paper plane).
    #[serde(default)]
    pub send_icon: Option<String>,
    /// How `logo_url` is fitted wherever it renders: `cover` (default — crops to a
    /// rounded tile, right for a square logo) or `contain` (no crop, no rounding,
    /// right for a bare mark on transparency that sits ON the brand colour).
    #[serde(default)]
    pub logo_fit: Option<String>,
    /// The brand gradient painted on the launcher, header, send button, avatar and
    /// panel ring. Absent ⇒ those surfaces stay the flat `primary_color`.
    #[serde(default)]
    pub gradient: Option<Gradient>,
    /// Primary colour — header background and accents (hex).
    #[serde(default)]
    pub primary_color: Option<String>,
    /// Text/icon colour on the primary colour (hex).
    #[serde(default)]
    pub primary_text: Option<String>,
    /// Customer message bubble background (hex).
    #[serde(default)]
    pub user_bubble: Option<String>,
    /// Customer message bubble text (hex).
    #[serde(default)]
    pub user_bubble_text: Option<String>,
    /// Agent message bubble background (hex).
    #[serde(default)]
    pub agent_bubble: Option<String>,
    /// Agent message bubble text (hex).
    #[serde(default)]
    pub agent_bubble_text: Option<String>,
    /// Thread background behind the messages (hex).
    #[serde(default)]
    pub surface: Option<String>,
    /// Card/panel background — header-less body, composer, inputs (hex).
    #[serde(default)]
    pub panel: Option<String>,
    /// Background of the message box the customer types in (hex). Defaults to
    /// `panel`, which is where it came from before this existed, so unset is a
    /// no-op. Scoped to the composer — the thread's own inline inputs (a feedback
    /// comment, a table filter) sit on `surface` and keep following it.
    #[serde(default)]
    pub input_bg: Option<String>,
    /// Hairline border colour (hex).
    #[serde(default)]
    pub border: Option<String>,
    /// Border around the composer's message box (hex). Defaults to `border`, which
    /// is what it used before this existed.
    #[serde(default)]
    pub input_border: Option<String>,
    /// Border around an agent answer bubble (hex). Defaults to `border`.
    #[serde(default)]
    pub agent_bubble_border: Option<String>,
    /// Secondary/muted text colour (hex).
    #[serde(default)]
    pub muted: Option<String>,
    /// Primary body-text colour on neutral surfaces (hex).
    #[serde(default)]
    pub text: Option<String>,
    /// CSS `font-family` stack for the widget.
    #[serde(default)]
    pub font_family: Option<String>,
    /// Optional stylesheet URL (https) that loads the web font named in
    /// `font_family` — e.g. a Google Fonts `css2?family=…` link, or a customer's
    /// own font CDN. The widget injects it as a `<link rel="stylesheet">`.
    #[serde(default)]
    pub font_url: Option<String>,
    /// Whether the docked chat panel casts a drop shadow. Default `true`.
    #[serde(default)]
    pub shadow: Option<bool>,
    /// Full-screen content layout: `centered` (a readable ~860px column, the
    /// default) or `wide` (use most of the width, so wide tables fit).
    #[serde(default)]
    pub fullscreen: Option<String>,
    /// Corner radius for the panel and launcher, e.g. `12px`.
    #[serde(default)]
    pub radius: Option<String>,
    /// Panel size in px when it opens — before the customer resizes it. Default
    /// 400 × 660. Clamped to a usable range on resolution, and the loader caps it
    /// to the host viewport, so a large value simply means "as big as it fits".
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
    /// The smallest the customer may drag the panel. Default 320 × 420. Clamped to
    /// the same usable floor, and never allowed to exceed `width`/`height` — a
    /// minimum above the opening size would mean opening below the minimum.
    #[serde(default)]
    pub min_width: Option<u32>,
    #[serde(default)]
    pub min_height: Option<u32>,
    /// Viewport side the widget docks to: `right` (default) or `left`.
    #[serde(default)]
    pub position: Option<String>,
    /// Whether the customer may drag the widget to another viewport corner, for
    /// hosts where no single corner is clear of the page's own controls. `position`
    /// stays the starting point; the customer's choice lasts their tab session.
    /// Default `false`.
    #[serde(default)]
    pub draggable: Option<bool>,
    /// Inset in px of the docked position from its corner, horizontal then
    /// vertical, before the customer moves it — so the launcher starts clear of a
    /// host control strip (a sticky Save bar) that a flush corner would cover.
    /// Default 0. Capped on resolution; the loader also clamps to the viewport.
    #[serde(default)]
    pub offset_x: Option<u32>,
    #[serde(default)]
    pub offset_y: Option<u32>,
    /// Colour scheme: `light` (default), `dark`, or `auto` — follow the host
    /// page's own light/dark theme (and the OS as a fallback). `light`/`dark` pin
    /// the widget; only `auto` inspects the host page.
    #[serde(default)]
    pub theme_mode: Option<String>,
    /// The dark-mode palette (`[embed.branding.dark]`), used when the resolved mode
    /// is `dark`. Any colour left unset falls back to the light brand colour (for
    /// brand tokens) or a curated generic-dark default (for neutrals), so a partial
    /// or absent block still renders a sane dark theme.
    #[serde(default)]
    pub dark: Option<DarkPalette>,
    /// Back-compat alias for `primary_color`.
    #[serde(default)]
    pub accent_color: Option<String>,
}

/// A brand gradient, authored as discrete stops rather than a raw CSS string so a
/// config typo can only restyle the widget, never smuggle a `url()` / `image-set()`
/// into a paint. The server composes the `linear-gradient(…)` from these after
/// validating every part; anything invalid drops the whole gradient back to the flat
/// `primary_color`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Gradient {
    /// First stop (hex). Required by the shorthand — without it (and without
    /// `stops`) there is no gradient.
    #[serde(default)]
    pub from: Option<String>,
    /// Optional middle stop (hex), placed at 50%.
    #[serde(default)]
    pub via: Option<String>,
    /// Last stop (hex). Required by the shorthand.
    #[serde(default)]
    pub to: Option<String>,
    /// Direction: a `to <side>` / `to <corner>` keyword or `<n>deg`. Default
    /// `to right`.
    #[serde(default)]
    pub angle: Option<String>,
    /// Explicit stops with positions, for finer control than the evenly-spaced
    /// `from`/`via`/`to` shorthand. Takes precedence when it yields at least two
    /// usable stops; otherwise the shorthand applies, so existing configs and the
    /// admin's slider can coexist.
    #[serde(default)]
    pub stops: Option<Vec<GradientStop>>,
}

/// A per-surface paint override: either a flat `color` or its own `gradient`. Used to
/// take one small brand surface off the shared paint — e.g. a solid avatar while the
/// launcher and header keep the gradient, or a different gradient on the send button.
/// A `gradient` that resolves wins over `color`; neither ⇒ the surface inherits.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SurfacePaint {
    /// Flat hex colour.
    #[serde(default)]
    pub color: Option<String>,
    /// This surface's own gradient, validated exactly like the brand one.
    #[serde(default)]
    pub gradient: Option<Gradient>,
}

/// One gradient stop: a colour and where it sits along the axis.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GradientStop {
    /// Hex colour. A stop without a valid one is dropped.
    #[serde(default)]
    pub color: Option<String>,
    /// Position along the gradient, 0–100 (%). Clamped; defaults to spreading the
    /// stop evenly among its siblings when absent.
    #[serde(default)]
    pub at: Option<f32>,
}

/// The dark-mode colour overrides. Every field is an optional hex colour, validated
/// on resolution exactly like the light palette; anything absent or invalid falls
/// back per [`crate::theme::resolve_theme`]. Structure (logo, font, radius, launcher,
/// position, …) is shared with the light theme and not repeated here.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DarkPalette {
    #[serde(default)]
    pub primary_color: Option<String>,
    #[serde(default)]
    pub primary_text: Option<String>,
    #[serde(default)]
    pub user_bubble: Option<String>,
    #[serde(default)]
    pub user_bubble_text: Option<String>,
    #[serde(default)]
    pub agent_bubble: Option<String>,
    #[serde(default)]
    pub agent_bubble_text: Option<String>,
    #[serde(default)]
    pub surface: Option<String>,
    #[serde(default)]
    pub panel: Option<String>,
    /// Dark composer background. Unset ⇒ the dark `panel`.
    #[serde(default)]
    pub input_bg: Option<String>,
    /// Dark borders for the composer and an agent bubble. Unset ⇒ the dark `border`.
    #[serde(default)]
    pub input_border: Option<String>,
    #[serde(default)]
    pub agent_bubble_border: Option<String>,
    #[serde(default)]
    pub border: Option<String>,
    #[serde(default)]
    pub muted: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    /// Dark-mode brand gradient. Unset ⇒ the light gradient is reused, like every
    /// other brand token.
    #[serde(default)]
    pub gradient: Option<Gradient>,
    /// Dark paint for the avatar / send button. Unset ⇒ the light override is reused
    /// (and if that is unset too, the surface inherits the brand paint), the same
    /// fallback every other brand token follows.
    #[serde(default)]
    pub avatar_paint: Option<SurfacePaint>,
    #[serde(default)]
    pub send_paint: Option<SurfacePaint>,
}

impl EmbedSettings {
    /// Whether `origin` (an HTTP `Origin` header value) may load this agent.
    pub fn origin_allowed(&self, origin: &str) -> bool {
        self.allowed_origins.iter().any(|allowed| allowed == origin)
    }

    /// Whether `tool` survives the closed allowlist (see [`allowlist_admits`]).
    pub fn tool_allowed(&self, tool: &str) -> bool {
        allowlist_admits(&self.allowed_tools, tool)
    }

    /// Whether a verified token's holder passes the email allowlist. With no
    /// allowlist, everyone from the realm passes. With one, the token must
    /// carry an `email` matching an entry (ASCII case-insensitive) that the
    /// IdP does not mark unverified — `email_verified: false` is rejected so
    /// nobody can pass by registering an account that merely *claims* an
    /// allowlisted address.
    pub fn email_allowed(&self, email: Option<&str>, email_verified: Option<bool>) -> bool {
        if self.allowed_emails.is_empty() {
            return true;
        }
        if email_verified == Some(false) {
            return false;
        }
        email.is_some_and(|e| {
            self.allowed_emails
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(e))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn embed(origins: &[&str], tools: &[&str]) -> EmbedSettings {
        EmbedSettings {
            allowed_origins: origins.iter().map(|s| s.to_string()).collect(),
            allowed_tools: tools.iter().map(|s| s.to_string()).collect(),
            keycloak_issuer: "https://kc/realms/x".into(),
            mcp: Some(McpSettings {
                url: "http://mcp".into(),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn grant(requires: &str, tools: &[&str]) -> ToolGrant {
        ToolGrant {
            source: GrantSource::BackendPermissions,
            requires: requires.into(),
            tools: tools.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn effective_tools_adds_only_satisfied_grants() {
        let mut e = embed(&[], &["read_a", "read_b"]);
        e.tool_grants = vec![
            grant("items:edit", &["write_item"]),
            grant("pickups:edit", &["write_pickup"]),
        ];
        // The user holds only items:edit.
        let held: std::collections::HashSet<&str> = ["items:edit"].into_iter().collect();
        let tools = e.effective_allowed_tools(|g| held.contains(g.requires.as_str()));
        assert!(tools.contains(&"read_a".to_string()));
        assert!(tools.contains(&"write_item".to_string()));
        assert!(!tools.contains(&"write_pickup".to_string()));
    }

    #[test]
    fn effective_tools_is_base_only_with_no_grants_or_when_none_satisfied() {
        let e = embed(&[], &["read_a"]);
        assert_eq!(
            e.effective_allowed_tools(|_| true),
            vec!["read_a".to_string()]
        );

        let mut e2 = embed(&[], &["read_a"]);
        e2.tool_grants = vec![grant("items:edit", &["write_item"])];
        // Nobody satisfies the grant ⇒ base only, no duplicates.
        assert_eq!(
            e2.effective_allowed_tools(|_| false),
            vec!["read_a".to_string()]
        );
    }

    #[test]
    fn is_granted_tool_distinguishes_write_tier_from_base() {
        let mut e = embed(&[], &["read_a"]);
        e.tool_grants = vec![grant("items:edit", &["write_item"])];
        assert!(e.is_granted_tool("write_item"));
        assert!(!e.is_granted_tool("read_a"));
        assert!(!e.is_granted_tool("unknown"));
    }

    #[test]
    fn the_primary_catalogue_is_the_union_of_base_and_all_grants() {
        let mut e = embed(&[], &["read_a"]);
        e.tool_grants = vec![
            grant("items:edit", &["write_item"]),
            grant("pickups:edit", &["write_pickup"]),
        ];
        let d = e.mcp_server_specs()[0].tools.catalogue();
        assert!(d.contains(&"read_a".to_string()));
        assert!(d.contains(&"write_item".to_string()));
        assert!(d.contains(&"write_pickup".to_string()));
    }

    #[test]
    fn confirm_writes_defaults_off_and_opts_in() {
        let mut e = embed(&[], &[]);
        assert!(!e.confirm_writes_enabled());
        e.policy = Some(EmbedPolicy {
            confirm_writes: Some(true),
        });
        assert!(e.confirm_writes_enabled());
    }

    #[test]
    fn grant_source_defaults_to_backend_permissions_and_parses_snake_case() {
        // default when omitted
        let g: ToolGrant = toml::from_str(r#"requires = "items:edit""#).unwrap();
        assert_eq!(g.source, GrantSource::BackendPermissions);
        // explicit snake_case values
        let g2: ToolGrant = toml::from_str(
            r#"source = "keycloak_role"
requires = "admin""#,
        )
        .unwrap();
        assert_eq!(g2.source, GrantSource::KeycloakRole);
    }

    #[test]
    fn rich_output_flags_default_off_and_require_explicit_opt_in() {
        let mut e = embed(&[], &[]);
        assert!(!e.data_tables_enabled());
        assert!(!e.charts_enabled());
        assert!(!e.forms_enabled());
        assert!(!e.maps_enabled());
        assert!(!e.diagrams_enabled());
        assert!(!e.excalidraw_export_enabled());
        e.data_tables = Some(false);
        e.charts = Some(false);
        e.forms = Some(false);
        e.maps = Some(false);
        e.diagrams = Some(false);
        e.excalidraw_export = Some(false);
        assert!(!e.data_tables_enabled());
        assert!(!e.charts_enabled());
        assert!(!e.forms_enabled());
        assert!(!e.maps_enabled());
        assert!(!e.diagrams_enabled());
        assert!(!e.excalidraw_export_enabled());
        e.data_tables = Some(true);
        e.charts = Some(true);
        e.forms = Some(true);
        e.maps = Some(true);
        e.diagrams = Some(true);
        e.excalidraw_export = Some(true);
        assert!(e.data_tables_enabled());
        assert!(e.charts_enabled());
        assert!(e.forms_enabled());
        assert!(e.maps_enabled());
        assert!(e.diagrams_enabled());
        assert!(e.excalidraw_export_enabled());
    }

    #[test]
    fn rich_output_flags_survive_serde_and_default_when_absent() {
        // Stored agents predating the flags (DB JSONB / git TOML) deserialize
        // with all off; explicit values round-trip.
        let legacy: EmbedSettings =
            serde_json::from_str(r#"{"keycloak_issuer":"https://kc/realms/x"}"#).unwrap();
        assert_eq!(legacy.data_tables, None);
        assert_eq!(legacy.charts, None);
        assert_eq!(legacy.forms, None);
        assert_eq!(legacy.maps, None);
        assert_eq!(legacy.diagrams, None);
        assert_eq!(legacy.excalidraw_export, None);

        let toml: EmbedSettings = toml::from_str(
            "keycloak_issuer = \"https://kc/realms/x\"\ndata_tables = true\ncharts = false\nforms = true\nmaps = true\ndiagrams = true\nexcalidraw_export = true\n",
        )
        .unwrap();
        assert_eq!(toml.data_tables, Some(true));
        assert_eq!(toml.charts, Some(false));
        assert_eq!(toml.forms, Some(true));
        assert_eq!(toml.maps, Some(true));
        assert_eq!(toml.diagrams, Some(true));
        assert_eq!(toml.excalidraw_export, Some(true));

        let mut e = embed(&[], &[]);
        e.data_tables = Some(true);
        e.forms = Some(true);
        let json = serde_json::to_string(&e).unwrap();
        let back: EmbedSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.data_tables, Some(true));
        assert_eq!(back.charts, None);
        assert_eq!(back.forms, Some(true));
        assert_eq!(back.maps, None);
        assert_eq!(back.diagrams, None);
        assert_eq!(back.excalidraw_export, None);
    }

    #[test]
    fn assembled_prompt_falls_back_to_legacy_then_uses_identity_files() {
        // No identity files ⇒ the legacy single prompt is used verbatim.
        let mut a = EdgeAgent {
            system_prompt: "legacy persona".into(),
            ..Default::default()
        };
        assert_eq!(a.assembled_prompt(), "legacy persona");

        // Any identity file present ⇒ assemble from the files, section-headered,
        // and the legacy prompt is ignored (blank files are skipped).
        a.identity = IdentityFiles {
            soul: "kind and concise".into(),
            identity: "the support bot".into(),
            agents: "  ".into(), // blank → skipped
            tools: String::new(),
        };
        let p = a.assembled_prompt();
        assert_eq!(
            p,
            "## Your Identity\nkind and concise\n\n## Who You Are\nthe support bot"
        );
        assert!(!p.contains("legacy persona"), "identity files win");
        assert!(!p.contains("Operational Guidelines"), "blank file skipped");
    }

    #[test]
    fn origin_match_is_exact() {
        let e = embed(&["https://app.company.com"], &[]);
        assert!(e.origin_allowed("https://app.company.com"));
        assert!(!e.origin_allowed("https://app.company.com:443"));
        assert!(!e.origin_allowed("https://evil.com"));
        assert!(!e.origin_allowed("http://app.company.com"));
    }

    #[test]
    fn tool_allowlist_exact_and_glob() {
        let e = embed(&[], &["harbor.*", "ping"]);
        assert!(e.tool_allowed("harbor.list_orders"));
        assert!(e.tool_allowed("harbor"));
        assert!(e.tool_allowed("ping"));
        assert!(!e.tool_allowed("harborx"));
        assert!(!e.tool_allowed("other.tool"));
        assert!(!e.tool_allowed("pingx"));
    }

    #[test]
    fn allowlist_admits_on_the_wire_treats_dot_and_underscore_alike() {
        let entries = ["harbor.*".to_string(), "ping.pong".to_string()];
        for tool in [
            "harbor.list_orders",
            "harbor_list_orders",
            "harbor",
            "ping_pong",
        ] {
            assert!(allowlist_admits_on_the_wire(&entries, tool), "{tool}");
        }
        for tool in ["harborx", "harborx_list", "ping", "other.tool"] {
            assert!(!allowlist_admits_on_the_wire(&entries, tool), "{tool}");
        }
    }

    #[test]
    fn allowlist_admits_exact_and_glob() {
        let entries = ["harbor.*".to_string(), "ping".to_string()];
        assert!(allowlist_admits(&entries, "harbor.list_orders"));
        assert!(allowlist_admits(&entries, "harbor.reports.daily"));
        assert!(allowlist_admits(&entries, "harbor"));
        assert!(allowlist_admits(&entries, "ping"));
        assert!(!allowlist_admits(&entries, "harborx"));
        assert!(!allowlist_admits(&entries, "harbor_list_orders"));
        assert!(!allowlist_admits(&entries, "other.tool"));
        assert!(!allowlist_admits(&entries, "pingx"));
        assert!(!allowlist_admits(&[], "ping"));
    }

    #[test]
    fn mcp_settings_debug_never_prints_the_credential() {
        let mcp = McpSettings {
            url: "https://mcp.example.test".into(),
            discovery_credential: Some("svc-secret-7".into()),
            ..Default::default()
        };
        let printed = format!("{mcp:?}");
        assert!(!printed.contains("svc-secret-7"), "{printed}");
        assert!(
            printed.contains("has_discovery_credential: true"),
            "{printed}"
        );
    }

    #[test]
    fn empty_allowlist_permits_no_tools() {
        let e = embed(&[], &[]);
        assert!(!e.tool_allowed("anything"));
    }

    #[test]
    fn empty_email_allowlist_admits_everyone() {
        let e = embed(&[], &[]);
        assert!(e.email_allowed(Some("anyone@example.com"), Some(true)));
        assert!(e.email_allowed(None, None), "no claim needed when unset");
    }

    #[test]
    fn email_allowlist_admits_only_verified_matches() {
        let mut e = embed(&[], &[]);
        e.allowed_emails = vec!["me@example.com".into()];
        // Match, case-insensitive; verified or verification-unstated pass.
        assert!(e.email_allowed(Some("me@example.com"), Some(true)));
        assert!(e.email_allowed(Some("Me@Example.COM"), None));
        // Explicitly-unverified email is rejected even when it matches —
        // otherwise registering an account that merely claims the address
        // would pass.
        assert!(!e.email_allowed(Some("me@example.com"), Some(false)));
        // Non-members and tokens with no email claim fail closed.
        assert!(!e.email_allowed(Some("other@example.com"), Some(true)));
        assert!(!e.email_allowed(None, Some(true)));
    }

    /// `image.fetch` needs BOTH halves: a renderer to show what it stages, and
    /// an allowlist to bound where it may connect. Either alone is wrong — a
    /// fetcher with no renderer stages images nothing can display, and an
    /// allowlist-less one is an unbounded SSRF surface.
    #[test]
    fn fetching_images_needs_a_renderer_and_an_allowlist() {
        let mut e = EmbedSettings::default();
        assert!(!e.image_fetch_enabled(), "off by default");

        e.images = Some(true);
        assert!(!e.image_fetch_enabled(), "a renderer alone is not enough");

        e.images = None;
        e.image_hosts = vec!["media.example.com".into()];
        assert!(!e.image_fetch_enabled(), "an allowlist alone is not enough");

        e.images = Some(true);
        assert!(e.image_fetch_enabled());
    }
}

#[cfg(test)]
mod image_config_tests {
    use super::*;

    /// The contoso agent definition lives in a git-backed TOML, so the field
    /// names here ARE the config contract. A rename that compiles would silently
    /// leave `images`/`image_hosts` at their defaults and the feature simply
    /// would not switch on.
    #[test]
    fn embed_toml_carries_the_image_keys() {
        let embed: EmbedSettings = toml::from_str(
            r#"
            keycloak_issuer = "https://kc"
            allowed_tools = ["staff_item_media_urls"]
            images = true
            image_hosts = ["storage.googleapis.com", ".storage.googleapis.com"]
            spill_skip_tools = ["staff_item_media_urls"]
            "#,
        )
        .expect("embed parses");
        assert!(embed.images_enabled());
        assert_eq!(embed.image_hosts.len(), 2);
        assert!(embed.image_fetch_enabled());
        assert!(embed
            .allowed_tools
            .contains(&"staff_item_media_urls".to_string()));
        assert!(embed
            .spill_skip_tools()
            .contains(&"staff_item_media_urls".to_string()));
    }

    /// The edge spills any tool result over 8 KB to an artifact handle. That is
    /// right for record sets and WRONG for a result the model must copy values
    /// out of — a handful of signed URLs crosses the threshold, and a handle
    /// cannot be turned back into URLs, so the model simply cannot do what it
    /// was asked. Each install names its own such tools; the name used to be
    /// hardcoded in the edge, which quietly made one customer's tool a property
    /// of the shared code.
    #[test]
    fn an_installs_own_tools_join_the_shared_spill_exemptions() {
        let embed: EmbedSettings = toml::from_str(
            r#"
            keycloak_issuer = "https://kc"
            spill_skip_tools = ["staff_item_media_urls", "acme_share_links"]
            "#,
        )
        .unwrap();
        let skip = embed.spill_skip_tools();
        assert!(skip.contains(&"staff_item_media_urls".to_string()));
        assert!(skip.contains(&"acme_share_links".to_string()));
        // …WITHOUT dropping the shared content namespaces, which is the bug an
        // install would hit if naming its own tools replaced the list.
        for shared in ["memory", "knowledge", "tool.retrieve"] {
            assert!(skip.contains(&shared.to_string()), "lost {shared}");
        }
    }

    /// An install that names none still gets the shared exemptions unchanged.
    #[test]
    fn naming_no_tools_leaves_the_shared_exemptions_intact() {
        let embed: EmbedSettings = toml::from_str(r#"keycloak_issuer = "https://kc""#).unwrap();
        assert_eq!(
            embed.spill_skip_tools(),
            workforce_common::session::SpillConfig::default().skip_tools
        );
    }

    /// An install that says nothing about images gets neither the renderer nor
    /// the fetcher — the whole feature is opt-in at both halves.
    #[test]
    fn an_embed_without_the_keys_stays_off() {
        let embed: EmbedSettings = toml::from_str(r#"keycloak_issuer = "https://kc""#).unwrap();
        assert!(!embed.images_enabled());
        assert!(!embed.image_fetch_enabled());
        assert!(embed.image_hosts.is_empty());
    }
}

#[cfg(test)]
mod mcp_server_tests {
    use super::*;

    fn names(tools: &[&str]) -> Vec<String> {
        tools.iter().map(|t| t.to_string()).collect()
    }

    fn primary(tools: &[&str]) -> EmbedSettings {
        EmbedSettings {
            keycloak_issuer: "https://kc.example.test/realms/x".into(),
            allowed_tools: names(tools),
            mcp: Some(McpSettings {
                url: "https://mcp.harbor.example".into(),
                discovery_credential: Some("primary-secret".into()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn server(id: &str, prefix: Option<&str>, tools: &[&str]) -> McpServerSettings {
        McpServerSettings {
            id: id.into(),
            connection: McpSettings {
                url: format!("https://{id}-mcp.harbor.example/mcp"),
                discovery_credential: Some(format!("{id}-secret")),
                ..Default::default()
            },
            tool_prefix: prefix.map(str::to_string),
            allowed_tools: names(tools),
            tool_grants: vec![],
        }
    }

    fn grant(requires: &str, tools: &[&str]) -> ToolGrant {
        ToolGrant {
            source: GrantSource::BackendPermissions,
            requires: requires.into(),
            tools: names(tools),
        }
    }

    fn refused(embed: &EmbedSettings) -> String {
        embed
            .check_mcp_servers()
            .expect_err("must be refused")
            .message
    }

    fn reason(embed: &EmbedSettings) -> &'static str {
        embed
            .check_mcp_servers()
            .expect_err("must be refused")
            .reason
    }

    /// The serialised form of a config that names none of the new keys is
    /// exactly what it was before they existed — stored rows, revisions and
    /// admin JSON all compare equal.
    #[test]
    fn an_embed_without_additional_servers_serialises_as_before() {
        let legacy = primary(&["harbor.*"]);
        let json = serde_json::to_value(&legacy).unwrap();
        assert!(json.get("mcp_servers").is_none(), "{json}");
        assert_eq!(
            json["mcp"],
            serde_json::json!({"url": "https://mcp.harbor.example", "discovery_credential": "primary-secret"})
        );
        let toml_text = toml::to_string(&legacy).unwrap();
        assert!(!toml_text.contains("mcp_servers"), "{toml_text}");
        let back: EmbedSettings = toml::from_str(&toml_text).unwrap();
        assert_eq!(serde_json::to_value(&back).unwrap(), json);
        let reparsed: EmbedSettings = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(serde_json::to_value(&reparsed).unwrap(), json);
    }

    #[test]
    fn additional_servers_parse_with_defaults() {
        let embed: EmbedSettings = toml::from_str(
            r#"
            keycloak_issuer = "https://kc.example.test/realms/x"
            allowed_tools = ["harbor.*"]

            [mcp]
            url = "https://mcp.harbor.example"

            [[mcp_servers]]
            id = "docs"
            url = "https://docs-mcp.harbor.example/mcp"
            tool_prefix = "docs"
            allowed_tools = ["docs.*"]
            audience = "docs-mcp"
            unavailable_skip_secs = 15
            tool_grants = [{ requires = "docs:edit", tools = ["docs.edit_page"] }]

            [[mcp_servers]]
            id = "billing"
            url = "https://billing-mcp.harbor.example/mcp"
            auth = "forwarded"
            required = true
            discovery_credential = "billing-secret"
            allowed_tools = ["billing.list_invoices"]
            "#,
        )
        .unwrap();
        let [docs, billing] = embed.mcp_servers.as_slice() else {
            panic!("two servers: {:?}", embed.mcp_servers);
        };
        assert_eq!(docs.connection.auth, McpAuth::Exchanged);
        assert_eq!(docs.connection.audience.as_deref(), Some("docs-mcp"));
        assert_eq!(docs.connection.unavailable_skip_secs, Some(15));
        assert_eq!(docs.tool_grants[0].tools, names(&["docs.edit_page"]));
        assert_eq!(billing.connection.auth, McpAuth::Forwarded);
        assert_eq!(billing.connection.required, Some(true));
        assert_eq!(
            billing.connection.discovery_credential.as_deref(),
            Some("billing-secret")
        );
        assert!(embed.check_mcp_servers().is_ok());

        let json = serde_json::to_value(&embed).unwrap();
        assert_eq!(
            json["mcp_servers"][1]["discovery_credential"], "billing-secret",
            "the credential sits flat in the server, where sealing looks"
        );
        let back: EmbedSettings = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(serde_json::to_value(&back).unwrap(), json);

        let bad = toml::from_str::<EmbedSettings>(
            r#"
            keycloak_issuer = "k"
            [[mcp_servers]]
            id = "docs"
            url = "https://docs.example.test"
            auth = "bogus"
            "#,
        );
        assert!(bad.is_err(), "an unknown auth mode is a parse error");
    }

    #[test]
    fn server_specs_put_the_primary_first_with_its_grant_tools() {
        let mut embed = primary(&["harbor.list_orders"]);
        embed.tool_grants = vec![grant("orders:edit", &["harbor.cancel_order"])];
        embed.mcp_servers = vec![
            server("docs", Some("docs"), &["docs.*"]),
            server("billing", None, &["billing.list_invoices"]),
        ];
        let specs = embed.mcp_server_specs();
        let ids: Vec<&str> = specs.iter().map(|s| s.id).collect();
        assert_eq!(ids, [PRIMARY_MCP_ID, "docs", "billing"]);
        assert_eq!(
            specs[0].tools.catalogue(),
            names(&["harbor.list_orders", "harbor.cancel_order"])
        );
        assert_eq!(specs[1].tools.catalogue(), names(&["docs.*"]));
        assert_eq!(
            specs.iter().map(|s| s.required).collect::<Vec<_>>(),
            [true, false, false]
        );
        assert_eq!(specs[0].connection.auth, McpAuth::Exchanged);
        assert_eq!(specs[1].tool_prefix, Some("docs"));
        assert_eq!(
            specs[1].unavailable_skip(),
            std::time::Duration::from_secs(DEFAULT_UNAVAILABLE_SKIP_SECS)
        );

        embed.mcp.as_mut().unwrap().required = Some(false);
        embed.mcp_servers[1].connection.required = Some(true);
        let specs = embed.mcp_server_specs();
        assert_eq!(
            specs.iter().map(|s| s.required).collect::<Vec<_>>(),
            [false, false, true],
            "required is chosen per server"
        );
    }

    #[test]
    fn tool_patterns_overlap_in_llm_name_space() {
        let cases = [
            ("crm.list", "crm.list", true),
            ("crm.list", "crm.get", false),
            ("crm.list", "crm_list", true),
            ("crm.*", "crm.list", true),
            ("crm.*", "crm", true),
            ("crm.*", "crm_x.y", true),
            ("crm.*", "crm.reports.*", true),
            ("cr.*", "crm.*", false),
            ("crm.*", "crmx.*", false),
            ("crm.*", "crmx", false),
        ];
        for (a, b, overlap) in cases {
            let (pa, pb) = (ToolPattern::parse(a), ToolPattern::parse(b));
            assert_eq!(pa.overlaps(&pb), overlap, "{a} vs {b}");
            assert_eq!(pb.overlaps(&pa), overlap, "{b} vs {a} (symmetry)");
        }
    }

    #[test]
    fn check_mcp_servers_refuses_each_misconfiguration() {
        let with = |servers: Vec<McpServerSettings>| EmbedSettings {
            mcp_servers: servers,
            ..primary(&["harbor.*"])
        };
        let docs = || server("docs", Some("docs"), &["docs.*"]);

        let mut orphan = with(vec![docs()]);
        orphan.mcp = None;
        assert!(
            refused(&orphan).contains("need the primary"),
            "{}",
            refused(&orphan)
        );

        let dup = with(vec![docs(), server("docs", Some("pages"), &["pages.*"])]);
        assert!(refused(&dup).contains("mcp_servers[docs].id is used by more than one"));

        let reserved = with(vec![server(PRIMARY_MCP_ID, None, &["other.*"])]);
        assert!(refused(&reserved).contains("names the primary server"));

        let bad_id = with(vec![server("Docs Server", None, &["docs.*"])]);
        assert!(refused(&bad_id).contains("mcp_servers[Docs Server].id must be"));

        for prefix in ["a.b", "", &"p".repeat(33)] {
            let e = with(vec![server("docs", Some(prefix), &["docs.*"])]);
            assert!(
                refused(&e).contains("mcp_servers[docs].tool_prefix"),
                "{prefix:?}"
            );
        }

        let empty = with(vec![server("docs", None, &[])]);
        assert!(refused(&empty).contains("mcp_servers[docs].allowed_tools must name"));

        let outside = with(vec![server("docs", Some("docs"), &["search"])]);
        let e = refused(&outside);
        assert!(
            e.contains(r#"mcp_servers[docs]: "search" can never match"#)
                && e.contains("docs.<tool>"),
            "{e}"
        );

        let mut grant_outside = docs();
        grant_outside.tool_grants = vec![grant("docs:edit", &["edit_page"])];
        assert!(refused(&with(vec![grant_outside])).contains(r#""edit_page" can never match"#));

        let mut blank_grant = docs();
        blank_grant.tool_grants = vec![grant(" ", &["docs.edit_page"])];
        assert!(refused(&with(vec![blank_grant]))
            .contains("mcp_servers[docs].tool_grants[0] needs the capability"));

        let base_overlap = with(vec![server("crm", None, &["harbor_list_orders"])]);
        let e = refused(&base_overlap);
        assert!(
            e.contains(
                r#"mcp_servers[crm]: "harbor_list_orders" overlaps "harbor.*" on server default"#
            ),
            "{e}"
        );

        let mut granted = primary(&["harbor.list_orders"]);
        granted.tool_grants = vec![grant("orders:edit", &["harbor.cancel_order"])];
        granted.mcp_servers = vec![server("crm", None, &["harbor_cancel_order"])];
        assert!(refused(&granted).contains(r#""harbor.cancel_order" on server default"#));

        let two = with(vec![
            server("crm", None, &["crm.*"]),
            server("leads", None, &["crm.leads.list"]),
        ]);
        assert_eq!(reason(&two), "name_collision");
        assert_eq!(reason(&orphan), "missing_primary");
        assert_eq!(reason(&dup), "duplicate_id");
        assert_eq!(reason(&outside), "outside_prefix");
        assert!(refused(&two)
            .contains(r#"mcp_servers[leads]: "crm.leads.list" overlaps "crm.*" on server crm"#));

        let mut forwarded_audience = docs();
        forwarded_audience.connection.auth = McpAuth::Forwarded;
        forwarded_audience.connection.audience = Some("docs-mcp".into());
        assert!(refused(&with(vec![forwarded_audience]))
            .contains("mcp_servers[docs].audience applies only to auth"));

        let mut blank_audience = primary(&["harbor.*"]);
        blank_audience.mcp.as_mut().unwrap().audience = Some("  ".into());
        assert!(refused(&blank_audience).contains("mcp.audience must not be blank"));

        let mut skipped_required = primary(&["harbor.*"]);
        skipped_required.mcp.as_mut().unwrap().unavailable_skip_secs = Some(30);
        assert!(refused(&skipped_required).contains("mcp.unavailable_skip_secs applies only"));
        skipped_required.mcp.as_mut().unwrap().required = Some(false);
        assert!(skipped_required.check_mcp_servers().is_ok());
    }

    #[test]
    fn check_mcp_servers_accepts_disjoint_and_prefixed_servers() {
        let mut embed = primary(&["search", "harbor.*"]);
        embed.mcp_servers = vec![
            server("docs", Some("docs"), &["docs.search", "docs.get_page"]),
            server("wiki", Some("wiki"), &["wiki.*"]),
            server("billing", None, &["billing.list_invoices"]),
        ];
        assert_eq!(embed.check_mcp_servers(), Ok(()));

        let mut legacy = primary(&["anything", "x.*", "x_y", "  "]);
        legacy.tool_grants = vec![grant(" ", &[])];
        assert_eq!(
            legacy.check_mcp_servers(),
            Ok(()),
            "with no additional servers nothing pre-existing is re-checked"
        );
        assert_eq!(EmbedSettings::default().check_mcp_servers(), Ok(()));
    }

    #[test]
    fn grants_resolve_per_server() {
        let mut embed = primary(&["harbor.list_orders"]);
        embed.tool_grants = vec![grant("orders:edit", &["harbor.cancel_order"])];
        let mut docs = server("docs", Some("docs"), &["docs.search"]);
        docs.tool_grants = vec![ToolGrant {
            source: GrantSource::KeycloakRole,
            ..grant("editor", &["docs.edit_page"])
        }];
        embed.mcp_servers = vec![docs];

        assert!(embed.is_granted_tool("harbor.cancel_order"));
        assert!(embed.is_granted_tool("docs.edit_page"));
        assert!(!embed.is_granted_tool("docs.search"));
        assert_eq!(
            embed
                .all_tool_grants()
                .map(|g| g.requires.as_str())
                .collect::<Vec<_>>(),
            ["orders:edit", "editor"]
        );
        let editor_only = embed.effective_allowed_tools(|g| g.requires == "editor");
        assert_eq!(
            editor_only,
            names(&["harbor.list_orders", "docs.search", "docs.edit_page"])
        );
        assert_eq!(
            embed.mcp_server_specs()[1].tools.catalogue(),
            names(&["docs.search", "docs.edit_page"])
        );
        assert!(embed.any_server_admits("docs.search"));
        assert!(!embed.any_server_admits("docs.edit_page"));
    }

    #[test]
    fn strip_and_adopt_credentials_cover_every_server() {
        let mut live = primary(&["harbor.*"]);
        live.mcp_servers = vec![
            server("docs", Some("docs"), &["docs.*"]),
            server("billing", None, &["billing.*"]),
        ];
        let mut snapshot = live.clone();
        snapshot.strip_credentials();
        assert!(snapshot
            .mcp_server_specs()
            .iter()
            .all(|s| s.connection.discovery_credential.is_none()));

        snapshot.mcp_servers[1].connection.url = "https://moved.example.test/mcp".into();
        snapshot.adopt_credentials(&live);
        let credentials: Vec<Option<&str>> = snapshot
            .mcp_server_specs()
            .iter()
            .map(|s| s.connection.discovery_credential.as_deref())
            .collect();
        assert_eq!(
            credentials,
            [Some("primary-secret"), Some("docs-secret"), None],
            "a moved server keeps no credential"
        );

        let mut renamed = live.clone();
        renamed.strip_credentials();
        renamed.mcp_servers[0].id = "pages".into();
        renamed.mcp.as_mut().unwrap().url = "https://elsewhere.example.test".into();
        renamed.adopt_credentials(&live);
        assert!(renamed.mcp.as_ref().unwrap().discovery_credential.is_none());
        assert!(renamed.mcp_servers[0]
            .connection
            .discovery_credential
            .is_none());
    }

    #[test]
    fn server_settings_debug_never_prints_the_credential() {
        let mut embed = primary(&["harbor.*"]);
        embed.mcp_servers = vec![server("docs", None, &["docs.*"])];
        let printed = format!("{embed:?}");
        assert!(!printed.contains("primary-secret"), "{printed}");
        assert!(!printed.contains("docs-secret"), "{printed}");
        assert!(printed.contains("has_discovery_credential: true"));
    }
}
