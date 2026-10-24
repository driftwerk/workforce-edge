import { afterEach, describe, expect, it, vi } from "vitest";

import {
  clearLlmKey,
  getSettings,
  llmBanner,
  llmStatus,
  setLlmKey,
  setSecret,
  type LlmStatus,
  type SettingsView,
} from "./api";
import { keyFieldAfterSubmit, providerChoices, toForm, toUpdate } from "./settingsForm";

/** A JSON key that would carry a secret — distinct from the `has_…` flags. */
const SECRET_KEY = /"(api_key|client_secret|password_hash|session_secret)"/;

const okStatus: LlmStatus = { state: "ok", checked_at: "2026-10-08T09:00:00Z", message: "" };

/** A `GET /admin/api/settings` body as server.rs's SettingsView serialises it. */
const view: SettingsView = {
  llm: {
    provider: "anthropic",
    allowed_providers: ["anthropic", "openrouter"],
    tier_key: null,
    default_model: "standard",
    failover: ["gemini:standard"],
    retry: { max_retries: 3, initial_backoff_ms: 1000, backoff_multiplier: 2, max_backoff_ms: 8000 },
  },
  grader: { enabled: true, model: "cheap", sample_rate: 0.25 },
  token_exchange: { issuer: "https://kc.example/realms/main", client_id: "edge", audience: null, scope: "openid" },
  admin: {
    oidc: { issuer: "https://accounts.google.com", audience: "cid", client_id: "cid", mode: "google" },
    allowed_emails: ["ops@acme.example"],
    allowed_domains: [],
    public_url: null,
    session_hours: 12,
  },
  limits: { rate_limit_per_min: 120, user_rate_limit_per_min: 20, max_concurrent_turns: 8 },
  audit: null,
  outbound: "public",
  llm_status: okStatus,
  has_llm_key: true,
  has_token_exchange_secret: true,
  has_admin_client_secret: false,
  stored: true,
  editable: true,
};

const fetchAnswering = (status: number, body: unknown) => {
  const fn = vi.fn(async () => new Response(JSON.stringify(body), { status }));
  vi.stubGlobal("fetch", fn);
  return fn;
};

describe("settings api", () => {
  afterEach(() => vi.unstubAllGlobals());

  it("parses GET /admin/api/settings into the view, secrets as flags only", async () => {
    fetchAnswering(200, view);
    const got = await getSettings();
    expect(got.llm.provider).toBe("anthropic");
    expect(got.llm.allowed_providers).toEqual(["anthropic", "openrouter"]);
    expect(got.has_llm_key).toBe(true);
    expect(got.has_admin_client_secret).toBe(false);
    expect(got.editable).toBe(true);
    expect(got.llm_status.state).toBe("ok");
    expect(JSON.stringify(got)).not.toMatch(SECRET_KEY);
  });

  it("PUT llm-key: 200 resolves ok with the verified status and updates the shell's copy", async () => {
    const fn = fetchAnswering(200, { ok: true, llm_status: okStatus });
    const out = await setLlmKey("sk-test");
    expect(out).toEqual({ ok: true, llm_status: okStatus });
    expect(llmStatus()).toEqual(okStatus);
    const [url, init] = fn.mock.calls[0] as unknown as [string, RequestInit];
    expect(url).toBe("/admin/api/settings/llm-key");
    expect(init.method).toBe("PUT");
    expect(JSON.parse(init.body as string)).toEqual({ key: "sk-test" });
  });

  it("PUT llm-key: 422 resolves not-ok with the server's fixed message (key not stored)", async () => {
    const failed: LlmStatus = {
      state: "failed",
      checked_at: "2026-10-08T09:01:00Z",
      message: "The provider rejected the key as unauthorized.",
    };
    fetchAnswering(422, { error: failed.message, llm_status: failed });
    const out = await setLlmKey("sk-bad");
    expect(out.ok).toBe(false);
    if (!out.ok) expect(out.error).toBe("The provider rejected the key as unauthorized.");
    expect(llmStatus()?.state).toBe("failed");
  });

  it("PUT llm-key: 409 (not editable) rejects with the server's reason", async () => {
    fetchAnswering(409, { error: "EDGE_MASTER_KEY is not set: this install's settings are read from edge.toml" });
    await expect(setLlmKey("sk-x")).rejects.toThrow("EDGE_MASTER_KEY is not set");
  });

  it("DELETE llm-key leaves the shell's status unset", async () => {
    const fn = fetchAnswering(200, { ok: true });
    await clearLlmKey();
    expect(llmStatus()?.state).toBe("unset");
    const [url, init] = fn.mock.calls[0] as unknown as [string, RequestInit];
    expect(url).toBe("/admin/api/settings/llm-key");
    expect(init.method).toBe("DELETE");
  });

  it("stored secrets go to their own route as {secret}", async () => {
    const fn = fetchAnswering(200, { ok: true });
    await setSecret("admin-client-secret", "shh");
    const [url, init] = fn.mock.calls[0] as unknown as [string, RequestInit];
    expect(url).toBe("/admin/api/settings/admin-client-secret");
    expect(JSON.parse(init.body as string)).toEqual({ secret: "shh" });
  });
});

