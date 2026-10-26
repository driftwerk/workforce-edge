import { afterEach, describe, expect, it, vi } from "vitest";

import { importGit, UnresolvedPlaceholders, type Agent } from "./api";
import {
  concreteTools,
  credentialField,
  describeImport,
  emptyServerRow,
  exposedName,
  importRequest,
  integrationEdit,
  integrationForm,
  mcpServerRows,
  toggleName,
  withUnresolved,
  type IntegrationForm,
} from "./agentForm";

const agent: Agent = {
  id: "assist",
  display_name: "Assist",
  identity: { soul: "", identity: "You help.", agents: "", tools: "" },
  model: null,
  welcome: "",
  playbooks: [],
  auto_playbook: null,
  allowed_origins: ["https://app.acme.test"],
  allowed_emails: [],
  keycloak_issuer: "https://kc.acme.test/realms/staff",
  keycloak_audience: null,
  allowed_tools: ["acme.list_orders", "acme.reports.*"],
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
  mcp_settings: {
    id: "default",
    url: "https://mcp.acme.test/mcp",
    auth: "exchanged",
    audience: null,
    required: true,
    unavailable_skip_secs: 60,
    has_discovery_credential: true,
    tool_prefix: null,
    allowed_tools: ["acme.list_orders", "acme.reports.*"],
    tool_grants: [],
  },
  mcp_servers: [
    {
      id: "docs",
      url: "https://docs-mcp.acme.test/mcp",
      auth: "exchanged",
      audience: "docs-mcp",
      required: false,
      unavailable_skip_secs: 30,
      has_discovery_credential: true,
      tool_prefix: "docs",
      allowed_tools: ["docs.search", "docs.pages.*"],
      tool_grants: [{ source: "keycloak_role", requires: "editor", tools: ["docs.edit_page"] }],
    },
  ],
  permissions: { url: "https://api.acme.test/users/{user}/permissions", cache_ttl_secs: 60 },
  tool_grants: [{ source: "backend_permissions", requires: "items:edit", tools: ["acme.item_transition"] }],
  policy: { confirm_writes: true },
  image_hosts: ["cdn.acme.test"],
  spill_skip_tools: ["acme.reports.signed_urls"],
  passive_memory: false,
};

/** The form with the primary connection patched. */
const withPrimary = (patch: Partial<IntegrationForm["mcp"]>): IntegrationForm => {
  const f = integrationForm(agent);
  return { ...f, mcp: { ...f.mcp, ...patch } };
};

