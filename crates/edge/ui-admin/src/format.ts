// Pure display formatters shared across the analytics dashboard. No React, no
// DOM — imported directly by unit tests.

/** Money in USD — the LLM provider's billing currency, stored raw as `cost_usd`
 *  (no FX conversion). 2 dp normally, 4 dp for sub-cent amounts (never a bare 0). */
export function money(n: number): string {
  if (!Number.isFinite(n) || n === 0) return "$0.00";
  if (Math.abs(n) < 0.01) return `$${n.toFixed(4)}`;
  return `$${n.toFixed(2)}`;
}

/** Compact token count with k/M suffixes for dense tables and axes. */
export function formatTokens(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}k`;
  return String(n);
}

/** A 0..1 ratio as a one-decimal percentage. */
export function percent(ratio: number): string {
  if (!Number.isFinite(ratio)) return "0.0%";
  return `${(ratio * 100).toFixed(1)}%`;
}

/** A thumbs rating (+1/-1) → display symbol, accessible label, and the pill
 *  status class (reusing the analytics `pill-ok`/`pill-bad` styling). Any value
 *  other than a positive number reads as a down-vote. */
export function formatRating(rating: number): { symbol: string; label: string; cls: string } {
  return rating > 0
    ? { symbol: "👍", label: "Positive", cls: "pill-ok" }
    : { symbol: "👎", label: "Negative", cls: "pill-bad" };
}

/** Human duration from milliseconds (matches the hub's granularity). */
export function formatDuration(ms: number): string {
  if (!Number.isFinite(ms) || ms <= 0) return "—";
  const totalSec = Math.floor(ms / 1000);
  if (totalSec < 1) return `${Math.round(ms)}ms`;
  if (totalSec < 60) return `${totalSec}s`;
  const min = Math.floor(totalSec / 60);
  const sec = totalSec % 60;
  if (min < 60) return sec > 0 ? `${min}m ${sec}s` : `${min}m`;
  const hr = Math.floor(min / 60);
  const remainMin = min % 60;
  return remainMin > 0 ? `${hr}h ${remainMin}m` : `${hr}h`;
}
