import { afterEach, describe, expect, it, vi } from "vitest";

import { checkOidc, discoverMcp, setup, type Agent, type SetupReport } from "./api";
import {
  agentCreateBody,
  agentEditBody,
  discoverBody,
  embedSnippet,
  emptyState,
  nextStep,
  normaliseOrigin,
  oidcCheckBody,
  prefill,
  previousStep,
  resumeStep,
  slug,
  stepDone,
  toggleTool,
  validateStep,
  type SetupState,
} from "./setupForm";

/** A `GET /admin/api/setup` body as server.rs's SetupReport serialises it. */
const bare: SetupReport = {
  llm_key: false,
  llm_status: { state: "unset", checked_at: "2026-10-09T09:00:00Z", message: "" },
  editable: true,
  agents: 0,
  agent: null,
  mcp: false,
  origins: false,
  oidc: false,
  demo: false,
  complete: false,
};

const done: SetupReport = {
  ...bare,
  llm_key: true,
  llm_status: { state: "ok", checked_at: "2026-10-09T09:00:00Z", message: "" },
  agents: 1,
  agent: "support",
  mcp: true,
  origins: true,
  oidc: true,
  demo: true,
  complete: true,
};

/** An agent as `GET /admin/api/agents` lists it, bound to everything. */
const agent: Agent = {
  id: "support",
  display_name: "Acme Support",
  identity: { soul: "", identity: "You help Acme staff.", agents: "", tools: "" },
  model: null,
  welcome: "Hello!",
  playbooks: [],
  auto_playbook: null,
  allowed_origins: ["https://app.acme.test"],
  allowed_emails: [],
  keycloak_issuer: "https://auth.acme.test/realms/acme",
  keycloak_audience: "widget",
  allowed_tools: ["acme.list_orders", "acme.item_transition"],
  branding: {},
  data_tables: false,
  charts: false,
  forms: false,
  maps: false,
  diagrams: false,
  excalidraw_export: false,
  inline_images: false,
  images: false,
  uploads: [],
  upload_max_mb: null,
  drawing: false,
  has_mcp: true,
  mcp_url: "https://mcp.acme.test/mcp",
  has_discovery_credential: true,
  permissions: null,
  tool_grants: [],
  policy: null,
  image_hosts: [],
  spill_skip_tools: [],
  passive_memory: true,
};

/** A state every step passes on. */
const filled = (): SetupState => ({
  ...emptyState("anthropic"),
  key: "sk-ant-1",
  mcp_url: "https://mcp.acme.test/mcp",
  discovered: ["acme.list_orders", "acme.item_transition", "acme.bulk_advance"],
  tools: ["acme.list_orders", "acme.item_transition"],
  origin: "https://app.acme.test",
  issuer: "https://auth.acme.test/realms/acme",
  audience: "widget",
  issuer_checked: "https://auth.acme.test/realms/acme",
  name: "Acme Support",
  persona: "You help Acme staff.",
  welcome: "Hello!",
});

describe("resume", () => {
  it("starts at the key, then the MCP step once a key is stored", () => {
    expect(resumeStep(bare)).toBe("llm");
    expect(resumeStep({ ...bare, llm_key: true })).toBe("mcp");
  });

  it("goes to the first unbound step of an existing agent", () => {
    expect(resumeStep({ ...done, complete: false, mcp: false })).toBe("mcp");
    expect(resumeStep({ ...done, complete: false, origins: false })).toBe("origin");
    expect(resumeStep({ ...done, complete: false, oidc: false })).toBe("oidc");
  });

  it("lands on the snippet when everything is done", () => {
    expect(resumeStep(done)).toBe("snippet");
  });

  it("marks each step from the report", () => {
    expect(stepDone("llm", bare)).toBe(false);
    expect(stepDone("agent", bare)).toBe(false);
    for (const step of ["llm", "mcp", "origin", "oidc", "agent", "snippet"] as const) {
      expect(stepDone(step, done)).toBe(true);
    }
    expect(stepDone("snippet", { ...done, complete: false })).toBe(false);
  });

  it("walks the steps in order", () => {
    expect(nextStep("llm")).toBe("mcp");
    expect(nextStep("agent")).toBe("snippet");
    expect(nextStep("snippet")).toBeNull();
    expect(previousStep("mcp")).toBe("llm");
    expect(previousStep("llm")).toBeNull();
  });
});

