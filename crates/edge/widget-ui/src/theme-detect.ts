// Pure host-theme detection for the loader's `auto` mode. No DOM access lives
// here — the loader reads the signals off the host page and passes them in, so
// every heuristic is a deterministic function unit-tested in a node runner.
//
// The cascade (strongest first): an explicit host signal, then the host's
// declared theme attributes (computed `color-scheme` catches next-themes on both
// the `class` and `data-theme` conventions), then background luminance as the
// framework-agnostic floor, then the OS preference.

import type { ColorMode } from "./protocol";

interface Rgba {
  r: number;
  g: number;
  b: number;
  a: number;
}

/** The host page's declared theme signals, read off `document.documentElement`. */
export interface ThemeSignals {
  /** `getComputedStyle(html).colorScheme`. */
  colorScheme: string;
  dataTheme: string | null;
  className: string;
  dataBsTheme: string | null;
  dataMuiColorScheme: string | null;
  dataColorMode: string | null;
}

/** Inputs to the `auto` resolution, all plain values so the resolver stays pure. */
export interface ModeInputs {
  /** Sticky explicit signal the host pushed (`wf-embed-theme`); wins outright. */
  explicit: ColorMode | null;
  attrs: ThemeSignals;
  /** First non-transparent background behind the widget, or null if none found. */
  background: string | null;
  /** `matchMedia('(prefers-color-scheme: dark)').matches`. */
  prefersDark: boolean;
}

const clamp = (n: number, lo: number, hi: number) => Math.min(hi, Math.max(lo, n));

function channel(v: string): number | null {
  const pct = v.endsWith("%");
  const n = parseFloat(v);
  if (!Number.isFinite(n)) return null;
  return clamp(Math.round(pct ? (n / 100) * 255 : n), 0, 255);
}

function alpha(v: string): number | null {
  const n = v.endsWith("%") ? parseFloat(v) / 100 : parseFloat(v);
  return Number.isFinite(n) ? clamp(n, 0, 1) : null;
}

function parseHex(hex: string): Rgba | null {
  let h = hex;
  if (h.length === 3 || h.length === 4) {
    h = h
      .split("")
      .map((c) => c + c)
      .join("");
  }
  if ((h.length !== 6 && h.length !== 8) || !/^[0-9a-f]+$/.test(h)) return null;
  return {
    r: parseInt(h.slice(0, 2), 16),
    g: parseInt(h.slice(2, 4), 16),
    b: parseInt(h.slice(4, 6), 16),
    a: h.length === 8 ? parseInt(h.slice(6, 8), 16) / 255 : 1,
  };
}

/** Parse a `#hex`, `rgb(...)` or `rgba(...)` colour (the forms `getComputedStyle`
 *  returns). Null for named colours, `transparent`, gradients, or empty. */
export function parseColor(css: string): Rgba | null {
  const s = css.trim().toLowerCase();
  if (!s || s === "transparent") return null;
  if (s[0] === "#") return parseHex(s.slice(1));
  const m = s.match(/^rgba?\(([^)]+)\)$/);
  if (!m) return null;
  const parts = m[1].split(/[,/\s]+/).filter(Boolean);
  if (parts.length < 3) return null;
  const r = channel(parts[0]);
  const g = channel(parts[1]);
  const b = channel(parts[2]);
  const a = parts[3] === undefined ? 1 : alpha(parts[3]);
  if (r === null || g === null || b === null || a === null) return null;
  return { r, g, b, a };
}

function relativeLuminance({ r, g, b }: Rgba): number {
  const lin = (v: number) => {
    const s = v / 255;
    return s <= 0.03928 ? s / 12.92 : Math.pow((s + 0.055) / 1.055, 2.4);
  };
  return 0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b);
}

/** Whether a CSS colour reads as dark (relative luminance below the midpoint).
 *  Null when unparseable or fully transparent — the caller must not guess. */
export function isDarkColor(css: string): boolean | null {
  const c = parseColor(css);
  if (!c || c.a === 0) return null;
  return relativeLuminance(c) < 0.5;
}

/** A `light`/`dark` token in a value like `color-scheme` or a `data-*` attribute.
 *  Requires exactly one of the two so `light dark` / `normal` stay inconclusive,
 *  and word boundaries so `darkblue` never matches. */
function tokenMode(v: string | null): ColorMode | null {
  if (!v) return null;
  const t = v.toLowerCase();
  const dark = /\bdark\b/.test(t);
  const light = /\blight\b/.test(t);
  if (dark && !light) return "dark";
  if (light && !dark) return "light";
  return null;
}

function classNameMode(className: string): ColorMode | null {
  const classes = className.split(/\s+/);
  if (classes.includes("dark")) return "dark";
  if (classes.includes("light")) return "light";
  return null;
}

/** Classify the host's declared theme signals, or null if none is conclusive.
 *  Order follows prevalence: the standard `color-scheme`, then the two dominant
 *  conventions (`data-theme`, Tailwind `class`), then the long-tail framework
 *  attributes. */
export function classifyThemeAttrs(s: ThemeSignals): ColorMode | null {
  return (
    tokenMode(s.colorScheme) ??
    tokenMode(s.dataTheme) ??
    classNameMode(s.className) ??
    tokenMode(s.dataBsTheme) ??
    tokenMode(s.dataMuiColorScheme) ??
    tokenMode(s.dataColorMode)
  );
}

/** Resolve the effective mode for `auto`: explicit host signal, then declared
 *  attributes, then background luminance, then the OS preference. */
export function resolveMode(i: ModeInputs): ColorMode {
  if (i.explicit) return i.explicit;
  const attr = classifyThemeAttrs(i.attrs);
  if (attr) return attr;
  if (i.background) {
    const dark = isDarkColor(i.background);
    if (dark !== null) return dark ? "dark" : "light";
  }
  return i.prefersDark ? "dark" : "light";
}
