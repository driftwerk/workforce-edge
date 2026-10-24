import { useEffect, useState } from "react";

import * as api from "./api";

export function Operators() {
  const [ops, setOps] = useState<api.Operator[] | null>(null);
  const [kind, setKind] = useState<"email" | "domain">("email");
  const [value, setValue] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");

  const load = () =>
    api
      .listOperators()
      .then(setOps)
      .catch((e) => setError(String(e)));

  useEffect(() => {
    void load();
  }, []);

  const add = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!value.trim()) return;
    setBusy(true);
    setError("");
    try {
      await api.addOperator(kind, value.trim());
      setValue("");
      void load();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <>
      <h2>Operators</h2>
      <p className="sub">
        Who can sign into this admin. Add a specific email, or a whole domain (e.g.{" "}
        <code>harbor.example</code>) to allow anyone with a verified address there.
      </p>
      {error && <p className="err">{error}</p>}
      <form className="op-add" onSubmit={add}>
        <select className="select" value={kind} onChange={(e) => setKind(e.target.value as "email" | "domain")}>
          <option value="email">Email</option>
          <option value="domain">Domain</option>
        </select>
        <input
          value={value}
          placeholder={kind === "email" ? "person@company.com" : "company.com"}
          onChange={(e) => setValue(e.target.value)}
        />
        <button className="btn-sm" type="submit" disabled={busy || !value.trim()}>
          Add
        </button>
      </form>
      {!ops && !error && <p className="muted">Loading…</p>}
      {ops?.length === 0 && (
        <p className="muted">
          No operators — with an empty allowlist, only the break-glass password can sign in.
        </p>
      )}
      {ops?.map((o) => (
        <div className="op-row" key={`${o.kind}:${o.value}`}>
          <span>
            <span className={`pill ${o.kind === "domain" ? "pill-domain" : ""}`}>{o.kind}</span>
            <strong>{o.value}</strong>
            <span className="muted"> · added by {o.added_by}</span>
          </span>
          <button
            className="btn-sm ghost danger"
            onClick={async () => {
              if (!window.confirm(`Remove ${o.kind} "${o.value}" from the allowlist?`)) return;
              try {
                await api.removeOperator(o.kind, o.value);
                void load();
              } catch (e) {
                setError(e instanceof Error ? e.message : String(e));
              }
            }}
          >
            Remove
          </button>
        </div>
      ))}
    </>
  );
}