describe("prefill", () => {
  it("is empty without an agent", () => {
    const s = prefill("anthropic", null);
    expect(s).toEqual(emptyState("anthropic"));
    expect(s.key).toBe("");
  });

  it("takes the agent's bindings, counting its MCP and issuer as verified", () => {
    const s = prefill("anthropic", agent);
    expect(s.mcp_url).toBe("https://mcp.acme.test/mcp");
    expect(s.discovered).toEqual(agent.allowed_tools);
    expect(s.tools).toEqual(agent.allowed_tools);
    expect(s.credential).toBe("");
    expect(s.origin).toBe("https://app.acme.test");
    expect(s.issuer).toBe("https://auth.acme.test/realms/acme");
    expect(s.issuer_checked).toBe("https://auth.acme.test/realms/acme");
    expect(s.audience).toBe("widget");
    expect(s.name).toBe("Acme Support");
    expect(s.persona).toBe("You help Acme staff.");
    expect(s.welcome).toBe("Hello!");
    for (const step of ["mcp", "origin", "oidc", "agent"] as const) {
      expect(validateStep(step, s)).toBeNull();
    }
  });

  it("falls back through the identity files for the persona", () => {
    const soulOnly = { ...agent, identity: { soul: "Warm and brief.", identity: "", agents: "", tools: "" } };
    expect(prefill("anthropic", soulOnly).persona).toBe("Warm and brief.");
    const pureChat = { ...agent, has_mcp: false, mcp_url: null, allowed_tools: [] };
    const s = prefill("anthropic", pureChat);
    expect(s.discovered).toEqual([]);
    expect(validateStep("mcp", s)).toBe("The MCP server URL must be an http(s) URL.");
  });
});

describe("step validation", () => {
  it("needs a provider and a key, unless one is already stored", () => {
    const s = filled();
    expect(validateStep("llm", s)).toBeNull();
    expect(validateStep("llm", { ...s, key: " " })).toBe("Paste the provider's API key.");
    expect(validateStep("llm", { ...s, key: "" }, true)).toBeNull();
    expect(validateStep("llm", { ...s, provider: "" })).toBe("Choose a provider.");
  });

  it("needs a discovered server and a chosen tool, or an explicit skip", () => {
    const s = filled();
    expect(validateStep("mcp", s)).toBeNull();
    expect(validateStep("mcp", { ...s, mcp_url: "mcp.acme.test" })).toMatch(/http\(s\) URL/);
    expect(validateStep("mcp", { ...s, mcp_url: "ftp://mcp.acme.test" })).toMatch(/http\(s\) URL/);
    expect(validateStep("mcp", { ...s, discovered: [] })).toBe("Discover the server's tools first.");
    expect(validateStep("mcp", { ...s, tools: [] })).toBe("Pick at least one tool the agent may use.");
    expect(validateStep("mcp", { ...s, mcp_url: "", discovered: [], tools: [], skip_mcp: true })).toBeNull();
  });

  it("needs an origin in origin form", () => {
    const s = filled();
    expect(validateStep("origin", s)).toBeNull();
    expect(validateStep("origin", { ...s, origin: "https://app.acme.test/" })).toBeNull();
    expect(validateStep("origin", { ...s, origin: "app.acme.test" })).toMatch(/scheme and host only/);
    expect(validateStep("origin", { ...s, origin: "https://app.acme.test/login" })).toMatch(/scheme and host only/);
    expect(validateStep("origin", { ...s, origin: "https://app.acme.test/?x=1" })).toMatch(/scheme and host only/);
  });

  it("needs the issuer checked, as typed", () => {
    const s = filled();
    expect(validateStep("oidc", s)).toBeNull();
    expect(validateStep("oidc", { ...s, issuer_checked: "" })).toBe("Check the issuer first.");
    expect(validateStep("oidc", { ...s, issuer: "https://other.test" })).toBe("Check the issuer first.");
    expect(validateStep("oidc", { ...s, issuer: "auth.acme.test" })).toBe("The issuer must be an http(s) URL.");
  });

  it("needs a name that slugs and a persona", () => {
    const s = filled();
    expect(validateStep("agent", s)).toBeNull();
    expect(validateStep("agent", { ...s, name: " " })).toBe("Give the agent a name.");
    expect(validateStep("agent", { ...s, name: "!!!" })).toBe("The name needs at least one letter or digit.");
    expect(validateStep("agent", { ...s, persona: "" })).toBe("Describe the agent's persona.");
    expect(validateStep("snippet", s)).toBeNull();
  });
});

