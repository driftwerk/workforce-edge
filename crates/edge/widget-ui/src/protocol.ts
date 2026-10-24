// The typed postMessage protocol between the loader (host-page context) and the
// chat SPA (sandboxed iframe). Every message carries the per-load `nonce`; both
// sides additionally pin `event.origin` (never `event.source`, never `"*"`) —
// the hardening the research calls for. Phase 1 covers the boot handshake,
// theme, and iframe resize; host-page actions (navigate/highlight) will extend
// `HostBound`/`FrameBound` with an approval-gated action channel later.

import type { Anchor } from "./anchor";

export const PROTOCOL = "wf-edge/1";

/** The resolved colour scheme the iframe paints. Brand tokens are mode-independent;
 *  the light/dark *neutral* sets live in the widget CSS, toggled by this mode. */
export type ColorMode = "light" | "dark";

/** Coerce an untrusted value (host-relayed or a URL param) to a valid mode, else
 *  null — never trusted into the DOM without this. */
export function asColorMode(v: unknown): ColorMode | null {
  return v === "light" || v === "dark" ? v : null;
}

/** Theme tokens for one colour mode, resolved server-side from `[branding]` (light)
 *  or `[branding.dark]` (dark) and forwarded verbatim. Colour fields differ per
 *  mode; structural fields (title, logo, launcher, font, radius, position, shadow,
 *  fullscreen) are identical across the pair. */
export interface ThemeTokens {
  title: string;
  subtitle: string;
  logo_url: string;
  launcher: "bubble" | "pill" | "expand";
  primary: string;
  primary_text: string;
  user_bubble: string;
  user_bubble_text: string;
  agent_bubble: string;
  agent_bubble_text: string;
  accent: string;
  surface: string;
  panel: string;
  /** Composer message-box background. Defaults server-side to `panel`; absent only
   *  on a stale server, where the CSS falls back to `panel` too. */
  input_bg?: string;
  border: string;
  /** Borders that default server-side to `border`; absent only on a stale server. */
  input_border?: string;
  agent_bubble_border?: string;
  muted: string;
  text: string;
  font_family: string;
  font_url: string;
  radius: string;
  position: "right" | "left";
  shadow: boolean;
  fullscreen: "centered" | "wide";
  /** Whether the customer may drag the widget to another viewport corner. */
  draggable: boolean;
  /** Inset in px of the docked position from its corner, horizontal then vertical,
   *  before the customer moves it. Server-capped; absent on a stale server (flush). */
  offset_x?: number;
  offset_y?: number;
  /** A composed `linear-gradient(…)` for the brand surfaces, or `""` for none.
   *  Paint-only — never used where CSS wants a `<color>`. Absent on a stale server. */
  gradient?: string;
  /** Whether agent replies carry a round brand avatar. Absent on a stale server. */
  avatar?: boolean;
  /** Each surface's OWN paint — a composed gradient or a flat hex — or `""`/absent to
   *  inherit the shared brand paint. Independent of each other and of `gradient`. */
  avatar_bg?: string;
  send_bg?: string;
  /** Framed presentation — brand paint fills the panel, conversation inset inside
   *  it. Opt-in per agent; absent/false keeps the flat panel + hairline border. */
  frame?: boolean;
  /** Frame thickness in px (server-clamped). Only used when `frame` is on. */
  frame_width?: number;
  send_shape?: "rounded" | "circle";
  send_icon?: "arrow" | "plane";
  /** How the logo is fitted: `cover` (rounded tile) or `contain` (bare mark). */
  logo_fit?: "cover" | "contain";
  /** Panel box on open, and the floor a customer may drag it to. Server-clamped to
   *  a usable range with `min_* <= width/height` already guaranteed. Absent on a
   *  stale server, in which case the widget's own constants apply. */
  width?: number;
  height?: number;
  min_width?: number;
  min_height?: number;
}

/** Per-agent rich-output flags, resolved server-side from the embed config.
 *  Gates which wf-* fenced blocks the chat renders as interactive components. */
export interface WidgetFeatures {
  tables: boolean;
  charts: boolean;
  forms: boolean;
  maps: boolean;
  diagrams: boolean;
  /** Show the "download for Excalidraw" control on a rendered diagram. Not a
   *  fence gate — the model is never told about it. */
  excalidraw: boolean;
  /** Render image attachments as pictures rather than download buttons. */
  inline_images: boolean;
  /** Opt-in rendering of `wf-image` blocks — the agent showing a picture back in
   *  its reply. Distinct from `inline_images`, which only decides how an
   *  attachment is drawn. */
  images: boolean;
  /** Kinds of file the customer may attach. Empty ⇒ uploads are off. The
   *  server enforces this too; the client only advertises it. */
  uploads: UploadKind[];
  /** Per-file cap in MB, for a client-side message before the round trip. */
  upload_max_mb: number;
  /** Offer the drawing canvas (needs `image` in `uploads`). */
  drawing: boolean;
}

