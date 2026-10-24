// The tenant form's pure logic: what the plan and admin forms hold, how
// their text becomes the API's shapes, and which of a tenant's editable
// fields changed — so a save sends only those, as `PATCH` wants.

import type { AdminConfig, Outbound, Plan, Tenant, TenantPatch } from "./api";

export type PlanField = keyof Plan;

/** The plan fields, in the order the form shows them. */
export const PLAN_FIELDS: ReadonlyArray<{ key: PlanField; label: string; hint: string }> = [
  { key: "turns_per_month", label: "Turns per month", hint: "customer turns answered, resetting monthly (UTC)" },
  { key: "active_users", label: "Active users", hint: "distinct end-users a month" },
  { key: "knowledge_chunks", label: "Knowledge chunks", hint: "embedded chunks across every agent" },
  { key: "memory_objects", label: "Memory objects", hint: "objects in the tenant's vector store" },
  { key: "agents", label: "Agents", hint: "agents the tenant may hold; a create or import past it is refused" },
  { key: "upload_mb", label: "Upload MB", hint: "stored, not yet metered" },
  { key: "max_concurrent_turns", label: "Concurrent turns", hint: "ceiling over the tenant's own limit" },
  { key: "rate_limit_per_min", label: "Requests per minute", hint: "ceiling over the tenant's own limit" },
  { key: "user_rate_limit_per_min", label: "Per-user requests per minute", hint: "ceiling over the tenant's own limit" },
];

export type PlanForm = Record<PlanField, string>;

/** A plan as the form shows it: each cap as text, empty when unbounded. */
export function planForm(plan: Plan): PlanForm {
  const form = {} as PlanForm;
  for (const { key } of PLAN_FIELDS) {
    const value = plan[key];
    form[key] = value === null || value === undefined ? "" : String(value);
  }
  return form;
}

/** The form's text as a plan: blank is no cap, else a whole number of at
 *  least one. The first field that is neither is the fault. */
export function parsePlan(form: PlanForm): { plan: Plan } | { error: string } {
  const plan: Plan = {};
  for (const { key, label } of PLAN_FIELDS) {
    const text = (form[key] ?? "").trim();
    if (text === "") continue;
    if (!/^\d+$/.test(text)) return { error: `${label}: a whole number, or blank for no cap.` };
    const value = Number(text);
    if (!Number.isSafeInteger(value) || value < 1) return { error: `${label}: at least 1, or blank for no cap.` };
    plan[key] = value;
  }
  return { plan };
}

/** A plan with every unbounded field dropped, so two plans compare by
 *  their caps alone. */
export function normalisePlan(plan: Plan): Plan {
  const out: Plan = {};
  for (const { key } of PLAN_FIELDS) {
    const value = plan[key];
    if (value !== null && value !== undefined) out[key] = value;
  }
  return out;
}

/** "no caps", or the set caps as "5,000 turns · 50 users · …". */
export function planSummary(plan: Plan): string {
  const parts: string[] = [];
  const short: Record<PlanField, string> = {
    turns_per_month: "turns/mo",
    active_users: "users",
    knowledge_chunks: "chunks",
    memory_objects: "memories",
    agents: "agents",
    upload_mb: "MB upload",
    max_concurrent_turns: "concurrent",
    rate_limit_per_min: "req/min",
    user_rate_limit_per_min: "req/min/user",
  };
  for (const { key } of PLAN_FIELDS) {
    const value = plan[key];
    if (value !== null && value !== undefined) parts.push(`${value.toLocaleString()} ${short[key]}`);
  }
  return parts.length === 0 ? "no caps" : parts.join(" · ");
}

export type AdminMode = "" | "google" | "redirect";

/** The admin sign-in form: the OIDC client and the seed allowlist, as text. */
export interface AdminForm {
  enabled: boolean;
  issuer: string;
  client_id: string;
  audience: string;
  mode: AdminMode;
  public_url: string;
  allowed_emails: string;
  allowed_domains: string;
  session_hours: string;
}

export const EMPTY_ADMIN_FORM: AdminForm = {
  enabled: false,
  issuer: "",
  client_id: "",
  audience: "",
  mode: "",
  public_url: "",
  allowed_emails: "",
  allowed_domains: "",
  session_hours: "12",
};

/** A tenant's admin section as the form shows it; the empty form when it
 *  has none. */
export function adminForm(admin: AdminConfig | null): AdminForm {
  if (!admin) return { ...EMPTY_ADMIN_FORM };
  const mode = admin.oidc?.mode;
  return {
    enabled: true,
    issuer: admin.oidc?.issuer ?? "",
    client_id: admin.oidc?.client_id ?? "",
    audience: admin.oidc?.audience ?? "",
    mode: mode === "google" || mode === "redirect" ? mode : "",
    public_url: admin.public_url ?? "",
    allowed_emails: admin.allowed_emails.join("\n"),
    allowed_domains: admin.allowed_domains.join("\n"),
    session_hours: String(admin.session_hours),
  };
}

