// The loader runs in the HOST page. It mounts the chat SPA in a sandboxed
// iframe, relays the customer token from the host (the existing
// `wf-embed-token` handshake) down to the iframe, forwards the theme, and sizes
// the iframe as the widget opens/closes. It is the ONLY code that touches the
// host page — the chat UI is fully isolated in the iframe.
//
// The edge serves this file at /embed/{agent} prefixed with a per-agent config:
//   window.__WF_EDGE__ = { agent, base, origins, theme, nonce }

import {
  anchorInsets,
  clampAnchor,
  dockedAnchor,
  parseAnchor,
  translateAnchor,
  type Anchor,
  type Box,
} from "../anchor";
import {
  asColorMode,
  asWidgetEvent,
  isFrameBound,
  NO_FEATURES,
  PROTOCOL,
  type ColorMode,
  type ThemeTokens,
  type WidgetEvent,
  type WidgetFeatures,
} from "../protocol";
import { parseColor, resolveMode, type ThemeSignals } from "../theme-detect";

interface EdgeConfig {
  agent: string;
  base: string;
  origins: string[];
  theme: ThemeTokens;
  theme_dark: ThemeTokens;
  /** `light` (default) / `dark` pin the scheme; `auto` follows the host page. */
  theme_mode?: "light" | "dark" | "auto";
  welcome: string;
  features?: WidgetFeatures;
  nonce: string;
}

declare global {
  interface Window {
    __WF_EDGE__?: EdgeConfig;
    __WF_EDGE_LOADED__?: boolean;
    // Set synchronously by the host to tear the widget down even if this loader
    // is still loading (a lost destroy message would otherwise orphan it).
    __WF_EDGE_KILL__?: boolean;
  }
}

