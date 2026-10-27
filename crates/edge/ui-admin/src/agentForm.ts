import type {
  Agent,
  AgentEdit,
  GitImportReport,
  GitImportRequest,
  GrantSource,
  McpAuth,
  McpDiscoverRequest,
  McpEdit,
  McpServer,
  McpServerEdit,
  ToolGrant,
} from "./api";

// The Integration tab and the import form as flat values (what inputs hold),
// and their conversions to request bodies. Kept out of the components so the
// bodies are unit-testable under node — in particular that the discovery
// credential travels only when the operator typed one or asked to clear it.

/** Trimmed, non-blank lines of a textarea. */
export const lines = (v: string): string[] =>
  v
    .split("\n")
    .map((s) => s.trim())
    .filter(Boolean);

/** Trimmed, non-blank names of a space/comma/newline separated field. */
export const names = (v: string): string[] =>
  v
    .split(/[\s,]+/)
    .map((s) => s.trim())
    .filter(Boolean);

export const GRANT_SOURCES: [GrantSource, string][] = [
  ["backend_permissions", "Backend permission"],
  ["keycloak_role", "Keycloak role"],
  ["host_hint", "Host hint"],
];

/** A tool grant row as the editor holds it: tools as free text. */
export interface GrantRow {
  source: GrantSource;
  requires: string;
  tools: string;
}

/** How long a server that is not required sits out after a timed-out turn
 *  start, when it names no window of its own. */
export const DEFAULT_SKIP_SECS = 60;

/** An MCP server's connection as the editor holds it. */
export interface McpConnectionRow {
  url: string;
  /** What was typed into the credential field; blank = say nothing. */
  credential: string;
  /** The operator asked to clear the stored credential on save. */
  clear_credential: boolean;
  auth: McpAuth;
  audience: string;
  required: boolean;
  unavailable_skip_secs: string;
}

/** An additional MCP server as the editor holds it. */
export interface McpServerRow extends McpConnectionRow {
  id: string;
  tool_prefix: string;
  /** Space/comma/newline separated. */
  allowed_tools: string;
  tool_grants: GrantRow[];
}

export interface IntegrationForm {
  /** The primary server; a blank URL is a pure-chat agent. */
  mcp: McpConnectionRow;
  mcp_servers: McpServerRow[];
  permissions_url: string;
  permissions_ttl: string;
  tool_grants: GrantRow[];
  confirm_writes: boolean;
  /** One per line. */
  image_hosts: string;
  spill_skip_tools: string[];
  passive_memory: boolean;
}

const grantRows = (grants: ToolGrant[]): GrantRow[] =>
  grants.map((g) => ({ source: g.source, requires: g.requires, tools: g.tools.join(" ") }));

/** Grant rows with nothing in them are dropped; half-filled ones go to the
 *  server, which names what is missing. */
const grantEdits = (rows: GrantRow[]): ToolGrant[] =>
  rows
    .map((g) => ({ source: g.source, requires: g.requires.trim(), tools: names(g.tools) }))
    .filter((g) => g.requires || g.tools.length);

function connectionRow(server: McpServer | null | undefined, requiredByDefault: boolean): McpConnectionRow {
  return {
    url: server?.url ?? "",
    credential: "",
    clear_credential: false,
    auth: server?.auth ?? "exchanged",
    audience: server?.audience ?? "",
    required: server?.required ?? requiredByDefault,
    unavailable_skip_secs: String(server?.unavailable_skip_secs ?? DEFAULT_SKIP_SECS),
  };
}

/** A new, empty additional server. */
export const emptyServerRow = (): McpServerRow => ({
  ...connectionRow(null, false),
  id: "",
  tool_prefix: "",
  allowed_tools: "",
  tool_grants: [],
});

/** An agent's additional servers as editor rows. */
export function mcpServerRows(agent?: Agent): McpServerRow[] {
  return (agent?.mcp_servers ?? []).map((s) => ({
    ...connectionRow(s, false),
    id: s.id,
    tool_prefix: s.tool_prefix ?? "",
    allowed_tools: s.allowed_tools.join(" "),
    tool_grants: grantRows(s.tool_grants),
  }));
}

export function integrationForm(agent?: Agent): IntegrationForm {
  return {
    mcp: connectionRow(agent?.mcp_settings, true),
    mcp_servers: mcpServerRows(agent),
    permissions_url: agent?.permissions?.url ?? "",
    permissions_ttl:
      agent?.permissions?.cache_ttl_secs === null || agent?.permissions?.cache_ttl_secs === undefined
        ? ""
        : String(agent.permissions.cache_ttl_secs),
    tool_grants: grantRows(agent?.tool_grants ?? []),
    confirm_writes: agent?.policy?.confirm_writes ?? false,
    image_hosts: (agent?.image_hosts ?? []).join("\n"),
    spill_skip_tools: agent?.spill_skip_tools ?? [],
    passive_memory: agent?.passive_memory ?? true,
  };
}

/** The write-only credential as a body carries it: the key is present only
 *  when the operator typed one or asked to clear it (sent as ""), so an
 *  ordinary save leaves the stored credential alone. */
export function credentialField(credential: string, clear: boolean): Pick<McpEdit, "discovery_credential"> {
  if (clear) return { discovery_credential: "" };
  const typed = credential.trim();
  return typed ? { discovery_credential: typed } : {};
}

/** A connection as a body carries it. A required server is never skipped, so
 *  its skip window is sent as null (the default). */
function connectionEdit(c: McpConnectionRow): McpEdit {
  const secs = Number.parseInt(c.unavailable_skip_secs, 10);
  return {
    url: c.url.trim(),
    auth: c.auth,
    audience: c.auth === "exchanged" ? c.audience.trim() || null : null,
    required: c.required,
    unavailable_skip_secs: c.required || Number.isNaN(secs) || secs < 0 ? null : secs,
    ...credentialField(c.credential, c.clear_credential),
  };
}

