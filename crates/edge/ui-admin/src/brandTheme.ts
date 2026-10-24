// Resolves raw `[branding]` into the tokens the widget actually paints, so the
// Appearance preview shows what a visitor will see rather than an approximation.
//
// This MIRRORS `crates/edge/src/theme.rs` (`resolve_theme`) — same defaults, same
// fallback rules, same validation. If you change one, change the other; the tests
// alongside this file pin the behaviour that matters. It is deliberately a copy
// rather than a round-trip to the server: the preview updates on every keystroke,
// and a request per keystroke would be worse than the duplication.

import type { Branding, DarkPalette, Gradient, SurfacePaint } from "./api";

/** `#rgb` / `#rgba` / `#rrggbb` / `#rrggbbaa` and nothing else — theme.rs `is_hex_color`. */
export const HEX = /^#(?:[0-9a-fA-F]{3,4}|[0-9a-fA-F]{6}|[0-9a-fA-F]{8})$/;

/** theme.rs `light_theme` defaults. */
const LIGHT = {
  primary: "#2563eb",
  primaryText: "#ffffff",
  userText: "#ffffff",
  agentBubble: "#f1f5f9",
  agentText: "#0f172a",
  surface: "#f7f9fc",
  panel: "#ffffff",
  border: "#e6eaf0",
  muted: "#64748b",
  text: "#0f172a",
  radius: "12px",
} as const;

/** theme.rs `dark_default` — the curated generic-dark neutrals. */
const DARK = {
  surface: "#1b2027",
  panel: "#242b34",
  border: "#333b46",
  muted: "#9aa5b4",
  text: "#e6eaf0",
  agentBubble: "#2a323d",
  agentText: "#e6eaf0",
} as const;

export const DEFAULT_FONT = "system-ui, -apple-system, 'Segoe UI', Roboto, sans-serif";

/** theme.rs `mod panel` — the usable floor, the resize ceiling, and the defaults. */
const PANEL = {
  floorW: 280,
  floorH: 360,
  ceil: 3000,
  w: 400,
  h: 660,
  minW: 320,
  minH: 420,
  frameW: 2,
  frameWMax: 24,
} as const;

const sizeOr = (v: number | null | undefined, def: number, floor: number): number =>
  Math.min(Math.max(typeof v === "number" && Number.isFinite(v) ? v : def, floor), PANEL.ceil);

const GRADIENT_ANGLES = [
  "to right",
  "to left",
  "to top",
  "to bottom",
  "to top right",
  "to top left",
  "to bottom right",
  "to bottom left",
] as const;

function colorOr(v: string | null | undefined, fallback: string): string {
  return v && HEX.test(v) ? v : fallback;
}

/** theme.rs `gradient_angle` — a side/corner keyword or a bounded `<n>deg`. */
export function gradientAngle(raw: string | null | undefined): string {
  const a = (raw ?? "").trim();
  if ((GRADIENT_ANGLES as readonly string[]).includes(a)) return a;
  const deg = a.endsWith("deg") ? Number(a.slice(0, -3).trim()) : NaN;
  if (Number.isFinite(deg) && deg >= 0 && deg <= 360) return `${deg}deg`;
  return "to right";
}

/** theme.rs `MAX_STOPS`. */
export const MAX_STOPS = 8;

/** A position as CSS, trimmed of pointless zeros — theme.rs `percent`. */
const percent = (v: number): string => String(Math.round(Math.min(Math.max(v, 0), 100) * 10) / 10);

/** theme.rs `explicit_stops` — validated, positioned and ordered, or null when
 *  fewer than two survive so the caller falls back to the shorthand. */
export function usableStops(g: Gradient | null | undefined): { color: string; at: number }[] | null {
  const usable = (g?.stops ?? []).filter((s) => s.color && HEX.test(s.color)).slice(0, MAX_STOPS);
  if (usable.length < 2) return null;
  const last = usable.length - 1;
  return usable
    .map((s, i) => ({
      color: s.color!,
      at: Math.min(Math.max(typeof s.at === "number" && Number.isFinite(s.at) ? s.at : (i / last) * 100, 0), 100),
    }))
    .sort((a, b) => a.at - b.at);
}

/** theme.rs `resolve_gradient` — explicit stops when usable, else the shorthand
 *  (both ends required and hex, `via` dropped alone if invalid), `""` when neither. */
