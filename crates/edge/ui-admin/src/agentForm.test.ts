import { afterEach, describe, expect, it, vi } from "vitest";

import { importGit, UnresolvedPlaceholders, type Agent } from "./api";
import {
  concreteTools,
  describeImport,
  importRequest,
  integrationEdit,
  integrationForm,
  withUnresolved,
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
  permissions: { url: "https://api.acme.test/users/{user}/permissions", cache_ttl_secs: 60 },
  tool_grants: [{ source: "backend_permissions", requires: "items:edit", tools: ["acme.item_transition"] }],
  policy: { confirm_writes: true },
  image_hosts: ["cdn.acme.test"],
  spill_skip_tools: ["acme.reports.signed_urls"],
  passive_memory: false,
};

describe("integrationEdit", () => {
  it("loads the agent's integration fields and never a credential", () => {
    const f = integrationForm(agent);
    expect(f.mcp_url).toBe("https://mcp.acme.test/mcp");
    expect(f.credential).toBe("");
    expect(f.clear_credential).toBe(false);
    expect(f.permissions_ttl).toBe("60");
    expect(f.tool_grants).toEqual([
      { source: "backend_permissions", requires: "items:edit", tools: "acme.item_transition" },
    ]);
    expect(integrationForm().passive_memory).toBe(true);
  });

  it("round-trips the loaded form without naming the credential", () => {
    const body = integrationEdit(integrationForm(agent));
    expect(body).toEqual({
      mcp: { url: "https://mcp.acme.test/mcp" },
      permissions: { url: "https://api.acme.test/users/{user}/permissions", cache_ttl_secs: 60 },
      tool_grants: [{ source: "backend_permissions", requires: "items:edit", tools: ["acme.item_transition"] }],
      policy: { confirm_writes: true },
      image_hosts: ["cdn.acme.test"],
      spill_skip_tools: ["acme.reports.signed_urls"],
      passive_memory: false,
    });
    expect("discovery_credential" in body.mcp!).toBe(false);
  });

  it("sends the credential only when typed, and the empty string to clear", () => {
    const typed = integrationEdit({ ...integrationForm(agent), credential: "  svc-9f8e  " });
    expect(typed.mcp?.discovery_credential).toBe("svc-9f8e");
    const cleared = integrationEdit({ ...integrationForm(agent), credential: "ignored", clear_credential: true });
    expect(cleared.mcp?.discovery_credential).toBe("");
    const blank = integrationEdit({ ...integrationForm(agent), credential: "   " });
    expect("discovery_credential" in blank.mcp!).toBe(false);
  });

  it("drops the blocks a blank field means to remove", () => {
    const body = integrationEdit({
      ...integrationForm(agent),
      mcp_url: " ",
      credential: "would be lost anyway",
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

  it("offers exact names, not globs, to the spill-skip picker", () => {
    expect(
      concreteTools(agent.allowed_tools, [
        { source: "host_hint", requires: "x", tools: "acme.item_transition acme.list_orders" },
      ]),
    ).toEqual(["acme.list_orders", "acme.item_transition"]);
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
