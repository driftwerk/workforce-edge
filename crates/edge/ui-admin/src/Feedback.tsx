import { useEffect, useMemo, useRef, useState } from "react";

import * as api from "./api";
import { Column, DataTable } from "./DataTable";
import { DateRangePicker } from "./DateRangePicker";
import { defaultRange, type DateRange } from "./daterange";
import { formatRating } from "./format";
import { mergePages } from "./pagination";

/** Customer thumbs ratings over a date range, filterable by agent and (via a
 *  clickable user cell) by owner. Mirrors the Analytics per-turn drill-down:
 *  cursor-paginated rows accumulated with `mergePages`, reset on any filter
 *  change. "Open" jumps straight to the exact transcript the rating is on
 *  (falling back to the customer's conversation list if it's no longer listed). */
export function Feedback({
  onOpen,
}: {
  onOpen: (agent: string, owner: string, conversation: string) => void;
}) {
  const [range, setRange] = useState<DateRange>(defaultRange);
  const [agent, setAgent] = useState("");
  const [owner, setOwner] = useState<{ key: string; label: string } | null>(null);
  const [agents, setAgents] = useState<api.Agent[]>([]);

  useEffect(() => {
    api.listAgents().then(setAgents).catch(() => setAgents([]));
  }, []);

  const filter = useMemo<api.AnalyticsFilter>(
    () => ({ from: range.from, to: range.to, agent: agent || undefined, owner: owner?.key }),
    [range, agent, owner],
  );
  // The serialized filter is the single query key that drives every reset.
  const key = api.analyticsQs(filter);

  const [rows, setRows] = useState<api.FeedbackRow[]>([]);
  const [nextCursor, setNextCursor] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState("");
  const loadSeq = useRef(0);

  const load = (cursor: string | null, reset: boolean) => {
    const seq = reset ? ++loadSeq.current : loadSeq.current;
    setLoading(true);
    setError("");
    api
      .getAnalyticsFeedback(filter, { cursor, limit: 50 })
      .then((page) => {
        if (seq !== loadSeq.current) return; // superseded by a newer filter
        setRows((prev) => (reset ? page.feedback : mergePages(prev, page.feedback, (r) => r.id)));
        setNextCursor(page.next_cursor);
      })
      .catch((e) => {
        if (seq === loadSeq.current) setError(e instanceof Error ? e.message : String(e));
      })
      .finally(() => {
        if (seq === loadSeq.current) setLoading(false);
      });
  };

  useEffect(() => {
    setRows([]);
    setNextCursor(null);
    load(null, true);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [key]);

  const agentName = (id: string) => agents.find((a) => a.id === id)?.display_name ?? id;

  const cols: Column<api.FeedbackRow>[] = [
    { key: "ts", header: "When", cell: (r) => api.relativeTime(r.ts) },
    {
      key: "user",
      header: "User",
      cell: (r) => (
        <button
          className="fb-link"
          title="Filter to this user"
          onClick={() => setOwner({ key: r.owner, label: r.display_name || r.owner })}
        >
          {r.display_name || r.owner}
        </button>
      ),
    },
    {
      key: "rating",
      header: "Rating",
      cell: (r) => {
        const f = formatRating(r.rating);
        return (
          <span className={`pill ${f.cls}`} title={f.label}>
            {f.symbol} {f.label}
          </span>
        );
      },
    },
    {
      key: "comment",
      header: "Comment",
      cell: (r) => (r.comment ? <span className="fb-comment">{r.comment}</span> : <span className="muted">—</span>),
    },
    { key: "agent", header: "Agent", cell: (r) => agentName(r.agent_id) },
    {
      key: "open",
      header: "",
      align: "right",
      cell: (r) => (
        <button className="btn-sm ghost" onClick={() => onOpen(r.agent_id, r.owner, r.conversation)} title="Open this conversation">
          Open
        </button>
      ),
    },
  ];

  return (
    <>
      <div className="head-row analytics-head">
        <div>
          <h2>Feedback</h2>
          <p className="sub">Customer thumbs ratings on assistant answers.</p>
        </div>
        <div className="analytics-filters">
          <DateRangePicker value={range} onChange={setRange} />
          <select className="select" value={agent} onChange={(e) => setAgent(e.target.value)}>
            <option value="">All agents</option>
            {agents.map((a) => (
              <option key={a.id} value={a.id}>
                {a.display_name}
              </option>
            ))}
          </select>
        </div>
      </div>

      {owner && (
        <div className="filter-chips">
          <button className="chip" onClick={() => setOwner(null)} title="Clear user filter">
            User: <strong>{owner.label}</strong>
            <span className="chip-x" aria-hidden>
              ✕
            </span>
          </button>
        </div>
      )}

      {error && <p className="err">{error}</p>}

      <DataTable<api.FeedbackRow>
        columns={cols}
        rows={rows}
        rowKey={(r) => r.id}
        isLoading={loading}
        minWidth={860}
        empty="No feedback in range."
        footer={
          nextCursor && (
            <button className="btn-sm ghost" disabled={loading} onClick={() => load(nextCursor, false)}>
              {loading ? "Loading…" : "Load more"}
            </button>
          )
        }
      />
    </>
  );
}