export function resolveGradient(g: Gradient | null | undefined): string {
  if (!g) return "";
  const angle = gradientAngle(g.angle);
  const stops = usableStops(g);
  if (stops) {
    return `linear-gradient(${angle}, ${stops.map((s) => `${s.color} ${percent(s.at)}%`).join(", ")})`;
  }
  const from = g.from && HEX.test(g.from) ? g.from : null;
  const to = g.to && HEX.test(g.to) ? g.to : null;
  if (!from || !to) return "";
  const via = g.via && HEX.test(g.via) ? g.via : null;
  return via
    ? `linear-gradient(${angle}, ${from} 0%, ${via} 50%, ${to} 100%)`
    : `linear-gradient(${angle}, ${from} 0%, ${to} 100%)`;
}

/** theme.rs `resolve_paint` — a surface's own gradient if it resolves, else its flat
 *  colour, else `""` meaning inherit the shared brand paint. */
export function resolvePaint(p: SurfacePaint | null | undefined): string {
  if (!p) return "";
  const g = resolveGradient(p.gradient);
  if (g) return g;
  return p.color && HEX.test(p.color) ? p.color : "";
}

/** theme.rs `lighten` — move a hex toward white, for the unset dark accent. */
export function lighten(hex: string, amount: number): string {
  const h = hex.replace("#", "");
  const rr =
    h.length === 3 || h.length === 4
      ? h
          .slice(0, 3)
          .split("")
          .map((c) => c + c)
          .join("")
      : h.length >= 6
        ? h.slice(0, 6)
        : null;
  if (!rr) return hex;
  const byte = (i: number) => parseInt(rr.slice(i, i + 2), 16);
  const [r, g, b] = [byte(0), byte(2), byte(4)];
  if ([r, g, b].some((n) => Number.isNaN(n))) return hex;
  const lift = (v: number) => Math.round(v + (255 - v) * amount);
  const hx = (v: number) => lift(v).toString(16).padStart(2, "0");
  return `#${hx(r)}${hx(g)}${hx(b)}`;
}

/** The resolved tokens for one colour mode. */
export interface Resolved {
  primary: string;
  primaryText: string;
  accent: string;
  userBubble: string;
  userText: string;
  agentBubble: string;
  agentText: string;
  surface: string;
  panel: string;
  inputBg: string;
  border: string;
  text: string;
  muted: string;
  /** A composed `linear-gradient(…)`, or `""` — paint-only, never a `<color>`. */
  gradient: string;
  radius: string;
  font: string;
  launcher: string;
  avatar: boolean;
  shadow: boolean;
  width: number;
  height: number;
  minWidth: number;
  minHeight: number;
  frame: boolean;
  frameWidth: number;
  /** Own paint, or `""` to inherit the brand paint. */
  avatarBg: string;
  sendBg: string;
  inputBorder: string;
  agentBorder: string;
  sendShape: string;
  sendIcon: string;
  logoFit: string;
}

/** theme.rs `resolve_theme`. `dark` picks the dark palette: unset BRAND colours
 *  fall back to the resolved light value, unset NEUTRALS to the curated dark
 *  defaults, and an unset dark accent is the light primary lifted toward white so
 *  it stays legible on dark surfaces. */
