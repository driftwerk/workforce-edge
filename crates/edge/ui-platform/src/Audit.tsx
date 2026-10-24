import { useEffect, useState } from "react";

import * as api from "./api";
import { errorText, Pill } from "./Dialog";
import { actionLabel, principalLabel, when } from "./format";

const PAGE = 100;

export function Audit({ onOpenTenant }: { onOpenTenant: (id: string) => void }) {
  const [entries, setEntries] = useState<api.AuditEntry[] | null>(null);
  const [exhausted, setExhausted] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");

  const load = async (before?: number) => {
    setBusy(true);
    setError("");
    try {
      const page = await api.listAudit(PAGE, before);
      setEntries((prev) => (before === undefined ? page : [...(prev ?? []), ...page]));
      setExhausted(page.length < PAGE);
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  };

  useEffect(() => {
    void load();
  }, []);

  const last = entries && entries.length > 0 ? entries[entries.length - 1].id : undefined;

  return (
    <>
      <div className="head-row">
        <div>
          <h2>Audit log</h2>
          <p className="sub">Every action the platform took on a tenant — by an operator here, or by a service such as billing — newest first.</p>
        </div>
        <div className="actions">
          <button className="btn-sm ghost" onClick={() => void load()} disabled={busy}>
            Refresh
          </button>
        </div>
      </div>
      {error && <p className="err">{error}</p>}
      {!entries && !error && <p className="spinner">Loading…</p>}
      {entries && entries.length === 0 && (
        <div className="empty">
          <div className="empty-title">Nothing recorded yet</div>
          Platform actions appear here as they happen.
        </div>
      )}
      {entries && entries.length > 0 && (
        <div className="table-wrap">
          <div className="table-scroll">
            <table className="audit" style={{ minWidth: 760 }}>
              <thead>
                <tr>
                  <th>When</th>
                  <th>Principal</th>
                  <th>Action</th>
                  <th>Tenant</th>
                  <th>Detail</th>
                </tr>
              </thead>
              <tbody>
                {entries.map((e) => {
                  const who = principalLabel(e.principal);
                  return (
                    <tr key={e.id}>
                      <td className="muted nowrap" title={e.at}>
                        {when(e.at)}
                      </td>
                      <td className="nowrap">
                        {who.service ? <Pill label={`${who.name} (service)`} tone="neutral" title={e.principal} /> : who.name}
                      </td>
                      <td>{actionLabel(e.action)}</td>
                      <td className="mono">
                        {e.tenant ? (
                          <button className="fb-link mono" onClick={() => onOpenTenant(e.tenant)}>
                            {e.tenant}
                          </button>
                        ) : (
                          <span className="muted">—</span>
                        )}
                      </td>
                      <td className="mono">{e.detail || <span className="muted">—</span>}</td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
          <div className="table-foot">
            {exhausted ? (
              <span className="muted">That is the whole log.</span>
            ) : (
              <button className="btn-sm ghost" onClick={() => void load(last)} disabled={busy}>
                {busy ? "Loading…" : "Load older"}
              </button>
            )}
            <span className="count-badge">{entries.length} entries</span>
          </div>
        </div>
      )}
    </>
  );
}
