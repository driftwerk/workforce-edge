import type { AdminSettings, LlmStatus, SettingsUpdate, SettingsView } from "./api";

// The settings form as flat strings/booleans (what inputs hold), and the two
// conversions: view → form on load, form → PUT body on save. Kept out of the
// component so both directions are unit-testable under node. The body never
// carries a secret: there is no field for one, and `toUpdate` builds the body
// from an explicit whitelist of the view's sections.

export interface SettingsForm {
  provider: string;
  tier_key: string;
  default_model: string;
  /** One model per line. */
  failover: string;
  grader_enabled: boolean;
  grader_model: string;
  grader_sample_rate: string;
  /** Blank issuer ⇒ token exchange off (section sent as null). */
  te_issuer: string;
  te_client_id: string;
  te_audience: string;
  te_scope: string;
  /** Blank issuer ⇒ no OIDC sign-in (admin.oidc sent as null). */
  admin_issuer: string;
  admin_client_id: string;
  admin_audience: string;
  admin_public_url: string;
  admin_session_hours: string;
  rate_limit_per_min: string;
  user_rate_limit_per_min: string;
  max_concurrent_turns: string;
  audit_enabled: boolean;
  audit_retention_days: string;
}

const str = (v: string | number | null | undefined): string =>
  v === null || v === undefined ? "" : String(v);

export function toForm(view: SettingsUpdate): SettingsForm {
  const te = view.token_exchange;
  const admin = view.admin;
  const oidc = admin?.oidc ?? null;
  return {
    provider: view.llm.provider,
    tier_key: str(view.llm.tier_key),
    default_model: str(view.llm.default_model),
    failover: view.llm.failover.join("\n"),
    grader_enabled: view.grader?.enabled ?? false,
    grader_model: str(view.grader?.model),
    grader_sample_rate: str(view.grader?.sample_rate ?? 1),
    te_issuer: str(te?.issuer),
    te_client_id: str(te?.client_id),
    te_audience: str(te?.audience),
    te_scope: str(te?.scope),
    admin_issuer: str(oidc?.issuer),
    admin_client_id: str(oidc?.client_id),
    admin_audience: str(oidc?.audience),
    admin_public_url: str(admin?.public_url),
    admin_session_hours: str(admin?.session_hours ?? 12),
    rate_limit_per_min: str(view.limits.rate_limit_per_min),
    user_rate_limit_per_min: str(view.limits.user_rate_limit_per_min),
    max_concurrent_turns: str(view.limits.max_concurrent_turns),
    audit_enabled: view.audit?.enabled ?? false,
    audit_retention_days: str(view.audit?.retention_days ?? 14),
  };
}

const blank = (s: string): null | string => (s.trim() ? s.trim() : null);
const lines = (s: string): string[] =>
  s
    .split("\n")
    .map((l) => l.trim())
    .filter(Boolean);

function int(label: string, s: string, min: number): number {
  const n = Number(s.trim());
  if (!Number.isInteger(n) || n < min) throw new Error(`${label} must be a whole number ≥ ${min}`);
  return n;
}

export type UpdateResult =
  | { ok: true; body: SettingsUpdate }
  | { ok: false; error: string };

/**
 * The PUT body for `form`, round-tripping from `base` what the form does not
 * edit (the retry policy, the provider allowlist, the admin allowlists, the
 * OIDC mode). The outbound policy is never in it: the platform sets it.
 * Client-side checks mirror the server's cheap ones so a typo never reaches
 * a rebuild.
 */
export function toUpdate(form: SettingsForm, base: SettingsUpdate): UpdateResult {
  try {
    const provider = form.provider.trim();
    if (!provider) throw new Error("provider is required");
    const sample = Number(form.grader_sample_rate.trim());
    if (form.grader_enabled && !(sample >= 0 && sample <= 1))
      throw new Error("grader sample rate must be between 0 and 1");

    const teIssuer = blank(form.te_issuer);
    const token_exchange = teIssuer
      ? {
          issuer: teIssuer,
          client_id: form.te_client_id.trim(),
          audience: blank(form.te_audience),
          scope: blank(form.te_scope),
        }
      : null;
    if (token_exchange && !token_exchange.client_id)
      throw new Error("token exchange client id is required when an issuer is set");

    const adminIssuer = blank(form.admin_issuer);
    const baseAdmin = base.admin;
    let admin: AdminSettings | null = null;
    if (adminIssuer || baseAdmin) {
      admin = {
        oidc: adminIssuer
          ? {
              issuer: adminIssuer,
              client_id: blank(form.admin_client_id),
              audience: blank(form.admin_audience),
              mode: baseAdmin?.oidc?.mode ?? null,
            }
          : null,
        allowed_emails: baseAdmin?.allowed_emails ?? [],
        allowed_domains: baseAdmin?.allowed_domains ?? [],
        public_url: blank(form.admin_public_url),
        session_hours: int("session hours", form.admin_session_hours, 1),
      };
    }

    const body: SettingsUpdate = {
      llm: {
        provider,
        allowed_providers: base.llm.allowed_providers,
        tier_key: blank(form.tier_key),
        default_model: blank(form.default_model),
        failover: lines(form.failover),
        retry: base.llm.retry,
      },
      grader: form.grader_enabled || base.grader
        ? {
            enabled: form.grader_enabled,
            model: blank(form.grader_model),
            sample_rate: Number.isFinite(sample) ? sample : 1,
          }
        : null,
      token_exchange,
      admin,
      limits: {
        rate_limit_per_min: int("rate limit", form.rate_limit_per_min, 1),
        user_rate_limit_per_min: int("per-user rate limit", form.user_rate_limit_per_min, 1),
        max_concurrent_turns: int("max concurrent turns", form.max_concurrent_turns, 1),
      },
      audit: form.audit_enabled || base.audit
        ? { enabled: form.audit_enabled, retention_days: int("audit retention", form.audit_retention_days, 1) }
        : null,
    };
    return { ok: true, body };
  } catch (e) {
    return { ok: false, error: e instanceof Error ? e.message : String(e) };
  }
}

/** Provider choices for the select: the install's allowlist, plus the current
 *  value when it is not on it (so the form shows what is set rather than
 *  silently picking another). Empty ⇒ unrestricted: render free text. */
export function providerChoices(view: Pick<SettingsView, "llm">): string[] {
  const allowed = view.llm.allowed_providers;
  if (allowed.length === 0) return [];
  return allowed.includes(view.llm.provider) ? allowed : [view.llm.provider, ...allowed];
}

/** The key field after a submit: cleared once the server kept the key, left
 *  as typed when it refused it so the operator can correct and retry. */
export function keyFieldAfterSubmit(typed: string, accepted: boolean): string {
  return accepted ? "" : typed;
}

/** The one-line verdict under the key field. */
export function describeKeyStatus(status: LlmStatus | null): string {
  if (!status) return "";
  const when = new Date(status.checked_at);
  const at = Number.isNaN(when.getTime()) ? status.checked_at : when.toLocaleString();
  switch (status.state) {
    case "ok":
      return `Key verified · ${at}`;
    case "failed":
      return `${status.message} (checked ${at})`;
    default:
      return "No key stored.";
  }
}