/** Whether a save keeps a stored credential for the additional server named
 *  `id`. The server keeps the credential of the stored server with the same
 *  id, so a renamed row starts without one and a re-added id gets the old one back. */
export const hasStoredCredential = (agent: Agent | undefined, id: string): boolean =>
  agent?.mcp_servers.some((s) => s.id === id.trim() && s.has_discovery_credential) ?? false;

/** The discovery probe for a connection: only a credential typed into the
 *  form travels, never a stored one, and none while clearing. */
export function discoverRequest(c: McpConnectionRow): McpDiscoverRequest {
  const typed = c.clear_credential ? "" : c.credential.trim();
  return { url: c.url.trim(), ...(typed ? { discovery_credential: typed } : {}) };
}

const blankServerRow = (r: McpServerRow): boolean =>
  !r.id.trim() &&
  !r.url.trim() &&
  !r.tool_prefix.trim() &&
  !names(r.allowed_tools).length &&
  !r.credential.trim() &&
  grantEdits(r.tool_grants).length === 0;

/** The integration slice of a PUT (or create) body. `mcp` is null for a
 *  blank URL (a pure-chat agent). `mcp_servers` is always the whole list the
 *  form holds, so removing every row sends []; rows with nothing in them are
 *  dropped and half-filled ones go to the server, which names what is
 *  missing. */
export function integrationEdit(
  f: IntegrationForm,
): Required<
  Pick<
    AgentEdit,
    | "mcp"
    | "mcp_servers"
    | "permissions"
    | "tool_grants"
    | "policy"
    | "image_hosts"
    | "spill_skip_tools"
    | "passive_memory"
  >
> {
  const permissionsUrl = f.permissions_url.trim();
  const ttl = Number.parseInt(f.permissions_ttl, 10);
  const mcp_servers: McpServerEdit[] = f.mcp_servers
    .filter((r) => !blankServerRow(r))
    .map((r) => ({
      ...connectionEdit(r),
      id: r.id.trim(),
      tool_prefix: r.tool_prefix.trim() || null,
      allowed_tools: names(r.allowed_tools),
      tool_grants: grantEdits(r.tool_grants),
    }));
  return {
    mcp: f.mcp.url.trim() ? connectionEdit(f.mcp) : null,
    mcp_servers,
    permissions: permissionsUrl
      ? { url: permissionsUrl, cache_ttl_secs: Number.isNaN(ttl) || ttl <= 0 ? null : ttl }
      : null,
    tool_grants: grantEdits(f.tool_grants),
    policy: f.confirm_writes ? { confirm_writes: true } : null,
    image_hosts: lines(f.image_hosts),
    spill_skip_tools: f.spill_skip_tools,
    passive_memory: f.passive_memory,
  };
}

/** What the spill-skip picker offers: every server's exact allowlist
 *  entries (a `prefix.*` glob names nothing to pick) and granted tools, plus
 *  whatever is already picked, so an entry an edit orphaned can be unticked. */
export function spillSkipChoices(allowedTools: string[], f: IntegrationForm): string[] {
  const exact = [
    ...allowedTools,
    ...f.tool_grants.flatMap((g) => names(g.tools)),
    ...f.mcp_servers.flatMap((s) => [...names(s.allowed_tools), ...s.tool_grants.flatMap((g) => names(g.tools))]),
  ].filter((t) => !t.endsWith(".*"));
  return [...new Set([...exact, ...f.spill_skip_tools])];
}

/** A discovered tool's name as the agent will see it, under `prefix`. */
export const exposedName = (prefix: string, tool: string): string =>
  prefix.trim() ? `${prefix.trim()}.${tool}` : tool;

/** `field` (a names list) with `tool` added or removed. */
export function toggleName(field: string, tool: string, on: boolean): string {
  const current = names(field);
  const next = on ? (current.includes(tool) ? current : [...current, tool]) : current.filter((t) => t !== tool);
  return next.join(" ");
}

export interface EnvRow {
  name: string;
  value: string;
}

export interface ImportForm {
  repo: string;
  git_ref: string;
  token: string;
  env: EnvRow[];
}

export const emptyImportForm = (): ImportForm => ({ repo: "", git_ref: "", token: "", env: [] });

/** The import body: trimmed, with `git_ref` and `env` present only when
 *  they say something. Rows with a blank name are not sent. */
export function importRequest(f: ImportForm): GitImportRequest {
  const req: GitImportRequest = { repo: f.repo.trim(), token: f.token.trim() };
  const ref = f.git_ref.trim();
  if (ref) req.git_ref = ref;
  const env: Record<string, string> = {};
  for (const row of f.env) {
    const name = row.name.trim();
    if (name) env[name] = row.value;
  }
  if (Object.keys(env).length) req.env = env;
  return req;
}

/** The env rows plus an empty row for each of `unresolved` not already
 *  named, so a refused import turns into a form with the blanks to fill. */
export function withUnresolved(rows: EnvRow[], unresolved: string[]): EnvRow[] {
  const have = new Set(rows.map((r) => r.name.trim()));
  return [...rows, ...unresolved.filter((n) => !have.has(n)).map((name) => ({ name, value: "" }))];
}

export function describeImport(r: GitImportReport): string {
  const n = (count: number, word: string) => `${count} ${word}${count === 1 ? "" : "s"}`;
  return `Imported ${n(r.agents, "agent")}, ${n(r.playbooks, "playbook")}, ${n(r.training_docs, "training doc")}.`;
}