function listOf(text: string): string[] {
  const out: string[] = [];
  for (const raw of text.split(/[\n,]/)) {
    const value = raw.trim().toLowerCase();
    if (value && !out.includes(value)) out.push(value);
  }
  return out;
}

/** The form as the API's admin section: `null` when sign-in is off (the
 *  form disabled), else the section, or the first fault. Omitted strings
 *  are omitted, not sent empty, so the server sees what it would in the
 *  file. */
export function parseAdmin(form: AdminForm): { admin: AdminConfig | null } | { error: string } {
  if (!form.enabled) return { admin: null };
  const issuer = form.issuer.trim();
  if (issuer === "") return { error: "Issuer: the OIDC issuer URL is required for a sign-in." };
  if (!/^https?:\/\/\S+$/.test(issuer)) return { error: "Issuer: a URL, e.g. https://accounts.google.com." };
  const hours = form.session_hours.trim();
  if (!/^\d+$/.test(hours) || Number(hours) < 1) return { error: "Session hours: a whole number of at least 1." };
  const publicUrl = form.public_url.trim();
  if (publicUrl !== "" && !/^https?:\/\/\S+$/.test(publicUrl)) return { error: "Public URL: a base URL, e.g. https://acme.marrilo.com." };
  const admin: AdminConfig = {
    oidc: {
      issuer,
      ...(form.client_id.trim() ? { client_id: form.client_id.trim() } : {}),
      ...(form.audience.trim() ? { audience: form.audience.trim() } : {}),
      ...(form.mode ? { mode: form.mode } : {}),
    },
    allowed_emails: listOf(form.allowed_emails),
    allowed_domains: listOf(form.allowed_domains),
    ...(publicUrl ? { public_url: publicUrl } : {}),
    session_hours: Number(hours),
  };
  return { admin };
}

/** An admin section reduced to what matters for comparison: no nulls, no
 *  empty strings, lists sorted. */
function adminKey(admin: AdminConfig | null): string {
  if (!admin) return "";
  const oidc = admin.oidc
    ? {
        issuer: admin.oidc.issuer,
        client_id: admin.oidc.client_id || undefined,
        audience: admin.oidc.audience || undefined,
        mode: admin.oidc.mode || undefined,
      }
    : undefined;
  return JSON.stringify({
    oidc,
    allowed_emails: [...admin.allowed_emails].sort(),
    allowed_domains: [...admin.allowed_domains].sort(),
    public_url: admin.public_url || undefined,
    session_hours: admin.session_hours,
  });
}

/** The editable fields of a tenant, as the detail page holds them. */
export interface Editable {
  display_name: string;
  plan: Plan;
  outbound: Outbound;
  admin: AdminConfig | null;
  /** The write-only client secret: empty leaves it, text sets it. */
  admin_client_secret: string;
  /** Whether the operator asked for the stored secret to be cleared. */
  clear_admin_client_secret: boolean;
}

export function editableOf(tenant: Tenant): Editable {
  return {
    display_name: tenant.display_name,
    plan: normalisePlan(tenant.plan),
    outbound: tenant.outbound,
    admin: tenant.admin,
    admin_client_secret: "",
    clear_admin_client_secret: false,
  };
}

/** The `PATCH` body that turns `original` into `edited`: only the fields
 *  that differ, or null when nothing does. A display name is compared
 *  trimmed; a blank one is no change (the server refuses it anyway). An
 *  admin section turned off is not a patch: the API has no way to remove
 *  one, and the form says so. */
export function diffTenant(original: Editable, edited: Editable): TenantPatch | null {
  const patch: TenantPatch = {};
  const name = edited.display_name.trim();
  if (name !== "" && name !== original.display_name.trim()) patch.display_name = name;
  const plan = normalisePlan(edited.plan);
  if (JSON.stringify(plan) !== JSON.stringify(normalisePlan(original.plan))) patch.plan = plan;
  if (edited.outbound !== original.outbound) patch.outbound = edited.outbound;
  if (edited.admin && adminKey(edited.admin) !== adminKey(original.admin)) patch.admin = edited.admin;
  if (edited.clear_admin_client_secret) patch.admin_client_secret = "";
  else if (edited.admin_client_secret.trim() !== "") patch.admin_client_secret = edited.admin_client_secret.trim();
  return Object.keys(patch).length === 0 ? null : patch;
}
