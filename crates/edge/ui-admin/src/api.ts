export type UploadKind = "image" | "spreadsheet" | "document";

// The operator admin API (same-origin, session cookie). Every call sends the
// cookie automatically; a 401 means "not signed in" (surface the login form).

export class Unauthorized extends Error {}

async function req(path: string, init?: RequestInit): Promise<Response> {
  const res = await fetch(path, { ...init, credentials: "same-origin" });
  if (res.status === 401) throw new Unauthorized();
  return res;
}

export interface LoginConfig {
  password: boolean;
  /** "google" = GSI button, "redirect" = Authorization Code (Keycloak), null = off. */
  oidc_mode: "google" | "redirect" | null;
  google_client_id: string | null;
}

export async function loginConfig(): Promise<LoginConfig> {
  const res = await fetch("/admin/api/login/config", {
    credentials: "same-origin",
  });
  if (!res.ok) throw new Error(`login config (${res.status})`);
  return await res.json();
}

// True when this install's config lives in git — agents/playbooks/training are
// read-only here (only appearance is editable). Set by `me()`, read by the UI.
let _gitManaged = false;
export function isGitManaged(): boolean {
  return _gitManaged;
}

// The tenant this session is signed into. Set by `me()`, shown in the shell.
let _tenant: { id: string; name: string } | null = null;
export function tenant(): { id: string; name: string } | null {
  return _tenant;
}

/** The last known state of the tenant's LLM key. `message` is a fixed
 *  per-class sentence from the server (never provider text); empty when ok. */
export interface LlmStatus {
  state: "ok" | "failed" | "unset";
  checked_at: string;
  message: string;
}

// Set by `me()` and by the key endpoints, so the shell banner follows a key
// change without another round trip.
let _llmStatus: LlmStatus | null = null;
export function llmStatus(): LlmStatus | null {
  return _llmStatus;
}

/** The caps the tenant's plan sets; a field absent or null is unbounded.
 *  `upload_mb` is not metered by the server and `max_concurrent_turns`
 *  binds on the turn semaphore, so neither has a usage figure. */
export interface Plan {
  turns_per_month?: number | null;
  active_users?: number | null;
  knowledge_chunks?: number | null;
  memory_objects?: number | null;
  agents?: number | null;
  upload_mb?: number | null;
  max_concurrent_turns?: number | null;
}

/** What the tenant has used of its plan, as the server counts it. The
 *  monthly figures restart on the first of each UTC month; `memory_objects`
 *  is null when the vector store could not be counted; `agents` is the
 *  agents mounted on the workspace. */
export interface Usage {
  turns_this_month: number;
  active_users_this_month: number;
  knowledge_chunks: number;
  memory_objects: number | null;
  agents: number;
}

/** The plan beside its usage, and which plan fields are at their cap. `usage`
 *  is null when the server could not count it. */
export interface PlanUsage {
  plan: Plan;
  usage: Usage | null;
  over: string[];
}

// Set by `me()`, so the shell's plan banner needs no second round trip.
let _planUsage: PlanUsage | null = null;
export function planUsage(): PlanUsage | null {
  return _planUsage;
}

/** The session probe's body, or null when not signed in. */
async function probe(): Promise<Record<string, unknown> | null> {
  try {
    const res = await req("/admin/api/me");
    if (!res.ok) return null;
    return await res.json();
  } catch {
    return null;
  }
}

function planUsageOf(body: Record<string, unknown>): PlanUsage {
  return {
    plan: (body.plan as Plan | undefined) ?? {},
    usage: (body.usage as Usage | null | undefined) ?? null,
    over: Array.isArray(body.over) ? (body.over as string[]) : [],
  };
}

/** The signed-in operator, or null. */
export async function me(): Promise<string | null> {
  const body = await probe();
  if (!body) return null;
  _gitManaged = !!body.git_managed;
  _tenant = (body.tenant as { id: string; name: string } | undefined) ?? null;
  _llmStatus = (body.llm_status as LlmStatus | undefined) ?? null;
  _planUsage = planUsageOf(body);
  return body.operator as string;
}

/** A fresh count of the tenant's usage against its plan, for the usage card. */
export async function fetchPlanUsage(): Promise<PlanUsage> {
  const body = await probe();
  if (!body) throw new Unauthorized();
  _planUsage = planUsageOf(body);
  return _planUsage;
}

/** What the shell banner says about the key, or null when it is fine. */
export function llmBanner(
  status: LlmStatus | null,
): { text: string; link: string } | null {
  if (!status || status.state === "ok") return null;
  if (status.state === "unset")
    return {
      text: "No LLM key — the assistant cannot answer.",
      link: "Set one in Settings.",
    };
  return {
    text: status.message || "The LLM key failed its last check.",
    link: "Replace the key in Settings.",
  };
}

export interface SyncReport {
  changed: boolean;
  commit: string;
  agents: number;
  playbooks: number;
  training: number;
}

/** Re-fetch the config repo and hot-apply it, no restart. Git-managed installs only. */
export async function syncGit(): Promise<SyncReport> {
  const res = await req("/admin/api/git/sync", { method: "POST" });
  if (!res.ok) {
    let msg = `Sync failed (${res.status})`;
    try {
      const body = await res.json();
      if (body.error) msg = body.error;
    } catch {
      /* no body */
    }
    throw new Error(msg);
  }
  return res.json();
}

