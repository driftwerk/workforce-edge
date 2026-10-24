import { useEffect, useState } from "react";

import * as api from "./api";
import { History } from "./History";

const TEMPLATE = `[playbook]
id = "new-flow"
name = "New flow"
description = "What this flow guides the user through."

[[step]]
id = "welcome"
title = "Welcome"
prompt = "Greet the user and learn what they need."

[[step]]
id = "wrapup"
title = "Wrap up"
prompt = "Summarise and finish."
`;

export function Playbooks({ nav }: { nav: import("./main").Nav }) {
  const [items, setItems] = useState<api.Playbook[] | null>(null);
  const [error, setError] = useState("");
  const [history, setHistory] = useState<string | null>(null);

  // /admin/playbooks/<id> opens a playbook; /admin/playbooks/new creates one.
  const param = nav.params[0] ?? null;
  const creating = param === "new";
  const editing = param && !creating ? (items?.find((p) => p.id === param) ?? null) : null;

  const load = () =>
    api
      .listPlaybooks()
      .then(setItems)
      .catch((e) => setError(String(e)));

  useEffect(() => {
    void load();
  }, []);

  const done = () => {
    nav.go();
    void load();
  };

  if (creating || editing) {
    return (
      <PlaybookEditor
        initialId={editing?.id ?? ""}
        initialSource={editing?.source ?? TEMPLATE}
        isNew={creating}
        onDone={done}
        onCancel={() => nav.go()}
      />
    );
  }

  return (
    <>
      <div className="head-row">
        <div>
          <h2>Playbooks</h2>
          <p className="sub">Guided flows. Edits apply to new conversations immediately.</p>
        </div>
        {!api.isGitManaged() && (
          <button className="btn-sm" onClick={() => nav.go("new")}>
            New playbook
          </button>
        )}
      </div>
      {api.isGitManaged() && (
        <p className="banner-info">Playbooks are managed in git and are read-only here.</p>
      )}
      {history && (
        <History
          kind="playbook"
          item={history}
          onClose={() => setHistory(null)}
          onRestored={() => {
            setHistory(null);
            void load();
          }}
        />
      )}
      {!history && error && <p className="err">{error}</p>}
      {!items && !error && <p className="muted">Loading…</p>}
      {items?.length === 0 && <p className="muted">No playbooks yet. Create one to get started.</p>}
      {!history && items?.map((p) => (
        <div className="card" key={p.id}>
          <div className="row">
            <h3>{p.name}</h3>
            <span className="id">{p.id}</span>
          </div>
          <p className="muted" style={{ margin: "6px 0 0" }}>
            {p.steps} step{p.steps === 1 ? "" : "s"}
          </p>
          <div className="actions">
            <button className="btn-sm" onClick={() => nav.go(p.id)}>
              Edit
            </button>
            <button className="btn-sm ghost" onClick={() => setHistory(p.id)}>
              History
            </button>
            <button
              className="btn-sm ghost danger"
              onClick={async () => {
                if (!window.confirm(`Delete playbook "${p.id}"?`)) return;
                try {
                  await api.deletePlaybook(p.id);
                  void load();
                } catch (e) {
                  setError(e instanceof Error ? e.message : String(e));
                }
              }}
            >
              Delete
            </button>
          </div>
        </div>
      ))}
    </>
  );
}

function PlaybookEditor({
  initialId,
  initialSource,
  isNew,
  onDone,
  onCancel,
}: {
  initialId: string;
  initialSource: string;
  isNew: boolean;
  onDone: () => void;
  onCancel: () => void;
}) {
  const [source, setSource] = useState(initialSource);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const readOnly = api.isGitManaged();

  // The id is taken from the TOML's [playbook] id; the PUT URL must match it.
  const idMatch = source.match(/^\s*id\s*=\s*"([^"]+)"/m);
  const id = idMatch?.[1] ?? initialId;

  const save = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!id) {
      setError('The TOML must set a [playbook] id = "…".');
      return;
    }
    setBusy(true);
    setError("");
    try {
      await api.savePlaybook(id, source);
      onDone();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
      setBusy(false);
    }
  };

  return (
    <form className="card editing" onSubmit={save}>
      <div className="row">
        <h3>{isNew ? "New playbook" : `Editing ${initialId}`}</h3>
        <span className="id">{id}</span>
      </div>
      {error && <p className="err">{error}</p>}
      {readOnly && (
        <p className="banner-info">Managed in git — view only. Edit this playbook in the config repository.</p>
      )}
      <textarea
        className="code"
        spellCheck={false}
        rows={22}
        value={source}
        readOnly={readOnly}
        onChange={(e) => setSource(e.target.value)}
      />
      <div className="actions">
        {!readOnly && (
          <button className="btn-sm" type="submit" disabled={busy}>
            {busy ? "Saving…" : "Save"}
          </button>
        )}
        <button className="btn-sm ghost" type="button" onClick={onCancel} disabled={busy}>
          {readOnly ? "Close" : "Cancel"}
        </button>
      </div>
      <p className="muted note">
        Steps run in order; the agent advances the flow with its <code>playbook</code> tool. Attach
        a saved flow to an agent from the Agents screen (coming soon) or in edge.toml.
      </p>
    </form>
  );
}
