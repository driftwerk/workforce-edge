import { useEffect, useState } from "react";

import * as api from "./api";
import { Dialog, errorText, Pill, StatCard } from "./Dialog";
import { llmPill, relative, statusPill, when } from "./format";
import {
  adminForm,
  diffTenant,
  editableOf,
  parseAdmin,
  parsePlan,
  planForm,
  PLAN_FIELDS,
  type AdminForm,
  type Editable,
  type PlanForm,
} from "./patch";
import { hostnameError, normaliseHostname } from "./slug";
import { usageHint, usageRows, usageText } from "./usage";
import { useAsync } from "./useAsync";

export function TenantDetail({ id, onBack, onDeleted }: { id: string; onBack: () => void; onDeleted: () => void }) {
  const tenant = useAsync(() => api.getTenant(id), [id]);
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  const [busy, setBusy] = useState<"" | "suspend" | "resume" | "rescue">("");
  const [suspending, setSuspending] = useState(false);
  const [deleting, setDeleting] = useState(false);

  /** Run a write that answers with the tenant, and show what it said. */
  const act = async (kind: "suspend" | "resume" | "rescue", run: () => Promise<api.Tenant | void>, done?: string) => {
    setBusy(kind);
    setError("");
    setNotice("");
    try {
      const next = await run();
      if (next) tenant.setData(next);
      if (done) setNotice(done);
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy("");
    }
  };

  const rescue = () =>
    act("rescue", async () => {
      // Open the tab first, so the browser treats it as the click's own
      // window rather than a pop-up; then point it at the one-time URL.
      const tab = window.open("", "_blank");
      try {
        const ticket = await api.rescueTenant(id);
        if (tab) tab.location.href = ticket.url;
        else window.location.href = ticket.url;
        setNotice(`Rescue session opened in a new tab (the link is good for ${ticket.expires_in_secs}s, once).`);
      } catch (e) {
        tab?.close();
        throw e;
      }
    });

  const t = tenant.data;
  return (
    <>
      <button className="crumb" onClick={onBack}>
        ← Tenants
      </button>
      {tenant.error && !t && <p className="err">{tenant.error}</p>}
      {tenant.loading && !t && <p className="spinner">Loading…</p>}
      {t && (
        <>
          <div className="head-row">
            <div>
              <div className="title-line">
                <h2>{t.display_name}</h2>
                <span className="id">{t.id}</span>
                <Pill {...statusPill(t)} />
                <Pill {...llmPill(t.llm_status)} />
              </div>
              <p className="sub">
                Created {when(t.created_at)} · updated {relative(t.updated_at)}
                {t.has_admin ? " · admin sign-in on" : " · no admin sign-in"}
              </p>
            </div>
            <div className="actions">
              {t.status === "active" ? (
                <button className="btn-sm ghost danger" onClick={() => setSuspending(true)} disabled={busy !== ""}>
                  Suspend
                </button>
              ) : (
                <button className="btn-sm" onClick={() => act("resume", () => api.resumeTenant(id), "Tenant resumed.")} disabled={busy !== ""}>
                  {busy === "resume" ? "Resuming…" : "Resume"}
                </button>
              )}
              <button
                className="btn-sm ghost"
                onClick={rescue}
                disabled={busy !== "" || !t.serving || !t.has_admin}
                title={!t.serving ? "The tenant is not being served" : !t.has_admin ? "The tenant has no admin surface" : "Open the tenant's admin as yourself"}
              >
                {busy === "rescue" ? "Opening…" : "Rescue"}
              </button>
            </div>
          </div>
          {t.status === "suspended" && (
            <p className="banner bad">
              <strong>Suspended.</strong> Its callers see: “{t.unavailable ?? "Suspended"}”
            </p>
          )}
          {t.status === "active" && !t.serving && (
            <p className="banner warn">
              <strong>Not serving.</strong> {t.unavailable ?? "No runtime answers on its hostnames; resume to rebuild it."}
            </p>
          )}
          {error && <p className="err">{error}</p>}
          {notice && <p className="ok-line">{notice}</p>}

          <UsageCard id={id} serving={t.serving} />
          <DetailsCard tenant={t} onSaved={tenant.setData} />
          <HostnamesCard tenant={t} onChanged={tenant.setData} />
          <OperatorsCard id={id} />
          <AdminCard tenant={t} onSaved={tenant.setData} />
          <DangerCard tenant={t} onDelete={() => setDeleting(true)} />

          {suspending && (
            <SuspendDialog
              tenant={t}
              busy={busy === "suspend"}
              onClose={() => setSuspending(false)}
              onSuspend={(message) =>
                act("suspend", () => api.suspendTenant(id, message), "Tenant suspended.").then(() => setSuspending(false))
              }
            />
          )}
          {deleting && <DeleteDialog tenant={t} onClose={() => setDeleting(false)} onDeleted={onDeleted} />}
        </>
      )}
    </>
  );
}