export async function loginOidc(
  token: string,
): Promise<{ ok: boolean; error?: string }> {
  const res = await fetch("/admin/api/login/oidc", {
    method: "POST",
    credentials: "same-origin",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ token }),
  });
  if (res.ok) return { ok: true };
  let error =
    res.status === 403
      ? "Your account is not on the operator allowlist."
      : "Sign-in failed.";
  try {
    const b = await res.json();
    if (b.error) error = b.error;
  } catch {
    /* none */
  }
  return { ok: false, error };
}

export interface Operator {
  kind: "email" | "domain";
  value: string;
  added_by: string;
  added_at: string;
}

export async function listOperators(): Promise<Operator[]> {
  const res = await req("/admin/api/operators");
  if (!res.ok) throw new Error(`operators (${res.status})`);
  return (await res.json()).operators;
}

export async function addOperator(
  kind: "email" | "domain",
  value: string,
): Promise<void> {
  const res = await req("/admin/api/operators", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ kind, value }),
  });
  if (!res.ok) throw new Error(await detail(res, `Add failed (${res.status})`));
}

export async function removeOperator(
  kind: string,
  value: string,
): Promise<void> {
  const res = await req(
    `/admin/api/operators/${encodeURIComponent(kind)}/${encodeURIComponent(value)}`,
    { method: "DELETE" },
  );
  if (!res.ok) throw new Error(`Remove failed (${res.status})`);
}

export async function login(password: string): Promise<boolean> {
  const res = await fetch("/admin/api/login", {
    method: "POST",
    credentials: "same-origin",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ password }),
  });
  return res.ok;
}

export async function logout(): Promise<void> {
  await fetch("/admin/api/logout", {
    method: "POST",
    credentials: "same-origin",
  });
}

/** A brand gradient as discrete stops. The server composes and validates the
 *  `linear-gradient(…)` — the UI never sends CSS. Mirrors `agent::Gradient`. */
export interface Gradient {
  from?: string | null;
  via?: string | null;
  to?: string | null;
  angle?: string | null;
  /** Explicit stops with positions. Wins over from/via/to once at least two are
   *  usable, so the shorthand keeps working for configs written by hand. */
  stops?: GradientStop[] | null;
}

/** One stop: a hex colour and where it sits along the axis, 0-100 (%). */
export interface GradientStop {
  color?: string | null;
  at?: number | null;
}

/** A per-surface paint override: a flat `color`, or the surface's own `gradient`.
 *  Neither ⇒ the surface inherits the shared brand paint. Mirrors `agent::SurfacePaint`. */
export interface SurfacePaint {
  color?: string | null;
  gradient?: Gradient | null;
}

/** Dark-mode colour overrides. Every field is optional: unset BRAND colours fall
 *  back to their light value, unset NEUTRALS to the server's curated dark
 *  defaults. Structure (logo, font, radius, launcher, …) is shared with light and
 *  deliberately absent here. Mirrors `agent::DarkPalette`. */
export interface DarkPalette {
  primary_color?: string | null;
  primary_text?: string | null;
  user_bubble?: string | null;
  user_bubble_text?: string | null;
  agent_bubble?: string | null;
  agent_bubble_text?: string | null;
  surface?: string | null;
  panel?: string | null;
  /** Dark composer background; unset follows the dark `panel`. */
  input_bg?: string | null;
  border?: string | null;
  muted?: string | null;
  text?: string | null;
  gradient?: Gradient | null;
  /** Dark paint for the avatar / send button; unset reuses the light override. */
  avatar_paint?: SurfacePaint | null;
  send_paint?: SurfacePaint | null;
  /** Dark borders; unset follows the dark `border`. */
  input_border?: string | null;
  agent_bubble_border?: string | null;
}

export interface Branding {
  title?: string | null;
  subtitle?: string | null;
  logo_url?: string | null;
  launcher?: string | null;
  position?: string | null;
  radius?: string | null;
  font_family?: string | null;
  primary_color?: string | null;
  primary_text?: string | null;
  user_bubble?: string | null;
  user_bubble_text?: string | null;
  agent_bubble?: string | null;
  agent_bubble_text?: string | null;
  surface?: string | null;
  panel?: string | null;
  /** Composer message-box background; unset follows `panel`. */
  input_bg?: string | null;
  /** Borders that default to `border` but can be split off it. */
  input_border?: string | null;
  agent_bubble_border?: string | null;
  border?: string | null;
  muted?: string | null;
  text?: string | null;
  accent_color?: string | null;
  font_url?: string | null;
  shadow?: boolean | null;
  fullscreen?: string | null;
  draggable?: boolean | null;
  /** Inset in px of the docked position from its corner, horizontal then vertical,
   *  before the customer moves it. Default 0 (flush); server-capped. */
  offset_x?: number | null;
  offset_y?: number | null;
  /** Round brand avatar beside each agent reply. */
  avatar?: boolean | null;
  /** Paint for the avatar / send button, each independent of the brand gradient and
   *  of each other. Unset ⇒ the surface inherits the shared brand paint. */
  avatar_paint?: SurfacePaint | null;
  send_paint?: SurfacePaint | null;
  /** Framed presentation — brand paint fills the panel, conversation inset inside.
   *  Independent of `gradient`: brands differ, so nothing rides on having one. */
  frame?: boolean | null;
  /** Frame thickness in px (default 2, server-clamped). Used only when framed. */
  frame_width?: number | null;
  /** `rounded` (default) or `circle`. */
  send_shape?: string | null;
  /** `arrow` (default) or `plane`. */
  send_icon?: string | null;
  /** `cover` (default, rounded tile) or `contain` (bare mark, no crop). */
  logo_fit?: string | null;
  /** Panel box on open (default 400 × 660) and the drag floor (default 320 × 420),
   *  in px. The server clamps these to a usable range and pulls the floor down to
   *  the opening size, so no value here can ship an unusable widget. */
  width?: number | null;
  height?: number | null;
  min_width?: number | null;
  min_height?: number | null;
  /** `light` (default), `dark`, or `auto` — follow the host page's own scheme. */
  theme_mode?: string | null;
  gradient?: Gradient | null;
  dark?: DarkPalette | null;
  // Nested objects are part of the value union so `gradient`/`dark` survive the
  // spread-and-resend the editor does; unknown keys from a newer server still
  // round-trip rather than being dropped on save.
  [k: string]: string | number | boolean | null | undefined | Gradient | DarkPalette | GradientStop[] | SurfacePaint;
}