describe("origins and ids", () => {
  it("normalises an origin to scheme, host and port", () => {
    expect(normaliseOrigin("https://app.acme.test/")).toBe("https://app.acme.test");
    expect(normaliseOrigin(" HTTPS://App.Acme.test:8443 ")).toBe("https://app.acme.test:8443");
    expect(normaliseOrigin("http://localhost:3000")).toBe("http://localhost:3000");
    expect(normaliseOrigin("https://app.acme.test/app")).toBeNull();
    expect(normaliseOrigin("https://u:p@app.acme.test")).toBeNull();
    expect(normaliseOrigin("ws://app.acme.test")).toBeNull();
    expect(normaliseOrigin("nope")).toBeNull();
  });

  it("slugs a display name into a valid agent id", () => {
    expect(slug("Acme Support")).toBe("acme-support");
    expect(slug("  Café — Help!  ")).toBe("cafe-help");
    expect(slug("!!!")).toBe("");
    expect(slug("a".repeat(70))).toHaveLength(64);
    expect(slug(`${"a".repeat(63)} b`)).toBe("a".repeat(63));
  });
});

describe("request bodies", () => {
  it("sends the credential to discovery only when typed", () => {
    const s = filled();
    expect(discoverBody(s)).toEqual({ url: "https://mcp.acme.test/mcp" });
    expect(discoverBody({ ...s, mcp_url: " https://mcp.acme.test/mcp ", credential: " svc " })).toEqual({
      url: "https://mcp.acme.test/mcp",
      discovery_credential: "svc",
    });
  });

  it("sends the audience and token to the issuer check only when given", () => {
    const s = filled();
    expect(oidcCheckBody({ ...s, audience: "" })).toEqual({ issuer: "https://auth.acme.test/realms/acme" });
    expect(oidcCheckBody(s, " eyJ.x.y ")).toEqual({
      issuer: "https://auth.acme.test/realms/acme",
      audience: "widget",
      token: "eyJ.x.y",
    });
  });

  it("keeps chosen tools in the server's order", () => {
    const s = filled();
    expect(toggleTool(s, "acme.bulk_advance", true)).toEqual([
      "acme.list_orders",
      "acme.item_transition",
      "acme.bulk_advance",
    ]);
    expect(toggleTool(s, "acme.list_orders", false)).toEqual(["acme.item_transition"]);
    expect(toggleTool({ ...s, tools: [] }, "acme.bulk_advance", true)).toEqual(["acme.bulk_advance"]);
  });

  it("builds the create body the agents route takes, credential absent unless typed", () => {
    const s = filled();
    expect(agentCreateBody(s)).toEqual({
      id: "acme-support",
      display_name: "Acme Support",
      identity: { soul: "", identity: "You help Acme staff.", agents: "", tools: "" },
      welcome: "Hello!",
      allowed_emails: [],
      branding: {},
      allowed_origins: ["https://app.acme.test"],
      keycloak_issuer: "https://auth.acme.test/realms/acme",
      keycloak_audience: "widget",
      allowed_tools: ["acme.list_orders", "acme.item_transition"],
      mcp: { url: "https://mcp.acme.test/mcp" },
    });
    const body = agentCreateBody({ ...s, credential: "svc-1", audience: " ", origin: "https://app.acme.test/" });
    expect(body.mcp).toEqual({ url: "https://mcp.acme.test/mcp", discovery_credential: "svc-1" });
    expect(body.keycloak_audience).toBeNull();
    expect(body.allowed_origins).toEqual(["https://app.acme.test"]);
    expect(JSON.stringify(agentCreateBody(s))).not.toContain("sk-ant-1");
  });

  it("binds the issuer in the spelling the check verified", () => {
    const s = { ...filled(), issuer: "https://auth.acme.test/realms/acme/", issuer_checked: "https://auth.acme.test/realms/acme" };
    expect(agentCreateBody(s).keycloak_issuer).toBe("https://auth.acme.test/realms/acme");
  });

  it("makes a pure-chat agent when the MCP step was skipped", () => {
    const body = agentCreateBody({ ...filled(), skip_mcp: true });
    expect(body.mcp).toBeNull();
    expect(body.allowed_tools).toEqual([]);
  });

  it("builds an edit body without the id, keeping a stored credential by saying nothing", () => {
    const edit = agentEditBody(filled());
    expect(edit).not.toHaveProperty("id");
    expect(edit.mcp).toEqual({ url: "https://mcp.acme.test/mcp" });
    expect(edit.display_name).toBe("Acme Support");
    expect(edit.allowed_tools).toEqual(["acme.list_orders", "acme.item_transition"]);
  });
});

