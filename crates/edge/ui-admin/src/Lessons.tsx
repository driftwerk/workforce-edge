import { useEffect, useState } from "react";

import * as api from "./api";

export function Lessons() {
  const [agents, setAgents] = useState<api.Agent[] | null>(null);
  const [agent, setAgent] = useState<string>("");
  const [lessons, setLessons] = useState<api.Lesson[] | null>(null);
  const [error, setError] = useState("");

  useEffect(() => {
    api
      .listAgents()
      .then((a) => {
        setAgents(a);
        if (a[0]) setAgent(a[0].id);
      })
      .catch((e) => setError(String(e)));
  }, []);

  const load = (a: string) =>
    api
      .listLessons(a)
      .then(setLessons)
      .catch((e) => setError(String(e)));

  useEffect(() => {
    if (!agent) return;
    setLessons(null);
    void load(agent);
  }, [agent]);

  return (
    <>
      <div className="head-row">
        <div>
          <h2>Lessons</h2>
          <p className="sub">
            What this agent has learned from conversations. Prune anything unhelpful.
          </p>
        </div>
        {agents && agents.length > 1 && (
          <select className="select" value={agent} onChange={(e) => setAgent(e.target.value)}>
            {agents.map((a) => (
              <option key={a.id} value={a.id}>
                {a.display_name}
              </option>
            ))}
          </select>
        )}
      </div>
      {error && <p className="err">{error}</p>}
      {!lessons && !error && <p className="muted">Loading…</p>}
      {lessons?.length === 0 && (
        <p className="muted">
          No lessons yet. The agent learns when a customer corrects it on something concrete.
        </p>
      )}
      {lessons?.map((l) => (
        <div className="card lesson" key={l.id}>
          <p className="lesson-body">{l.content}</p>
          <div className="lesson-foot">
            <span>
              {l.triggers.map((t) => (
                <span className="pill" key={t}>
                  {t}
                </span>
              ))}
              <span className="muted">importance {l.importance.toFixed(2)}</span>
            </span>
            <button
              className="btn-sm ghost danger"
              onClick={async () => {
                if (!window.confirm("Delete this lesson? The agent will forget it.")) return;
                try {
                  await api.deleteLesson(agent, l.id);
                  void load(agent);
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
