// @vitest-environment jsdom
//
// The brand-customisation layer: the gradient paint token, the `expand` launcher,
// and the agent avatar.
//
// The avatar test is the important one. `styles.css` gives chart/table/map messages a
// definite width through `.wf-row-assistant > .wf-bubble` and `> .wf-msg` — DIRECT
// child selectors. Rendering the avatar as a wrapper instead of a sibling would break
// them silently: no error, no exception, just every chart and table collapsed to text
// width. Nothing else in the suite would catch it, so the structure is asserted here.
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";

import { App, type AppProps } from "./App";
import { applyTheme } from "./theme";
import { dockedAnchor } from "../anchor";
import { NO_FEATURES, type ThemeTokens } from "../protocol";

/** Enough of a socket for the thread to mount; it never needs to carry traffic here. */
class IdleWebSocket {
  static OPEN = 1;
  readyState = 1;
  onmessage: ((e: { data: string }) => void) | null = null;
  onerror: (() => void) | null = null;
  onclose: (() => void) | null = null;
  constructor(
    public url: string,
    public protocols?: string[],
  ) {}
  send() {}
  close() {
    this.readyState = 3;
    this.onclose?.();
  }
}

beforeAll(() => {
  Element.prototype.scrollTo = vi.fn();
  // jsdom ships neither of these; the thread observes its own scroll container.
  window.ResizeObserver = class {
    observe() {}
    unobserve() {}
    disconnect() {}
  } as unknown as typeof ResizeObserver;
  window.WebSocket = IdleWebSocket as unknown as typeof WebSocket;
  // The thread lists conversations on mount; keep it off the network.
  window.fetch = vi.fn(async () => new Response("[]", { status: 200 })) as typeof fetch;
});

const GRADIENT = "linear-gradient(to right, #054D5C 0%, #828AFA 50%, #FBAEC3 100%)";

function tokens(over: Partial<ThemeTokens> = {}): ThemeTokens {
  return {
    title: "Harbor Intelligence",
    subtitle: "",
    logo_url: "",
    launcher: "bubble",
    primary: "#5b72bc",
    primary_text: "#ffffff",
    user_bubble: "#26425a",
    user_bubble_text: "#ffffff",
    agent_bubble: "#f9f9f9",
    agent_bubble_text: "#272727",
    accent: "#5b72bc",
    surface: "#f7f9fc",
    panel: "#ffffff",
    border: "#e6eaf0",
    muted: "#64748b",
    text: "#0f172a",
    font_family: "system-ui",
    font_url: "",
    radius: "18px",
    position: "right",
    shadow: true,
    fullscreen: "centered",
    draggable: false,
    ...over,
  };
}

function renderApp(over: Partial<ThemeTokens> = {}, welcome = "Hi — I'm Harbor Assist.") {
  const resize = vi.fn();
  const props: AppProps = {
    base: "https://edge.example",
    agent: "assistant",
    // A token is needed for the panel to get past "Connecting…" and mount the thread.
    token: "test-token",
    theme: tokens(over),
    welcome,
    features: NO_FEATURES,
    resize,
    anchor: dockedAnchor("right"),
    move: vi.fn(),
    reanchor: vi.fn(),
    emit: vi.fn(),
    context: null,
  };
  return { resize, ...render(<App {...props} />) };
}

/** Open the panel so the thread (and its agent rows) render. */
function open() {
  fireEvent.click(screen.getByLabelText("Open Harbor Intelligence"));
}

const OPT_IN_CLASSES = ["wf-grad", "wf-frame", "wf-send-circle", "wf-logo-contain"];

afterEach(() => {
  cleanup();
  document.documentElement.classList.remove(...OPT_IN_CLASSES);
  document.documentElement.removeAttribute("style");
  // The widget remembers the panel size per agent for the session; without this a
  // test that opened the panel pins the size every later test sees.
  sessionStorage.clear();
});

/** The most recent call's args. (`Array.prototype.at` is outside this tsconfig lib.) */
function lastCall(m: { mock: { calls: unknown[][] } }): unknown[] | undefined {
  return m.mock.calls[m.mock.calls.length - 1];
}

