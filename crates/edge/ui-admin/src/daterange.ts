// Date-range presets and the pure math behind the picker. No React — imported
// directly by unit tests as well as by DateRangePicker.tsx.

export interface DateRange {
  from: string; // ISO-8601
  to: string; // ISO-8601
}

export const PRESETS: { label: string; days: number }[] = [
  { label: "24h", days: 1 },
  { label: "7d", days: 7 },
  { label: "30d", days: 30 },
  { label: "90d", days: 90 },
];

const DAY_MS = 24 * 3600 * 1000;

/** A range ending now, spanning `days` back. */
export function rangeForDays(days: number): DateRange {
  const to = new Date();
  const from = new Date(to.getTime() - days * DAY_MS);
  return { from: from.toISOString(), to: to.toISOString() };
}

/**
 * The preset label whose day-span matches `value`, or "custom". Compared in
 * whole days (not exact ms) so a custom range spanning a whole number of days —
 * and a preset whose from/to round to the same span — both map to their label.
 */
export function activePreset(value: DateRange): string {
  const span = new Date(value.to).getTime() - new Date(value.from).getTime();
  const days = Math.round(span / DAY_MS);
  return PRESETS.find((p) => p.days === days)?.label ?? "custom";
}

/** ISO → YYYY-MM-DD for <input type="date">. */
export function isoToDateInput(iso: string): string {
  return new Date(iso).toISOString().slice(0, 10);
}

/** YYYY-MM-DD → ISO; `endOfDay` pins to 23:59:59 UTC for an inclusive `to`. */
export function dateInputToIso(d: string, endOfDay = false): string {
  const dt = new Date(`${d}T${endOfDay ? "23:59:59" : "00:00:00"}Z`);
  return dt.toISOString();
}

/** Buckets short windows by hour so a 24h range is not a single day-point. */
export function bucketFor(value: DateRange): "day" | "hour" {
  const span = new Date(value.to).getTime() - new Date(value.from).getTime();
  return span <= 2 * DAY_MS ? "hour" : "day";
}

export function defaultRange(): DateRange {
  return rangeForDays(30);
}
