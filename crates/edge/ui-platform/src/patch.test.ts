import { describe, expect, it } from "vitest";

import type { AdminConfig, Tenant } from "./api";
import {
  adminForm,
  diffTenant,
  editableOf,
  EMPTY_ADMIN_FORM,
  normalisePlan,
  parseAdmin,
  parsePlan,
  planForm,
  planSummary,
} from "./patch";

const tenant: Tenant = {
  id: "acme",
  display_name: "Acme Ltd",
  status: "active",
  hostnames: ["acme.test"],
  plan: { turns_per_month: 5000, active_users: null },
  created_at: "2026-10-01T00:00:00Z",
  updated_at: "2026-10-01T00:00:00Z",
  has_settings: true,
  has_admin: true,
  has_admin_client_secret: false,
  llm_status: { state: "ok", checked_at: "2026-10-01T00:00:00Z", message: "" },
  usage: null,
  serving: true,
  unavailable: null,
  outbound: "public",
  admin: {
    oidc: { issuer: "https://kc.test/realms/acme", client_id: "edge-admin", mode: "redirect" },
    allowed_emails: ["ops@acme.test"],
    allowed_domains: [],
    public_url: "https://acme.test",
    session_hours: 12,
  },
};

describe("plan form", () => {
  it("round-trips a plan through text, dropping unbounded caps", () => {
    const form = planForm(tenant.plan);
    expect(form.turns_per_month).toBe("5000");
    expect(form.active_users).toBe("");
    expect(parsePlan(form)).toEqual({ plan: { turns_per_month: 5000 } });
  });

  it("refuses text that is not a cap", () => {
    expect(parsePlan({ ...planForm({}), active_users: "ten" })).toEqual({
      error: "Active users: a whole number, or blank for no cap.",
    });
    expect(parsePlan({ ...planForm({}), knowledge_chunks: "0" })).toEqual({
      error: "Knowledge chunks: at least 1, or blank for no cap.",
    });
  });

  it("summarises the set caps", () => {
    expect(planSummary({})).toBe("no caps");
    expect(planSummary({ turns_per_month: 5000, memory_objects: 1000, agents: 5, upload_mb: null })).toBe(
      "5,000 turns/mo · 1,000 memories · 5 agents",
    );
    expect(normalisePlan({ turns_per_month: null, active_users: 3 })).toEqual({ active_users: 3 });
  });
});

describe("admin form", () => {
  it("shows a tenant's sign-in and parses it back unchanged", () => {
    const form = adminForm(tenant.admin);
    expect(form.enabled).toBe(true);
    expect(form.mode).toBe("redirect");
    expect(form.allowed_emails).toBe("ops@acme.test");
    expect(parseAdmin(form)).toEqual({ admin: tenant.admin });
  });

  it("is off for a tenant without one, and parses to null when off", () => {
    expect(adminForm(null)).toEqual(EMPTY_ADMIN_FORM);
    expect(parseAdmin(EMPTY_ADMIN_FORM)).toEqual({ admin: null });
  });

  it("needs an issuer URL and sane hours once on", () => {
    const on = { ...EMPTY_ADMIN_FORM, enabled: true };
    expect(parseAdmin(on)).toEqual({ error: "Issuer: the OIDC issuer URL is required for a sign-in." });
    expect(parseAdmin({ ...on, issuer: "kc.test" })).toMatchObject({ error: expect.stringMatching(/URL/) });
    expect(parseAdmin({ ...on, issuer: "https://kc.test", session_hours: "0" })).toMatchObject({
      error: expect.stringMatching(/Session hours/),
    });
  });

  it("splits and lowercases the allowlists, omitting blank optional strings", () => {
    const parsed = parseAdmin({
      ...EMPTY_ADMIN_FORM,
      enabled: true,
      issuer: "https://accounts.google.com",
      allowed_emails: "Ops@Acme.test, dev@acme.test\n ops@acme.test ",
      allowed_domains: "Acme.test",
    });
    expect(parsed).toEqual({
      admin: {
        oidc: { issuer: "https://accounts.google.com" },
        allowed_emails: ["ops@acme.test", "dev@acme.test"],
        allowed_domains: ["acme.test"],
        session_hours: 12,
      },
    });
  });
});

describe("diffTenant", () => {
  it("is null when nothing changed", () => {
    const original = editableOf(tenant);
    expect(diffTenant(original, { ...original })).toBeNull();
    expect(diffTenant(original, { ...original, display_name: "  Acme Ltd " })).toBeNull();
    expect(diffTenant(original, { ...original, plan: { turns_per_month: 5000, upload_mb: null } })).toBeNull();
  });

  it("sends only the fields that differ", () => {
    const original = editableOf(tenant);
    expect(diffTenant(original, { ...original, display_name: "Acme Limited" })).toEqual({
      display_name: "Acme Limited",
    });
    expect(diffTenant(original, { ...original, plan: {} })).toEqual({ plan: {} });
    expect(diffTenant(original, { ...original, outbound: "private" })).toEqual({ outbound: "private" });
  });

  it("ignores a blank display name", () => {
    const original = editableOf(tenant);
    expect(diffTenant(original, { ...original, display_name: "  " })).toBeNull();
  });

  it("compares the admin section by content, list order aside", () => {
    const original = editableOf(tenant);
    const reordered: AdminConfig = {
      ...tenant.admin!,
      allowed_emails: ["ops@acme.test"],
      oidc: { ...tenant.admin!.oidc!, audience: null },
    };
    expect(diffTenant(original, { ...original, admin: reordered })).toBeNull();
    const changed: AdminConfig = { ...tenant.admin!, allowed_domains: ["acme.test"] };
    expect(diffTenant(original, { ...original, admin: changed })).toEqual({ admin: changed });
  });

  it("carries the write-only client secret: set, or cleared", () => {
    const original = editableOf(tenant);
    expect(diffTenant(original, { ...original, admin_client_secret: " s3cret " })).toEqual({
      admin_client_secret: "s3cret",
    });
    expect(diffTenant(original, { ...original, clear_admin_client_secret: true })).toEqual({
      admin_client_secret: "",
    });
  });
});