/** The multi-file persona (SOUL/IDENTITY/AGENTS/TOOLS md). */
export interface IdentityFiles {
  soul: string;
  identity: string;
  agents: string;
  tools: string;
}

/** Where a tool grant's `requires` is checked. */
export type GrantSource = "backend_permissions" | "keycloak_role" | "host_hint";

/** A per-user write tier: `tools` unlock for a user holding `requires`. */
export interface ToolGrant {
  source: GrantSource;
  requires: string;
  tools: string[];
}

/** How the edge resolves a user's permission set for `backend_permissions`
 *  grants: `url` is called with `{user}` replaced by the signed-in subject. */
export interface PermissionsConfig {
  url: string;
  cache_ttl_secs: number | null;
}

export interface EmbedPolicy {
  confirm_writes: boolean | null;
}

/** How a server's tool calls authenticate as the customer: the turn's
 *  exchanged token for the server's audience, or always the customer's own. */
export type McpAuth = "exchanged" | "forwarded";

/** An MCP server as the agent list shows it, with its effective defaults.
 *  The credential itself never comes back. */
export interface McpServer {
  id: string;
  url: string;
  auth: McpAuth;
  audience: string | null;
  required: boolean;
  unavailable_skip_secs: number;
  has_discovery_credential: boolean;
  tool_prefix: string | null;
  allowed_tools: string[];
  tool_grants: ToolGrant[];
}

/** An MCP server's connection as PUT/POST carry it. The credential is
 *  write-only: absent or null keeps what is stored, "" clears it, anything
 *  else replaces it. Any other field left out keeps its stored value; null
 *  resets it to the default. */
export interface McpEdit {
  url: string;
  discovery_credential?: string | null;
  auth?: McpAuth;
  audience?: string | null;
  required?: boolean | null;
  unavailable_skip_secs?: number | null;
}

/** An additional MCP server as PUT/POST carry it: its connection plus its
 *  own allowlist and write tiers, in the names the agent sees. */
export interface McpServerEdit extends McpEdit {
  id: string;
  tool_prefix: string | null;
  allowed_tools: string[];
  tool_grants: ToolGrant[];
}

export interface Agent {
  id: string;
  display_name: string;
  identity: IdentityFiles;
  model: string | null;
  welcome: string;
  playbooks: string[];
  auto_playbook: string | null;
  allowed_origins: string[];
  allowed_emails: string[];
  keycloak_issuer: string;
  keycloak_audience: string | null;
  allowed_tools: string[];
  branding: Branding;
  data_tables: boolean;
  charts: boolean;
  forms: boolean;
  maps: boolean;
  diagrams: boolean;
  excalidraw_export: boolean;
  inline_images: boolean;
  images: boolean;
  uploads: UploadKind[];
  upload_max_mb: number | null;
  drawing: boolean;
  has_mcp: boolean;
  mcp_url: string | null;
  /** Whether a discovery credential is stored; the credential itself never
   *  comes back. */
  has_discovery_credential: boolean;
  /** The primary server's connection, or null for a pure-chat agent. */
  mcp_settings: McpServer | null;
  /** The servers used alongside the primary, in declared order. */
  mcp_servers: McpServer[];
  permissions: PermissionsConfig | null;
  tool_grants: ToolGrant[];
  policy: EmbedPolicy | null;
  image_hosts: string[];
  spill_skip_tools: string[];
  passive_memory: boolean;
}

export async function listAgents(): Promise<Agent[]> {
  const res = await req("/admin/api/agents");
  if (!res.ok) throw new Error(`agents failed (${res.status})`);
  return (await res.json()).agents;
}

export interface AgentCreate {
  id: string;
  display_name: string;
  identity: IdentityFiles;
  welcome: string;
  model?: string | null;
  allowed_origins: string[];
  allowed_emails: string[];
  keycloak_issuer: string;
  keycloak_audience?: string | null;
  allowed_tools: string[];
  mcp?: McpEdit | null;
  mcp_servers?: McpServerEdit[];
  branding: Branding;
  data_tables?: boolean;
  charts?: boolean;
  forms?: boolean;
  maps?: boolean;
  diagrams?: boolean;
  excalidraw_export?: boolean;
  inline_images?: boolean;
  images?: boolean;
  uploads?: UploadKind[];
  upload_max_mb?: number | null;
  drawing?: boolean;
  permissions?: PermissionsConfig | null;
  tool_grants?: ToolGrant[];
  policy?: EmbedPolicy | null;
  image_hosts?: string[];
  spill_skip_tools?: string[];
  passive_memory?: boolean;
}

export async function createAgent(spec: AgentCreate): Promise<void> {
  const res = await req("/admin/api/agents", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(spec),
  });
  if (!res.ok)
    throw new Error(await detail(res, `Create failed (${res.status})`));
}

