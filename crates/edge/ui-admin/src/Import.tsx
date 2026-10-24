import { useState } from "react";

import * as api from "./api";
import {
  describeImport,
  emptyImportForm,
  importRequest,
  withUnresolved,
  type ImportForm,
} from "./agentForm";

/** One-shot import of a config repo: its agents, playbooks and training land
 *  in this install, which stays editable afterwards. The token is used for
 *  the fetch and dropped; placeholders in the repo's agent config bind from
 *  the rows here and nothing else. */
export function Import() {
  const [form, setForm] = useState<ImportForm>(emptyImportForm);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [unresolved, setUnresolved] = useState<string[]>([]);
  const [done, setDone] = useState("");
  const gitManaged = api.isGitManaged();

  const set = <K extends keyof ImportForm>(k: K, v: ImportForm[K]) => setForm((f) => ({ ...f, [k]: v }));
  const setRow = (i: number, patch: Partial<{ name: string; value: string }>) =>
    setForm((f) => ({ ...f, env: f.env.map((r, j) => (j === i ? { ...r, ...patch } : r)) }));

  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    setError("");
    setUnresolved([]);
    setDone("");
    if (
      !window.confirm(
        "Agents, playbooks and training docs in the repo replace any here with the same id. Continue?",
      )
    )
      return;
    setBusy(true);
    try {
      const report = await api.importGit(importRequest(form));
      setDone(describeImport(report));
      setForm((f) => ({ ...f, token: "" }));
    } catch (err) {
      if (err instanceof api.UnresolvedPlaceholders) {
        setUnresolved(err.names);
        setForm((f) => ({ ...f, env: withUnresolved(f.env, err.names) }));
      }
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <>
      <div className="head-row">
        <div>
          <h2>Import from git</h2>
          <p className="sub">
            Read a config repository once and bring its agents, playbooks and training data into
            this install. Everything stays editable here afterwards.
          </p>
        </div>
      </div>
      {gitManaged && (
        <p className="banner-info">
          This install is managed in git: its repository already owns these items and syncs on its
          own, so there is nothing to import.
        </p>
      )}
      <form className="card editing" onSubmit={submit}>
        {error && <p className="err">{error}</p>}
        {done && <p className="banner-info">{done}</p>}
        <fieldset disabled={gitManaged || busy} style={{ border: 0, padding: 0, margin: 0, minInlineSize: "auto" }}>
          <div className="form-grid">
            <label>
              Repository<span className="hint">GitHub owner/name</span>
            </label>
            <input value={form.repo} onChange={(e) => set("repo", e.target.value)} placeholder="acme/workforce-edge-config" />
            <label>
              Ref<span className="hint">branch, tag or commit; blank = main</span>
            </label>
            <input value={form.git_ref} onChange={(e) => set("git_ref", e.target.value)} placeholder="main" />
            <label>
              Token<span className="hint">read access to the repo; used once, never stored</span>
            </label>
            <input
              type="password"
              autoComplete="off"
              value={form.token}
              onChange={(e) => set("token", e.target.value)}
              placeholder="github_pat_…"
            />
            <label>
              Placeholders
              <span className="hint">values for the ${"{NAME}"} bindings in the repo's agent config</span>
            </label>
            <div>
              {form.env.map((row, i) => (
                <div className="env-row" key={i}>
                  <input
                    className="mono-area"
                    value={row.name}
                    onChange={(e) => setRow(i, { name: e.target.value })}
                    placeholder="EDGE_KC_ISSUER"
                  />
                  <input
                    value={row.value}
                    onChange={(e) => setRow(i, { value: e.target.value })}
                    placeholder="https://auth.example.com/realms/staff"
                  />
                  <button
                    className="btn-sm ghost"
                    type="button"
                    onClick={() => set("env", form.env.filter((_, j) => j !== i))}
                  >
                    Remove
                  </button>
                </div>
              ))}
              <button
                className="btn-sm ghost"
                type="button"
                onClick={() => set("env", [...form.env, { name: "", value: "" }])}
              >
                + Add placeholder
              </button>
              {unresolved.length > 0 && (
                <p className="hint" style={{ margin: "8px 2px 0" }}>
                  The repo needs values for: {unresolved.join(", ")}. Rows were added above — fill them
                  in and import again.
                </p>
              )}
            </div>
          </div>
          <div className="actions">
            <button className="btn-sm" type="submit" disabled={busy || !form.repo.trim() || !form.token.trim()}>
              {busy ? "Importing…" : "Import"}
            </button>
          </div>
        </fieldset>
      </form>
    </>
  );
}
