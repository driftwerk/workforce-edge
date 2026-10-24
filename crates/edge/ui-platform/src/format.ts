// Display formatters for the console. Pure: no React, no DOM.

import type { LlmStatus, Tenant } from "./api";

/** A principal as the audit log names it, split for display: an operator's
 *  email, or a service by its name. */
export function principalLabel(principal: string): { name: string; service: boolean } {
  const service = principal.startsWith("service:");
  return { name: service ? principal.slice("service:".length) : principal, service };
}

/** What an audit action is called on screen. */
export function actionLabel(action: string): string {
  switch (action) {
    case "tenant.create":
      return "created tenant";
    case "tenant.update":
      return "updated tenant";
    case "tenant.suspend":
      return "suspended tenant";
    case "tenant.resume":
      return "resumed tenant";
    case "tenant.delete":
      return "deleted tenant";
    case "tenant.rescue":
      return "opened admin (rescue)";
    case "hostname.add":
      return "added hostname";
    case "hostname.remove":
      return "removed hostname";
    case "operator.add":
      return "added operator";
    case "operator.remove":
      return "removed operator";
    default:
      return action;
  }
}

/** An ISO timestamp as a short local date and time, or the raw text when
 *  it does not parse. */
export function when(iso: string): string {
  const date = new Date(iso);
  if (Number.isNaN(date.getTime())) return iso;
  return date.toLocaleString(undefined, {
    year: "numeric",
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  });
}

/** "just now", "5 min ago", "3 h ago", "2 d ago", else the date. */
export function relative(iso: string, now: Date = new Date()): string {
  const date = new Date(iso);
  if (Number.isNaN(date.getTime())) return iso;
  const seconds = Math.max(0, Math.round((now.getTime() - date.getTime()) / 1000));
  if (seconds < 60) return "just now";
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) return `${minutes} min ago`;
  const hours = Math.round(minutes / 60);
  if (hours < 24) return `${hours} h ago`;
  const days = Math.round(hours / 24);
  if (days < 14) return `${days} d ago`;
  return date.toLocaleDateString(undefined, { year: "numeric", month: "short", day: "numeric" });
}

/** The status pill of a tenant: suspended, not yet served, or active. */
export function statusPill(tenant: Pick<Tenant, "status" | "serving" | "unavailable">): {
  label: string;
  tone: "ok" | "bad" | "warn";
  title: string;
} {
  if (tenant.status === "suspended") {
    return { label: "suspended", tone: "bad", title: tenant.unavailable ?? "Suspended" };
  }
  if (!tenant.serving) {
    return { label: "not serving", tone: "warn", title: tenant.unavailable ?? "Active, but no runtime answers on its hostnames" };
  }
  return { label: "active", tone: "ok", title: "Serving" };
}

/** The LLM key pill: ok, failed, unset, or unknown while the tenant is not
 *  served. */
export function llmPill(status: LlmStatus | null): { label: string; tone: "ok" | "bad" | "warn" | "neutral"; title: string } {
  if (!status) return { label: "unknown", tone: "neutral", title: "Not served, so the key's state is unknown" };
  switch (status.state) {
    case "ok":
      return { label: "key ok", tone: "ok", title: `Checked ${when(status.checked_at)}` };
    case "failed":
      return { label: "key failed", tone: "bad", title: status.message || "The key failed its last check" };
    default:
      return { label: "no key", tone: "warn", title: "No LLM key: the tenant cannot answer" };
  }
}