export async function deleteAgent(id: string): Promise<void> {
  const res = await req(`/admin/api/agents/${encodeURIComponent(id)}`, {
    method: "DELETE",
  });
  if (!res.ok)
    throw new Error(await detail(res, `Delete failed (${res.status})`));
}

// The fields an operator may edit. A `null` model/auto_playbook clears to the
// install default; absent = unchanged. `mcp: null` makes a pure-chat agent;
// its credential travels write-only inside `mcp` (see `McpEdit`).
// `mcp_servers` replaces the whole list; absent leaves it as it is.
export interface AgentEdit {
  display_name?: string;
  identity?: IdentityFiles;
  welcome?: string;
  model?: string | null;
  auto_playbook?: string | null;
  playbooks?: string[];
  allowed_origins?: string[];
  allowed_emails?: string[];
  allowed_tools?: string[];
  keycloak_issuer?: string;
  keycloak_audience?: string | null;
  branding?: Branding;
  data_tables?: boolean;
  charts?: boolean;
  forms?: boolean;
  maps?: boolean;
  diagrams?: boolean;
  excalidraw_export?: boolean;
  inline_images?: boolean;
  images?: boolean;
  uploads?: UploadKind[];
  upload_max_mb?: number | null;
  drawing?: boolean;
  mcp?: McpEdit | null;
  mcp_servers?: McpServerEdit[];
  permissions?: PermissionsConfig | null;
  tool_grants?: ToolGrant[];
  policy?: EmbedPolicy | null;
  image_hosts?: string[];
  spill_skip_tools?: string[];
  passive_memory?: boolean;
}

export async function updateAgent(id: string, edit: AgentEdit): Promise<void> {
  const res = await req(`/admin/api/agents/${encodeURIComponent(id)}`, {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(edit),
  });
  if (!res.ok) {
    let msg = `Save failed (${res.status})`;
    try {
      const body = await res.json();
      if (body.error) msg = body.error;
    } catch {
      /* no body */
    }
    throw new Error(msg);
  }
}

// ─── Import from git ─────────────────────────────────────────────────────

/** `POST /admin/api/import/git`: read a config repo once. The token is used
 *  for the fetch and dropped; `env` binds the repo's `${NAME}` placeholders
 *  (the server never reads its own environment for them). */
export interface GitImportRequest {
  repo: string;
  git_ref?: string;
  token: string;
  env?: Record<string, string>;
}

export interface GitImportReport {
  agents: number;
  playbooks: number;
  training_docs: number;
  unresolved: string[];
}

/** Thrown when the repo names placeholders the request did not bind. */
export class UnresolvedPlaceholders extends Error {
  constructor(
    message: string,
    public readonly names: string[],
  ) {
    super(message);
  }
}

export async function importGit(req: GitImportRequest): Promise<GitImportReport> {
  const res = await fetch("/admin/api/import/git", {
    method: "POST",
    credentials: "same-origin",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(req),
  });
  if (res.status === 401) throw new Unauthorized();
  if (res.ok) return res.json();
  let error = `Import failed (${res.status})`;
  let unresolved: string[] = [];
  try {
    const body = await res.json();
    if (body.error) error = body.error;
    if (Array.isArray(body.unresolved)) unresolved = body.unresolved;
  } catch {
    /* no body */
  }
  if (unresolved.length) throw new UnresolvedPlaceholders(error, unresolved);
  throw new Error(error);
}

// ─── Settings ────────────────────────────────────────────────────────────
// Mirrors `SettingsView` / `SettingsUpdate` in server.rs. Secrets never
// appear in either direction: the view says only whether each is set, and a
// save body that names one is refused (422) rather than stripped.

export interface LlmSettings {
  provider: string;
  /** Install policy, read-only: empty leaves the provider unrestricted. */
  allowed_providers: string[];
  tier_key: string | null;
  default_model: string | null;
  failover: string[];
  /** Retry policy; not edited here, round-tripped as the server sent it. */
  retry: Record<string, unknown>;
}

export interface GraderSettings {
  enabled: boolean;
  model: string | null;
  sample_rate: number;
}

export interface TokenExchangeSettings {
  issuer: string;
  client_id: string;
  audience: string | null;
  scope: string | null;
}

export interface AdminOidcSettings {
  issuer: string;
  audience: string | null;
  client_id: string | null;
  mode: string | null;
}

export interface AdminSettings {
  oidc: AdminOidcSettings | null;
  allowed_emails: string[];
  allowed_domains: string[];
  public_url: string | null;
  session_hours: number;
}

export interface TenantLimits {
  rate_limit_per_min: number;
  user_rate_limit_per_min: number;
  max_concurrent_turns: number;
}

export interface AuditSettings {
  enabled: boolean;
  retention_days: number;
}

export type Outbound = "private" | "public";

/** The non-secret sections a save carries — exactly the PUT body. The
 *  outbound policy is not among them: the platform sets it, and a save that
 *  names it is refused. */
export interface SettingsUpdate {
  llm: LlmSettings;
  grader: GraderSettings | null;
  token_exchange: TokenExchangeSettings | null;
  admin: AdminSettings | null;
  limits: TenantLimits;
  audit: AuditSettings | null;
}

export interface SettingsView extends SettingsUpdate {
  /** How this tenant's outbound HTTP is held; shown, never edited here. */
  outbound: Outbound;
  llm_status: LlmStatus;
  has_llm_key: boolean;
  has_token_exchange_secret: boolean;
  has_admin_client_secret: boolean;
  /** False ⇒ no settings row yet: the values shown are the file's. */
  stored: boolean;
  /** False ⇒ no master key on this install: the file governs, read-only. */
  editable: boolean;
}