describe("gradient paint token", () => {
  it("sets the paint and gates the panel ring only when a gradient is configured", () => {
    applyTheme(tokens({ gradient: GRADIENT }));
    const root = document.documentElement;
    expect(root.style.getPropertyValue("--wf-brand-gradient")).toBe(GRADIENT);
    expect(root.classList.contains("wf-grad")).toBe(true);
    // The flat primary must survive alongside it: the stylesheet feeds it to 17
    // color-mix() calls and accent-color, none of which accept a gradient.
    expect(root.style.getPropertyValue("--wf-primary")).toBe("#5b72bc");
  });

  it("falls back to the flat primary and drops the ring gate when unset", () => {
    applyTheme(tokens());
    expect(document.documentElement.style.getPropertyValue("--wf-brand-gradient")).toBe(
      "var(--wf-primary)",
    );
    expect(document.documentElement.classList.contains("wf-grad")).toBe(false);
  });

  it("clears the gate when a live theme change removes the gradient", () => {
    applyTheme(tokens({ gradient: GRADIENT }));
    applyTheme(tokens({ gradient: "" }));
    expect(document.documentElement.classList.contains("wf-grad")).toBe(false);
    expect(document.documentElement.style.getPropertyValue("--wf-brand-gradient")).toBe(
      "var(--wf-primary)",
    );
  });

  it("paints rather than nothing when an older server ships no gradient field", () => {
    // `setProperty(undefined)` is a no-op, which would leave var(--wf-brand-gradient)
    // unresolved and the launcher transparent.
    const stale = tokens();
    delete (stale as Partial<ThemeTokens>).gradient;
    applyTheme(stale);
    expect(document.documentElement.style.getPropertyValue("--wf-brand-gradient")).toBe(
      "var(--wf-primary)",
    );
    expect(document.documentElement.classList.contains("wf-grad")).toBe(false);
  });
});

describe("presentation opt-ins are per-agent", () => {
  // The rule these pin: different clients want different treatments, so no visual
  // change may ride on merely having configured a gradient. Each is its own switch,
  // and a client that sets none renders exactly as it did before they existed.
  it("does NOT restructure the panel just because a gradient is set", () => {
    applyTheme(tokens({ gradient: GRADIENT }));
    const cl = document.documentElement.classList;
    expect(cl.contains("wf-grad")).toBe(true);
    expect(cl.contains("wf-frame")).toBe(false);
    expect(cl.contains("wf-send-circle")).toBe(false);
    expect(cl.contains("wf-logo-contain")).toBe(false);
  });

  it("sets nothing at all for a client that opts into none of it", () => {
    applyTheme(tokens());
    for (const c of OPT_IN_CLASSES) {
      expect(document.documentElement.classList.contains(c)).toBe(false);
    }
  });

  it("switches each one on independently", () => {
    applyTheme(tokens({ frame: true }));
    expect(document.documentElement.classList.contains("wf-frame")).toBe(true);
    // …without dragging the others along, and with no gradient configured either.
    expect(document.documentElement.classList.contains("wf-grad")).toBe(false);
    expect(document.documentElement.classList.contains("wf-send-circle")).toBe(false);

    applyTheme(tokens({ send_shape: "circle", logo_fit: "contain" }));
    const cl = document.documentElement.classList;
    expect(cl.contains("wf-send-circle")).toBe(true);
    expect(cl.contains("wf-logo-contain")).toBe(true);
    expect(cl.contains("wf-frame")).toBe(false);
  });

  it("clears them again when a client turns them back off", () => {
    applyTheme(tokens({ frame: true, send_shape: "circle", logo_fit: "contain" }));
    applyTheme(tokens());
    for (const c of OPT_IN_CLASSES) {
      expect(document.documentElement.classList.contains(c)).toBe(false);
    }
  });

  it("keeps the original arrow glyph unless a client asks for the plane", () => {
    const plain = renderApp();
    open();
    const arrow = plain.container.querySelector(".wf-send path")!;
    // The dart is a straight-edged polygon: no curve commands, and no stroke.
    expect(/[CQ]/.test(arrow.getAttribute("d")!)).toBe(false);
    expect(arrow.getAttribute("stroke")).toBeNull();
    cleanup();

    const branded = renderApp({ send_icon: "plane" });
    open();
    // The plane is the swept shape — curved back, and a hairline stroke that rounds
    // its joins so it doesn't read as a triangle at ~20px.
    const plane = branded.container.querySelector(".wf-send path")!;
    expect(/[CQ]/.test(plane.getAttribute("d")!)).toBe(true);
    expect(plane.getAttribute("stroke")).toBe("currentColor");
  });
});

