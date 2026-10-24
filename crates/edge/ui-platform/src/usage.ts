// Usage against plan, as the detail page's tiles show it. Pure: no React.

import type { Plan, Usage } from "./api";

export type Metered = keyof Usage;

/** The plan field each usage figure is capped by. */
const CAP_OF: Record<Metered, keyof Plan> = {
  turns_this_month: "turns_per_month",
  active_users_this_month: "active_users",
  knowledge_chunks: "knowledge_chunks",
  memory_objects: "memory_objects",
  agents: "agents",
};

const LABEL: Record<Metered, string> = {
  turns_this_month: "Turns this month",
  active_users_this_month: "Active users this month",
  knowledge_chunks: "Knowledge chunks",
  memory_objects: "Memory objects",
  agents: "Agents",
};

/** The metered figures, in the order the tiles show them. */
export const METERED: readonly Metered[] = [
  "turns_this_month",
  "active_users_this_month",
  "knowledge_chunks",
  "memory_objects",
  "agents",
];

export interface UsageRow {
  field: Metered;
  label: string;
  /** The figure, or null when the server could not (or did not) count it. */
  used: number | null;
  /** The cap, or null when the plan leaves it unbounded. */
  cap: number | null;
  /** Whether the cap is reached. */
  over: boolean;
  /** Used over cap, 0..1, or null without both. */
  ratio: number | null;
}

/** One row per metered figure, against its cap. */
export function usageRows(plan: Plan, usage: Usage | null): UsageRow[] {
  return METERED.map((field) => {
    const cap = plan[CAP_OF[field]] ?? null;
    const used = usage ? usage[field] : null;
    const over = used !== null && cap !== null && used >= cap;
    const ratio = used !== null && cap !== null && cap > 0 ? Math.min(1, used / cap) : null;
    return { field, label: LABEL[field], used, cap, over, ratio };
  });
}

/** "1,234 / 5,000", "1,234" when unbounded, or "—" when uncounted. */
export function usageText(row: UsageRow): string {
  if (row.used === null) return "—";
  const used = row.used.toLocaleString();
  return row.cap === null ? used : `${used} / ${row.cap.toLocaleString()}`;
}

/** The small line under a figure: at the cap, how far along, unbounded,
 *  or why there is no figure. */
export function usageHint(row: UsageRow, serving: boolean): string {
  if (row.used === null) {
    if (!serving) return "not served";
    return row.field === "memory_objects" && row.cap === null ? "counted only under a cap" : "could not be counted";
  }
  if (row.over) return "at the plan limit";
  if (row.ratio !== null) return `${Math.round(row.ratio * 100)}% of the cap`;
  return "no limit";
}