export async function getSettings(): Promise<SettingsView> {
  const res = await req("/admin/api/settings");
  if (!res.ok) throw new Error(`settings (${res.status})`);
  return await res.json();
}

export async function saveSettings(update: SettingsUpdate): Promise<void> {
  const res = await req("/admin/api/settings", {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(update),
  });
  if (!res.ok) throw new Error(await detail(res, `Save failed (${res.status})`));
}

export type KeyOutcome =
  | { ok: true; llm_status: LlmStatus }
  | { ok: false; error: string; llm_status: LlmStatus | null };

/** Validate and store the LLM key. A 422 means the key was NOT stored and
 *  carries the server's fixed reason; it resolves (not rejects) so the form
 *  can keep the typed key for another try. Anything else rejects. */
export async function setLlmKey(key: string): Promise<KeyOutcome> {
  const res = await req("/admin/api/settings/llm-key", {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ key }),
  });
  if (res.ok) {
    const body = await res.json();
    _llmStatus = body.llm_status ?? _llmStatus;
    return { ok: true, llm_status: body.llm_status };
  }
  if (res.status === 422) {
    const body = await res.json().catch(() => ({}));
    if (body.llm_status) _llmStatus = body.llm_status;
    return {
      ok: false,
      error: body.error ?? "The key was not accepted.",
      llm_status: body.llm_status ?? null,
    };
  }
  throw new Error(await detail(res, `Set key failed (${res.status})`));
}

export async function clearLlmKey(): Promise<void> {
  const res = await req("/admin/api/settings/llm-key", { method: "DELETE" });
  if (!res.ok) throw new Error(await detail(res, `Remove key failed (${res.status})`));
  _llmStatus = { state: "unset", checked_at: new Date().toISOString(), message: "" };
}

export type StoredSecret = "token-exchange-secret" | "admin-client-secret";

export async function setSecret(which: StoredSecret, secret: string): Promise<void> {
  const res = await req(`/admin/api/settings/${which}`, {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ secret }),
  });
  if (!res.ok) throw new Error(await detail(res, `Set secret failed (${res.status})`));
}

export async function clearSecret(which: StoredSecret): Promise<void> {
  const res = await req(`/admin/api/settings/${which}`, { method: "DELETE" });
  if (!res.ok) throw new Error(await detail(res, `Clear secret failed (${res.status})`));
}

// ─── Setup wizard ────────────────────────────────────────────────────────
// Mirrors `SetupReport` and the two probes in server.rs. The probes are
// reads: nothing is stored by them, and the wizard writes only through the
// settings and agents routes above.

/** Which first-run steps are done, read from the live state. */
export interface SetupReport {
  llm_key: boolean;
  llm_status: LlmStatus;
  /** False ⇒ no master key: the key cannot be set here. */
  editable: boolean;
  agents: number;
  /** The first agent by id — the one the wizard resumes on. */
  agent: string | null;
  mcp: boolean;
  origins: boolean;
  oidc: boolean;
  /** `/demo/{agent}` answers on this tenant. */
  demo: boolean;
  complete: boolean;
}

export async function setup(): Promise<SetupReport> {
  const res = await req("/admin/api/setup");
  if (!res.ok) throw new Error(`setup (${res.status})`);
  return await res.json();
}

export interface McpToolInfo {
  name: string;
  description: string;
}

/** What `POST /admin/api/mcp/discover` found: the server's name and every
 *  tool it advertises. `truncated` ⇒ the list is a floor, not the whole. */
export interface McpSurvey {
  server: string | null;
  version: string | null;
  protocol_version: string;
  tools: McpToolInfo[];
  truncated: boolean;
}

export interface McpDiscoverRequest {
  url: string;
  /** The bearer the agent would keep as its discovery credential. Absent ⇒
   *  the probe sends no Authorization header. */
  discovery_credential?: string;
}

/** Connect, initialise and list an MCP server's tools. Rejects with the
 *  server's reason: 422 for a URL the tenant's policy refuses, 502 for a
 *  server that did not answer. */
export async function discoverMcp(body: McpDiscoverRequest): Promise<McpSurvey> {
  const res = await req("/admin/api/mcp/discover", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  if (!res.ok) throw new Error(await detail(res, `Discovery failed (${res.status})`));
  return (await res.json()).survey;
}

export interface OidcCheckRequest {
  issuer: string;
  audience?: string;
  /** A customer token to verify the way the widget's socket would. */
  token?: string;
}

/** What the issuer's discovery document and JWKS say. */
export interface IssuerReport {
  /** The `issuer` the document claims; null when it names none. */
  issuer: string | null;
  jwks_uri: string;
  algs: string[];
  /** Signing keys this edge can verify with. */
  keys: number;
}

export type TokenVerdict =
  | { ok: true; sub: string; email: string | null; email_verified: boolean | null; name: string | null }
  | { ok: false; error: string };

export interface OidcCheck {
  /** The document's issuer is the one asked for (a trailing slash aside). */
  issuer_matches: boolean;
  report: IssuerReport;
  /** Null when no token was sent. */
  token: TokenVerdict | null;
}

/** Fetch an issuer's discovery document and JWKS, and verify a token
 *  through the validator the public socket uses. Rejects with the reason
 *  when the issuer could not be read. */
export async function checkOidc(body: OidcCheckRequest): Promise<OidcCheck> {
  const res = await req("/admin/api/oidc/check", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  if (!res.ok) throw new Error(await detail(res, `Issuer check failed (${res.status})`));
  return await res.json();
}

// ─── Playbooks ───────────────────────────────────────────────────────────

export interface Playbook {
  id: string;
  name: string;
  steps: number;
  source: string;
}

export async function listPlaybooks(): Promise<Playbook[]> {
  const res = await req("/admin/api/playbooks");
  if (!res.ok) throw new Error(`playbooks failed (${res.status})`);
  return (await res.json()).playbooks;
}

async function detail(res: Response, fallback: string): Promise<string> {
  try {
    const body = await res.json();
    // A coded refusal (`plan_limit`) carries its sentence in `message`.
    if (typeof body.message === "string" && body.message) return body.message;
    if (body.error) {
      return body.agents
        ? `${body.error} (${body.agents.join(", ")})`
        : body.error;
    }
  } catch {
    /* no body */
  }
  return fallback;
}

export async function savePlaybook(id: string, source: string): Promise<void> {
  const res = await req(`/admin/api/playbooks/${encodeURIComponent(id)}`, {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ source }),
  });
  if (!res.ok)
    throw new Error(await detail(res, `Save failed (${res.status})`));
}

