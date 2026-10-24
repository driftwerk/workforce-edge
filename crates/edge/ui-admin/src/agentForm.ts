import type {
  Agent,
  AgentEdit,
  GitImportReport,
  GitImportRequest,
  GrantSource,
  McpEdit,
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

export interface IntegrationForm {
  mcp_url: string;
  /** What was typed into the credential field; blank = say nothing. */
  credential: string;
  /** The operator asked to clear the stored credential on save. */
  clear_credential: boolean;
  permissions_url: string;
  permissions_ttl: string;
  tool_grants: GrantRow[];
  confirm_writes: boolean;
  /** One per line. */
  image_hosts: string;
  spill_skip_tools: string[];
  passive_memory: boolean;
}

export function integrationForm(agent?: Agent): IntegrationForm {
  return {
    mcp_url: agent?.mcp_url ?? "",
    credential: "",
    clear_credential: false,
    permissions_url: agent?.permissions?.url ?? "",
    permissions_ttl:
      agent?.permissions?.cache_ttl_secs === null || agent?.permissions?.cache_ttl_secs === undefined
        ? ""
        : String(agent.permissions.cache_ttl_secs),
    tool_grants: (agent?.tool_grants ?? []).map((g) => ({
      source: g.source,
      requires: g.requires,
      tools: g.tools.join(" "),
    })),
    confirm_writes: agent?.policy?.confirm_writes ?? false,
    image_hosts: (agent?.image_hosts ?? []).join("\n"),
    spill_skip_tools: agent?.spill_skip_tools ?? [],
    passive_memory: agent?.passive_memory ?? true,
  };
}

/** The integration slice of a PUT (or create) body. `mcp` is null for a
 *  blank URL (a pure-chat agent); inside it the credential key is present
 *  only when the operator typed one or asked to clear it (sent as ""), so
 *  an ordinary save leaves the stored credential alone. Grant rows with
 *  nothing in them are dropped; half-filled ones go to the server, which
 *  names what is missing. */
export function integrationEdit(
  f: IntegrationForm,
): Required<
  Pick<
    AgentEdit,
    | "mcp"
    | "permissions"
    | "tool_grants"
    | "policy"
    | "image_hosts"
    | "spill_skip_tools"
    | "passive_memory"
  >
> {
  const url = f.mcp_url.trim();
  let mcp: McpEdit | null = null;
  if (url) {
    mcp = { url };
    if (f.clear_credential) mcp.discovery_credential = "";
    else if (f.credential.trim()) mcp.discovery_credential = f.credential.trim();
  }
  const permissionsUrl = f.permissions_url.trim();
  const ttl = Number.parseInt(f.permissions_ttl, 10);
  const tool_grants: ToolGrant[] = f.tool_grants
    .map((g) => ({ source: g.source, requires: g.requires.trim(), tools: names(g.tools) }))
    .filter((g) => g.requires || g.tools.length);
  return {
    mcp,
    permissions: permissionsUrl
      ? { url: permissionsUrl, cache_ttl_secs: Number.isNaN(ttl) || ttl <= 0 ? null : ttl }
      : null,
    tool_grants,
    policy: f.confirm_writes ? { confirm_writes: true } : null,
    image_hosts: lines(f.image_hosts),
    spill_skip_tools: f.spill_skip_tools,
    passive_memory: f.passive_memory,
  };
}

/** The exact tool names an agent has — what the spill-skip picker offers:
 *  the allowlist's exact entries (a `prefix.*` glob names nothing to pick)
 *  plus every granted tool. */
export function concreteTools(allowedTools: string[], grants: GrantRow[]): string[] {
  const out: string[] = [];
  for (const t of [...allowedTools, ...grants.flatMap((g) => names(g.tools))]) {
    if (!t.endsWith(".*") && !out.includes(t)) out.push(t);
  }
  return out;
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
