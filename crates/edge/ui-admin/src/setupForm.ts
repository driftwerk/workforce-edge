import type {
  Agent,
  AgentCreate,
  AgentEdit,
  McpDiscoverRequest,
  OidcCheckRequest,
  SetupReport,
} from "./api";

// The first-run wizard as flat values (what inputs hold), and the pure parts
// around it: which step to resume on, what each step needs before the next,
// and the request bodies the steps send. Kept out of the component so all
// of it is unit-testable under node. The wizard writes only through the
// settings and agents routes; the bodies here are theirs.

export type Step = "llm" | "mcp" | "origin" | "oidc" | "agent" | "snippet";

export const STEPS: { id: Step; title: string; blurb: string }[] = [
  { id: "llm", title: "LLM provider and key", blurb: "The model that answers, on your own key." },
  { id: "mcp", title: "MCP server", blurb: "Where the agent's tools come from." },
  { id: "origin", title: "App origin", blurb: "The site that will embed the widget." },
  { id: "oidc", title: "Recognising users", blurb: "How the widget trusts your signed-in users." },
  { id: "agent", title: "Agent", blurb: "Name, persona and welcome." },
  { id: "snippet", title: "Embed", blurb: "The snippet, and a live preview." },
];

export interface SetupState {
  provider: string;
  /** The key as typed; never stored client-side beyond the submit. */
  key: string;
  mcp_url: string;
  /** The discovery credential as typed; blank = send none / keep stored. */
  credential: string;
  /** Tool names the last survey listed; empty until one succeeds. */
  discovered: string[];
  /** The tools the agent may use, chosen from `discovered`. */
  tools: string[];
  /** The operator chose a pure-chat agent: no MCP server for now. */
  skip_mcp: boolean;
  origin: string;
  issuer: string;
  audience: string;
  /** The issuer the last successful check verified, in the document's
   *  spelling; blank until one succeeds, so the step is not passable on
   *  an unchecked issuer. */
  issuer_checked: string;
  name: string;
  persona: string;
  welcome: string;
}

export function emptyState(provider: string): SetupState {
  return {
    provider,
    key: "",
    mcp_url: "",
    credential: "",
    discovered: [],
    tools: [],
    skip_mcp: false,
    origin: "",
    issuer: "",
    audience: "",
    issuer_checked: "",
    name: "",
    persona: "",
    welcome: "",
  };
}

/** The state a returning operator sees: whatever the agent the wizard
 *  resumes on already has. An MCP or issuer the agent is already bound to
 *  counts as verified — the host built on it — so its step is passable
 *  without a fresh probe, which a changed value would need. */
export function prefill(provider: string, agent: Agent | null): SetupState {
  const s = emptyState(provider);
  if (!agent) return s;
  const id = agent.identity;
  return {
    ...s,
    mcp_url: agent.mcp_url ?? "",
    discovered: agent.has_mcp ? [...agent.allowed_tools] : [],
    tools: agent.has_mcp ? [...agent.allowed_tools] : [],
    origin: agent.allowed_origins[0] ?? "",
    issuer: agent.keycloak_issuer,
    audience: agent.keycloak_audience ?? "",
    issuer_checked: agent.keycloak_issuer.trim(),
    name: agent.display_name,
    persona: [id.identity, id.soul, id.agents, id.tools].find((f) => f.trim()) ?? "",
    welcome: agent.welcome,
  };
}

/** Where to pick up: the first step the report says is not done. With a
 *  key and no agent the MCP step is next; the agent steps that follow
 *  create it. Everything done lands on the snippet. */
export function resumeStep(report: SetupReport): Step {
  if (!report.llm_key) return "llm";
  if (report.agents === 0 || !report.mcp) return "mcp";
  if (!report.origins) return "origin";
  if (!report.oidc) return "oidc";
  return "snippet";
}

/** Whether the report says `step` is done, for the progress list. The
 *  agent step is done when an agent exists; the snippet when everything is. */
export function stepDone(step: Step, report: SetupReport): boolean {
  switch (step) {
    case "llm":
      return report.llm_key;
    case "mcp":
      return report.mcp;
    case "origin":
      return report.origins;
    case "oidc":
      return report.oidc;
    case "agent":
      return report.agents > 0;
    case "snippet":
      return report.complete;
  }
}

export function nextStep(step: Step): Step | null {
  const i = STEPS.findIndex((s) => s.id === step);
  return STEPS[i + 1]?.id ?? null;
}

export function previousStep(step: Step): Step | null {
  const i = STEPS.findIndex((s) => s.id === step);
  return i > 0 ? STEPS[i - 1].id : null;
}

/** A slug-safe agent id from a display name, as `valid_agent_id` admits:
 *  lowercase letters, digits and dashes, 1–64 chars. */
export function slug(name: string): string {
  return name
    .toLowerCase()
    .normalize("NFKD")
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 64)
    .replace(/-+$/g, "");
}

/** An http(s) URL, parsed. Null for anything else. */
function httpUrl(raw: string): URL | null {
  try {
    const url = new URL(raw.trim());
    return url.protocol === "http:" || url.protocol === "https:" ? url : null;
  } catch {
    return null;
  }
}

