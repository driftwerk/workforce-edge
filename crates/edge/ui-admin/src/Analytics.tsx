import { useEffect, useMemo, useRef, useState } from "react";

import * as api from "./api";
import { CostBarChart, UsageAreaChart, type Metric } from "./Charts";
import { Column, DataTable } from "./DataTable";
import { DateRangePicker } from "./DateRangePicker";
import { StatCard } from "./StatCard";
import { bucketFor, defaultRange, type DateRange } from "./daterange";
import { formatDuration, formatTokens, money, percent } from "./format";
import { mergePages } from "./pagination";
import { useAsync } from "./useAsync";

const METRICS: { id: Metric; label: string }[] = [
  { id: "turns", label: "Turns" },
  { id: "tokens", label: "Tokens" },
  { id: "cost", label: "Cost" },
];

const DIMS: { id: api.Dimension; label: string }[] = [
  { id: "user", label: "Users" },
  { id: "agent", label: "Agents" },
  { id: "model", label: "Models" },
  { id: "outcome", label: "Outcomes" },
];

/** A stable status class for an outcome pill. */
function outcomeClass(outcome: string): string {
  if (outcome === "ok") return "pill-ok";
  if (outcome === "error" || outcome === "timeout") return "pill-bad";
  return "pill-neutral";
}