describe("integrationEdit", () => {
  it("loads the agent's integration fields and never a credential", () => {
    const f = integrationForm(agent);
    expect(f.mcp.url).toBe("https://mcp.acme.test/mcp");
    expect(f.mcp.credential).toBe("");
    expect(f.mcp.clear_credential).toBe(false);
    expect(f.mcp.has_credential).toBe(true);
    expect(f.mcp.required).toBe(true);
    expect(f.permissions_ttl).toBe("60");
    expect(f.tool_grants).toEqual([
      { source: "backend_permissions", requires: "items:edit", tools: "acme.item_transition" },
    ]);
    expect(integrationForm().passive_memory).toBe(true);
    expect(integrationForm().mcp.required).toBe(true);
    expect(integrationForm().mcp_servers).toEqual([]);
  });

  it("round-trips the loaded form without naming a credential", () => {
    const body = integrationEdit(integrationForm(agent));
    expect(body).toEqual({
      mcp: {
        url: "https://mcp.acme.test/mcp",
        auth: "exchanged",
        audience: null,
        required: true,
        unavailable_skip_secs: null,
      },
      mcp_servers: [
        {
          id: "docs",
          url: "https://docs-mcp.acme.test/mcp",
          auth: "exchanged",
          audience: "docs-mcp",
          required: false,
          unavailable_skip_secs: 30,
          tool_prefix: "docs",
          allowed_tools: ["docs.search", "docs.pages.*"],
          tool_grants: [{ source: "keycloak_role", requires: "editor", tools: ["docs.edit_page"] }],
        },
      ],
      permissions: { url: "https://api.acme.test/users/{user}/permissions", cache_ttl_secs: 60 },
      tool_grants: [{ source: "backend_permissions", requires: "items:edit", tools: ["acme.item_transition"] }],
      policy: { confirm_writes: true },
      image_hosts: ["cdn.acme.test"],
      spill_skip_tools: ["acme.reports.signed_urls"],
      passive_memory: false,
    });
    expect("discovery_credential" in body.mcp!).toBe(false);
    expect("discovery_credential" in body.mcp_servers[0]).toBe(false);
  });

  it("sends the credential only when typed, and the empty string to clear", () => {
    const typed = integrationEdit(withPrimary({ credential: "  svc-9f8e  " }));
    expect(typed.mcp?.discovery_credential).toBe("svc-9f8e");
    const cleared = integrationEdit(withPrimary({ credential: "ignored", clear_credential: true }));
    expect(cleared.mcp?.discovery_credential).toBe("");
    const blank = integrationEdit(withPrimary({ credential: "   " }));
    expect("discovery_credential" in blank.mcp!).toBe(false);
    expect(credentialField(" x ", false)).toEqual({ discovery_credential: "x" });
    expect(credentialField("", true)).toEqual({ discovery_credential: "" });
    expect(credentialField(" ", false)).toEqual({});
  });

  it("sends a skip window only for a server that is not required, and an audience only when exchanged", () => {
    const optional = integrationEdit(withPrimary({ required: false, unavailable_skip_secs: "15" }));
    expect(optional.mcp).toMatchObject({ required: false, unavailable_skip_secs: 15 });
    const required = integrationEdit(withPrimary({ required: true, unavailable_skip_secs: "15" }));
    expect(required.mcp?.unavailable_skip_secs).toBeNull();
    const forwarded = integrationEdit(withPrimary({ auth: "forwarded", audience: "acme-mcp" }));
    expect(forwarded.mcp).toMatchObject({ auth: "forwarded", audience: null });
    const exchanged = integrationEdit(withPrimary({ audience: " acme-mcp " }));
    expect(exchanged.mcp?.audience).toBe("acme-mcp");
  });

  it("drops the blocks a blank field means to remove", () => {
    const body = integrationEdit({
      ...withPrimary({ url: " ", credential: "would be lost anyway" }),
      permissions_url: "",
      confirm_writes: false,
      image_hosts: " \n",
      tool_grants: [{ source: "host_hint", requires: " ", tools: " , " }],
    });
    expect(body.mcp).toBeNull();
    expect(body.permissions).toBeNull();
    expect(body.policy).toBeNull();
    expect(body.image_hosts).toEqual([]);
    expect(body.tool_grants).toEqual([]);
  });

  it("parses the TTL and tool names, keeping a half-filled grant for the server to name", () => {
    const body = integrationEdit({
      ...integrationForm(agent),
      permissions_ttl: "abc",
      tool_grants: [{ source: "keycloak_role", requires: "", tools: "acme.a, acme.b\nacme.c" }],
    });
    expect(body.permissions?.cache_ttl_secs).toBeNull();
    expect(body.tool_grants).toEqual([
      { source: "keycloak_role", requires: "", tools: ["acme.a", "acme.b", "acme.c"] },
    ]);
    expect(integrationEdit({ ...integrationForm(agent), permissions_ttl: "0" }).permissions?.cache_ttl_secs).toBeNull();
  });

  it("offers every server's exact names, not globs, to the spill-skip picker", () => {
    expect(
      concreteTools(
        agent.allowed_tools,
        [{ source: "host_hint", requires: "x", tools: "acme.item_transition acme.list_orders" }],
        mcpServerRows(agent),
      ),
    ).toEqual(["acme.list_orders", "acme.item_transition", "docs.search", "docs.edit_page"]);
  });
});

