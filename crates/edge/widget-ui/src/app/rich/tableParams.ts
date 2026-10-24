// Pure translation between the widget's table state and the server table
// endpoint's params. Kept out of the component so it is unit-tested directly.

import type { ColumnFiltersState } from "@tanstack/react-table";

import type { TableFilterParam } from "../api";
import type { ArtifactColumn, ArtifactColumnType, ColumnType } from "./spec";

/** The server's `"text"` maps to the inline table's `"string"`. */
export function clientType(t: ArtifactColumnType): ColumnType {
  return t === "text" ? "string" : t;
}

/** Translate TanStack column-filter state into server filter params: text → a
 *  `contains`, a numeric/date `[min, max]` → `gte`/`lte` (date bounds are epoch
 *  millis, as {@link RangeInputs} produces). Filters on unknown columns drop. */
export function toServerFilters(
  filters: ColumnFiltersState,
  columns: ArtifactColumn[],
): TableFilterParam[] {
  const typeOf = (id: string) => columns.find((c) => c.key === id)?.type;
  return filters.flatMap((f): TableFilterParam[] => {
    const type = typeOf(f.id);
    if (type === undefined) return []; // filter for a column not in the schema
    if (type === "text") {
      return typeof f.value === "string" && f.value
        ? [{ col: f.id, op: "contains", value: f.value }]
        : [];
    }
    const [min, max] = (f.value as [number | null, number | null] | undefined) ?? [null, null];
    const out: TableFilterParam[] = [];
    if (min != null) out.push({ col: f.id, op: "gte", value: min });
    if (max != null) out.push({ col: f.id, op: "lte", value: max });
    return out;
  });
}
