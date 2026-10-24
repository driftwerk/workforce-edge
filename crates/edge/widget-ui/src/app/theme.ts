import type { ColorMode, ThemeTokens } from "../protocol";

/** Mark the resolved colour mode on the iframe root. Only toggles the `wf-dark`
 *  class — the active palette's colours are applied by `applyTheme`. Deliberately
 *  does NOT set `color-scheme` on the root: that paints the iframe canvas opaque
 *  and puts a black box behind the transparent launcher. `color-scheme` for the
 *  scrollbars/native controls is scoped to the solid `.wf-panel` in the CSS.
 *  Idempotent — safe to call before render and on every live mode change. */
export function applyMode(mode: ColorMode): void {
  document.documentElement.classList.toggle("wf-dark", mode === "dark");
}

/** Apply one colour mode's theme tokens as CSS custom properties on the iframe
 *  root. The full palette (brand + neutral) comes from the server per mode, so
 *  swapping mode is just re-applying the other palette. */
export function applyTheme(t: ThemeTokens): void {
  const s = document.documentElement.style;
  s.setProperty("--wf-primary", t.primary);
  s.setProperty("--wf-primary-text", t.primary_text);
  s.setProperty("--wf-user", t.user_bubble);
  s.setProperty("--wf-user-text", t.user_bubble_text);
  s.setProperty("--wf-agent", t.agent_bubble);
  s.setProperty("--wf-agent-text", t.agent_bubble_text);
  s.setProperty("--wf-accent", t.accent);
  s.setProperty("--wf-surface", t.surface);
  s.setProperty("--wf-panel", t.panel);
  // Falls back to the panel, matching where the composer's colour came from before
  // it was configurable — so a stale server that ships no field is a no-op.
  s.setProperty("--wf-input-bg", t.input_bg || t.panel);
  s.setProperty("--wf-border", t.border);
  // Both default to the shared border, which is where they came from.
  s.setProperty("--wf-input-border", t.input_border || t.border);
  s.setProperty("--wf-agent-border", t.agent_bubble_border || t.border);
  s.setProperty("--wf-muted", t.muted);
  s.setProperty("--wf-strong", t.text);
  s.setProperty("--wf-font", t.font_family);
  s.setProperty("--wf-radius", t.radius);
  // Drop shadow toggle (default on) and full-screen content width — driven by
  // per-agent branding, so nothing here is install-specific.
  s.setProperty("--wf-shadow", t.shadow === false ? "none" : "var(--wf-shadow-default)");
  s.setProperty("--wf-content-max", t.fullscreen === "wide" ? "min(1400px, 94%)" : "860px");
  // The brand gradient is a PAINT, applied only to backgrounds. Unset (or a stale
  // server that ships no field) falls back to the flat primary, so those surfaces
  // render exactly as they did before gradients existed.
  const gradient = t.gradient || "";
  s.setProperty("--wf-brand-gradient", gradient || "var(--wf-primary)");
  // Two independent opt-ins, each a root class so the CSS can gate on it:
  //   wf-grad  — a gradient is configured; the ring can't ride the token fallback
  //              because its border is normally the NEUTRAL --wf-border.
  //   wf-frame — the framed presentation. Deliberately NOT implied by wf-grad:
  //              brands differ, and one wanting a gradient does not mean it wants
  //              its panel restructured.
  const root = document.documentElement;
  root.classList.toggle("wf-grad", !!gradient);
  root.classList.toggle("wf-frame", t.frame === true);
  // Per-surface paint. Empty ⇒ inherit the brand paint, which is what both did before
  // they were configurable. A surface with its OWN paint also opts out of the
  // mid-gradient zoom: that zoom exists to keep an inherited brand gradient legible on
  // a small control, but a gradient chosen FOR this control should show in full.
  for (const [surface, own] of [
    ["avatar", t.avatar_bg],
    ["send", t.send_bg],
  ] as const) {
    s.setProperty(`--wf-${surface}-bg`, own || "var(--wf-brand-gradient)");
    s.setProperty(`--wf-${surface}-zoom`, own ? "100% 100%" : "var(--wf-brand-zoom-size)");
  }
  // Only meaningful under .wf-frame, but set unconditionally: an inline custom
  // property beats the `:root.wf-frame` default, which is what lets config win.
  if (typeof t.frame_width === "number") s.setProperty("--wf-frame-w", `${t.frame_width}px`);
  root.classList.toggle("wf-send-circle", t.send_shape === "circle");
  root.classList.toggle("wf-logo-contain", t.logo_fit === "contain");
  loadFont(t.font_url);
}

/** Load the operator-configured web font stylesheet once (validated https on
 *  the server). Idempotent — re-applying the theme won't add duplicate links. */
function loadFont(url: string): void {
  if (!url) return;
  const existing = document.getElementById("wf-font-link") as HTMLLinkElement | null;
  if (existing) {
    if (existing.href !== url) existing.href = url;
    return;
  }
  const link = document.createElement("link");
  link.id = "wf-font-link";
  link.rel = "stylesheet";
  link.href = url;
  document.head.appendChild(link);
}
