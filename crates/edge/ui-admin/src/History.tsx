import { useEffect, useState } from "react";

import * as api from "./api";

/** A reusable revision-history panel for an item (agent/playbook/knowledge). */
export function History({
  kind,
  item,
  onRestored,
  onClose,
}: {
  kind: string;
  item: string;
  onRestored: () => void;
  onClose: () => void;
}) {
  const [revs, setRevs] = useState<api.RevisionMeta[] | null>(null);
  const [preview, setPreview] = useState<{ rev: number; content: string } | null>(null);
  const [error, setError] = useState("");

  const load = () =>
    api
      .listRevisions(kind, item)
      .then(setRevs)
      .catch((e) => setError(String(e)));

  useEffect(() => {
    void load();
  }, [kind, item]);

  return (
    <div className="history">
      <div className="head-row">
        <h3>History — {item}</h3>
        <button className="btn-sm ghost" onClick={onClose}>
          Close
        </button>
      </div>
      {error && <p className="err">{error}</p>}
      {!revs && !error && <p className="muted">Loading…</p>}
      {revs?.length === 0 && <p className="muted">No saved revisions yet.</p>}
      {revs?.map((r) => (
        <div className="rev-row" key={r.revision}>
          <span>
            <strong>#{r.revision}</strong>
            <span className="muted"> · {new Date(r.created_at).toLocaleString()} · {r.edited_by}</span>
          </span>
          <span style={{ display: "flex", gap: 6 }}>
            <button
              className="btn-sm ghost"
              onClick={async () => {
                try {
                  setPreview({ rev: r.revision, content: await api.getRevision(kind, item, r.revision) });
                } catch (e) {
                  setError(String(e));
                }
              }}
            >
              View
            </button>
            <button
              className="btn-sm"
              onClick={async () => {
                if (!window.confirm(`Restore revision #${r.revision}? This becomes the live version.`))
                  return;
                try {
                  await api.restoreRevision(kind, item, r.revision);
                  onRestored();
                } catch (e) {
                  setError(e instanceof Error ? e.message : String(e));
                }
              }}
            >
              Restore
            </button>
          </span>
        </div>
      ))}
      {preview && (
        <div className="rev-preview">
          <div className="muted" style={{ marginBottom: 6 }}>Revision #{preview.rev}</div>
          <pre>{preview.content}</pre>
        </div>
      )}
    </div>
  );
}