describe("per-surface paint", () => {
  const v = (n: string) => document.documentElement.style.getPropertyValue(n);

  it("inherits the brand paint, and its zoom, when unset", () => {
    applyTheme(tokens({ gradient: GRADIENT }));
    for (const s of ["avatar", "send"]) {
      expect(v(`--wf-${s}-bg`)).toBe("var(--wf-brand-gradient)");
      expect(v(`--wf-${s}-zoom`)).toBe("var(--wf-brand-zoom-size)");
    }
  });

  it("takes its own paint independently, and drops the zoom when it does", () => {
    // The zoom exists to keep an INHERITED brand gradient legible on a small control;
    // a paint chosen for this control should show in full instead.
    applyTheme(tokens({ gradient: GRADIENT, avatar_bg: "#5b72bc" }));
    expect(v("--wf-avatar-bg")).toBe("#5b72bc");
    expect(v("--wf-avatar-zoom")).toBe("100% 100%");
    // …while the send button still inherits.
    expect(v("--wf-send-bg")).toBe("var(--wf-brand-gradient)");
    expect(v("--wf-send-zoom")).toBe("var(--wf-brand-zoom-size)");
  });

  it("accepts a whole gradient per surface", () => {
    const own = "linear-gradient(to right, #111111 0%, #222222 100%)";
    applyTheme(tokens({ gradient: GRADIENT, send_bg: own }));
    expect(v("--wf-send-bg")).toBe(own);
    expect(v("--wf-send-zoom")).toBe("100% 100%");
  });
});

describe("split borders", () => {
  const v = (n: string) => document.documentElement.style.getPropertyValue(n);

  it("default to the shared border", () => {
    applyTheme(tokens({ border: "#e6eaf0" }));
    expect(v("--wf-input-border")).toBe("#e6eaf0");
    expect(v("--wf-agent-border")).toBe("#e6eaf0");
  });

  it("split off it independently", () => {
    applyTheme(
      tokens({ border: "#e6eaf0", input_border: "#c7d2fe", agent_bubble_border: "#d1d5db" }),
    );
    expect(v("--wf-input-border")).toBe("#c7d2fe");
    expect(v("--wf-agent-border")).toBe("#d1d5db");
    expect(v("--wf-border")).toBe("#e6eaf0");
  });

  it("still paint when a stale server ships neither", () => {
    const stale = tokens({ border: "#abcdef" });
    delete (stale as Partial<ThemeTokens>).input_border;
    delete (stale as Partial<ThemeTokens>).agent_bubble_border;
    applyTheme(stale);
    expect(v("--wf-input-border")).toBe("#abcdef");
    expect(v("--wf-agent-border")).toBe("#abcdef");
  });
});

describe("composer background", () => {
  const inputBg = () => document.documentElement.style.getPropertyValue("--wf-input-bg");

  it("falls back to the panel when unset, so it is a no-op", () => {
    applyTheme(tokens({ panel: "#fafbfc" }));
    expect(inputBg()).toBe("#fafbfc");
  });

  it("uses the configured colour when set", () => {
    applyTheme(tokens({ panel: "#ffffff", input_bg: "#f1f5f9" }));
    expect(inputBg()).toBe("#f1f5f9");
    // …without disturbing the panel it used to share.
    expect(document.documentElement.style.getPropertyValue("--wf-panel")).toBe("#ffffff");
  });

  it("still paints when a stale server ships no field", () => {
    const stale = tokens({ panel: "#eeeeee" });
    delete (stale as Partial<ThemeTokens>).input_bg;
    applyTheme(stale);
    expect(inputBg()).toBe("#eeeeee");
  });
});