export async function deletePlaybook(id: string): Promise<void> {
  const res = await req(`/admin/api/playbooks/${encodeURIComponent(id)}`, {
    method: "DELETE",
  });
  if (!res.ok)
    throw new Error(await detail(res, `Delete failed (${res.status})`));
}

// ─── Conversations (operator-wide) ───────────────────────────────────────

export interface Conversation {
  session: string;
  /** The public conversation id, so a feedback/turn deep-link (which carries
   *  that id, not the internal session id) can open this exact transcript.
   *  null for a session outside the agent's embed namespace. */
  conversation: string | null;
  owner: string;
  /** The customer's captured name, when resolved — else null (fall back to the
   *  parsed owner key). */
  display_name: string | null;
  title: string;
  updated_at: string;
  messages: number;
}

export interface TranscriptMessage {
  role: "user" | "assistant";
  content: string;
  at: string;
}

export interface Transcript {
  owner: string;
  display_name: string | null;
  title: string;
  messages: TranscriptMessage[];
}

export async function listConversations(
  agent: string,
): Promise<Conversation[]> {
  const res = await req(
    `/admin/api/agents/${encodeURIComponent(agent)}/conversations`,
  );
  if (!res.ok) throw new Error(`conversations failed (${res.status})`);
  return (await res.json()).conversations;
}

export async function getTranscript(
  agent: string,
  session: string,
): Promise<Transcript> {
  const res = await req(
    `/admin/api/agents/${encodeURIComponent(agent)}/conversations/${encodeURIComponent(session)}`,
  );
  if (!res.ok) throw new Error(`transcript failed (${res.status})`);
  return await res.json();
}

/** How one kind of a customer's data came out of an erasure. */
export interface ErasurePart {
  erased: number;
  failures: string[];
}

export interface Erasure {
  memories: ErasurePart;
  conversations: ErasurePart;
  analytics: ErasurePart;
  files: ErasurePart;
}

/** Erase everything held about the customer with this owner key, on every
 *  agent. Resolves with the per-part outcome whether or not every part
 *  succeeded (the server answers 500 with the same body when one failed);
 *  rejects with the server's reason when it erased nothing, such as a
 *  customer still active. */
export async function forgetCustomer(owner: string): Promise<Erasure> {
  const res = await req("/admin/api/customers/forget", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ owner }),
  });
  if (res.ok || res.status === 500) {
    const body = await res.clone().json().catch(() => null);
    if (body?.memories) return body as Erasure;
  }
  throw new Error(await detail(res, `Forget failed (${res.status})`));
}

export function erasureComplete(e: Erasure): boolean {
  return [e.memories, e.conversations, e.analytics, e.files].every((p) => p.failures.length === 0);
}

/** One line for the operator: what went, or which parts did not. */
export function describeErasure(e: Erasure): string {
  const parts = [
    ["memories", e.memories],
    ["conversations", e.conversations],
    ["analytics rows", e.analytics],
    ["files", e.files],
  ] as const;
  if (erasureComplete(e))
    return `Forgotten: ${parts.map(([name, p]) => `${p.erased} ${name}`).join(", ")}.`;
  return `Not fully forgotten. Failed: ${parts
    .filter(([, p]) => p.failures.length > 0)
    .map(([name, p]) => `${name} (${p.failures.join("; ")})`)
    .join(", ")}. Try again.`;
}

export function relativeTime(iso: string): string {
  const then = new Date(iso).getTime();
  const mins = Math.round((Date.now() - then) / 60000);
  if (mins < 1) return "just now";
  if (mins < 60) return `${mins}m ago`;
  const hrs = Math.round(mins / 60);
  if (hrs < 24) return `${hrs}h ago`;
  return new Date(iso).toLocaleDateString();
}

// ─── Lessons ─────────────────────────────────────────────────────────────

export interface Lesson {
  id: string;
  content: string;
  importance: number;
  triggers: string[];
  created_at: string;
}

export async function listLessons(agent: string): Promise<Lesson[]> {
  const res = await req(
    `/admin/api/agents/${encodeURIComponent(agent)}/lessons`,
  );
  if (!res.ok) throw new Error(`lessons failed (${res.status})`);
  return (await res.json()).lessons;
}

export async function deleteLesson(agent: string, id: string): Promise<void> {
  const res = await req(
    `/admin/api/agents/${encodeURIComponent(agent)}/lessons/${encodeURIComponent(id)}`,
    { method: "DELETE" },
  );
  if (!res.ok) throw new Error(`Delete failed (${res.status})`);
}

