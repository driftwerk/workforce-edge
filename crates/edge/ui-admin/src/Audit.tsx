import { useEffect, useState } from "react";

import * as api from "./api";

const LABEL: Record<string, string> = {
  "agent.update": "updated agent",
  "playbook.save": "saved playbook",
  "playbook.delete": "deleted playbook",
  "knowledge.save": "saved document",
  "knowledge.delete": "deleted document",
  "lesson.delete": "deleted lesson",
  "operator.add": "added operator",
  "operator.remove": "removed operator",
};

export function Audit() {
  const [entries, setEntries] = useState<api.AuditEntry[] | null>(null);
  const [error, setError] = useState("");

  useEffect(() => {
    api
      .listAudit()
      .then(setEntries)
      .catch((e) => setError(String(e)));
  }, []);

  return (
    <>
      <h2>Audit log</h2>
      <p className="sub">Every change made through this admin, newest first.</p>
      {error && <p className="err">{error}</p>}
      {!entries && !error && <p className="muted">Loading…</p>}
      {entries?.length === 0 && <p className="muted">No changes recorded yet.</p>}
      {entries && entries.length > 0 && (
        <table className="audit">
          <thead>
            <tr>
              <th>When</th>
              <th>Operator</th>
              <th>Action</th>
              <th>Target</th>
            </tr>
          </thead>
          <tbody>
            {entries.map((e) => (
              <tr key={e.id}>
                <td className="muted">{new Date(e.at).toLocaleString()}</td>
                <td>{e.operator}</td>
                <td>{LABEL[e.action] ?? e.action}</td>
                <td className="mono">
                  {e.target}
                  {e.detail && <span className="muted"> — {e.detail}</span>}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </>
  );
}
