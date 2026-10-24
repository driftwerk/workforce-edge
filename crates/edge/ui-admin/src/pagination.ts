// Cursor-pagination accumulation for the per-turn drill-down. Pure — appends a
// freshly-fetched page onto the rows already shown, skipping any key already
// present so an overlapping cursor page never duplicates a row.

export function mergePages<T>(prev: T[], incoming: T[], keyOf: (row: T) => string | number): T[] {
  if (prev.length === 0) return incoming;
  const seen = new Set(prev.map(keyOf));
  const merged = prev.slice();
  for (const row of incoming) {
    if (!seen.has(keyOf(row))) {
      seen.add(keyOf(row));
      merged.push(row);
    }
  }
  return merged;
}
