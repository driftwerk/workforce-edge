import { useEffect, useRef, useState } from "react";

import * as api from "./api";
import { History } from "./History";

function slug(title: string): string {
  return (
    title
      .toLowerCase()
      .replace(/[^a-z0-9]+/g, "-")
      .replace(/^-+|-+$/g, "")
      .slice(0, 80) || "doc"
  );
}

export function Knowledge({ nav }: { nav: import("./main").Nav }) {
  const [agents, setAgents] = useState<api.Agent[] | null>(null);
  const [docs, setDocs] = useState<api.KnowledgeDoc[] | null>(null);
  const [editing, setEditing] = useState<api.KnowledgeDoc | "new" | null>(null);
  const [history, setHistory] = useState<string | null>(null);
  const [uploads, setUploads] = useState<api.UploadResult[] | null>(null);
  const [busy, setBusy] = useState(false);
  const [drag, setDrag] = useState(false);
  const [error, setError] = useState("");
  const fileRef = useRef<HTMLInputElement>(null);

  // /admin/knowledge/<agent>/<doc?> — whose training we're viewing, and an
  // optional doc to open. Agent defaults to the first when the URL omits it.
  const agent = nav.params[0] || agents?.[0]?.id || "";
  const viewing = nav.params[1] ? (docs?.find((d) => d.id === nav.params[1]) ?? null) : null;

  useEffect(() => {
    api.listAgents().then(setAgents).catch((e) => setError(String(e)));
  }, []);

  const load = (a: string) => api.listKnowledge(a).then(setDocs).catch((e) => setError(String(e)));

  useEffect(() => {
    if (!agent) return;
    setDocs(null);
    setEditing(null);
    void load(agent);
  }, [agent]);

  const upload = async (files: File[]) => {
    if (!files.length) return;
    setBusy(true);
    setError("");
    setUploads(null);
    try {
      const results = await api.uploadKnowledge(agent, files);
      setUploads(results);
      void load(agent);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
      if (fileRef.current) fileRef.current.value = "";
    }
  };

  const totalChunks = docs?.reduce((n, d) => n + d.chunks, 0) ?? 0;
  const agentName = agents?.find((a) => a.id === agent)?.display_name;

  if (editing) {
    return (
      <DocEditor
        agent={agent}
        doc={editing === "new" ? null : editing}
        onDone={() => {
          setEditing(null);
          void load(agent);
        }}
        onCancel={() => setEditing(null)}
      />
    );
  }
  if (viewing) return <DocViewer doc={viewing} onBack={() => nav.go(agent)} />;

  return (
    <>
      <div className="head-row">
        <div>
          <h2>Training data</h2>
          <p className="sub">
            Per agent — documents{agentName ? ` ${agentName}` : " the selected agent"} answers from.
            {docs ? ` ${docs.length} document(s), ${totalChunks} indexed chunks.` : ""}
          </p>
        </div>
        <span style={{ display: "flex", gap: 8, alignItems: "center" }}>
          {agents && agents.length > 1 ? (
            <label style={{ display: "flex", gap: 6, alignItems: "center" }}>
              <span className="sub" style={{ fontSize: 12 }}>
                Agent
              </span>
              <select className="select" value={agent} onChange={(e) => nav.go(e.target.value)}>
                {agents.map((a) => (
                  <option key={a.id} value={a.id}>
                    {a.display_name}
                  </option>
                ))}
              </select>
            </label>
          ) : (
            agentName && <span className="pill">Agent: {agentName}</span>
          )}
          {!api.isGitManaged() && (
            <button className="btn-sm ghost" onClick={() => setEditing("new")}>
              Paste text
            </button>
          )}
        </span>
      </div>
      {api.isGitManaged() && (
        <p className="banner-info">
          Training data is managed in git and is read-only here.
        </p>
      )}

      {history && (
        <History
          kind="knowledge"
          item={history}
          onClose={() => setHistory(null)}
          onRestored={() => {
            setHistory(null);
            void load(agent);
          }}
        />
      )}

      {!history && !api.isGitManaged() && (
        <>
          <div
            className={drag ? "dropzone over" : "dropzone"}
            onDragOver={(e) => {
              e.preventDefault();
              setDrag(true);
            }}
            onDragLeave={() => setDrag(false)}
            onDrop={(e) => {
              e.preventDefault();
              setDrag(false);
              void upload(Array.from(e.dataTransfer.files));
            }}
            onClick={() => fileRef.current?.click()}
          >
            <input
              ref={fileRef}
              type="file"
              multiple
              accept={api.UPLOAD_ACCEPT}
              hidden
              onChange={(e) => void upload(Array.from(e.target.files ?? []))}
            />
            <div className="dz-icon">⬆</div>
            <div className="dz-title">{busy ? "Uploading…" : "Drop files here or click to upload"}</div>
            <div className="dz-sub">PDF, Word (.docx), Markdown, text, CSV, HTML — up to 32 MB each</div>
          </div>

          {uploads && (
            <div className="upload-results">
              {uploads.map((r, i) => (
                <div className={r.ok ? "ur ur-ok" : "ur ur-err"} key={i}>
                  <span>{r.ok ? "✓" : "✕"}</span>
                  <strong>{r.name}</strong>
                  <span className="muted">{r.ok ? `${r.chunks} chunks indexed` : r.error}</span>
                </div>
              ))}
              <button className="btn-sm ghost" onClick={() => setUploads(null)} style={{ marginTop: 6 }}>
                Dismiss
              </button>
            </div>
          )}
        </>
      )}

      {/* The doc list shows in BOTH modes — read-only when git-managed. */}
      {!history && (
        <>
          {error && <p className="err">{error}</p>}
          {!docs && !error && <p className="spinner">Loading…</p>}
          {docs?.length === 0 && (
            <div className="empty">
              <div className="icon">📄</div>
              <div className="empty-title">No training data yet</div>
              {api.isGitManaged()
                ? "Add markdown to this agent's training/ folder in the config repo."
                : "Upload product docs, FAQs, or policies — the agent grounds its answers in them."}
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
                <button className="btn-sm ghost" onClick={() => nav.go(agent, d.id)}>
                  View
                </button>
                {!api.isGitManaged() && (
                  <button className="btn-sm ghost" onClick={() => setEditing(d)}>
                    Edit
                  </button>
                )}
                <button className="btn-sm ghost" onClick={() => setHistory(`${agent}/${d.id}`)}>
                  History
                </button>
                {!api.isGitManaged() && (
                  <button
                    className="btn-sm ghost danger"
                    onClick={async () => {
                      if (!window.confirm(`Delete "${d.title}"? The agent forgets it and it's removed from the index.`)) return;
                      try {
                        await api.deleteKnowledge(agent, d.id);
                        void load(agent);
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
        </>
      )}
    </>
  );
}

function DocViewer({ doc, onBack }: { doc: api.KnowledgeDoc; onBack: () => void }) {
  return (
    <>
      <div className="transcript-head">
        <button className="btn-sm ghost" onClick={onBack}>
          ← Back
        </button>
        <div>
          <h2 style={{ marginBottom: 1 }}>{doc.title}</h2>
          <p className="sub" style={{ marginBottom: 0 }}>
            {doc.chunks} indexed chunk{doc.chunks === 1 ? "" : "s"} · {doc.content.length.toLocaleString()} characters
          </p>
        </div>
      </div>
      <pre className="raw">{doc.content}</pre>
    </>
  );
}

function DocEditor({
  agent,
  doc,
  onDone,
  onCancel,
}: {
  agent: string;
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
      const id = doc?.id ?? `${slug(title)}-${Date.now().toString(36)}`;
      await api.saveKnowledge(agent, id, title.trim(), content);
      onDone();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
      setBusy(false);
    }
  };

  return (
    <form className="card editing" onSubmit={save}>
      <div className="row">
        <h3>{doc ? "Edit document" : "New document"}</h3>
      </div>
      {error && <p className="err">{error}</p>}
      <div className="form-grid">
        <label>Title</label>
        <input value={title} onChange={(e) => setTitle(e.target.value)} placeholder="e.g. Refund policy" />
        <label>
          Content
          <span className="hint">plain text or markdown; split into paragraphs</span>
        </label>
        <textarea rows={16} value={content} onChange={(e) => setContent(e.target.value)} />
      </div>
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