/** Categories of upload, mirroring the server's `UploadKind`. */
export type UploadKind = "image" | "spreadsheet" | "document";

export const NO_FEATURES: WidgetFeatures = {
  tables: false,
  charts: false,
  forms: false,
  maps: false,
  diagrams: false,
  excalidraw: false,
  inline_images: false,
  images: false,
  uploads: [],
  upload_max_mb: 10,
  drawing: false,
};

/** One labelled entity visible on the host screen (e.g. `Selected order: Express`). */
export interface ScreenItem {
  label: string;
  value: string;
}

/** A bounded snapshot of what the signed-in user is viewing on the host page,
 *  forwarded verbatim to the agent turn as UNTRUSTED background context. The host
 *  page supplies it (via `wf-embed-context`); the edge caps + fences it. */
export interface ScreenContext {
  route?: string;
  title?: string;
  org?: string;
  items: ScreenItem[];
}

/** The visual state the iframe wants; the loader owns host-viewport sizing. */
export type FrameState = "closed" | "open" | "fullscreen";

const WIDGET_EVENTS = [
  "opened",
  "closed",
  "fullscreen",
  "moved",
  "message-sent",
  "conversation-list-opened",
] as const;

/** A user gesture the widget reports for the host page's analytics, surfaced to
 *  it as `wf-embed-event`. Product-neutral names — the host maps them onto its
 *  own vocabulary, or ignores the message entirely. */
export type WidgetEvent = (typeof WIDGET_EVENTS)[number];

/** Coerce an untrusted value to a widget event name, else null — the host page
 *  only ever sees names this build defines. */
export function asWidgetEvent(v: unknown): WidgetEvent | null {
  return WIDGET_EVENTS.includes(v as WidgetEvent) ? (v as WidgetEvent) : null;
}

/** Messages the iframe sends up to the loader. The two positioning messages are the
 *  iframe asking; the loader alone can see the host viewport, so it resolves them
 *  and pushes the result back as `anchor`. */
export type FrameBound =
  | { proto: typeof PROTOCOL; nonce: string; type: "ready" }
  | {
      proto: typeof PROTOCOL;
      nonce: string;
      type: "resize";
      state: FrameState;
      width?: number;
      height?: number;
    }
  | { proto: typeof PROTOCOL; nonce: string; type: "token-request" }
  /** "Put the widget here" — the restored position on mount, and the corner cycle. */
  | { proto: typeof PROTOCOL; nonce: string; type: "anchor"; anchor: Anchor }
  /** One live drag step, in screen px. */
  | { proto: typeof PROTOCOL; nonce: string; type: "move"; dx: number; dy: number }
  /** A usage signal for the host page (relayed onward as `wf-embed-event`). */
  | { proto: typeof PROTOCOL; nonce: string; type: "event"; event: WidgetEvent };

/** Messages the loader sends down to the iframe. */
export type HostBound =
  | {
      proto: typeof PROTOCOL;
      nonce: string;
      type: "init";
      token: string | null;
      theme: ThemeTokens;
      /** The dark palette. Absent on a stale loader ⇒ the SPA reuses `theme`. */
      themeDark?: ThemeTokens;
      welcome: string;
      /** Absent on a stale loader — the SPA degrades to no rich output. */
      features?: WidgetFeatures;
      /** Initial resolved colour mode. Absent on a stale loader ⇒ SPA keeps light. */
      mode?: ColorMode;
    }
  | { proto: typeof PROTOCOL; nonce: string; type: "token"; token: string }
  | {
      proto: typeof PROTOCOL;
      nonce: string;
      type: "context";
      context: ScreenContext;
    }
  | { proto: typeof PROTOCOL; nonce: string; type: "mode"; mode: ColorMode }
  /** The RESOLVED position after clamping to the host viewport, pushed on change.
   *  The iframe renders and persists it; it never echoes it back. */
  | { proto: typeof PROTOCOL; nonce: string; type: "anchor"; anchor: Anchor };

export function isFrameBound(nonce: string, data: unknown): data is FrameBound {
  return (
    !!data &&
    typeof data === "object" &&
    (data as { proto?: unknown }).proto === PROTOCOL &&
    (data as { nonce?: unknown }).nonce === nonce
  );
}

export function isHostBound(nonce: string, data: unknown): data is HostBound {
  return (
    !!data &&
    typeof data === "object" &&
    (data as { proto?: unknown }).proto === PROTOCOL &&
    (data as { nonce?: unknown }).nonce === nonce
  );
}