function UsageCard({ id, serving }: { id: string; serving: boolean }) {
  const snapshot = useAsync(() => api.tenantUsage(id), [id]);
  const rows = usageRows(snapshot.data?.plan ?? {}, snapshot.data?.usage ?? null);
  return (
    <div className="card">
      <div className="row">
        <h3>Usage against plan</h3>
        <button className="btn-sm ghost" onClick={snapshot.reload} disabled={snapshot.loading}>
          Recount
        </button>
      </div>
      <p className="muted blurb">Counted from the tenant's own stores, within a minute. Monthly figures restart on the first of the month (UTC).</p>
      {snapshot.error && <p className="err">{snapshot.error}</p>}
      <div className="kpi-grid">
        {rows.map((row) => (
          <StatCard
            key={row.field}
            label={row.label}
            value={usageText(row)}
            hint={usageHint(row, snapshot.data?.serving ?? serving)}
            isLoading={snapshot.loading && !snapshot.data}
            over={row.over}
          />
        ))}
      </div>
    </div>
  );
}

function DetailsCard({ tenant, onSaved }: { tenant: api.Tenant; onSaved: (t: api.Tenant) => void }) {
  const [name, setName] = useState(tenant.display_name);
  const [outbound, setOutbound] = useState<api.Outbound>(tenant.outbound);
  const [plan, setPlan] = useState<PlanForm>(() => planForm(tenant.plan));
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [saved, setSaved] = useState(false);
  useEffect(() => {
    setName(tenant.display_name);
    setOutbound(tenant.outbound);
    setPlan(planForm(tenant.plan));
  }, [tenant]);

  const parsed = parsePlan(plan);
  const planError = "error" in parsed ? parsed.error : null;
  const original = editableOf(tenant);
  const edited: Editable = { ...original, display_name: name, outbound, plan: "plan" in parsed ? parsed.plan : original.plan };
  const patch = planError ? null : diffTenant(original, edited);

  const save = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!patch) return;
    setBusy(true);
    setError("");
    setSaved(false);
    try {
      onSaved(await api.patchTenant(tenant.id, patch));
      setSaved(true);
    } catch (err) {
      setError(errorText(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <form className="card" onSubmit={save}>
      <h3>Details</h3>
      <p className="muted blurb">The name, the outbound posture its own URLs are reached under, and the caps its plan sets. A change rebuilds the running tenant at once.</p>
      {error && <p className="err">{error}</p>}
      {saved && <p className="ok-line">Saved.</p>}
      <div className="form-grid">
        <label htmlFor="d-name">Display name</label>
        <input id="d-name" value={name} onChange={(e) => setName(e.target.value)} />
        <label htmlFor="d-outbound">Outbound</label>
        <div>
          <select id="d-outbound" value={outbound} onChange={(e) => setOutbound(e.target.value as api.Outbound)}>
            <option value="public">public — https only, no private addresses</option>
            <option value="private">private — any address (self-hosted posture)</option>
          </select>
          <p className="field-hint">Set here and nowhere else: the tenant's own settings cannot loosen it.</p>
        </div>
        <label>Plan caps</label>
        <div>
          <div className="plan-grid">
            {PLAN_FIELDS.map((f) => (
              <div className="field" key={f.key}>
                <label htmlFor={`d-plan-${f.key}`} className="muted" style={{ fontWeight: 500 }} title={f.hint}>
                  {f.label}
                </label>
                <input
                  id={`d-plan-${f.key}`}
                  inputMode="numeric"
                  value={plan[f.key]}
                  onChange={(e) => setPlan({ ...plan, [f.key]: e.target.value })}
                  placeholder="no cap"
                />
              </div>
            ))}
          </div>
          {planError && <p className="field-err">{planError}</p>}
        </div>
      </div>
      <div className="actions">
        <button className="btn-sm" type="submit" disabled={!patch || busy}>
          {busy ? "Saving…" : "Save changes"}
        </button>
        {patch && !busy && <span className="muted">Changes: {Object.keys(patch).join(", ")}</span>}
      </div>
    </form>
  );
}

function HostnamesCard({ tenant, onChanged }: { tenant: api.Tenant; onChanged: (t: api.Tenant) => void }) {
  const [value, setValue] = useState("");
  const [busy, setBusy] = useState("");
  const [error, setError] = useState("");
  const fault = value.trim() ? hostnameError(value) : null;
  const add = async (e: React.FormEvent) => {
    e.preventDefault();
    if (fault || !value.trim()) return;
    setBusy("add");
    setError("");
    try {
      onChanged(await api.addHostname(tenant.id, normaliseHostname(value)));
      setValue("");
    } catch (err) {
      setError(errorText(err));
    } finally {
      setBusy("");
    }
  };
  const remove = async (host: string) => {
    if (!window.confirm(`Stop answering on ${host}? Its certificate stops being issued too.`)) return;
    setBusy(host);
    setError("");
    try {
      onChanged(await api.removeHostname(tenant.id, host));
    } catch (err) {
      setError(errorText(err));
    } finally {
      setBusy("");
    }
  };
  return (
    <div className="card">
      <h3>Hostnames</h3>
      <p className="muted blurb">Where the tenant answers. A name is live the moment it is added, and the ingress issues its certificate on the first visit.</p>
      {error && <p className="err">{error}</p>}
      <form className="op-add" onSubmit={add}>
        <input className={`code${fault ? " invalid" : ""}`} value={value} onChange={(e) => setValue(e.target.value)} placeholder={`${tenant.id}.marrilo.com`} spellCheck={false} />
        <button className="btn-sm" type="submit" disabled={busy !== "" || !value.trim() || !!fault}>
          Add
        </button>
      </form>
      {fault && <p className="field-err" style={{ marginTop: -8, marginBottom: 12 }}>{fault}</p>}
      {tenant.hostnames.length === 0 && <p className="muted">No hostnames: nothing reaches this tenant.</p>}
      {tenant.hostnames.map((h) => (
        <div className="op-row" key={h}>
          <span className="mono">{h}</span>
          <button className="btn-sm ghost danger" onClick={() => remove(h)} disabled={busy !== ""}>
            {busy === h ? "Removing…" : "Remove"}
          </button>
        </div>
      ))}
    </div>
  );
}

function OperatorsCard({ id }: { id: string }) {
  const ops = useAsync(() => api.listOperators(id), [id]);
  const [kind, setKind] = useState<api.OperatorKind>("email");
  const [value, setValue] = useState("");
  const [busy, setBusy] = useState("");
  const [error, setError] = useState("");
  const add = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!value.trim()) return;
    setBusy("add");
    setError("");
    try {
      await api.addOperator(id, kind, value.trim());
      setValue("");
      ops.reload();
    } catch (err) {
      setError(errorText(err));
    } finally {
      setBusy("");
    }
  };
  const remove = async (op: api.Operator) => {
    if (!window.confirm(`Remove ${op.kind} "${op.value}" from the tenant's admin allowlist?`)) return;
    setBusy(`${op.kind}:${op.value}`);
    setError("");
    try {
      await api.removeOperator(id, op.kind, op.value);
      ops.reload();
    } catch (err) {
      setError(errorText(err));
    } finally {
      setBusy("");
    }
  };
  return (
    <div className="card">
      <h3>Operators</h3>
      <p className="muted blurb">
        Who may sign into the tenant's admin: an exact email, or a whole domain. In force at their next sign-in; a rescue can always open the admin regardless.
      </p>
      {error && <p className="err">{error}</p>}
      {ops.error && <p className="err">{ops.error}</p>}
      <form className="op-add" onSubmit={add}>
        <select value={kind} onChange={(e) => setKind(e.target.value as api.OperatorKind)}>
          <option value="email">Email</option>
          <option value="domain">Domain</option>
        </select>
        <input value={value} placeholder={kind === "email" ? "person@company.com" : "company.com"} onChange={(e) => setValue(e.target.value)} />
        <button className="btn-sm" type="submit" disabled={busy !== "" || !value.trim()}>
          Add
        </button>
      </form>
      {ops.loading && !ops.data && <p className="muted">Loading…</p>}
      {ops.data?.length === 0 && <p className="muted">No operators: nobody can sign into this tenant's admin yet.</p>}
      {ops.data?.map((o) => (
        <div className="op-row" key={`${o.kind}:${o.value}`}>
          <span>
            <Pill label={o.kind} tone={o.kind === "domain" ? "ok" : "accent"} /> <strong>{o.value}</strong>
            <span className="muted">
              {" "}
              · added by {o.added_by} {relative(o.added_at)}
            </span>
          </span>
          <button className="btn-sm ghost danger" onClick={() => remove(o)} disabled={busy !== ""}>
            {busy === `${o.kind}:${o.value}` ? "Removing…" : "Remove"}
          </button>
        </div>
      ))}
    </div>
  );
}