// ─── Knowledge (training data) ───────────────────────────────────────────

export interface KnowledgeDoc {
  id: string;
  title: string;
  content: string;
  chunks: number;
  updated_at: string;
}

export async function listKnowledge(agent: string): Promise<KnowledgeDoc[]> {
  const res = await req(
    `/admin/api/agents/${encodeURIComponent(agent)}/knowledge`,
  );
  if (!res.ok) throw new Error(`knowledge failed (${res.status})`);
  return (await res.json()).documents;
}

export async function saveKnowledge(
  agent: string,
  id: string,
  title: string,
  content: string,
): Promise<number> {
  const res = await req(
    `/admin/api/agents/${encodeURIComponent(agent)}/knowledge/${encodeURIComponent(id)}`,
    {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ title, content }),
    },
  );
  if (!res.ok)
    throw new Error(await detail(res, `Save failed (${res.status})`));
  return (await res.json()).chunks;
}

export async function deleteKnowledge(
  agent: string,
  id: string,
): Promise<void> {
  const res = await req(
    `/admin/api/agents/${encodeURIComponent(agent)}/knowledge/${encodeURIComponent(id)}`,
    { method: "DELETE" },
  );
  if (!res.ok) throw new Error(`Delete failed (${res.status})`);
}

export interface UploadResult {
  name: string;
  ok: boolean;
  chunks: number;
  error: string | null;
}

/** Upload one or more training files (multipart); each is extracted + indexed. */
export async function uploadKnowledge(
  agent: string,
  files: File[],
): Promise<UploadResult[]> {
  const form = new FormData();
  for (const f of files) form.append("file", f, f.name);
  const res = await fetch(
    `/admin/api/agents/${encodeURIComponent(agent)}/knowledge/upload`,
    {
      method: "POST",
      credentials: "same-origin",
      body: form,
    },
  );
  if (!res.ok)
    throw new Error(await detail(res, `Upload failed (${res.status})`));
  return (await res.json()).results;
}

/** File extensions the uploader accepts (advertised to the file picker). */
export const UPLOAD_ACCEPT =
  ".txt,.text,.md,.markdown,.csv,.tsv,.json,.log,.html,.htm,.pdf,.docx";

// ─── Audit log ───────────────────────────────────────────────────────────

export interface AuditEntry {
  id: number;
  operator: string;
  action: string;
  target: string;
  detail: string;
  at: string;
}

export async function listAudit(): Promise<AuditEntry[]> {
  const res = await req("/admin/api/audit");
  if (!res.ok) throw new Error(`audit (${res.status})`);
  return (await res.json()).entries;
}

// ─── Revision history ────────────────────────────────────────────────────

export interface RevisionMeta {
  revision: number;
  edited_by: string;
  created_at: string;
}

export async function listRevisions(
  kind: string,
  item: string,
): Promise<RevisionMeta[]> {
  const res = await req(
    `/admin/api/revisions/${encodeURIComponent(kind)}/${encodeURIComponent(item)}`,
  );
  if (!res.ok) throw new Error(`revisions (${res.status})`);
  return (await res.json()).revisions;
}

export async function getRevision(
  kind: string,
  item: string,
  revision: number,
): Promise<string> {
  const res = await req(
    `/admin/api/revisions/${encodeURIComponent(kind)}/${encodeURIComponent(item)}/${revision}`,
  );
  if (!res.ok) throw new Error(`revision (${res.status})`);
  return (await res.json()).content;
}

export async function restoreRevision(
  kind: string,
  item: string,
  revision: number,
): Promise<void> {
  const res = await req(
    `/admin/api/revisions/${encodeURIComponent(kind)}/${encodeURIComponent(item)}/${revision}/restore`,
    { method: "POST" },
  );
  if (!res.ok)
    throw new Error(await detail(res, `Restore failed (${res.status})`));
}

// ─── Usage/cost analytics ────────────────────────────────────────────────
// Shapes mirror the `edge_analytics` read endpoints (server.rs); every call
// serialises the shared `{from,to,agent,owner}` filter to a query string,
// omitting empty values so a blank filter matches everything.

/** The shared filter every analytics endpoint accepts. */
export interface AnalyticsFilter {
  from?: string;
  to?: string;
  agent?: string;
  owner?: string;
}

export interface AnalyticsSummary {
  turns: number;
  input_tokens: number;
  output_tokens: number;
  cache_read_tokens: number;
  cache_write_tokens: number;
  total_cost_usd: number;
  unique_users: number;
  /** 0..1; excludes `cancelled` (client disconnect) from the numerator. */
  failure_rate: number;
  avg_duration_ms: number;
}

export interface TimeseriesBucket {
  ts: string;
  turns: number;
  input_tokens: number;
  output_tokens: number;
  cost_usd: number;
}

/** A dimension the `/by/{dim}` breakdown can group on. */
export type Dimension = "user" | "agent" | "model" | "outcome";

export interface DimensionRow {
  /** The raw group key — `owner` for user, `agent_id` for agent — passed back
   *  verbatim as the drill-down filter value. */
  key: string;
  /** The human label for display (resolved name, model id, outcome, …). */
  label: string;
  turns: number;
  tokens: number;
  cost_usd: number;
}

export interface TurnRow {
  id: number;
  ts: string;
  owner: string;
  display_name: string;
  agent_id: string;
  conversation: string;
  model: string;
  input_tokens: number;
  output_tokens: number;
  cost_usd: number;
  rounds: number;
  tool_count: number;
  tool_failures: number;
  duration_ms: number;
  outcome: string;
}