/** The origin form of what was typed — scheme, host and port, nothing
 *  else — since the embed matches origins exactly. A path, query or
 *  fragment is a mistake, not something to drop silently. */
export function normaliseOrigin(raw: string): string | null {
  const url = httpUrl(raw);
  if (!url) return null;
  if (url.pathname !== "/" || url.search || url.hash || url.username || url.password) return null;
  return url.origin;
}

/** What stops the operator leaving `step`, or null when they may. The
 *  key step is passable on a key already stored (`keyStored`), since the
 *  wizard never reads a stored key back. */
export function validateStep(step: Step, s: SetupState, keyStored = false): string | null {
  switch (step) {
    case "llm":
      if (!s.provider.trim()) return "Choose a provider.";
      if (!keyStored && !s.key.trim()) return "Paste the provider's API key.";
      return null;
    case "mcp":
      if (s.skip_mcp) return null;
      if (!httpUrl(s.mcp_url)) return "The MCP server URL must be an http(s) URL.";
      if (s.discovered.length === 0) return "Discover the server's tools first.";
      if (s.tools.length === 0) return "Pick at least one tool the agent may use.";
      return null;
    case "origin":
      if (!normaliseOrigin(s.origin))
        return "Enter the origin that will embed the widget: scheme and host only, e.g. https://app.example.com.";
      return null;
    case "oidc": {
      const issuer = s.issuer.trim();
      if (!httpUrl(issuer)) return "The issuer must be an http(s) URL.";
      if (!s.issuer_checked || s.issuer_checked !== issuer) return "Check the issuer first.";
      return null;
    }
    case "agent":
      if (!s.name.trim()) return "Give the agent a name.";
      if (!slug(s.name)) return "The name needs at least one letter or digit.";
      if (!s.persona.trim()) return "Describe the agent's persona.";
      return null;
    case "snippet":
      return null;
  }
}

export function discoverBody(s: SetupState): McpDiscoverRequest {
  const body: McpDiscoverRequest = { url: s.mcp_url.trim() };
  if (s.credential.trim()) body.discovery_credential = s.credential.trim();
  return body;
}

export function oidcCheckBody(s: SetupState, token = ""): OidcCheckRequest {
  const body: OidcCheckRequest = { issuer: s.issuer.trim() };
  if (s.audience.trim()) body.audience = s.audience.trim();
  if (token.trim()) body.token = token.trim();
  return body;
}

/** The chosen tools in the order the server listed them. */
export function toggleTool(s: SetupState, name: string, on: boolean): string[] {
  const chosen = new Set(s.tools);
  if (on) chosen.add(name);
  else chosen.delete(name);
  return s.discovered.filter((t) => chosen.has(t));
}

/** The embed slice every agent body carries: MCP (or none), the chosen
 *  tools, the origin and the issuer in the spelling the check verified.
 *  The credential key is present only when one was typed, so a create
 *  sends none and an edit keeps the stored one (see `McpEdit`). */
function embedSlice(s: SetupState) {
  const credential = s.credential.trim();
  return {
    allowed_origins: [normaliseOrigin(s.origin) ?? s.origin.trim()],
    keycloak_issuer: s.issuer_checked || s.issuer.trim(),
    keycloak_audience: s.audience.trim() || null,
    allowed_tools: s.skip_mcp ? [] : s.tools,
    mcp: s.skip_mcp
      ? null
      : { url: s.mcp_url.trim(), ...(credential ? { discovery_credential: credential } : {}) },
  };
}

/** The persona as identity files: it goes to IDENTITY.md, where the editor
 *  also puts a migrated legacy prompt. */
function identityFiles(s: SetupState) {
  return { soul: "", identity: s.persona.trim(), agents: "", tools: "" };
}

/** The create body for a new agent, its id slugged from the name. */
export function agentCreateBody(s: SetupState): AgentCreate {
  return {
    id: slug(s.name),
    display_name: s.name.trim(),
    identity: identityFiles(s),
    welcome: s.welcome.trim(),
    allowed_emails: [],
    branding: {},
    ...embedSlice(s),
  };
}

/** The edit body for an agent the wizard resumed on: the same fields, the
 *  stored credential kept unless a new one was typed. */
export function agentEditBody(s: SetupState): AgentEdit {
  return {
    display_name: s.name.trim(),
    identity: identityFiles(s),
    welcome: s.welcome.trim(),
    ...embedSlice(s),
  };
}

/** The snippet a host page pastes: the loader, and the handshake that
 *  hands it the signed-in user's token (`wf-embed-token-request` →
 *  `wf-embed-token`, both on the host window, as the loader expects). */
export function embedSnippet(base: string, agentId: string): string {
  const src = `${base.replace(/\/$/, "")}/embed/${encodeURIComponent(agentId)}`;
  return [
    `<script src="${src}"></script>`,
    `<script>`,
    `  // The widget asks for the signed-in user's OIDC id_token; answer with yours.`,
    `  window.addEventListener("message", function (e) {`,
    `    if (e.source !== window || !e.data || e.data.type !== "wf-embed-token-request") return;`,
    `    window.postMessage({ type: "wf-embed-token", token: YOUR_ID_TOKEN }, window.location.origin);`,
    `  });`,
    `</script>`,
  ].join("\n");
}
