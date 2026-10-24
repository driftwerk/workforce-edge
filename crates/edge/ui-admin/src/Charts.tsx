import {
  Area,
  AreaChart,
  Bar,
  BarChart,
  CartesianGrid,
  Cell,
  ResponsiveContainer,
  Tooltip,
  XAxis,
  YAxis,
} from "recharts";

import type { DimensionRow, TimeseriesBucket } from "./api";
import { formatTokens, money } from "./format";

export type Metric = "turns" | "tokens" | "cost";

const ACCENT = "#4f46e5";
const INPUT = "#4f46e5";
const OUTPUT = "#14b8a6";

const SERIES: Record<Metric, { dataKey: keyof TimeseriesBucket; name: string; color: string }[]> = {
  turns: [{ dataKey: "turns", name: "Turns", color: ACCENT }],
  tokens: [
    { dataKey: "input_tokens", name: "Input", color: INPUT },
    { dataKey: "output_tokens", name: "Output", color: OUTPUT },
  ],
  cost: [{ dataKey: "cost_usd", name: "Cost", color: ACCENT }],
};

const TOOLTIP_STYLE: React.CSSProperties = {
  background: "var(--panel)",
  border: "1px solid var(--border)",
  borderRadius: "var(--r-sm)",
  fontSize: "12px",
  boxShadow: "var(--shadow)",
};

function axisLabel(ts: string, bucket: "day" | "hour"): string {
  const d = new Date(ts);
  return bucket === "hour"
    ? d.toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit", timeZone: "UTC" })
    : d.toLocaleDateString(undefined, { month: "short", day: "numeric", timeZone: "UTC" });
}

/** Area chart of turns, tokens (input+output), or cost over the bucketed range.
 *  Its dimension tables are the text alternative; the SVG carries an aria-label.
 *  `gradientId` namespaces the fill gradients so multiple charts coexist. */
export function UsageAreaChart({
  data,
  metric,
  bucket,
  gradientId,
  height = 260,
}: {
  data: TimeseriesBucket[];
  metric: Metric;
  bucket: "day" | "hour";
  gradientId: string;
  height?: number;
}) {
  if (data.length === 0) {
    return <p className="muted chart-empty">No usage in range.</p>;
  }
  const fmt = metric === "cost" ? money : formatTokens;
  const series = SERIES[metric];
  const points = data.map((b) => ({ ...b, label: axisLabel(b.ts, bucket) }));
  return (
    <div role="img" aria-label={`${metric} over time`} className="chart-box">
      <ResponsiveContainer width="100%" height={height}>
        <AreaChart data={points} margin={{ top: 6, right: 8, left: 4, bottom: 0 }}>
          <defs>
            {series.map((s) => (
              <linearGradient key={s.name} id={`${gradientId}-${s.name}`} x1="0" y1="0" x2="0" y2="1">
                <stop offset="5%" stopColor={s.color} stopOpacity={0.28} />
                <stop offset="95%" stopColor={s.color} stopOpacity={0} />
              </linearGradient>
            ))}
          </defs>
          <CartesianGrid strokeDasharray="3 3" stroke="var(--border)" />
          <XAxis dataKey="label" tick={{ fontSize: 11, fill: "var(--muted)" }} stroke="var(--border-2)" />
          <YAxis tickFormatter={(v) => fmt(Number(v))} tick={{ fontSize: 11, fill: "var(--muted)" }} stroke="var(--border-2)" width={48} />
          <Tooltip formatter={(v: unknown, name: unknown) => [fmt(Number(v ?? 0)), String(name)]} contentStyle={TOOLTIP_STYLE} />
          {series.map((s) => (
            <Area
              key={s.name}
              type="monotone"
              dataKey={s.dataKey}
              name={s.name}
              stroke={s.color}
              strokeWidth={2}
              fill={`url(#${gradientId}-${s.name})`}
            />
          ))}
        </AreaChart>
      </ResponsiveContainer>
    </div>
  );
}

/** Horizontal cost bars for one dimension breakdown. `onSelect` (when given)
 *  makes the bars a clickable drill-down; the paired table is the text mirror. */
export function CostBarChart({
  title,
  rows,
  onSelect,
  loading = false,
  height = 240,
}: {
  title: string;
  rows: DimensionRow[];
  onSelect?: (row: DimensionRow) => void;
  loading?: boolean;
  height?: number;
}) {
  const top = rows.slice(0, 10);
  return (
    <div className="card chart-card">
      <div className="chart-title">{title}</div>
      {loading && top.length === 0 ? (
        <p className="spinner chart-empty">Loading…</p>
      ) : top.length === 0 ? (
        <p className="muted chart-empty">No data in range.</p>
      ) : (
        <div role="img" aria-label={title} className="chart-box">
          <ResponsiveContainer width="100%" height={height}>
            <BarChart data={top} layout="vertical" margin={{ left: 8, right: 12, top: 2, bottom: 2 }}>
              <CartesianGrid horizontal={false} strokeDasharray="3 3" stroke="var(--border)" />
              <XAxis type="number" tickFormatter={(v) => money(Number(v))} tick={{ fontSize: 11, fill: "var(--muted)" }} stroke="var(--border-2)" />
              <YAxis
                type="category"
                dataKey="label"
                width={110}
                tick={{ fontSize: 11, fill: "var(--ink-2)" }}
                stroke="var(--border-2)"
              />
              <Tooltip cursor={{ fill: "var(--hover)" }} formatter={(v: unknown) => money(Number(v ?? 0))} contentStyle={TOOLTIP_STYLE} />
              <Bar
                dataKey="cost_usd"
                name="Cost"
                radius={[0, 4, 4, 0]}
                onClick={
                  onSelect
                    ? (d) => onSelect(((d?.payload as DimensionRow | undefined) ?? (d as unknown as DimensionRow)))
                    : undefined
                }
                cursor={onSelect ? "pointer" : undefined}
              >
                {top.map((r) => (
                  <Cell key={r.key} fill={ACCENT} />
                ))}
              </Bar>
            </BarChart>
          </ResponsiveContainer>
        </div>
      )}
    </div>
  );
}