export interface TurnsPage {
  turns: TurnRow[];
  next_cursor: string | null;
}

/**
 * Serialise the filter (plus any endpoint-specific extras) to a `?query`,
 * omitting empty/absent values. Returns "" when nothing is set. Values are
 * URL-encoded (owner keys carry `:`, `/` and a U+001F separator).
 */
export function analyticsQs(
  filter: AnalyticsFilter,
  extra?: Record<string, string | number | undefined | null>,
): string {
  const p = new URLSearchParams();
  const add = (k: string, v: string | number | undefined | null) => {
    if (v !== undefined && v !== null && v !== "") p.set(k, String(v));
  };
  add("from", filter.from);
  add("to", filter.to);
  add("agent", filter.agent);
  add("owner", filter.owner);
  if (extra) for (const [k, v] of Object.entries(extra)) add(k, v);
  const s = p.toString();
  return s ? `?${s}` : "";
}

export async function getAnalyticsSummary(
  filter: AnalyticsFilter,
): Promise<AnalyticsSummary> {
  const res = await req(`/admin/api/analytics/summary${analyticsQs(filter)}`);
  if (!res.ok) throw new Error(`analytics summary (${res.status})`);
  return await res.json();
}

export async function getAnalyticsTimeseries(
  filter: AnalyticsFilter,
  bucket: "day" | "hour",
): Promise<TimeseriesBucket[]> {
  const res = await req(
    `/admin/api/analytics/timeseries${analyticsQs(filter, { bucket })}`,
  );
  if (!res.ok) throw new Error(`analytics timeseries (${res.status})`);
  return (await res.json()).buckets;
}

export async function getAnalyticsByDimension(
  dimension: Dimension,
  filter: AnalyticsFilter,
): Promise<DimensionRow[]> {
  const res = await req(
    `/admin/api/analytics/by/${encodeURIComponent(dimension)}${analyticsQs(filter)}`,
  );
  if (!res.ok) throw new Error(`analytics by-${dimension} (${res.status})`);
  return (await res.json()).rows;
}

export async function getAnalyticsTurns(
  filter: AnalyticsFilter,
  opts?: { limit?: number; cursor?: string | null },
): Promise<TurnsPage> {
  const res = await req(
    `/admin/api/analytics/turns${analyticsQs(filter, { limit: opts?.limit, cursor: opts?.cursor })}`,
  );
  if (!res.ok) throw new Error(`analytics turns (${res.status})`);
  return await res.json();
}

/** One customer rating of an assistant answer. `rating` is +1/-1; `display_name`
 *  is resolved server-side (falls back to the owner key); `comment` is optional. */
export interface FeedbackRow {
  id: number;
  ts: string;
  owner: string;
  display_name: string;
  agent_id: string;
  conversation: string;
  message_id: string;
  rating: number;
  comment: string | null;
}

export interface FeedbackPage {
  feedback: FeedbackRow[];
  next_cursor: string | null;
}

export async function getAnalyticsFeedback(
  filter: AnalyticsFilter,
  opts?: { limit?: number; cursor?: string | null },
): Promise<FeedbackPage> {
  const res = await req(
    `/admin/api/analytics/feedback${analyticsQs(filter, { limit: opts?.limit, cursor: opts?.cursor })}`,
  );
  if (!res.ok) throw new Error(`analytics feedback (${res.status})`);
  return await res.json();
}

/** Answer-quality distribution over the filtered window (the grader KPIs). */
export interface GradesSummary {
  graded: number;
  good: number;
  poor: number;
  incorrect: number;
  unanswerable: number;
}

/** A gap the agent answered badly, grouped by exact `topic` string (near-duplicate
 *  phrasings stay separate — semantic clustering is a follow-up). `remedy`/
 *  `example_reason` are UNTRUSTED grader output (LLM-derived from customer/agent
 *  text) — render as plain text only, never as HTML/markdown. */
export interface GapRow {
  topic: string;
  count: number;
  unanswerable: number;
  incorrect: number;
  remedy: string | null;
  example_reason: string | null;
  last_seen: string;
  /** A few recent failing questions for this topic — UNTRUSTED customer text;
   *  render as plain text only. */
  example_questions: string[];
}

/** One graded turn. `gap_topic`/`reason` are UNTRUSTED grader output. */
export interface GradeRow {
  id: number;
  ts: string;
  owner: string;
  display_name: string;
  agent_id: string;
  conversation: string;
  message_id: string;
  quality: string;
  gap_topic: string | null;
  remedy: string | null;
  reason: string | null;
}

export interface GradesPage {
  grades: GradeRow[];
  next_cursor: string | null;
}

export async function getGradesSummary(
  filter: AnalyticsFilter,
): Promise<GradesSummary> {
  const res = await req(
    `/admin/api/analytics/grades/summary${analyticsQs(filter)}`,
  );
  if (!res.ok) throw new Error(`grades summary (${res.status})`);
  return await res.json();
}

export async function getGaps(filter: AnalyticsFilter): Promise<GapRow[]> {
  const res = await req(`/admin/api/analytics/gaps${analyticsQs(filter)}`);
  if (!res.ok) throw new Error(`gaps (${res.status})`);
  return (await res.json()).rows;
}

export async function getGrades(
  filter: AnalyticsFilter,
  opts?: { limit?: number; cursor?: string | null },
): Promise<GradesPage> {
  const res = await req(
    `/admin/api/analytics/grades${analyticsQs(filter, { limit: opts?.limit, cursor: opts?.cursor })}`,
  );
  if (!res.ok) throw new Error(`grades (${res.status})`);
  return await res.json();
}
