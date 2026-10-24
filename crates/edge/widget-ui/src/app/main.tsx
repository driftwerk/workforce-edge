import { useCallback, useEffect, useState } from "react";
import { createRoot } from "react-dom/client";

import { dockedAnchor, parseAnchor, type Anchor } from "../anchor";
import {
  asColorMode,
  type ColorMode,
  isHostBound,
  NO_FEATURES,
  PROTOCOL,
  type FrameState,
  type ScreenContext,
  type ThemeTokens,
  type WidgetEvent,
  type WidgetFeatures,
} from "../protocol";
import { App } from "./App";
import { applyMode, applyTheme } from "./theme";
import "./styles.css";

const params = new URLSearchParams(location.search);
const agent = params.get("agent") ?? "";
const nonce = params.get("nonce") ?? "";
const parentOrigin = params.get("parent") ?? "";
const base = location.origin;

// Mark the resolved scheme before first render so the panel never flashes the
// wrong mode. The palettes arrive with `init`; a stale loader without the param
// leaves the default light set.
applyMode(asColorMode(params.get("mode")) ?? "light");

function postToHost(msg: Record<string, unknown>) {
  if (!parentOrigin) return;
  window.parent.postMessage({ proto: PROTOCOL, nonce, ...msg }, parentOrigin);
}

/** Where the customer last put the widget. sessionStorage so it survives navigation
 *  within the tab (the position outlives the page, which is the point) without
 *  becoming durable client-side storage; wrapped because a sandboxed iframe without
 *  same-origin access throws on any storage access. */
const anchorKey = "wf-anchor:" + agent;
function readStoredAnchor(): Anchor | null {
  try {
    return parseAnchor(JSON.parse(sessionStorage.getItem(anchorKey) ?? "null"));
  } catch {
    return null;
  }
}
function writeStoredAnchor(a: Anchor): void {
  try {
    sessionStorage.setItem(anchorKey, JSON.stringify(a));
  } catch {
    /* storage unavailable (sandboxed) — the position still holds for this page */
  }
}

/** The `exp` (ms since epoch) of a JWT, or null if it can't be read. Used only to
 *  time the next token refresh; the token itself is validated server-side. */
function jwtExpMs(token: string): number | null {
  try {
    const b64 = token.split(".")[1].replace(/-/g, "+").replace(/_/g, "/");
    const padded = b64.padEnd(b64.length + ((4 - (b64.length % 4)) % 4), "=");
    const exp = JSON.parse(atob(padded)).exp;
    return typeof exp === "number" ? exp * 1000 : null;
  } catch {
    return null;
  }
}