export function resolveBrand(b: Branding, dark = false): Resolved {
  const primary = colorOr(b.primary_color ?? b.accent_color, LIGHT.primary);
  const panelBg = colorOr(b.panel, LIGHT.panel);
  const borderCol = colorOr(b.border, LIGHT.border);
  const width = sizeOr(b.width, PANEL.w, PANEL.floorW);
  const height = sizeOr(b.height, PANEL.h, PANEL.floorH);
  const light: Resolved = {
    primary,
    accent: primary,
    primaryText: colorOr(b.primary_text, LIGHT.primaryText),
    userBubble: colorOr(b.user_bubble, primary),
    userText: colorOr(b.user_bubble_text, LIGHT.userText),
    agentBubble: colorOr(b.agent_bubble, LIGHT.agentBubble),
    agentText: colorOr(b.agent_bubble_text, LIGHT.agentText),
    surface: colorOr(b.surface, LIGHT.surface),
    panel: panelBg,
    // Defaults to the panel — where the composer's colour came from before this
    // existed, so unset is a no-op.
    inputBg: colorOr(b.input_bg, panelBg),
    border: colorOr(b.border, LIGHT.border),
    muted: colorOr(b.muted, LIGHT.muted),
    text: colorOr(b.text, LIGHT.text),
    gradient: resolveGradient(b.gradient),
    radius: b.radius?.trim() || LIGHT.radius,
    font: b.font_family?.trim() || DEFAULT_FONT,
    launcher: ["bubble", "pill", "expand"].includes(b.launcher ?? "")
      ? b.launcher!
      : "bubble",
    // All four default to the pre-existing look — none rides on having a gradient.
    avatar: b.avatar === true,
    shadow: b.shadow !== false,
    frame: b.frame === true,
    avatarBg: resolvePaint(b.avatar_paint),
    sendBg: resolvePaint(b.send_paint),
    inputBorder: colorOr(b.input_border, borderCol),
    agentBorder: colorOr(b.agent_bubble_border, borderCol),
    frameWidth: Math.min(
      typeof b.frame_width === "number" && Number.isFinite(b.frame_width) ? b.frame_width : PANEL.frameW,
      PANEL.frameWMax,
    ),
    width,
    height,
    // The floor can never exceed the opening size — theme.rs pulls it down too.
    minWidth: Math.min(sizeOr(b.min_width, PANEL.minW, PANEL.floorW), width),
    minHeight: Math.min(sizeOr(b.min_height, PANEL.minH, PANEL.floorH), height),
    sendShape: b.send_shape === "circle" ? "circle" : "rounded",
    sendIcon: b.send_icon === "plane" ? "plane" : "arrow",
    logoFit: b.logo_fit === "contain" ? "contain" : "cover",
  };
  if (!dark) return light;

  const d: DarkPalette = b.dark ?? {};
  const darkBorder = colorOr(d.border, DARK.border);
  const darkPrimary = d.primary_color && HEX.test(d.primary_color) ? d.primary_color : null;
  const p = darkPrimary ?? light.primary;
  return {
    ...light,
    primary: p,
    // An authored dark primary is trusted as-is; an unset one is lifted, or a dark
    // brand colour would sit unreadable on the dark neutrals.
    accent: darkPrimary ?? lighten(light.primary, 0.32),
    primaryText: colorOr(d.primary_text, light.primaryText),
    userBubble: colorOr(d.user_bubble, p),
    userText: colorOr(d.user_bubble_text, light.userText),
    agentBubble: colorOr(d.agent_bubble, DARK.agentBubble),
    agentText: colorOr(d.agent_bubble_text, DARK.agentText),
    surface: colorOr(d.surface, DARK.surface),
    panel: colorOr(d.panel, DARK.panel),
    inputBg: colorOr(d.input_bg, colorOr(d.panel, DARK.panel)),
    border: darkBorder,
    inputBorder: colorOr(d.input_border, darkBorder),
    agentBorder: colorOr(d.agent_bubble_border, darkBorder),
    // Brand tokens: an unset dark override reuses the light one.
    avatarBg: resolvePaint(d.avatar_paint) || light.avatarBg,
    sendBg: resolvePaint(d.send_paint) || light.sendBg,
    muted: colorOr(d.muted, DARK.muted),
    text: colorOr(d.text, DARK.text),
    // The gradient is a BRAND token: an unset (or unusable) dark one reuses light.
    gradient: resolveGradient(d.gradient) || light.gradient,
  };
}

/** `light` (default), `dark` or `auto` — theme.rs `theme_mode`. */
export function themeMode(b: Branding): "light" | "dark" | "auto" {
  return b.theme_mode === "dark" || b.theme_mode === "auto" ? b.theme_mode : "light";
}

/** Relative luminance (WCAG). */
function luminance(hex: string): number {
  const h = hex.replace("#", "");
  const full =
    h.length === 3
      ? h
          .split("")
          .map((c) => c + c)
          .join("")
      : h.slice(0, 6);
  const ch = [0, 2, 4].map((i) => {
    const v = parseInt(full.slice(i, i + 2), 16) / 255;
    return v <= 0.03928 ? v / 12.92 : Math.pow((v + 0.055) / 1.055, 2.4);
  });
  return 0.2126 * ch[0] + 0.7152 * ch[1] + 0.0722 * ch[2];
}

/** WCAG contrast ratio between two hex colours, or null if either is unparseable.
 *  Used to warn when header text lands on a gradient stop it can't be read on. */
export function contrast(a: string, b: string): number | null {
  if (!HEX.test(a) || !HEX.test(b)) return null;
  const [la, lb] = [luminance(a), luminance(b)];
  const [hi, lo] = la > lb ? [la, lb] : [lb, la];
  return (hi + 0.05) / (lo + 0.05);
}