describe("panel sizing", () => {
  const openSize = (resize: ReturnType<typeof vi.fn>) => {
    const call = [...resize.mock.calls].reverse().find((c) => c[0] === "open");
    return call ? { w: call[1], h: call[2] } : null;
  };

  it("opens at the configured box", () => {
    const { resize } = renderApp({ width: 520, height: 700 });
    open();
    expect(openSize(resize)).toEqual({ w: 520, h: 700 });
  });

  it("falls back to the built-in box when a stale server ships no sizing", () => {
    const { resize } = renderApp();
    open();
    expect(openSize(resize)).toEqual({ w: 400, h: 660 });
  });

  it("clamps a stored session size back into the configured range", () => {
    // A size the customer dragged to before the operator lowered the ceiling — or
    // raised the floor — must not survive as-is.
    sessionStorage.setItem("wf-size:assistant", JSON.stringify({ w: 900, h: 200 }));
    const { resize } = renderApp({ width: 420, height: 640, min_width: 380, min_height: 500 });
    open();
    const s = openSize(resize)!;
    expect(s.w).toBe(900); // above the opening size is fine — the customer grew it
    expect(s.h).toBe(500); // below the floor is not
    sessionStorage.clear();
  });

  it("never lets the drag floor exceed the opening size, even if the server slips", () => {
    // The server guarantees min <= open, but a partial/stale payload must not be
    // able to pin the panel larger than it opens.
    const { resize } = renderApp({ width: 360, height: 400, min_width: 900, min_height: 900 });
    open();
    expect(openSize(resize)).toEqual({ w: 360, h: 400 });
  });
});

describe("composer sizing", () => {
  // The panel mounts while the iframe is still launcher-narrow. `scrollHeight` on an
  // EMPTY textarea reflects the wrapped placeholder, so measuring there returned 295px
  // for an empty box — and because the height only recomputed on input, the composer
  // opened several rows tall and snapped to one at the first keystroke.
  it("never carries a measured height while empty", () => {
    const { container } = renderApp();
    open();
    const ta = container.querySelector<HTMLTextAreaElement>(".wf-input")!;
    // Stand in for a bogus height measured mid-resize.
    ta.style.height = "295px";
    fireEvent.change(ta, { target: { value: "typed" } });
    fireEvent.change(ta, { target: { value: "" } });
    expect(ta.style.height).toBe("");
  });

  it("measures once there is something to measure", () => {
    const { container } = renderApp();
    open();
    const ta = container.querySelector<HTMLTextAreaElement>(".wf-input")!;
    fireEvent.change(ta, { target: { value: "typed" } });
    // jsdom reports 0 for scrollHeight, so the value is not meaningful — that it is
    // SET at all is: the empty-guard must not swallow the non-empty case.
    expect(ta.style.height.endsWith("px")).toBe(true);
  });
});

describe("framed panel structure", () => {
  // The frame CSS insets and rounds `.wf-thread` and relies on its `overflow: hidden`
  // to clip the composer's corners. That only works because the composer is a CHILD
  // of the thread. Treating them as siblings — which they look like in the CSS — put
  // the inset on both (double-inset horizontally) and left the thread running to the
  // panel's bottom edge, swallowing the bottom of the frame. Pin the nesting.
  it("keeps the composer inside the thread, which the frame CSS depends on", () => {
    const { container } = renderApp({ frame: true });
    open();
    const thread = container.querySelector(".wf-thread")!;
    const composer = container.querySelector(".wf-composer")!;
    expect(thread).toBeTruthy();
    expect(composer).toBeTruthy();
    expect(thread.contains(composer)).toBe(true);
    // …and the thread is a direct child of the panel, which is what carries the inset.
    expect(thread.parentElement?.classList.contains("wf-panel")).toBe(true);
  });
});