(function () {
  "use strict";
  const cfg = window.__WF_EDGE__;
  if (!cfg || window.__WF_EDGE_LOADED__) return;
  window.__WF_EDGE__ = undefined;
  window.__WF_EDGE_LOADED__ = true;

  const { agent, base, origins, theme, welcome, nonce } = cfg;
  const themeDark = cfg.theme_dark;
  const features = cfg.features ?? NO_FEATURES;
  let token: string | null = null;

  // Colour scheme. `light`/`dark` pin the widget; `auto` follows the host page —
  // an explicit host signal, then its declared theme attributes / background, then
  // the OS — re-resolved live. Every heuristic lives in the pure `theme-detect`
  // module; the code here is only the DOM glue that feeds it and the relay.
  const themeMode = cfg.theme_mode ?? "light";
  const darkMql = window.matchMedia?.("(prefers-color-scheme: dark)");
  let explicit: ColorMode | null = null;
  let scheduled = 0;
  let modeObserver: MutationObserver | null = null;

  function readSignals(): ThemeSignals {
    const el = document.documentElement;
    return {
      colorScheme: getComputedStyle(el).colorScheme,
      dataTheme: el.getAttribute("data-theme"),
      className: el.className,
      dataBsTheme: el.getAttribute("data-bs-theme"),
      dataMuiColorScheme: el.getAttribute("data-mui-color-scheme"),
      dataColorMode: el.getAttribute("data-color-mode"),
    };
  }

  // First painted background behind the widget — the luminance floor. Reads the
  // host body then the root; either may be transparent, in which case resolution
  // falls through to the OS rather than guessing.
  function hostBackground(): string | null {
    for (let el: Element | null = document.body; el; el = el.parentElement) {
      const bg = getComputedStyle(el).backgroundColor;
      const c = parseColor(bg);
      if (c && c.a !== 0) return bg;
    }
    return null;
  }

  function resolvedMode(): ColorMode {
    if (themeMode !== "auto") return themeMode;
    return resolveMode({
      explicit,
      attrs: readSignals(),
      background: hostBackground(),
      prefersDark: !!darkMql?.matches,
    });
  }

  let currentMode: ColorMode = resolvedMode();

  function pushMode() {
    const next = resolvedMode();
    if (next === currentMode) return;
    currentMode = next;
    toFrame({ type: "mode", mode: currentMode });
  }

  // Coalesce bursts of host mutations (a class swap often touches class + style)
  // into one resolve per frame.
  function scheduleResolve() {
    if (scheduled) return;
    scheduled = requestAnimationFrame(() => {
      scheduled = 0;
      pushMode();
    });
  }

  // getComputedStyle + the luminance walk are unreliable before layout, and this
  // loader can run before <body> exists, so re-resolve once the DOM is ready.
  function onDomReady() {
    pushMode();
  }

  if (themeMode === "auto") {
    modeObserver = new MutationObserver(scheduleResolve);
    modeObserver.observe(document.documentElement, {
      attributes: true,
      attributeFilter: [
        "class",
        "style",
        "data-theme",
        "data-bs-theme",
        "data-mui-color-scheme",
        "data-color-mode",
      ],
    });
    darkMql?.addEventListener?.("change", scheduleResolve);
    // Before <body>/layout exists the initial resolve above can only see the root
    // attributes + OS; re-resolve once the DOM is ready to pick up the background.
    if (document.readyState === "loading") {
      window.addEventListener("DOMContentLoaded", onDomReady);
    }
  }

  const side = theme.position === "left" ? "left" : "right";
  const frame = document.createElement("iframe");
  frame.title = theme.title || "Chat";
  frame.setAttribute("allow", "clipboard-write");
  // Cross-origin to the host in real deploys → full isolation. allow-same-origin
  // keeps the iframe's OWN (edge) origin so its WS/REST calls stay same-origin;
  // top-navigation is deliberately withheld — page control goes through the
  // controlled action bridge, never raw navigation from the frame. allow-downloads
  // lets the agent's generated files (e.g. an xlsx export) save from the frame.
  frame.setAttribute(
    "sandbox",
    "allow-scripts allow-same-origin allow-forms allow-popups allow-downloads",
  );
  const src = new URL(base + "/widget/");
  src.searchParams.set("agent", agent);
  src.searchParams.set("nonce", nonce);
  src.searchParams.set("parent", location.origin);
  // Initial mode as a param so the SPA paints the right scheme before the init
  // round-trip — no light→dark flash. `init` carries the latest resolved mode too.
  src.searchParams.set("mode", currentMode);
  frame.src = src.toString();

  const LAUNCHER = 84;
  const TRANSITION =
    "width .2s cubic-bezier(.4,0,.2,1),height .2s cubic-bezier(.4,0,.2,1)";
  const st = frame.style;
  st.cssText =
    "position:fixed;border:0;background:transparent;color-scheme:normal;" +
    "z-index:2147483000;transition:" + TRANSITION;

  // Position. The iframe cannot see the host viewport — its own `innerWidth` IS the
  // widget box — so the loader owns the anchor and every clamp, and pushes the
  // resolved value back down for the iframe to render and remember. Off unless the
  // operator opted in, which keeps the geometry config-driven, not frame-driven.
  const movable = theme.draggable === true;
  let anchor: Anchor = dockedAnchor(side, theme);
  let pushed = "";

  const viewport = (): Box => ({ w: window.innerWidth, h: window.innerHeight });

  // The pixel box for a reported state, or null in fullscreen (which ignores the
  // anchor and covers the viewport).
  function boxFor(s: { state: string; width?: number; height?: number }): Box | null {
    if (s.state === "fullscreen") return null;
    const vp = viewport();
    if (s.state !== "open") {
      // Normally the launcher's own box. An `expand` launcher asks for more while it
      // is hovered so its pill has room to grow — never less than the launcher, and
      // never wider than the viewport.
      return {
        w: Math.min(Math.max(s.width ?? LAUNCHER, LAUNCHER), vp.w),
        h: Math.min(Math.max(s.height ?? LAUNCHER, LAUNCHER), vp.h),
      };
    }
    return {
      w: Math.min(s.width ?? 400, vp.w),
      h: Math.min(s.height ?? 660, vp.h),
    };
  }

  // The iframe reports the state it wants; the loader picks the actual pixel size
  // and position. Re-applied on host resize.
  let lastState: { state: string; width?: number; height?: number } = {
    state: "closed",
  };
  function applyState(s: { state: string; width?: number; height?: number }) {
    // Animate open/close/fullscreen transitions. The one case that must NOT ease is a
    // same-state resize while OPEN — that is a live drag of the resize grip, and easing
    // lags the cursor. A same-state resize while closed is the expand launcher's hover,
    // which has to ease in step with the pill growing inside it.
    const dragging = s.state === lastState.state && s.state === "open";
    st.transition = dragging ? "none" : TRANSITION;
    lastState = s;
    const box = boxFor(s);
    if (box) anchor = clampAnchor(anchor, box, viewport());
    const ins = box
      ? anchorInsets(anchor)
      : { top: "0", right: "0", bottom: "0", left: "0" };
    st.top = ins.top;
    st.right = ins.right;
    st.bottom = ins.bottom;
    st.left = ins.left;
    st.width = box ? box.w + "px" : "100%";
    st.height = box ? box.h + "px" : "100%";
    pushAnchor();
  }
  function setAnchor(next: Anchor) {
    anchor = next;
    applyState(lastState);
  }
  // Tell the iframe where it actually ended up, so its launcher, resize grip and
  // stored position follow the widget rather than the configured side.
  function pushAnchor() {
    if (!movable || !frame.contentWindow) return;
    const key = anchor.corner + ":" + anchor.dx + ":" + anchor.dy;
    if (key === pushed) return;
    pushed = key;
    toFrame({ type: "anchor", anchor });
  }
  applyState(lastState);
  const onResize = () => applyState(lastState);
  window.addEventListener("resize", onResize);

  function mount() {
    document.body.appendChild(frame);
  }
  if (document.body) mount();
  else window.addEventListener("DOMContentLoaded", mount);

  // Tear down everything this loader added to the host page, so the host can
  // remove the widget mid-session (e.g. its org toggle flips off). Removing the
  // <script> that ran us is the host's job; clearing the loaded flag lets a
  // fresh inject re-initialise cleanly.
  function destroy() {
    window.removeEventListener("message", onMessage);
    window.removeEventListener("resize", onResize);
    window.removeEventListener("DOMContentLoaded", mount);
    window.removeEventListener("DOMContentLoaded", onDomReady);
    modeObserver?.disconnect();
    darkMql?.removeEventListener?.("change", scheduleResolve);
    if (scheduled) cancelAnimationFrame(scheduled);
    frame.remove();
    window.__WF_EDGE_LOADED__ = false;
  }

  function toFrame(msg: object) {
    frame.contentWindow?.postMessage({ proto: PROTOCOL, nonce, ...msg }, base);
  }

  // Ask the host for the customer token (existing contract). The host page's
  // own integration answers with { type: "wf-embed-token", token }.
  function requestTokenFromHost() {
    window.parent.postMessage({ type: "wf-embed-token-request", agent }, "*");
  }

  // Surface a widget usage event to the host page, which may forward it to its
  // own analytics. The name is validated against the WidgetEvent union before
  // this is called, and carries nothing else — safe on the broadcast channel.
  function forwardEventToHost(event: WidgetEvent) {
    window.parent.postMessage({ type: "wf-embed-event", agent, event }, "*");
  }

  // Messages FROM the sandboxed iframe (must come from the edge origin).
  function onMessage(e: MessageEvent) {
    if (e.origin === base && isFrameBound(nonce, e.data)) {
      const m = e.data;
      if (m.type === "ready") {
        toFrame({ type: "init", token, theme, themeDark, welcome, features, mode: currentMode });
        if (!token) requestTokenFromHost();
      } else if (m.type === "token-request") {
        requestTokenFromHost();
      } else if (m.type === "resize") {
        applyState({ state: m.state, width: m.width, height: m.height });
      } else if (m.type === "anchor" && movable) {
        const a = parseAnchor(m.anchor);
        if (a) setAnchor(a);
      } else if (m.type === "move" && movable) {
        const box = boxFor(lastState);
        if (box) setAnchor(translateAnchor(anchor, box, viewport(), m.dx, m.dy));
      } else if (m.type === "event") {
        const ev = asWidgetEvent(m.event);
        if (ev) forwardEventToHost(ev);
      }
      return;
    }
    // Everything below is delivered by the host page — accept only from the
    // configured origins.
    if (origins.indexOf(e.origin) === -1 || !e.data) return;
    // Token delivered by the host page.
    if (e.data.type === "wf-embed-token" && typeof e.data.token === "string") {
      token = e.data.token;
      toFrame({ type: "token", token });
      return;
    }
    // Screen context delivered by the host page — relayed verbatim to the iframe
    // on the SAME origin-validated channel as the token (never a new one). It is
    // UNTRUSTED: the edge caps and fences it, so the loader only checks the origin
    // allowlist and that a context object is present.
    if (
      e.data.type === "wf-embed-context" &&
      e.data.context &&
      typeof e.data.context === "object" &&
      !Array.isArray(e.data.context)
    ) {
      toFrame({ type: "context", context: e.data.context });
      return;
    }
    // Explicit theme from the host page (opt-in): the host's own light/dark toggle
    // posts this so `auto` follows it exactly. Sticky and wins over detection; a
    // bad value is ignored. A no-op when the operator pinned light/dark.
    if (e.data.type === "wf-embed-theme") {
      const m = asColorMode(e.data.mode);
      if (m) {
        explicit = m;
        scheduleResolve();
      }
      return;
    }
    if (e.data.type === "wf-embed-destroy") {
      destroy();
    }
  }
  window.addEventListener("message", onMessage);

  // The host may have unmounted before this loader finished loading, in which
  // case its destroy message was posted with no listener to receive it. Honour
  // the kill flag it sets synchronously so a late loader self-destructs instead
  // of orphaning an iframe that no remount could recover.
  if (window.__WF_EDGE_KILL__) {
    destroy();
    return;
  }

  requestTokenFromHost();
})();
