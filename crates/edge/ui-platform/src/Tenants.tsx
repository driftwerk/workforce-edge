import { useMemo, useState } from "react";

import * as api from "./api";
import { Dialog, errorText, Pill } from "./Dialog";
import { llmPill, relative, statusPill } from "./format";
import { parsePlan, planForm, planSummary, PLAN_FIELDS, type PlanForm } from "./patch";
import { parseHostnames, suggestTenantId, tenantIdError } from "./slug";
import { useAsync } from "./useAsync";

/** Whether `tenant` matches a filter word: by id, name or any hostname. */
export function matchesFilter(tenant: api.Tenant, filter: string): boolean {
  const q = filter.trim().toLowerCase();
  if (!q) return true;
  return (
    tenant.id.includes(q) ||
    tenant.display_name.toLowerCase().includes(q) ||
    tenant.hostnames.some((h) => h.includes(q)) ||
    tenant.status.includes(q)
  );
}

export function Tenants({ onOpen }: { onOpen: (id: string) => void }) {
  const tenants = useAsync(() => api.listTenants(), []);
  const [filter, setFilter] = useState("");
  const [creating, setCreating] = useState(false);
  const rows = useMemo(() => (tenants.data ?? []).filter((t) => matchesFilter(t, filter)), [tenants.data, filter]);

  return (
    <>
      <div className="head-row">
        <div>
          <h2>Tenants</h2>
          <p className="sub">Every workspace this deployment serves: who they are, where they answer, what they may use.</p>
        </div>
        <div className="actions">
          <button className="btn-sm ghost" onClick={tenants.reload} disabled={tenants.loading}>
            Refresh
          </button>
          <button className="btn-sm" onClick={() => setCreating(true)}>
            New tenant
          </button>
        </div>
      </div>
      {tenants.error && <p className="err">{tenants.error}</p>}
      <div className="filter-row">
        <input placeholder="Filter by id, name, hostname or status…" value={filter} onChange={(e) => setFilter(e.target.value)} />
        {tenants.data && (
          <span className="count-badge">
            {rows.length === tenants.data.length ? `${tenants.data.length} tenants` : `${rows.length} of ${tenants.data.length}`}
          </span>
        )}
      </div>
      {tenants.loading && !tenants.data && <p className="spinner">Loading…</p>}
      {tenants.data && tenants.data.length === 0 && (
        <div className="empty">
          <div className="empty-title">No tenants yet</div>
          Create the first one, or wait for the portal to.
        </div>
      )}
      {tenants.data && tenants.data.length > 0 && rows.length === 0 && <p className="muted table-empty">Nothing matches that filter.</p>}
      {rows.length > 0 && (
        <div className="table-wrap">
          <div className="table-scroll">
            <table className="audit" style={{ minWidth: 820 }}>
              <thead>
                <tr>
                  <th>Tenant</th>
                  <th>Hostnames</th>
                  <th>Plan</th>
                  <th>Status</th>
                  <th>LLM key</th>
                  <th>Outbound</th>
                  <th>Created</th>
                </tr>
              </thead>
              <tbody>
                {rows.map((t) => {
                  const status = statusPill(t);
                  const llm = llmPill(t.llm_status);
                  return (
                    <tr key={t.id} className="clickable" onClick={() => onOpen(t.id)}>
                      <td>
                        <span className="cell-title">{t.display_name}</span>
                        <span className="cell-sub">{t.id}</span>
                      </td>
                      <td>
                        {t.hostnames.length === 0 ? (
                          <span className="muted">none</span>
                        ) : (
                          <div className="host-list">
                            {t.hostnames.map((h) => (
                              <span key={h}>{h}</span>
                            ))}
                          </div>
                        )}
                      </td>
                      <td className="muted">{planSummary(t.plan)}</td>
                      <td className="nowrap">
                        <Pill label={status.label} tone={status.tone} title={status.title} />
                      </td>
                      <td className="nowrap">
                        <Pill label={llm.label} tone={llm.tone} title={llm.title} />
                      </td>
                      <td className="nowrap">
                        <Pill label={t.outbound} tone={t.outbound === "public" ? "accent" : "neutral"} title={t.outbound === "public" ? "https only, no private addresses" : "any address: self-hosted posture"} />
                      </td>
                      <td className="muted nowrap" title={t.created_at}>
                        {relative(t.created_at)}
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        </div>
      )}
      {creating && (
        <NewTenantDialog
          onClose={() => setCreating(false)}
          onCreated={(id) => {
            setCreating(false);
            onOpen(id);
          }}
        />
      )}
    </>
  );
}

function NewTenantDialog({ onClose, onCreated }: { onClose: () => void; onCreated: (id: string) => void }) {
  const [id, setId] = useState("");
  const [idTouched, setIdTouched] = useState(false);
  const [name, setName] = useState("");
  const [hostnames, setHostnames] = useState("");
  const [plan, setPlan] = useState<PlanForm>(() => planForm({}));
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");

  const idError = tenantIdError(id);
  const hosts = parseHostnames(hostnames);
  const hostError = "error" in hosts ? hosts.error : null;
  const planParsed = parsePlan(plan);
  const planError = "error" in planParsed ? planParsed.error : null;
  const ready = !idError && name.trim() !== "" && !hostError && !planError && !busy;

  const onName = (value: string) => {
    setName(value);
    if (!idTouched) setId(suggestTenantId(value));
  };

  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!ready || "error" in hosts || "error" in planParsed) return;
    setBusy(true);
    setError("");
    try {
      const created = await api.createTenant({ id, display_name: name.trim(), hostnames: hosts.hostnames });
      if (Object.keys(planParsed.plan).length > 0) {
        try {
          await api.patchTenant(created.id, { plan: planParsed.plan });
        } catch (err) {
          // The tenant exists; its plan did not take. Say so, then open it.
          window.alert(`Tenant ${created.id} was created, but its plan was not set: ${errorText(err)}`);
        }
      }
      onCreated(created.id);
    } catch (err) {
      setError(errorText(err));
      setBusy(false);
    }
  };

  return (
    <Dialog
      title="New tenant"
      blurb="The registry row, a vector-store tenant with a key of its own, its hostnames and a running runtime. The id becomes its subdomain and cannot change."
      onClose={onClose}
      actions={
        <>
          <button className="btn-sm ghost" type="button" onClick={onClose} disabled={busy}>
            Cancel
          </button>
          <button className="btn-sm" type="submit" form="new-tenant" disabled={!ready}>
            {busy ? "Creating…" : "Create tenant"}
          </button>
        </>
      }
    >
      {error && <p className="err">{error}</p>}
      <form id="new-tenant" onSubmit={submit}>
        <div className="field">
          <label htmlFor="nt-name">Display name</label>
          <input id="nt-name" value={name} onChange={(e) => onName(e.target.value)} placeholder="Acme Ltd" autoFocus />
        </div>
        <div className="field">
          <label htmlFor="nt-id">
            Id <span className="hint">3–63 lowercase letters, digits and hyphens</span>
          </label>
          <input
            id="nt-id"
            className={`code${id && idError ? " invalid" : ""}`}
            value={id}
            onChange={(e) => {
              setIdTouched(true);
              setId(e.target.value);
            }}
            placeholder="acme"
            spellCheck={false}
          />
          {id && idError && <p className="field-err">{idError}</p>}
        </div>
        <div className="field">
          <label htmlFor="nt-hosts">
            Hostnames <span className="hint">one per line; optional</span>
          </label>
          <textarea
            id="nt-hosts"
            className={`code${hostError ? " invalid" : ""}`}
            rows={2}
            value={hostnames}
            onChange={(e) => setHostnames(e.target.value)}
            placeholder={id ? `${id}.marrilo.com` : "acme.marrilo.com"}
            spellCheck={false}
          />
          {hostError && <p className="field-err">{hostError}</p>}
        </div>
        <div className="field">
          <label>
            Plan caps <span className="hint">blank is no cap; set after creation</span>
          </label>
          <div className="plan-grid">
            {PLAN_FIELDS.map((f) => (
              <div className="field" key={f.key}>
                <label htmlFor={`nt-plan-${f.key}`} className="muted" style={{ fontWeight: 500 }}>
                  {f.label}
                </label>
                <input
                  id={`nt-plan-${f.key}`}
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
      </form>
    </Dialog>
  );
}
