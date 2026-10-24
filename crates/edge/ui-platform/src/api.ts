// The platform API (same-origin, platform session cookie). Every call sends
// the cookie, and every write is a same-origin fetch, which is what the
// server's CSRF rule (`Sec-Fetch-Site: same-origin`, or a matching `Origin`)
// asks of a browser. A 401 means "not signed in": the app shows the sign-in
// page; the other failures carry the server's sentence.

/** The session is gone (or never was): the app returns to sign-in. */
export class Unauthorized extends Error {
  constructor() {
    super("Not signed in.");
  }
}

/** A refused call, with the status the server answered and its sentence. */
export class ApiError extends Error {
  readonly status: number;
  constructor(status: number, message: string) {
    super(message);
    this.status = status;
  }
}

let onUnauthorized: (() => void) | null = null;
/** Register what happens when a call finds the session gone. */
export function setUnauthorizedHandler(handler: (() => void) | null): void {
  onUnauthorized = handler;
}

/** The sentence a refusal is shown with: the server's, under a word for
 *  the status it came with. */
export function describeFailure(status: number, serverMessage: string | null, fallback: string): string {
  const detail = serverMessage ?? fallback;
  switch (status) {
    case 403:
      return `Not allowed: ${detail}`;
    case 404:
      return `Not found: ${detail}`;
    case 409:
      return `Conflict: ${detail}`;
    case 422:
      return `Rejected: ${detail}`;
    case 502:
      return `Upstream failure: ${detail}`;
    default:
      return status >= 500 ? `Server error (${status}): ${detail}` : detail;
  }
}

async function serverMessage(res: Response): Promise<string | null> {
  try {
    const body = await res.json();
    return typeof body?.error === "string" ? body.error : null;
  } catch {
    return null;
  }
}

async function call(path: string, init?: RequestInit, fallback = "The request failed."): Promise<Response> {
  const res = await fetch(path, { ...init, credentials: "same-origin" });
  if (res.status === 401) {
    onUnauthorized?.();
    throw new Unauthorized();
  }
  if (!res.ok) {
    throw new ApiError(res.status, describeFailure(res.status, await serverMessage(res), `${fallback} (${res.status})`));
  }
  return res;
}

function jsonInit(method: string, body: unknown): RequestInit {
  return {
    method,
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  };
}

export interface Me {
  operator: string;
  role: string;
}

/** The session probe: who is signed in, or null when nobody is. */
export async function me(): Promise<Me | null> {
  const res = await fetch("/platform/api/me", { credentials: "same-origin" });
  if (res.status === 401) return null;
  if (!res.ok) throw new ApiError(res.status, describeFailure(res.status, await serverMessage(res), "Session probe failed"));
  return (await res.json()) as Me;
}

export async function logout(): Promise<void> {
  await fetch("/platform/api/logout", { method: "POST", credentials: "same-origin" });
}

/** The caps a plan sets; a field absent or null is unbounded. */
export interface Plan {
  turns_per_month?: number | null;
  active_users?: number | null;
  knowledge_chunks?: number | null;
  memory_objects?: number | null;
  agents?: number | null;
  upload_mb?: number | null;
  max_concurrent_turns?: number | null;
  rate_limit_per_min?: number | null;
  user_rate_limit_per_min?: number | null;
}

/** What a tenant has used of its plan, as the server counts it. */
export interface Usage {
  turns_this_month: number;
  active_users_this_month: number;
  knowledge_chunks: number;
  memory_objects: number | null;
  agents: number;
}

export interface LlmStatus {
  state: "ok" | "failed" | "unset";
  checked_at: string;
  message: string;
}

export type Outbound = "private" | "public";
export type TenantStatus = "active" | "suspended";

/** A tenant's admin sign-in, secrets omitted (the server never sends them). */
export interface AdminOidc {
  issuer: string;
  audience?: string | null;
  client_id?: string | null;
  mode?: string | null;
}

export interface AdminConfig {
  oidc?: AdminOidc | null;
  allowed_emails: string[];
  allowed_domains: string[];
  public_url?: string | null;
  session_hours: number;
}

/** A tenant as `GET /platform/api/tenants[/{id}]` shows it. */
export interface Tenant {
  id: string;
  display_name: string;
  status: TenantStatus;
  hostnames: string[];
  plan: Plan;
  created_at: string;
  updated_at: string;
  has_settings: boolean;
  has_admin: boolean;
  has_admin_client_secret: boolean;
  llm_status: LlmStatus | null;
  usage: Usage | null;
  serving: boolean;
  unavailable: string | null;
  outbound: Outbound;
  admin: AdminConfig | null;
}

export async function listTenants(): Promise<Tenant[]> {
  const res = await call("/platform/api/tenants", undefined, "Listing tenants failed");
  return (await res.json()) as Tenant[];
}

