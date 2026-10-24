import { useEffect, useState } from "react";

import * as api from "./api";
import { History } from "./History";

// One agent's training corpus — the docs it grounds answers in. Rendered inside
// the agent editor's "Training" tab, so training is always in the context of its
// agent (git-managed installs are read-only; edits go via the config repo).
export function AgentTraining({ agentId }: { agentId: string }) {
  const [docs, setDocs] = useState<api.KnowledgeDoc[] | null>(null);
  const [viewing, setViewing] = useState<api.KnowledgeDoc | null>(null);
  const [history, setHistory] = useState<string | null>(null);
  const [editing, setEditing] = useState<api.KnowledgeDoc | "new" | null>(null);
  const [error, setError] = useState("");
  const gitManaged = api.isGitManaged();

  const load = () => api.listKnowledge(agentId).then(setDocs).catch((e) => setError(String(e)));
  useEffect(() => {
    setDocs(null);
    setViewing(null);
    void load();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [agentId]);

  if (viewing) {
    return (
      <div className="card">
        <div className="row">
          <h3>{viewing.title}</h3>
          <button className="btn-sm ghost" onClick={() => setViewing(null)}>
            Back
          </button>
        </div>
        <p className="sub">
          {viewing.chunks} indexed chunk{viewing.chunks === 1 ? "" : "s"} ·{" "}
          {viewing.content.length.toLocaleString()} characters
        </p>
        <pre className="raw">{viewing.content}</pre>
      </div>
    );
  }
  if (history) {
    return (
      <History
        kind="knowledge"
        item={history}
        onClose={() => setHistory(null)}
        onRestored={() => {
          setHistory(null);
          void load();
        }}
      />
    );
  }
  if (editing) {
    return (
      <DocEditor
        agentId={agentId}
        doc={editing === "new" ? null : editing}
        onDone={() => {
          setEditing(null);
          void load();
        }}
        onCancel={() => setEditing(null)}
      />
    );
  }

  const total = docs?.reduce((n, d) => n + d.chunks, 0) ?? 0;
  return (
    <div>
      <div className="head-row" style={{ marginTop: 4 }}>
        <p className="sub">
          {docs
            ? `${docs.length} document(s), ${total} indexed chunks — recalled into this agent's answers.`
            : "Loading…"}
        </p>
        {!gitManaged && (
          <button className="btn-sm ghost" onClick={() => setEditing("new")}>
            Paste text
          </button>
        )}
      </div>
      {gitManaged && (
        <p className="banner-info">
          Managed in git — add markdown to this agent's <code>training/</code> folder in the config
          repo.
        </p>
      )}
      {error && <p className="err">{error}</p>}
      {docs?.length === 0 && (
        <div className="empty">
          <div className="icon">📄</div>
          <div className="empty-title">No training data for this agent yet</div>
          {gitManaged
            ? "Add markdown under agents/<id>/training/ in the config repo."
            : "Paste text or upload docs — the agent grounds its answers in them."}
        </div>
      )}
      {docs?.map((d) => (
        <div className="card" key={d.id}>
          <div className="row">
            <h3>{d.title}</h3>
            <span className="count-badge">
              {d.chunks} chunk{d.chunks === 1 ? "" : "s"}
            </span>
          </div>
          <p className="clamp" style={{ marginTop: 8 }}>
            {d.content}
          </p>
          <div className="actions">
            <button className="btn-sm ghost" onClick={() => setViewing(d)}>
              View
            </button>
            {!gitManaged && (
              <button className="btn-sm ghost" onClick={() => setEditing(d)}>
                Edit
              </button>
            )}
            <button className="btn-sm ghost" onClick={() => setHistory(`${agentId}/${d.id}`)}>
              History
            </button>
            {!gitManaged && (
              <button
                className="btn-sm ghost danger"
                onClick={async () => {
                  if (!window.confirm(`Delete "${d.title}"? The agent forgets it.`)) return;
                  try {
                    await api.deleteKnowledge(agentId, d.id);
                    void load();
                  } catch (e) {
                    setError(e instanceof Error ? e.message : String(e));
                  }
                }}
              >
                Delete
              </button>
            )}
          </div>
        </div>
      ))}
    </div>
  );
}

function DocEditor({
  agentId,
  doc,
  onDone,
  onCancel,
}: {
  agentId: string;
  doc: api.KnowledgeDoc | null;
  onDone: () => void;
  onCancel: () => void;
}) {
  const [title, setTitle] = useState(doc?.title ?? "");
  const [content, setContent] = useState(doc?.content ?? "");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const save = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!title.trim() || !content.trim()) {
      setError("Give the document a title and some content.");
      return;
    }
    setBusy(true);
    setError("");
    try {
      const id = doc?.id ?? title.trim().toLowerCase().replace(/[^a-z0-9]+/g, "-").slice(0, 80);
      await api.saveKnowledge(agentId, id, title.trim(), content);
      onDone();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
      setBusy(false);
    }
  };
  return (
    <form className="card editing" onSubmit={save}>
      {error && <p className="err">{error}</p>}
      <label>Title</label>
      <input value={title} onChange={(e) => setTitle(e.target.value)} placeholder="e.g. Refund policy" />
      <label style={{ marginTop: 10, display: "block" }}>Content</label>
      <textarea rows={16} value={content} onChange={(e) => setContent(e.target.value)} />
      <div className="actions">
        <button className="btn-sm" type="submit" disabled={busy}>
          {busy ? "Indexing…" : "Save & index"}
        </button>
        <button className="btn-sm ghost" type="button" onClick={onCancel} disabled={busy}>
          Cancel
        </button>
      </div>
    </form>
  );
}