export function Analytics({
  onOpen,
}: {
  onOpen: (agent: string, owner: string, conversation: string) => void;
}) {
  const [range, setRange] = useState<DateRange>(defaultRange);
  const [agent, setAgent] = useState("");
  const [owner, setOwner] = useState<{ key: string; label: string } | null>(null);
  const [metric, setMetric] = useState<Metric>("cost");
  const [dim, setDim] = useState<api.Dimension>("user");
  const [agents, setAgents] = useState<api.Agent[]>([]);

  useEffect(() => {
    api.listAgents().then(setAgents).catch(() => setAgents([]));
  }, []);

  const filter = useMemo<api.AnalyticsFilter>(
    () => ({ from: range.from, to: range.to, agent: agent || undefined, owner: owner?.key }),
    [range, agent, owner],
  );
  // The serialized filter is the single query key that drives every refetch and
  // the turns-table reset (advisor: key off the qs string, not the object).
  const key = api.analyticsQs(filter);
  const bucket = bucketFor(range);

  const summary = useAsync(() => api.getAnalyticsSummary(filter), [key]);
  const series = useAsync(() => api.getAnalyticsTimeseries(filter, bucket), [key]);
  const byUser = useAsync(() => api.getAnalyticsByDimension("user", filter), [key]);
  const byAgent = useAsync(() => api.getAnalyticsByDimension("agent", filter), [key]);
  const byModel = useAsync(() => api.getAnalyticsByDimension("model", filter), [key]);
  const byOutcome = useAsync(() => api.getAnalyticsByDimension("outcome", filter), [key]);
  const byDim = { user: byUser, agent: byAgent, model: byModel, outcome: byOutcome };

  // The per-turn drill-down accumulates pages in its own state; it resets on any
  // filter change (keyed off `key`) so a new filter never appends onto stale rows.
  const [turns, setTurns] = useState<api.TurnRow[]>([]);
  const [nextCursor, setNextCursor] = useState<string | null>(null);
  const [turnsLoading, setTurnsLoading] = useState(true);
  const [turnsError, setTurnsError] = useState("");
  const loadSeq = useRef(0);

  const loadTurns = (cursor: string | null, reset: boolean) => {
    const seq = reset ? ++loadSeq.current : loadSeq.current;
    setTurnsLoading(true);
    setTurnsError("");
    api
      .getAnalyticsTurns(filter, { cursor, limit: 50 })
      .then((page) => {
        if (seq !== loadSeq.current) return; // superseded by a newer filter
        setTurns((prev) => (reset ? page.turns : mergePages(prev, page.turns, (t) => t.id)));
        setNextCursor(page.next_cursor);
      })
      .catch((e) => {
        if (seq === loadSeq.current) setTurnsError(e instanceof Error ? e.message : String(e));
      })
      .finally(() => {
        if (seq === loadSeq.current) setTurnsLoading(false);
      });
  };

  useEffect(() => {
    setTurns([]);
    setNextCursor(null);
    loadTurns(null, true);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [key]);

  const drill = (dimension: api.Dimension, row: api.DimensionRow) => {
    if (dimension === "user") setOwner({ key: row.key, label: row.label });
    else if (dimension === "agent") setAgent(row.key);
  };

  // Keep the select consistent when a drill-in sets an agent not in the list.
  const agentMissing = agent && !agents.some((a) => a.id === agent);

  const s = summary.data;
  const dimCols = (dimension: api.Dimension): Column<api.DimensionRow>[] => [
    {
      key: "label",
      header: dimension === "model" ? <span className="mono">Model</span> : DIMS.find((d) => d.id === dimension)?.label,
      cell: (r) =>
        dimension === "outcome" ? (
          <span className={`pill ${outcomeClass(r.key)}`}>{r.label}</span>
        ) : dimension === "model" ? (
          <span className="mono">{r.label}</span>
        ) : (
          r.label
        ),
    },
    { key: "turns", header: "Turns", align: "right", cell: (r) => r.turns.toLocaleString() },
    { key: "tokens", header: "Tokens", align: "right", cell: (r) => formatTokens(r.tokens) },
    { key: "cost", header: "Cost", align: "right", cell: (r) => money(r.cost_usd) },
  ];

  const turnCols: Column<api.TurnRow>[] = [
    { key: "ts", header: "When", cell: (t) => api.relativeTime(t.ts) },
    { key: "user", header: "User", cell: (t) => t.display_name || t.owner },
    { key: "agent", header: "Agent", cell: (t) => t.agent_id },
    { key: "model", header: "Model", cell: (t) => <span className="mono">{t.model}</span> },
    { key: "in", header: "In", align: "right", cell: (t) => formatTokens(t.input_tokens) },
    { key: "out", header: "Out", align: "right", cell: (t) => formatTokens(t.output_tokens) },
    { key: "cost", header: "Cost", align: "right", cell: (t) => money(t.cost_usd) },
    { key: "rounds", header: "Rounds", align: "right", cell: (t) => t.rounds },
    { key: "tools", header: "Tools", align: "right", cell: (t) => t.tool_count },
    { key: "fails", header: "Fails", align: "right", cell: (t) => t.tool_failures },
    { key: "dur", header: "Latency", align: "right", cell: (t) => formatDuration(t.duration_ms) },
    { key: "outcome", header: "Outcome", cell: (t) => <span className={`pill ${outcomeClass(t.outcome)}`}>{t.outcome}</span> },
    {
      key: "open",
      header: "",
      align: "right",
      cell: (t) => (
        <button
          className="btn-sm ghost"
          // Stop the row's onClick (which sets the user filter) from also firing.
          onClick={(e) => {
            e.stopPropagation();
            onOpen(t.agent_id, t.owner, t.conversation);
          }}
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
          <h2>Analytics</h2>
          <p className="sub">Usage, cost and outcomes across this install.</p>
        </div>
        <div className="analytics-filters">
          <DateRangePicker value={range} onChange={setRange} />
          <select className="select" value={agent} onChange={(e) => setAgent(e.target.value)}>
            <option value="">All agents</option>
            {agentMissing && <option value={agent}>{agent}</option>}
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

      {summary.error && <p className="err">{summary.error}</p>}

      <div className="kpi-grid">
        <StatCard label="Turns" value={(s?.turns ?? 0).toLocaleString()} isLoading={summary.loading} />
        <StatCard label="Total cost" value={money(s?.total_cost_usd ?? 0)} isLoading={summary.loading} />
        <StatCard label="Tokens in" value={formatTokens(s?.input_tokens ?? 0)} isLoading={summary.loading} />
        <StatCard label="Tokens out" value={formatTokens(s?.output_tokens ?? 0)} isLoading={summary.loading} />
        <StatCard label="Unique users" value={(s?.unique_users ?? 0).toLocaleString()} isLoading={summary.loading} />
        <StatCard label="Failure rate" value={percent(s?.failure_rate ?? 0)} isLoading={summary.loading} />
        <StatCard label="Avg latency" value={s ? formatDuration(s.avg_duration_ms) : "—"} isLoading={summary.loading} />
      </div>

      <div className="card chart-card">
        <div className="chart-head">
          <div className="chart-title">Usage over time</div>
          <div className="seg">
            {METRICS.map((m) => (
              <button key={m.id} className={`seg-btn${metric === m.id ? " active" : ""}`} onClick={() => setMetric(m.id)}>
                {m.label}
              </button>
            ))}
          </div>
        </div>
        {series.error ? (
          <p className="err">{series.error}</p>
        ) : series.loading && !series.data ? (
          <p className="spinner">Loading…</p>
        ) : (
          <UsageAreaChart data={series.data ?? []} metric={metric} bucket={bucket} gradientId="usage" />
        )}
      </div>

      <div className="chart-grid">
        <CostBarChart title="Cost by user" rows={byUser.data ?? []} loading={byUser.loading} onSelect={(r) => drill("user", r)} />
        <CostBarChart title="Cost by agent" rows={byAgent.data ?? []} loading={byAgent.loading} onSelect={(r) => drill("agent", r)} />
        <CostBarChart title="Cost by model" rows={byModel.data ?? []} loading={byModel.loading} />
      </div>

      <div className="dim-section">
        <div className="tabs">
          {DIMS.map((d) => (
            <button key={d.id} className={`tab${dim === d.id ? " active" : ""}`} onClick={() => setDim(d.id)}>
              {d.label}
            </button>
          ))}
        </div>
        {byDim[dim].error ? (
          <p className="err">{byDim[dim].error}</p>
        ) : (
          <DataTable<api.DimensionRow>
            columns={dimCols(dim)}
            rows={byDim[dim].data ?? []}
            rowKey={(r) => r.key}
            isLoading={byDim[dim].loading}
            onRowClick={dim === "user" || dim === "agent" ? (r) => drill(dim, r) : undefined}
            empty={`No ${dim} activity in range.`}
          />
        )}
      </div>

      <div className="turns-section">
        <div className="chart-title">Per-turn detail</div>
        {turnsError && <p className="err">{turnsError}</p>}
        <DataTable<api.TurnRow>
          columns={turnCols}
          rows={turns}
          rowKey={(t) => t.id}
          isLoading={turnsLoading}
          minWidth={1040}
          onRowClick={(t) => setOwner({ key: t.owner, label: t.display_name || t.owner })}
          empty="No turns in range."
          footer={
            nextCursor && (
              <button className="btn-sm ghost" disabled={turnsLoading} onClick={() => loadTurns(nextCursor, false)}>
                {turnsLoading ? "Loading…" : "Load more"}
              </button>
            )
          }
        />
      </div>
    </>
  );
}