function Boot() {
  const [theme, setTheme] = useState<ThemeTokens | null>(null);
  const [themeDark, setThemeDark] = useState<ThemeTokens | null>(null);
  const [mode, setMode] = useState<ColorMode>(asColorMode(params.get("mode")) ?? "light");
  const [welcome, setWelcome] = useState("");
  const [features, setFeatures] = useState<WidgetFeatures>(NO_FEATURES);
  const [token, setToken] = useState<string | null>(null);
  // Latest UNTRUSTED screen context pushed by the host page (via the loader). It
  // rides the SAME origin-pinned + nonce-matched channel as the token push; the
  // edge caps and fences it. Null until the host sends one (many hosts never do).
  const [context, setContext] = useState<ScreenContext | null>(null);
  // Where the loader has placed the widget. The loader resolves every position
  // against the host viewport, so this is display + persistence only and is never
  // echoed back — which is also what keeps the two `anchor` messages loop-free.
  const [anchor, setAnchor] = useState<Anchor>(() => dockedAnchor("right"));

  useEffect(() => {
    function onMessage(e: MessageEvent) {
      // Origin-pin to the exact host page + match the per-load nonce.
      if (e.origin !== parentOrigin || !isHostBound(nonce, e.data)) return;
      const m = e.data;
      if (m.type === "init") {
        setTheme(m.theme);
        setThemeDark(m.themeDark ?? null);
        setMode(asColorMode(m.mode) ?? "light");
        setWelcome(m.welcome);
        setFeatures(m.features ?? NO_FEATURES);
        setToken(m.token);
      } else if (m.type === "token") {
        setToken(m.token);
      } else if (m.type === "context") {
        setContext(m.context);
      } else if (m.type === "mode") {
        const next = asColorMode(m.mode);
        if (next) setMode(next);
      } else if (m.type === "anchor") {
        const next = parseAnchor(m.anchor);
        if (next) setAnchor(next);
      }
    }
    window.addEventListener("message", onMessage);
    postToHost({ type: "ready" });
    return () => window.removeEventListener("message", onMessage);
  }, []);

  // Token lifecycle. The CONTRACT is that the host pushes a fresh token BEFORE
  // the current one expires (it alone holds the refresh token — see the host
  // integration docs); pushed tokens arrive via the `token` message above.
  // This interval is the FALLBACK for hosts that only answer on demand: it keeps
  // re-asking whenever the token is missing or in its final stretch, so even a
  // host that can only mint a fresh token after expiry gets asked again within
  // seconds of it — never a one-shot timer, which dies the first time the host
  // returns the same still-valid token.
  useEffect(() => {
    const ask = () => postToHost({ type: "token-request" });
    if (!token) ask();
    const t = setInterval(
      () => {
        if (!token) {
          ask();
          return;
        }
        const exp = jwtExpMs(token);
        if (!exp || exp - Date.now() < 45_000) ask();
      },
      token ? 15_000 : 2_000,
    );
    return () => clearInterval(t);
  }, [token]);

  const resize = useCallback(
    (state: FrameState, width?: number, height?: number) =>
      postToHost({ type: "resize", state, width, height }),
    [],
  );

  const reanchor = useCallback((a: Anchor) => postToHost({ type: "anchor", anchor: a }), []);
  const move = useCallback((dx: number, dy: number) => postToHost({ type: "move", dx, dy }), []);
  const emit = useCallback((event: WidgetEvent) => postToHost({ type: "event", event }), []);

  // Claim the remembered position once the theme says the agent is draggable. The
  // restore is gated too, so turning the config off leaves nothing that can still
  // reposition the widget.
  const draggable = theme?.draggable === true;
  const position = theme?.position;
  const offsetX = theme?.offset_x;
  const offsetY = theme?.offset_y;
  useEffect(() => {
    if (!position) return;
    const docked = dockedAnchor(position, { offset_x: offsetX, offset_y: offsetY });
    const start = (draggable ? readStoredAnchor() : null) ?? docked;
    setAnchor(start);
    if (draggable) reanchor(start);
  }, [draggable, position, offsetX, offsetY, reanchor]);

  // The loader resolves a new anchor on every pointer move of a drag, so persist the
  // settled position rather than stringifying into sessionStorage 100 times a second.
  // Gated on `draggable` so an agent nobody can move never writes a position key at
  // all, and on `position` so this cannot fire before the restore above has read one.
  useEffect(() => {
    if (!draggable || !position) return;
    const write = () => writeStoredAnchor(anchor);
    const settled = setTimeout(write, 300);
    // A host navigation inside the debounce window would otherwise drop the move.
    window.addEventListener("pagehide", write);
    return () => {
      clearTimeout(settled);
      window.removeEventListener("pagehide", write);
    };
  }, [anchor, draggable, position]);

  // The active palette for the resolved mode — dark falls back to light if a stale
  // loader sent no dark palette. Applied imperatively (CSS vars on the root) and
  // re-applied whenever the mode flips or the palettes arrive.
  const activeTheme = mode === "dark" ? (themeDark ?? theme) : theme;
  useEffect(() => {
    if (activeTheme) applyTheme(activeTheme);
  }, [activeTheme]);
  useEffect(() => {
    applyMode(mode);
  }, [mode]);

  if (!activeTheme) return null;
  return (
    <App
      base={base}
      agent={agent}
      token={token}
      theme={activeTheme}
      welcome={welcome}
      features={features}
      resize={resize}
      anchor={anchor}
      move={move}
      reanchor={reanchor}
      emit={emit}
      context={context}
    />
  );
}

createRoot(document.getElementById("root")!).render(<Boot />);
