import type { Plan, PlanUsage, Usage } from "./api";

/** The plan fields the server meters, in the order the card shows them. */
export const METERED: Array<keyof Usage> = [
  "turns_this_month",
  "active_users_this_month",
  "knowledge_chunks",
  "memory_objects",
  "agents",
];

/** The plan field each usage figure is capped by. */
const CAP_OF: Record<keyof Usage, keyof Plan> = {
  turns_this_month: "turns_per_month",
  active_users_this_month: "active_users",
  knowledge_chunks: "knowledge_chunks",
  memory_objects: "memory_objects",
  agents: "agents",
};

/** What a plan field is called on screen. */
export function limitLabel(field: string): string {
  switch (field) {
    case "turns_per_month":
      return "monthly turn";
    case "active_users":
      return "monthly active user";
    case "knowledge_chunks":
      return "knowledge";
    case "memory_objects":
      return "memory";
    case "agents":
      return "agent";
    case "upload_mb":
      return "upload";
    case "max_concurrent_turns":
      return "concurrent turn";
    default:
      return field.replace(/_/g, " ");
  }
}

/** The shell banner's sentence for the caps that are reached, or null when
 *  none is. Lists every reached cap, since each refuses something different. */
export function planBanner(over: string[]): string | null {
  if (over.length === 0) return null;
  const names = over.map(limitLabel);
  const list =
    names.length === 1
      ? names[0]
      : `${names.slice(0, -1).join(", ")} and ${names[names.length - 1]}`;
  return `This workspace has reached its ${list} limit${names.length > 1 ? "s" : ""}.`;
}

export interface UsageRow {
  field: keyof Usage;
  label: string;
  /** The figure, or null when the server could not count it. */
  used: number | null;
  /** The cap, or null when the plan leaves it unbounded. */
  cap: number | null;
  /** Whether the cap is reached, as the server reported it. */
  over: boolean;
}

const ROW_LABEL: Record<keyof Usage, string> = {
  turns_this_month: "Turns this month",
  active_users_this_month: "Active users this month",
  knowledge_chunks: "Knowledge chunks",
  memory_objects: "Memories",
  agents: "Agents",
};

/** One row per metered figure, against its cap. */
export function usageRows({ plan, usage, over }: PlanUsage): UsageRow[] {
  return METERED.map((field) => {
    const capField = CAP_OF[field];
    return {
      field,
      label: ROW_LABEL[field],
      used: usage ? usage[field] : null,
      cap: plan[capField] ?? null,
      over: over.includes(capField),
    };
  });
}

/** "1,234 / 5,000", "1,234 (no limit)", or "—" when uncounted. */
export function usageText(row: UsageRow): string {
  if (row.used === null) return "—";
  const used = row.used.toLocaleString();
  return row.cap === null ? used : `${used} / ${row.cap.toLocaleString()}`;
}

/** The small line under a figure: at the cap, unbounded, or uncounted. */
export function usageHint(row: UsageRow): string {
  if (row.used === null) return "could not be counted";
  if (row.over) return "at the plan limit";
  return row.cap === null ? "no limit" : "";
}