describe("llmBanner", () => {
  it("is hidden when the key is ok or unknown", () => {
    expect(llmBanner(okStatus)).toBeNull();
    expect(llmBanner(null)).toBeNull();
  });

  it("says the assistant cannot answer when no key is set", () => {
    const b = llmBanner({ state: "unset", checked_at: "2026-10-08T09:00:00Z", message: "" });
    expect(b?.text).toBe("No LLM key — the assistant cannot answer.");
    expect(b?.link).toBe("Set one in Settings.");
  });

  it("carries the backend's fixed message when the key failed", () => {
    const b = llmBanner({
      state: "failed",
      checked_at: "2026-10-08T09:00:00Z",
      message: "The provider could not be reached.",
    });
    expect(b?.text).toBe("The provider could not be reached.");
    expect(b?.link).toBe("Replace the key in Settings.");
  });
});

describe("settings form", () => {
  it("round-trips the view through the form into an identical PUT body, outbound left out", () => {
    const out = toUpdate(toForm(view), view);
    expect(out.ok).toBe(true);
    if (!out.ok) return;
    const { llm_status, has_llm_key, has_token_exchange_secret, has_admin_client_secret, stored, editable, outbound, ...sections } = view;
    void [llm_status, has_llm_key, has_token_exchange_secret, has_admin_client_secret, stored, editable, outbound];
    expect(out.body).toEqual(sections);
    expect(out.body).not.toHaveProperty("outbound");
  });

  it("never puts a secret field in the PUT body", () => {
    const out = toUpdate(toForm(view), view);
    expect(out.ok).toBe(true);
    if (!out.ok) return;
    const body = out.body as unknown as Record<string, Record<string, unknown>>;
    expect(body.llm).not.toHaveProperty("api_key");
    expect(body.grader).not.toHaveProperty("api_key");
    expect(body.token_exchange).not.toHaveProperty("client_secret");
    expect(body.admin).not.toHaveProperty("password_hash");
    expect(body.admin).not.toHaveProperty("session_secret");
    expect(body.admin!.oidc).not.toHaveProperty("client_secret");
    expect(JSON.stringify(out.body)).not.toMatch(SECRET_KEY);
  });

  it("turns edited fields into typed values and drops blank optionals", () => {
    const form = toForm(view);
    const out = toUpdate(
      { ...form, failover: " a \n\n b ", te_issuer: "", grader_sample_rate: "0.5", max_concurrent_turns: "3", audit_enabled: true, audit_retention_days: "30" },
      view,
    );
    expect(out.ok).toBe(true);
    if (!out.ok) return;
    expect(out.body.llm.failover).toEqual(["a", "b"]);
    expect(out.body.token_exchange).toBeNull();
    expect(out.body.grader?.sample_rate).toBe(0.5);
    expect(out.body.limits.max_concurrent_turns).toBe(3);
    expect(out.body.audit).toEqual({ enabled: true, retention_days: 30 });
  });

  it("refuses a limit under 1 before it reaches the server", () => {
    const out = toUpdate({ ...toForm(view), rate_limit_per_min: "0" }, view);
    expect(out).toEqual({ ok: false, error: "rate limit must be a whole number ≥ 1" });
  });

  it("offers the allowlist as provider choices, with the current value kept visible", () => {
    expect(providerChoices(view)).toEqual(["anthropic", "openrouter"]);
    expect(providerChoices({ llm: { ...view.llm, provider: "gemini" } })).toEqual(["gemini", "anthropic", "openrouter"]);
    expect(providerChoices({ llm: { ...view.llm, allowed_providers: [] } })).toEqual([]);
  });

  it("clears the key field after an accepted submit and keeps it after a refusal", () => {
    expect(keyFieldAfterSubmit("sk-good", true)).toBe("");
    expect(keyFieldAfterSubmit("sk-typo", false)).toBe("sk-typo");
  });
});
