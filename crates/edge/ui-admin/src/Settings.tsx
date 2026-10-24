import { useEffect, useState } from "react";

import * as api from "./api";
import { UsageCard } from "./UsageCard";
import {
  describeKeyStatus,
  keyFieldAfterSubmit,
  providerChoices,
  toForm,
  toUpdate,
  type SettingsForm,
} from "./settingsForm";

/** Install settings: the non-secret sections save together; each secret has
 *  its own write-only control and is never rendered back. */
export function Settings({ onLlmStatus }: { onLlmStatus: (s: api.LlmStatus) => void }) {
  const [view, setView] = useState<api.SettingsView | null>(null);
  const [form, setForm] = useState<SettingsForm | null>(null);
  const [error, setError] = useState("");
  const [saved, setSaved] = useState("");
  const [busy, setBusy] = useState(false);

  const load = () =>
    api
      .getSettings()
      .then((v) => {
        setView(v);
        setForm(toForm(v));
        onLlmStatus(v.llm_status);
      })
      .catch((e) => setError(e instanceof Error ? e.message : String(e)));

  useEffect(() => {
    void load();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  if (error && !view) return <p className="err">{error}</p>;
  if (!view || !form) return <p className="spinner">Loading…</p>;

  const readOnly = !view.editable;
  const set = <K extends keyof SettingsForm>(k: K, v: SettingsForm[K]) =>
    setForm((f) => (f ? { ...f, [k]: v } : f));

  const save = async (e: React.FormEvent) => {
    e.preventDefault();
    setError("");
    setSaved("");
    const update = toUpdate(form, view);
    if (!update.ok) {
      setError(update.error);
      return;
    }
    setBusy(true);
    try {
      await api.saveSettings(update.body);
      setSaved("Saved. The tenant is running on these settings.");
      await load();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  };

  const providers = providerChoices(view);

  return (
    <>
      <h2>Settings</h2>
      <p className="sub">
        This tenant's install settings. Secrets are write-only: set or replace them below, and only
        whether one is set is ever shown.
      </p>
      {readOnly && (
        <p className="banner-info">
          <strong>Read-only.</strong> This install has no master key, so its settings are read from{" "}
          <code>edge.toml</code> and cannot be edited or stored here.
        </p>
      )}
      {!readOnly && !view.stored && (
        <p className="banner-info">
          Nothing has been saved for this tenant yet — the values shown are the file's. Saving stores
          them for this tenant; the file still supplies anything a saved section leaves out.
        </p>
      )}

      <UsageCard />

      <LlmKey view={view} readOnly={readOnly} onChanged={load} onLlmStatus={onLlmStatus} />

      <form onSubmit={save}>
        <fieldset className={readOnly ? "view-only" : undefined} style={{ border: 0, padding: 0, margin: 0, minInlineSize: "auto" }}>
          <Section title="LLM" blurb="Which provider and model answer for this tenant.">
            <label>Provider</label>
            {providers.length > 0 ? (
              <select className="select" value={form.provider} onChange={(e) => set("provider", e.target.value)}>
                {providers.map((p) => (
                  <option key={p} value={p}>
                    {p}
                  </option>
                ))}
              </select>
            ) : (
              <input value={form.provider} onChange={(e) => set("provider", e.target.value)} placeholder="anthropic" />
            )}
            <label>
              Default model<span className="hint">a tier (cheap / standard / powerful) or a model id</span>
            </label>
            <input value={form.default_model} onChange={(e) => set("default_model", e.target.value)} placeholder="standard" />
            <label>
              Tier key<span className="hint">blank = derived from the provider</span>
            </label>
            <input value={form.tier_key} onChange={(e) => set("tier_key", e.target.value)} placeholder="anthropic" />
            <label>
              Failover<span className="hint">one model per line, tried in order when the primary fails</span>
            </label>
            <textarea className="mono-area" rows={3} value={form.failover} onChange={(e) => set("failover", e.target.value)} />
          </Section>

          <Section
            title="Grader"
            blurb="Grades each answered turn off the critical path. It runs on this tenant's own key and multiplies LLM spend by roughly 1 + sample rate."
          >
            <label>Enabled</label>
            <div className="check-row">
              <label className="check">
                <input type="checkbox" checked={form.grader_enabled} disabled={readOnly} onChange={(e) => set("grader_enabled", e.target.checked)} />
                Grade answers
              </label>
            </div>
            <label>
              Model<span className="hint">blank = the agent's own model; "cheap" bounds the cost</span>
            </label>
            <input value={form.grader_model} onChange={(e) => set("grader_model", e.target.value)} placeholder="cheap" />
            <label>
              Sample rate<span className="hint">0 – 1, the fraction of turns graded</span>
            </label>
            <input value={form.grader_sample_rate} onChange={(e) => set("grader_sample_rate", e.target.value)} placeholder="1.0" inputMode="decimal" />
          </Section>

          <Section title="Token exchange" blurb="The edge's confidential Keycloak client, used to mint audience-scoped tokens for MCP. Leave the issuer blank to turn it off.">
            <label>Issuer</label>
            <input value={form.te_issuer} onChange={(e) => set("te_issuer", e.target.value)} placeholder="https://auth.company.example/realms/main" />
            <label>Client id</label>
            <input value={form.te_client_id} onChange={(e) => set("te_client_id", e.target.value)} />
            <label>
              Audience<span className="hint">optional; the MCP server's client id</span>
            </label>
            <input value={form.te_audience} onChange={(e) => set("te_audience", e.target.value)} />
            <label>
              Scope<span className="hint">optional; e.g. openid when the MCP server checks tokens against userinfo</span>
            </label>
            <input value={form.te_scope} onChange={(e) => set("te_scope", e.target.value)} />
          </Section>

          <Section title="Admin sign-in" blurb="How operators sign into this admin. Leave the issuer blank for no OIDC sign-in (break-glass password only).">
            <label>OIDC issuer</label>
            <input value={form.admin_issuer} onChange={(e) => set("admin_issuer", e.target.value)} placeholder="https://accounts.google.com" />
            <label>Client id</label>
            <input value={form.admin_client_id} onChange={(e) => set("admin_client_id", e.target.value)} />
            <label>
              Audience<span className="hint">usually the client id</span>
            </label>
            <input value={form.admin_audience} onChange={(e) => set("admin_audience", e.target.value)} />
            <label>
              Public URL<span className="hint">for the redirect flow; blank = from the request host</span>
            </label>
            <input value={form.admin_public_url} onChange={(e) => set("admin_public_url", e.target.value)} placeholder="https://edge.company.example" />
            <label>Session hours</label>
            <input value={form.admin_session_hours} onChange={(e) => set("admin_session_hours", e.target.value)} inputMode="numeric" />
          </Section>

          <Section title="Limits" blurb="This tenant's request and turn budgets. Each must be at least 1.">
            <label>Requests per minute</label>
            <input value={form.rate_limit_per_min} onChange={(e) => set("rate_limit_per_min", e.target.value)} inputMode="numeric" />
            <label>Per-user requests per minute</label>
            <input value={form.user_rate_limit_per_min} onChange={(e) => set("user_rate_limit_per_min", e.target.value)} inputMode="numeric" />
            <label>Max concurrent turns</label>
            <input value={form.max_concurrent_turns} onChange={(e) => set("max_concurrent_turns", e.target.value)} inputMode="numeric" />
          </Section>

          <Section title="Audit" blurb="Keep the tool evidence behind each answer, so an answer's grounding can be checked afterwards. Stores a second copy of tool results.">
            <label>Enabled</label>
            <div className="check-row">
              <label className="check">
                <input type="checkbox" checked={form.audit_enabled} disabled={readOnly} onChange={(e) => set("audit_enabled", e.target.checked)} />
                Retain evidence
              </label>
            </div>
            <label>Retention days</label>
            <input value={form.audit_retention_days} onChange={(e) => set("audit_retention_days", e.target.value)} inputMode="numeric" />
          </Section>

        </fieldset>

        <Section title="Outbound" blurb="Whether this tenant's outbound HTTP (OIDC discovery, MCP, token exchange) may reach private addresses. Set by the platform, not here: public refuses private and loopback addresses, as a cloud tenant is held to.">
          <label>Policy</label>
          <input value={outboundLabel(view.outbound)} readOnly aria-readonly="true" />
        </Section>

        {error && <p className="err">{error}</p>}
        {saved && <p className="muted ok-line">{saved}</p>}
        {!readOnly && (
          <div className="actions">
            <button className="btn-sm" type="submit" disabled={busy}>
              {busy ? "Saving…" : "Save settings"}
            </button>
          </div>
        )}
      </form>

      <SecretControl
        title="Token exchange secret"
        which="token-exchange-secret"
        isSet={view.has_token_exchange_secret}
        readOnly={readOnly}
        onChanged={load}
      />
      <SecretControl
        title="Admin OIDC client secret"
        which="admin-client-secret"
        isSet={view.has_admin_client_secret}
        readOnly={readOnly}
        onChanged={load}
      />
    </>
  );
}

function outboundLabel(policy: api.Outbound): string {
  return policy === "public"
    ? "public — private addresses refused"
    : "private — in-cluster URLs allowed";
}

function Section({ title, blurb, children }: { title: string; blurb: string; children: React.ReactNode }) {
  return (
    <div className="card settings-section">
      <h3>{title}</h3>
      <p className="muted settings-blurb">{blurb}</p>
      <div className="form-grid">{children}</div>
    </div>
  );
}

/** The LLM key: a password field that is never populated from the server.
 *  Set ⇒ one validation probe; on success the field is cleared and the
 *  server's verdict shown, on refusal the typed key stays for a retry. */
function LlmKey({
  view,
  readOnly,
  onChanged,
  onLlmStatus,
}: {
  view: api.SettingsView;
  readOnly: boolean;
  onChanged: () => Promise<void>;
  onLlmStatus: (s: api.LlmStatus) => void;
}) {
  const [key, setKey] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [status, setStatus] = useState<api.LlmStatus | null>(view.llm_status);

  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!key.trim()) return;
    setBusy(true);
    setError("");
    try {
      const outcome = await api.setLlmKey(key);
      setKey(keyFieldAfterSubmit(key, outcome.ok));
      if (outcome.llm_status) {
        setStatus(outcome.llm_status);
        onLlmStatus(outcome.llm_status);
      }
      if (outcome.ok) await onChanged();
      else setError(outcome.error);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  };

  const remove = async () => {
    if (!window.confirm("Remove the stored LLM key? The assistant cannot answer without one.")) return;
    setBusy(true);
    setError("");
    try {
      await api.clearLlmKey();
      const next = api.llmStatus();
      if (next) {
        setStatus(next);
        onLlmStatus(next);
      }
      await onChanged();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="card settings-section">
      <div className="row">
        <h3>LLM key</h3>
        <span className={`pill ${view.has_llm_key ? "pill-domain" : "pill-neutral"}`}>
          {view.has_llm_key ? "set" : "not set"}
        </span>
      </div>
      <p className="muted settings-blurb">
        Validated with one probe against the provider and model above before it is stored. Shown only as
        set or not set.
      </p>
      {status && <p className={status.state === "failed" ? "err" : "muted"}>{describeKeyStatus(status)}</p>}
      {error && <p className="err">{error}</p>}
      {!readOnly && (
        <form className="op-add" onSubmit={submit}>
          <input
            type="password"
            autoComplete="off"
            value={key}
            placeholder={view.has_llm_key ? "Replace the key" : "Paste the provider key"}
            onChange={(e) => setKey(e.target.value)}
          />
          <button className="btn-sm" type="submit" disabled={busy || !key.trim()}>
            {busy ? "Checking…" : view.has_llm_key ? "Replace key" : "Set key"}
          </button>
          {view.has_llm_key && (
            <button className="btn-sm ghost danger" type="button" disabled={busy} onClick={remove}>
              Remove key
            </button>
          )}
        </form>
      )}
    </div>
  );
}

/** One sealed secret: write-only, with set/replace and clear. */
function SecretControl({
  title,
  which,
  isSet,
  readOnly,
  onChanged,
}: {
  title: string;
  which: api.StoredSecret;
  isSet: boolean;
  readOnly: boolean;
  onChanged: () => Promise<void>;
}) {
  const [secret, setSecret] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");

  const run = async (op: () => Promise<void>) => {
    setBusy(true);
    setError("");
    try {
      await op();
      setSecret("");
      await onChanged();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="card settings-section">
      <div className="row">
        <h3>{title}</h3>
        <span className={`pill ${isSet ? "pill-domain" : "pill-neutral"}`}>{isSet ? "set" : "not set"}</span>
      </div>
      {error && <p className="err">{error}</p>}
      {!readOnly && (
        <form
          className="op-add"
          onSubmit={(e) => {
            e.preventDefault();
            if (secret.trim()) void run(() => api.setSecret(which, secret));
          }}
        >
          <input
            type="password"
            autoComplete="off"
            value={secret}
            placeholder={isSet ? "Replace the secret" : "Paste the secret"}
            onChange={(e) => setSecret(e.target.value)}
          />
          <button className="btn-sm" type="submit" disabled={busy || !secret.trim()}>
            {isSet ? "Replace" : "Set"}
          </button>
          {isSet && (
            <button
              className="btn-sm ghost danger"
              type="button"
              disabled={busy}
              onClick={() => {
                if (window.confirm(`Clear the ${title.toLowerCase()}?`)) void run(() => api.clearSecret(which));
              }}
            >
              Clear
            </button>
          )}
        </form>
      )}
    </div>
  );
}