describe("snippet", () => {
  it("loads the agent from the edge host and answers the token handshake", () => {
    const html = embedSnippet("https://acme.marrilo.com/", "acme support");
    expect(html).toContain('<script src="https://acme.marrilo.com/embed/acme%20support"></script>');
    expect(html).toContain('e.data.type !== "wf-embed-token-request"');
    expect(html).toContain('{ type: "wf-embed-token", token: YOUR_ID_TOKEN }');
    expect(html).toContain("window.location.origin");
  });
});

const fetchAnswering = (status: number, body: unknown) => {
  const fn = vi.fn(async () => new Response(JSON.stringify(body), { status }));
  vi.stubGlobal("fetch", fn);
  return fn;
};

describe("setup api", () => {
  afterEach(() => vi.unstubAllGlobals());

  it("reads the setup report", async () => {
    const fn = fetchAnswering(200, done);
    expect(await setup()).toEqual(done);
    const [url] = fn.mock.calls[0] as unknown as [string];
    expect(url).toBe("/admin/api/setup");
  });

  it("posts the discovery body and unwraps the survey", async () => {
    const survey = { server: "acme-mcp", version: "1.2", protocol_version: "2025-06-18", tools: [{ name: "acme.list_orders", description: "d" }], truncated: false };
    const fn = fetchAnswering(200, { ok: true, survey });
    expect(await discoverMcp({ url: "https://mcp.acme.test/mcp", discovery_credential: "svc" })).toEqual(survey);
    const [url, init] = fn.mock.calls[0] as unknown as [string, RequestInit];
    expect(url).toBe("/admin/api/mcp/discover");
    expect(init.method).toBe("POST");
    expect(JSON.parse(init.body as string)).toEqual({ url: "https://mcp.acme.test/mcp", discovery_credential: "svc" });
  });

  it("rejects discovery with the server's reason", async () => {
    fetchAnswering(502, { error: "MCP initialize failed: transport error" });
    await expect(discoverMcp({ url: "https://mcp.acme.test/mcp" })).rejects.toThrow("MCP initialize failed");
    fetchAnswering(422, { error: "refusing the MCP URL: only https URLs may be called" });
    await expect(discoverMcp({ url: "http://mcp.acme.test/mcp" })).rejects.toThrow("only https");
  });

  it("posts the issuer check and returns the verdicts", async () => {
    const check = {
      issuer_matches: true,
      report: { issuer: "https://auth.acme.test/realms/acme", jwks_uri: "https://auth.acme.test/jwks", algs: ["RS256"], keys: 2 },
      token: { ok: false, error: "token audience/azp does not match `widget`" },
    };
    const fn = fetchAnswering(200, { ok: true, ...check });
    const got = await checkOidc({ issuer: "https://auth.acme.test/realms/acme", audience: "widget", token: "t" });
    expect(got.issuer_matches).toBe(true);
    expect(got.report).toEqual(check.report);
    expect(got.token).toEqual(check.token);
    const [url, init] = fn.mock.calls[0] as unknown as [string, RequestInit];
    expect(url).toBe("/admin/api/oidc/check");
    expect(JSON.parse(init.body as string)).toEqual({ issuer: "https://auth.acme.test/realms/acme", audience: "widget", token: "t" });
  });
});