describe("additional MCP servers", () => {
  it("maps an agent's servers to rows, never with a credential", () => {
    expect(mcpServerRows(agent)).toEqual([
      {
        id: "docs",
        url: "https://docs-mcp.acme.test/mcp",
        credential: "",
        clear_credential: false,
        has_credential: true,
        auth: "exchanged",
        audience: "docs-mcp",
        required: false,
        unavailable_skip_secs: "30",
        tool_prefix: "docs",
        allowed_tools: "docs.search docs.pages.*",
        tool_grants: [{ source: "keycloak_role", requires: "editor", tools: "docs.edit_page" }],
      },
    ]);
    expect(mcpServerRows()).toEqual([]);
  });

  it("sends a server's credential only when typed or cleared, and drops fully blank rows", () => {
    const f = integrationForm(agent);
    const [docs] = f.mcp_servers;
    const body = integrationEdit({
      ...f,
      mcp_servers: [
        { ...docs, credential: " docs-svc " },
        emptyServerRow(),
        { ...emptyServerRow(), id: "half", url: "" },
      ],
    });
    expect(body.mcp_servers.map((s) => s.id)).toEqual(["docs", "half"]);
    expect(body.mcp_servers[0].discovery_credential).toBe("docs-svc");
    expect("discovery_credential" in body.mcp_servers[1]).toBe(false);
    expect(body.mcp_servers[1]).toMatchObject({ url: "", tool_prefix: null, allowed_tools: [], required: false });
    const cleared = integrationEdit({ ...f, mcp_servers: [{ ...docs, clear_credential: true }] });
    expect(cleared.mcp_servers[0].discovery_credential).toBe("");
  });

  it("always sends the list, so removing every row removes them all", () => {
    expect(integrationEdit({ ...integrationForm(agent), mcp_servers: [] }).mcp_servers).toEqual([]);
  });

  it("names discovered tools under the prefix and toggles them in the allowlist", () => {
    expect(exposedName(" docs ", "search")).toBe("docs.search");
    expect(exposedName("", "search")).toBe("search");
    expect(toggleName("docs.search", "docs.get_page", true)).toBe("docs.search docs.get_page");
    expect(toggleName("docs.search docs.get_page", "docs.search", true)).toBe("docs.search docs.get_page");
    expect(toggleName("docs.search, docs.get_page", "docs.search", false)).toBe("docs.get_page");
  });
});

describe("import from git", () => {
  afterEach(() => vi.unstubAllGlobals());

  it("builds the request from the form, omitting what was left blank", () => {
    expect(
      importRequest({
        repo: " acme/edge-config ",
        git_ref: " ",
        token: " ghp_x ",
        env: [{ name: " ", value: "dropped" }],
      }),
    ).toEqual({ repo: "acme/edge-config", token: "ghp_x" });
    expect(
      importRequest({
        repo: "acme/edge-config",
        git_ref: "release",
        token: "ghp_x",
        env: [
          { name: " EDGE_KC_ISSUER ", value: "https://kc" },
          { name: "EDGE_MCP_URL", value: "https://mcp" },
        ],
      }),
    ).toEqual({
      repo: "acme/edge-config",
      git_ref: "release",
      token: "ghp_x",
      env: { EDGE_KC_ISSUER: "https://kc", EDGE_MCP_URL: "https://mcp" },
    });
  });

  it("posts the request as JSON and parses the report", async () => {
    const fetchMock = vi.fn(async () =>
      new Response(JSON.stringify({ agents: 1, playbooks: 2, training_docs: 0, unresolved: [] }), {
        status: 200,
      }),
    );
    vi.stubGlobal("fetch", fetchMock);
    const report = await importGit({ repo: "acme/edge-config", token: "ghp_x", env: { A: "1" } });
    expect(report.playbooks).toBe(2);
    expect(describeImport(report)).toBe("Imported 1 agent, 2 playbooks, 0 training docs.");
    const [url, init] = fetchMock.mock.calls[0] as unknown as [string, RequestInit];
    expect(url).toBe("/admin/api/import/git");
    expect(init.method).toBe("POST");
    expect(init.credentials).toBe("same-origin");
    expect(JSON.parse(init.body as string)).toEqual({ repo: "acme/edge-config", token: "ghp_x", env: { A: "1" } });
  });

  it("turns a 422 with unresolved names into rows to fill", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () =>
        new Response(JSON.stringify({ error: "2 placeholders unbound", unresolved: ["EDGE_KC_ISSUER", "EDGE_MCP_URL"] }), {
          status: 422,
        }),
      ),
    );
    const err = await importGit({ repo: "acme/edge-config", token: "ghp_x" }).catch((e) => e);
    expect(err).toBeInstanceOf(UnresolvedPlaceholders);
    expect((err as UnresolvedPlaceholders).names).toEqual(["EDGE_KC_ISSUER", "EDGE_MCP_URL"]);
    expect(withUnresolved([{ name: "EDGE_MCP_URL", value: "https://mcp" }], (err as UnresolvedPlaceholders).names)).toEqual([
      { name: "EDGE_MCP_URL", value: "https://mcp" },
      { name: "EDGE_KC_ISSUER", value: "" },
    ]);
  });

  it("rejects with the server's reason otherwise", async () => {
    vi.stubGlobal("fetch", vi.fn(async () => new Response(JSON.stringify({ error: "this install is managed in git" }), { status: 409 })));
    await expect(importGit({ repo: "acme/edge-config", token: "ghp_x" })).rejects.toThrow("managed in git");
  });
});