function AdminCard({ tenant, onSaved }: { tenant: api.Tenant; onSaved: (t: api.Tenant) => void }) {
  const [form, setForm] = useState<AdminForm>(() => adminForm(tenant.admin));
  const [secret, setSecret] = useState("");
  const [clearSecret, setClearSecret] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [saved, setSaved] = useState(false);
  useEffect(() => {
    setForm(adminForm(tenant.admin));
    setSecret("");
    setClearSecret(false);
  }, [tenant]);
  const set = (patch: Partial<AdminForm>) => setForm((f) => ({ ...f, ...patch }));

  const parsed = parseAdmin(form);
  const formError = "error" in parsed ? parsed.error : null;
  const original = editableOf(tenant);
  const edited: Editable = {
    ...original,
    admin: "admin" in parsed ? parsed.admin : original.admin,
    admin_client_secret: secret,
    clear_admin_client_secret: clearSecret,
  };
  const patch = formError ? null : diffTenant(original, edited);
  const turnedOff = tenant.admin !== null && !form.enabled;

  const save = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!patch) return;
    setBusy(true);
    setError("");
    setSaved(false);
    try {
      onSaved(await api.patchTenant(tenant.id, patch));
      setSaved(true);
    } catch (err) {
      setError(errorText(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <form className="card" onSubmit={save}>
      <h3>Admin sign-in</h3>
      <p className="muted blurb">
        The OIDC client the tenant's operators sign into its admin with, and the allowlist it is seeded with when empty. The platform's way to provision a sign-in for a tenant that has none.
        {tenant.has_admin_client_secret ? " A client secret is sealed on the row." : " No client secret is stored (a public PKCE client)."}
      </p>
      {error && <p className="err">{error}</p>}
      {saved && <p className="ok-line">Saved.</p>}
      {turnedOff && <p className="note">A sign-in cannot be removed through the platform API; the form is kept so the section can be edited again.</p>}
      <div className="form-grid">
        <label htmlFor="a-enabled">Sign-in</label>
        <div className="inline">
          <input id="a-enabled" type="checkbox" style={{ width: 15, height: 15 }} checked={form.enabled} onChange={(e) => set({ enabled: e.target.checked })} />
          <span className="muted">{form.enabled ? "configured below" : "none: the admin is reachable through rescue only"}</span>
        </div>
        {form.enabled && (
          <>
            <label htmlFor="a-issuer">Issuer</label>
            <input id="a-issuer" className="code" value={form.issuer} onChange={(e) => set({ issuer: e.target.value })} placeholder="https://accounts.google.com" spellCheck={false} />
            <label htmlFor="a-client">Client id</label>
            <input id="a-client" className="code" value={form.client_id} onChange={(e) => set({ client_id: e.target.value })} spellCheck={false} />
            <label htmlFor="a-aud">
              Audience <span className="hint">defaults to the client id</span>
            </label>
            <input id="a-aud" className="code" value={form.audience} onChange={(e) => set({ audience: e.target.value })} spellCheck={false} />
            <label htmlFor="a-mode">Mode</label>
            <select id="a-mode" value={form.mode} onChange={(e) => set({ mode: e.target.value as AdminForm["mode"] })}>
              <option value="">by issuer</option>
              <option value="google">google — Identity Services button</option>
              <option value="redirect">redirect — Authorization Code + PKCE</option>
            </select>
            <label htmlFor="a-url">
              Public URL <span className="hint">the exact redirect base; derived from the host when blank</span>
            </label>
            <input id="a-url" className="code" value={form.public_url} onChange={(e) => set({ public_url: e.target.value })} placeholder={`https://${tenant.hostnames[0] ?? `${tenant.id}.marrilo.com`}`} spellCheck={false} />
            <label htmlFor="a-emails">
              Seed emails <span className="hint">one per line</span>
            </label>
            <textarea id="a-emails" className="code" rows={2} value={form.allowed_emails} onChange={(e) => set({ allowed_emails: e.target.value })} spellCheck={false} />
            <label htmlFor="a-domains">
              Seed domains <span className="hint">one per line</span>
            </label>
            <textarea id="a-domains" className="code" rows={2} value={form.allowed_domains} onChange={(e) => set({ allowed_domains: e.target.value })} spellCheck={false} />
            <label htmlFor="a-hours">Session hours</label>
            <div className="inline">
              <input id="a-hours" inputMode="numeric" value={form.session_hours} onChange={(e) => set({ session_hours: e.target.value })} />
            </div>
            <label htmlFor="a-secret">
              Client secret <span className="hint">write-only</span>
            </label>
            <div>
              <input
                id="a-secret"
                type="password"
                autoComplete="new-password"
                value={secret}
                disabled={clearSecret}
                onChange={(e) => setSecret(e.target.value)}
                placeholder={tenant.has_admin_client_secret ? "unchanged" : "none (public client)"}
              />
              {tenant.has_admin_client_secret && (
                <label className="inline" style={{ marginTop: 8, fontWeight: 500 }}>
                  <input type="checkbox" style={{ width: 15, height: 15 }} checked={clearSecret} onChange={(e) => setClearSecret(e.target.checked)} />
                  <span className="muted">clear the stored secret</span>
                </label>
              )}
            </div>
          </>
        )}
      </div>
      {formError && form.enabled && <p className="field-err">{formError}</p>}
      <div className="actions">
        <button className="btn-sm" type="submit" disabled={!patch || busy}>
          {busy ? "Saving…" : "Save sign-in"}
        </button>
        {patch && !busy && <span className="muted">Changes: {Object.keys(patch).join(", ")}</span>}
      </div>
    </form>
  );
}

function DangerCard({ tenant, onDelete }: { tenant: api.Tenant; onDelete: () => void }) {
  const isDefault = tenant.id === "default";
  return (
    <div className="card danger-zone">
      <h3>Delete tenant</h3>
      <p className="muted blurb">
        Retires the runtime, drops its vector-store tenant (its data, keys and backups with it), removes its files and every row of its own. There is no undo.
        {isDefault && " The default tenant cannot be deleted."}
      </p>
      <button className="btn-sm ghost danger" onClick={onDelete} disabled={isDefault}>
        Delete {tenant.id}…
      </button>
    </div>
  );
}

function SuspendDialog({
  tenant,
  busy,
  onClose,
  onSuspend,
}: {
  tenant: api.Tenant;
  busy: boolean;
  onClose: () => void;
  onSuspend: (message: string) => void;
}) {
  const [message, setMessage] = useState("");
  return (
    <Dialog
      title={`Suspend ${tenant.display_name}`}
      blurb="Its runtime stops, its open sockets close, and every route answers with the message below until the tenant is resumed. Its data stays."
      onClose={onClose}
      actions={
        <>
          <button className="btn-sm ghost" type="button" onClick={onClose} disabled={busy}>
            Cancel
          </button>
          <button className="btn-sm solid-danger" type="button" onClick={() => onSuspend(message)} disabled={busy}>
            {busy ? "Suspending…" : "Suspend tenant"}
          </button>
        </>
      }
    >
      <div className="field">
        <label htmlFor="s-msg">
          Message to its callers <span className="hint">optional; a fixed sentence when blank</span>
        </label>
        <textarea id="s-msg" rows={3} value={message} onChange={(e) => setMessage(e.target.value)} placeholder="This workspace is suspended. Contact billing@marrilo.com." autoFocus />
      </div>
    </Dialog>
  );
}

function DeleteDialog({ tenant, onClose, onDeleted }: { tenant: api.Tenant; onClose: () => void; onDeleted: () => void }) {
  const [typed, setTyped] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const confirmed = typed === tenant.id;
  const remove = async () => {
    if (!confirmed) return;
    setBusy(true);
    setError("");
    try {
      await api.deleteTenant(tenant.id);
      onDeleted();
    } catch (e) {
      setError(errorText(e));
      setBusy(false);
    }
  };
  return (
    <Dialog
      title={`Delete ${tenant.display_name}`}
      blurb={
        <>
          This deletes everything the tenant owns. Type <code>{tenant.id}</code> to confirm.
        </>
      }
      onClose={onClose}
      actions={
        <>
          <button className="btn-sm ghost" type="button" onClick={onClose} disabled={busy}>
            Cancel
          </button>
          <button className="btn-sm solid-danger" type="button" onClick={remove} disabled={!confirmed || busy}>
            {busy ? "Deleting…" : "Delete tenant"}
          </button>
        </>
      }
    >
      {error && <p className="err">{error}</p>}
      <div className="field">
        <label htmlFor="del-id">Tenant id</label>
        <input id="del-id" className="code" value={typed} onChange={(e) => setTyped(e.target.value)} placeholder={tenant.id} spellCheck={false} autoFocus />
      </div>
    </Dialog>
  );
}
