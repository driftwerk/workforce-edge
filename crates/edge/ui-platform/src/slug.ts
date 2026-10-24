// Tenant ids and hostnames as the platform API accepts them, checked here
// so the form says what is wrong before the server does. The rules mirror
// `tenant::TenantId::parse` and `platform::hostname` exactly.

/** The longest tenant id. */
export const MAX_TENANT_ID_LEN = 63;
/** The shortest id the platform hands out: `a1` is a slug, but a one- or
 *  two-character subdomain is a name the product keeps. */
export const MIN_TENANT_ID_LEN = 3;

/** Ids the platform keeps for itself: product hostnames and routes. */
export const RESERVED_TENANT_IDS: readonly string[] = [
  "app",
  "api",
  "auth",
  "edge",
  "www",
  "mail",
  "admin",
  "platform",
  "status",
  "docs",
  "help",
  "support",
  "billing",
  "portal",
  "login",
  "signup",
  "marrilo",
  "default",
];

/** Why `raw` is not a tenant id, or null when it is one. */
export function tenantIdError(raw: string): string | null {
  if (raw.length === 0) return "An id is required.";
  if (raw.length > MAX_TENANT_ID_LEN) return `At most ${MAX_TENANT_ID_LEN} characters.`;
  if (!/^[a-z0-9-]+$/.test(raw)) return "Lowercase letters, digits and hyphens only.";
  if (raw.startsWith("-")) return "Cannot start with a hyphen.";
  if (raw.length < MIN_TENANT_ID_LEN) return `At least ${MIN_TENANT_ID_LEN} characters.`;
  if (RESERVED_TENANT_IDS.includes(raw)) return `"${raw}" is reserved.`;
  return null;
}

/** A display name turned into the id it suggests: lowercased, runs of
 *  anything else collapsed to one hyphen, hyphens trimmed at both ends. */
export function suggestTenantId(displayName: string): string {
  return displayName
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, MAX_TENANT_ID_LEN);
}

/** A hostname as the registry keys it: trimmed, lowercased, no trailing dot. */
export function normaliseHostname(raw: string): string {
  return raw.trim().replace(/\.+$/, "").toLowerCase();
}

/** Why `raw` is not a hostname (lowercase DNS labels, no scheme, port or
 *  path), or null when it is one. */
export function hostnameError(raw: string): string | null {
  const host = normaliseHostname(raw);
  if (host.length === 0) return "A hostname is required.";
  if (host.length > 253) return "At most 253 characters.";
  if (/[/:@?#\s]/.test(host)) return "A bare hostname: no scheme, port or path.";
  const labels = host.split(".");
  for (const label of labels) {
    if (label.length === 0) return "Empty label: check the dots.";
    if (label.startsWith("-") || label.endsWith("-")) return "A label cannot start or end with a hyphen.";
    if (!/^[a-z0-9-]+$/.test(label)) return "Lowercase letters, digits, hyphens and dots only.";
  }
  return null;
}

/** The hostnames in a one-per-line (or comma-separated) box, each checked,
 *  deduplicated in order: the list, or the first fault. */
export function parseHostnames(text: string): { hostnames: string[] } | { error: string } {
  const hostnames: string[] = [];
  for (const raw of text.split(/[\n,]/)) {
    if (raw.trim() === "") continue;
    const error = hostnameError(raw);
    if (error) return { error: `${raw.trim()}: ${error}` };
    const host = normaliseHostname(raw);
    if (!hostnames.includes(host)) hostnames.push(host);
  }
  return { hostnames };
}
