import { useEffect, useMemo, useRef, useState } from "react";

import * as api from "./api";
import { Column, DataTable } from "./DataTable";
import { DateRangePicker } from "./DateRangePicker";
import { StatCard } from "./StatCard";
import { defaultRange, type DateRange } from "./daterange";
import { percent } from "./format";
import { buildGapReport } from "./gapReport";
import { mergePages } from "./pagination";
import { useAsync } from "./useAsync";

/** A stable status class for a quality label. */
function qualityClass(q: string): string {
  if (q === "good") return "pill-ok";
  if (q === "unanswerable" || q === "incorrect") return "pill-bad";
  return "pill-neutral";
}

/** The gaps view: what the assistant answers badly or can't, mined from the
 *  automated grader. `gap_topic` / `reason` / `remedy` / `example_reason` are
 *  UNTRUSTED grader output (LLM-derived from customer + agent text) — rendered
 *  as plain text only (JSX escapes by default; never `dangerouslySetInnerHTML`). */
export function Gaps({
  onOpen,
}: {
  onOpen: (agent: string, owner: string, conversation: string) => void;
}) {
  const [range, setRange] = useState<DateRange>(defaultRange);
  const [agent, setAgent] = useState("");
  const [agents, setAgents] = useState<api.Agent[]>([]);

  useEffect(() => {
    api
      .listAgents()
      .then(setAgents)
      .catch(() => setAgents([]));
  }, []);

  const filter = useMemo<api.AnalyticsFilter>(
    () => ({ from: range.from, to: range.to, agent: agent || undefined }),
    [range, agent],
  );
  // The serialized filter is the single query key that drives every refetch.
  const key = api.analyticsQs(filter);

  const summary = useAsync(() => api.getGradesSummary(filter), [key]);
  const gaps = useAsync(() => api.getGaps(filter), [key]);

  // Per-grade drill-down (cursor-paginated, accumulated with mergePages).
  const [rows, setRows] = useState<api.GradeRow[]>([]);
  const [nextCursor, setNextCursor] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState("");
  const loadSeq = useRef(0);

  const load = (cursor: string | null, reset: boolean) => {
    const seq = reset ? ++loadSeq.current : loadSeq.current;
    setLoading(true);
    setError("");
    api
      .getGrades(filter, { cursor, limit: 50 })
      .then((page) => {
        if (seq !== loadSeq.current) return; // superseded by a newer filter
        setRows((prev) =>
          reset ? page.grades : mergePages(prev, page.grades, (r) => r.id),
        );
        setNextCursor(page.next_cursor);
      })
      .catch((e) => {
        if (seq === loadSeq.current)
          setError(e instanceof Error ? e.message : String(e));
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

  const agentName = (id: string) =>
    agents.find((a) => a.id === id)?.display_name ?? id;
  const s = summary.data;
  const badRate =
    s && s.graded > 0 ? (s.poor + s.incorrect + s.unanswerable) / s.graded : 0;

  // Download the current gaps as a Claude-Code brief the operator points CC at
  // (in the config repo) to draft training-doc PRs. A blob URL is CSP-safe.
  const exportReport = () => {
    const md = buildGapReport(gaps.data ?? [], summary.data, range, {
      name: agentName(agent),
      id: agent,
    });
    const url = URL.createObjectURL(new Blob([md], { type: "text/markdown" }));
    const a = document.createElement("a");
    a.href = url;
    a.download = `${agent || "workforce"}-gaps-${range.from.slice(0, 10)}.md`;
    a.click();
    URL.revokeObjectURL(url);
  };

  const gapCols: Column<api.GapRow>[] = [
    { key: "topic", header: "Gap topic", cell: (g) => g.topic },
    {
      key: "count",
      header: "Count",
      align: "right",
      cell: (g) => g.count.toLocaleString(),
    },
    {
      key: "split",
      header: "Unans. / Incorr.",
      align: "right",
      cell: (g) => `${g.unanswerable} / ${g.incorrect}`,
    },
    {
      key: "remedy",
      header: "Fix",
      cell: (g) =>
        g.remedy ? (
          <span className="pill pill-neutral">{g.remedy}</span>
        ) : (
          <span className="muted">—</span>
        ),
    },
    {
      key: "example",
      header: "Example",
      cell: (g) =>
        g.example_reason ? (
          <span className="fb-comment">{g.example_reason}</span>
        ) : (
          <span className="muted">—</span>
        ),
    },
    {
      key: "last",
      header: "Last seen",
      cell: (g) => api.relativeTime(g.last_seen),
    },
  ];

  const gradeCols: Column<api.GradeRow>[] = [
    { key: "ts", header: "When", cell: (r) => api.relativeTime(r.ts) },
    { key: "user", header: "User", cell: (r) => r.display_name || r.owner },
    {
      key: "quality",
      header: "Quality",
      cell: (r) => (
        <span className={`pill ${qualityClass(r.quality)}`}>{r.quality}</span>
      ),
    },
    {
      key: "topic",
      header: "Topic",
      cell: (r) => r.gap_topic ?? <span className="muted">—</span>,
    },
    {
      key: "reason",
      header: "Reason",
      cell: (r) =>
        r.reason ? (
          <span className="fb-comment">{r.reason}</span>
        ) : (
          <span className="muted">—</span>
        ),
    },
    { key: "agent", header: "Agent", cell: (r) => agentName(r.agent_id) },
    {
      key: "open",
      header: "",
      align: "right",
      cell: (r) => (
        <button
          className="btn-sm ghost"
          onClick={() => onOpen(r.agent_id, r.owner, r.conversation)}
          title="Open this conversation"
        >
          Open
        </button>
      ),
    },
  ];

  return (
    <>
      <div className="head-row analytics-head">
        <div>
          <h2>Gaps</h2>
          <p className="sub">
            What the assistant answers badly or can't — from automated grading.
          </p>
        </div>
        <div className="analytics-filters">
          <DateRangePicker value={range} onChange={setRange} />
          <select
            className="select"
            value={agent}
            onChange={(e) => setAgent(e.target.value)}
          >
            <option value="">All agents</option>
            {agents.map((a) => (
              <option key={a.id} value={a.id}>
                {a.display_name}
              </option>
            ))}
          </select>
          <button
            className="btn-sm"
            onClick={exportReport}
            disabled={!gaps.data || gaps.data.length === 0}
            title="Download a Claude Code brief to draft training-doc PRs from these gaps"
          >
            Export for Claude Code
          </button>
        </div>
      </div>

      {summary.error && <p className="err">{summary.error}</p>}

      <div className="kpi-grid">
        <StatCard
          label="Graded"
          value={(s?.graded ?? 0).toLocaleString()}
          isLoading={summary.loading}
        />
        <StatCard
          label="Good"
          value={(s?.good ?? 0).toLocaleString()}
          isLoading={summary.loading}
        />
        <StatCard
          label="Poor"
          value={(s?.poor ?? 0).toLocaleString()}
          isLoading={summary.loading}
        />
        <StatCard
          label="Incorrect"
          value={(s?.incorrect ?? 0).toLocaleString()}
          isLoading={summary.loading}
        />
        <StatCard
          label="Unanswerable"
          value={(s?.unanswerable ?? 0).toLocaleString()}
          isLoading={summary.loading}
        />
        <StatCard
          label="Bad-answer rate"
          value={percent(badRate)}
          isLoading={summary.loading}
        />
      </div>

      <div className="card">
        <div className="chart-head">
          <div className="chart-title">Top gaps</div>
        </div>
        {gaps.error ? (
          <p className="err">{gaps.error}</p>
        ) : (
          <DataTable<api.GapRow>
            columns={gapCols}
            rows={gaps.data ?? []}
            rowKey={(g) => g.topic}
            isLoading={gaps.loading && !gaps.data}
            minWidth={860}
            empty="No gaps in range (grading may be off, or every answer was good)."
          />
        )}
      </div>

      <div className="card">
        <div className="chart-head">
          <div className="chart-title">Graded turns</div>
        </div>
        {error && <p className="err">{error}</p>}
        <DataTable<api.GradeRow>
          columns={gradeCols}
          rows={rows}
          rowKey={(r) => r.id}
          isLoading={loading}
          minWidth={920}
          empty="No graded turns in range."
          footer={
            nextCursor && (
              <button
                className="btn-sm ghost"
                disabled={loading}
                onClick={() => load(nextCursor, false)}
              >
                {loading ? "Loading…" : "Load more"}
              </button>
            )
          }
        />
      </div>
    </>
  );
}