export async function getTenant(id: string): Promise<Tenant> {
  const res = await call(`/platform/api/tenants/${encodeURIComponent(id)}`, undefined, "Reading the tenant failed");
  return (await res.json()) as Tenant;
}

export interface NewTenant {
  id: string;
  display_name: string;
  hostnames: string[];
}

export async function createTenant(body: NewTenant): Promise<Tenant> {
  const res = await call("/platform/api/tenants", jsonInit("POST", body), "Creating the tenant failed");
  return (await res.json()) as Tenant;
}

/** `PATCH /platform/api/tenants/{id}`: only the fields present change. */
export interface TenantPatch {
  display_name?: string;
  plan?: Plan;
  outbound?: Outbound;
  admin?: AdminConfig;
  /** Write-only: `""` clears the sealed OIDC client secret. */
  admin_client_secret?: string;
}

export async function patchTenant(id: string, patch: TenantPatch): Promise<Tenant> {
  const res = await call(`/platform/api/tenants/${encodeURIComponent(id)}`, jsonInit("PATCH", patch), "Updating the tenant failed");
  return (await res.json()) as Tenant;
}

export async function suspendTenant(id: string, message: string): Promise<Tenant> {
  const body = message.trim() ? { message: message.trim() } : {};
  const res = await call(`/platform/api/tenants/${encodeURIComponent(id)}/suspend`, jsonInit("POST", body), "Suspending the tenant failed");
  return (await res.json()) as Tenant;
}

export async function resumeTenant(id: string): Promise<Tenant> {
  const res = await call(`/platform/api/tenants/${encodeURIComponent(id)}/resume`, jsonInit("POST", {}), "Resuming the tenant failed");
  return (await res.json()) as Tenant;
}

export async function addHostname(id: string, hostname: string): Promise<Tenant> {
  const res = await call(`/platform/api/tenants/${encodeURIComponent(id)}/hostnames`, jsonInit("POST", { hostname }), "Adding the hostname failed");
  return (await res.json()) as Tenant;
}

export async function removeHostname(id: string, hostname: string): Promise<Tenant> {
  const res = await call(
    `/platform/api/tenants/${encodeURIComponent(id)}/hostnames/${encodeURIComponent(hostname)}`,
    { method: "DELETE" },
    "Removing the hostname failed",
  );
  return (await res.json()) as Tenant;
}

export type OperatorKind = "email" | "domain";

export interface Operator {
  kind: OperatorKind;
  value: string;
  added_by: string;
  added_at: string;
}

export async function listOperators(id: string): Promise<Operator[]> {
  const res = await call(`/platform/api/tenants/${encodeURIComponent(id)}/operators`, undefined, "Listing operators failed");
  return (await res.json()) as Operator[];
}

export async function addOperator(id: string, kind: OperatorKind, value: string): Promise<Operator> {
  const res = await call(`/platform/api/tenants/${encodeURIComponent(id)}/operators`, jsonInit("POST", { kind, value }), "Adding the operator failed");
  return (await res.json()) as Operator;
}

export async function removeOperator(id: string, kind: OperatorKind, value: string): Promise<void> {
  await call(
    `/platform/api/tenants/${encodeURIComponent(id)}/operators/${encodeURIComponent(kind)}/${encodeURIComponent(value)}`,
    { method: "DELETE" },
    "Removing the operator failed",
  );
}

export interface RescueTicket {
  url: string;
  expires_in_secs: number;
}

export async function rescueTenant(id: string): Promise<RescueTicket> {
  const res = await call(`/platform/api/tenants/${encodeURIComponent(id)}/rescue`, jsonInit("POST", {}), "Rescue failed");
  return (await res.json()) as RescueTicket;
}

export async function deleteTenant(id: string): Promise<void> {
  await call(
    `/platform/api/tenants/${encodeURIComponent(id)}?confirm=${encodeURIComponent(id)}`,
    { method: "DELETE" },
    "Deleting the tenant failed",
  );
}

export interface TenantUsage {
  plan: Plan;
  usage: Usage | null;
  serving: boolean;
}

export async function tenantUsage(id: string): Promise<TenantUsage> {
  const res = await call(`/platform/api/tenants/${encodeURIComponent(id)}/usage`, undefined, "Counting usage failed");
  return (await res.json()) as TenantUsage;
}

export interface AuditEntry {
  id: number;
  /** The operator's email, or `service:<name>`. */
  principal: string;
  action: string;
  tenant: string;
  detail: string;
  at: string;
}

export async function listAudit(limit: number, before?: number): Promise<AuditEntry[]> {
  const params = new URLSearchParams({ limit: String(limit) });
  if (before !== undefined) params.set("before", String(before));
  const res = await call(`/platform/api/audit?${params}`, undefined, "Reading the audit log failed");
  return ((await res.json()) as { entries: AuditEntry[] }).entries;
}