describe("expand launcher", () => {
  it("carries the title as a label the CSS can reveal, without a bubble launcher doing so", () => {
    const { container, unmount } = renderApp({ launcher: "expand" });
    const launcher = container.querySelector(".wf-launch")!;
    expect(launcher.classList.contains("wf-launch-expand")).toBe(true);
    expect(launcher.querySelector(".wf-launch-label")?.textContent).toBe(
      "Harbor Intelligence",
    );
    // The button already names itself for assistive tech; the visual label must not
    // announce the title a second time.
    expect(launcher.querySelector(".wf-launch-label")?.getAttribute("aria-hidden")).toBe("true");
    expect(screen.getByLabelText("Open Harbor Intelligence")).toBeTruthy();
    unmount();

    const bubble = renderApp().container.querySelector(".wf-launch")!;
    expect(bubble.classList.contains("wf-launch-bubble")).toBe(true);
    expect(bubble.querySelector(".wf-launch-label")).toBeNull();
  });
});

describe("expand launcher frame sizing", () => {
  // The launcher is drawn INSIDE the iframe and the loader sizes that frame to the
  // launcher, so an expanding pill is clipped at the frame edge unless the frame is
  // asked to grow. Caught by eye on a real render; pinned here.
  it("asks for a wider frame while hovered and gives it back on leave", () => {
    const { resize, container } = renderApp({ launcher: "expand" });
    const launcher = container.querySelector(".wf-launch")!;
    resize.mockClear();

    fireEvent.mouseEnter(launcher);
    const widened = lastCall(resize)!;
    expect(widened[0]).toBe("closed");
    // jsdom lays nothing out, so the measured pill is 0 and the floor applies. The
    // contract under test is that a closed resize carries an explicit width at all —
    // before the fix it passed none and the loader pinned the frame to the launcher.
    expect(widened[1]).toBeGreaterThanOrEqual(84);

    resize.mockClear();
    fireEvent.mouseLeave(launcher);
    expect(lastCall(resize)).toEqual(["closed", 84, 84]);
  });

  it("widens on keyboard focus too, since the label reveals on focus-visible", () => {
    const { resize, container } = renderApp({ launcher: "expand" });
    const launcher = container.querySelector(".wf-launch")!;
    resize.mockClear();
    fireEvent.focus(launcher);
    expect(lastCall(resize)?.[0]).toBe("closed");
    resize.mockClear();
    fireEvent.blur(launcher);
    expect(lastCall(resize)).toEqual(["closed", 84, 84]);
  });

  it("leaves a bubble launcher at the launcher-sized frame", () => {
    const { resize, container } = renderApp();
    const launcher = container.querySelector(".wf-launch")!;
    resize.mockClear();
    fireEvent.mouseEnter(launcher);
    // No pill to make room for — the frame must not grow and start eating host clicks.
    for (const call of resize.mock.calls) expect(call).toEqual(["closed", 84, 84]);
  });
});

describe("agent avatar", () => {
  it("renders as a SIBLING of the bubble, keeping the direct-child width selectors", () => {
    const { container } = renderApp({ avatar: true });
    open();
    const row = container.querySelector(".wf-row-assistant")!;
    expect(row.querySelector(".wf-avatar")).toBeTruthy();
    // The contract the rich-output CSS depends on: still a DIRECT child of the row.
    expect(row.querySelector(":scope > .wf-bubble, :scope > .wf-msg")).toBeTruthy();
    // And the avatar is a sibling of it, not an ancestor.
    const avatar = row.querySelector(".wf-avatar")!;
    expect(avatar.parentElement).toBe(row);
    expect(avatar.querySelector(".wf-bubble")).toBeNull();
  });

  it("is omitted entirely when unset, and never decorates a customer row", () => {
    const { container } = renderApp();
    open();
    expect(container.querySelector(".wf-avatar")).toBeNull();
    expect(container.querySelector(":scope .wf-row-assistant > .wf-bubble")).toBeTruthy();
  });

  it("uses the brand mark when one is configured and an icon otherwise", () => {
    const { container, unmount } = renderApp({
      avatar: true,
      logo_url: "https://cdn.example.com/mark.svg",
    });
    open();
    expect(container.querySelector(".wf-avatar img")?.getAttribute("src")).toBe(
      "https://cdn.example.com/mark.svg",
    );
    unmount();

    const plain = renderApp({ avatar: true });
    open();
    expect(plain.container.querySelector(".wf-avatar svg")).toBeTruthy();
  });
});
