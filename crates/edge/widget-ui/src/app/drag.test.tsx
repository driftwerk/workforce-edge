// @vitest-environment jsdom
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";

import { App, type AppProps } from "./App";
import { dockedAnchor, type Anchor } from "../anchor";
import { NO_FEATURES, type ThemeTokens } from "../protocol";
import { theme } from "./testTheme";

// jsdom ships no PointerEvent and no pointer capture. MouseEvent already carries
// the screen coordinates and `buttons` the drag reads, so it only needs the missing
// `pointerId` bolted on for Testing Library to construct real pointer events.
class TestPointerEvent extends MouseEvent {
  readonly pointerId: number;
  readonly pointerType: string;
  readonly isPrimary: boolean;
  constructor(type: string, init: PointerEventInit = {}) {
    super(type, init);
    this.pointerId = init.pointerId ?? 0;
    this.pointerType = init.pointerType ?? "mouse";
    this.isPrimary = init.isPrimary ?? true;
  }
}

beforeAll(() => {
  window.PointerEvent = TestPointerEvent as unknown as typeof PointerEvent;
  Element.prototype.setPointerCapture = vi.fn();
  Element.prototype.releasePointerCapture = vi.fn();
  Element.prototype.hasPointerCapture = () => false;
  Element.prototype.scrollTo = vi.fn();
});

function renderApp(
  draggable: boolean,
  anchor: Anchor = dockedAnchor("right"),
  themeExtra: Partial<ThemeTokens> = {},
) {
  const move = vi.fn();
  const reanchor = vi.fn();
  const emit = vi.fn();
  const props: AppProps = {
    base: "https://edge.example",
    agent: "assistant",
    token: null,
    theme: { ...theme(draggable), ...themeExtra },
    welcome: "",
    features: NO_FEATURES,
    resize: vi.fn(),
    anchor,
    move,
    reanchor,
    emit,
    context: null,
  };
  const utils = render(<App {...props} />);
  const launcher = () => screen.getByLabelText("Open Helper");
  return { move, reanchor, emit, launcher, ...utils };
}

/** One press-drag-release in screen coordinates. */
function drag(el: Element, from: [number, number], ...points: [number, number][]) {
  fireEvent.pointerDown(el, { pointerId: 1, buttons: 1, screenX: from[0], screenY: from[1] });
  for (const [screenX, screenY] of points) {
    fireEvent.pointerMove(el, { pointerId: 1, buttons: 1, screenX, screenY });
  }
  const last = points.length ? points[points.length - 1] : from;
  fireEvent.pointerUp(el, { pointerId: 1, buttons: 0, screenX: last[0], screenY: last[1] });
  fireEvent.click(el, { detail: 1 });
}

afterEach(cleanup);

describe("dragging the launcher", () => {
  it("opens the widget when the press never becomes a drag", () => {
    const { move, launcher } = renderApp(true);
    drag(launcher(), [200, 200], [202, 201]);
    expect(move).not.toHaveBeenCalled();
    expect(screen.getByLabelText("Close")).toBeTruthy();
  });

  it("moves the widget and swallows the click once armed", () => {
    const { move, launcher } = renderApp(true);
    drag(launcher(), [200, 200], [180, 190], [160, 180]);
    expect(move.mock.calls).toEqual([
      [-20, -10],
      [-20, -10],
    ]);
    expect(screen.queryByLabelText("Close")).toBeNull();
  });

  it("stays put and still opens when the agent is not draggable", () => {
    const { move, launcher } = renderApp(false);
    drag(launcher(), [200, 200], [100, 100]);
    expect(move).not.toHaveBeenCalled();
    expect(screen.getByLabelText("Close")).toBeTruthy();
  });

  it("ends a drag whose release this iframe never sees", () => {
    const { move, launcher } = renderApp(true);
    const el = launcher();
    fireEvent.pointerDown(el, { pointerId: 1, buttons: 1, screenX: 200, screenY: 200 });
    fireEvent.pointerMove(el, { pointerId: 1, buttons: 1, screenX: 150, screenY: 200 });
    expect(move).toHaveBeenCalledTimes(1);
    // Released outside the iframe: the next move arrives with no button held.
    fireEvent.pointerMove(el, { pointerId: 1, buttons: 0, screenX: 100, screenY: 200 });
    fireEvent.pointerMove(el, { pointerId: 1, buttons: 0, screenX: 50, screenY: 200 });
    expect(move).toHaveBeenCalledTimes(1);
  });

  it("still opens from the keyboard after a mouse drag", () => {
    const { launcher } = renderApp(true);
    drag(launcher(), [200, 200], [140, 160]);
    expect(screen.queryByLabelText("Close")).toBeNull();
    // Enter/Space reach the button as a bare click, with no pointerdown to clear the
    // drag flag — it must not be left holding the widget shut.
    fireEvent.click(launcher());
    expect(screen.getByLabelText("Close")).toBeTruthy();
  });

  it("ignores a second finger rather than re-originating the drag from it", () => {
    const { move, launcher } = renderApp(true);
    const el = launcher();
    fireEvent.pointerDown(el, { pointerId: 1, buttons: 1, pointerType: "touch", screenX: 200, screenY: 200 });
    // A second finger lands far away; without a pointer-identity guard it would
    // become the origin and the next 3px of finger one would teleport the widget.
    fireEvent.pointerDown(el, {
      pointerId: 2,
      buttons: 1,
      pointerType: "touch",
      isPrimary: false,
      screenX: 600,
      screenY: 500,
    });
    fireEvent.pointerMove(el, { pointerId: 2, buttons: 1, pointerType: "touch", screenX: 560, screenY: 470 });
    expect(move).not.toHaveBeenCalled();
    fireEvent.pointerMove(el, { pointerId: 1, buttons: 1, pointerType: "touch", screenX: 180, screenY: 200 });
    expect(move.mock.calls).toEqual([[-20, 0]]);
  });

  it("stays draggable after a press released outside the iframe without moving", () => {
    const { move, launcher } = renderApp(true);
    const el = launcher();
    // Pressed, then released off-widget having never travelled: no end event lands.
    fireEvent.pointerDown(el, { pointerId: 1, buttons: 1, screenX: 200, screenY: 200 });
    // The next press must still be able to take over.
    drag(el, [200, 200], [150, 170]);
    expect(move).toHaveBeenCalled();
  });

  it("does not move on a right-button drag", () => {
    const { move, launcher } = renderApp(true);
    const el = launcher();
    fireEvent.pointerDown(el, { pointerId: 1, button: 2, buttons: 2, screenX: 200, screenY: 200 });
    fireEvent.pointerMove(el, { pointerId: 1, buttons: 2, screenX: 140, screenY: 180 });
    expect(move).not.toHaveBeenCalled();
  });

  it("treats a shaky tap as a tap, not a drag", () => {
    const { move, launcher } = renderApp(true);
    const el = launcher();
    fireEvent.pointerDown(el, { pointerId: 1, buttons: 1, pointerType: "touch", screenX: 200, screenY: 200 });
    fireEvent.pointerMove(el, { pointerId: 1, buttons: 1, pointerType: "touch", screenX: 206, screenY: 207 });
    fireEvent.pointerUp(el, { pointerId: 1, buttons: 0, pointerType: "touch", screenX: 206, screenY: 207 });
    fireEvent.click(el, { detail: 1 });
    expect(move).not.toHaveBeenCalled();
    expect(screen.getByLabelText("Close")).toBeTruthy();
  });
});

describe("the moved usage event", () => {
  const movedCalls = (emit: ReturnType<typeof vi.fn>) =>
    emit.mock.calls.filter(([e]) => e === "moved").length;

  it("fires exactly once per completed drag gesture", () => {
    const { emit, launcher } = renderApp(true);
    drag(launcher(), [200, 200], [180, 190], [160, 180]);
    expect(movedCalls(emit)).toBe(1);
    drag(launcher(), [160, 180], [120, 150], [110, 140]);
    expect(movedCalls(emit)).toBe(2);
  });

  it("does not fire for a tap or on a non-draggable agent", () => {
    const tap = renderApp(true);
    drag(tap.launcher(), [200, 200], [202, 201]);
    expect(movedCalls(tap.emit)).toBe(0);
    cleanup();
    const fixed = renderApp(false);
    drag(fixed.launcher(), [200, 200], [100, 100]);
    expect(movedCalls(fixed.emit)).toBe(0);
  });

  it("fires once even when the release lands outside the iframe", () => {
    const { emit, launcher } = renderApp(true);
    const el = launcher();
    fireEvent.pointerDown(el, { pointerId: 1, buttons: 1, screenX: 200, screenY: 200 });
    fireEvent.pointerMove(el, { pointerId: 1, buttons: 1, screenX: 150, screenY: 200 });
    // Released off-widget: the drag learns of it from a button-less move.
    fireEvent.pointerMove(el, { pointerId: 1, buttons: 0, screenX: 100, screenY: 200 });
    fireEvent.pointerMove(el, { pointerId: 1, buttons: 0, screenX: 50, screenY: 200 });
    expect(movedCalls(emit)).toBe(1);
  });

  it("defers an armed press whose end was lost to the next press, still once", () => {
    const { emit, launcher } = renderApp(true);
    const el = launcher();
    // Arms, then no end event of any kind reaches the iframe.
    fireEvent.pointerDown(el, { pointerId: 1, buttons: 1, screenX: 200, screenY: 200 });
    fireEvent.pointerMove(el, { pointerId: 1, buttons: 1, screenX: 150, screenY: 200 });
    expect(movedCalls(emit)).toBe(0);
    // The next press drops the stale listeners — the lost gesture reports here.
    fireEvent.pointerDown(el, { pointerId: 2, buttons: 1, screenX: 150, screenY: 200 });
    expect(movedCalls(emit)).toBe(1);
    // And the new press is a tap: no second moved.
    fireEvent.pointerUp(el, { pointerId: 2, buttons: 0, screenX: 150, screenY: 200 });
    fireEvent.click(el, { detail: 1 });
    expect(movedCalls(emit)).toBe(1);
  });
});

describe("the open panel", () => {
  const openPanel = (draggable: boolean, anchor?: Anchor, themeExtra?: Partial<ThemeTokens>) => {
    const r = renderApp(draggable, anchor, themeExtra);
    fireEvent.click(r.launcher());
    return r;
  };

  it("drags by the header", () => {
    const { move, container } = openPanel(true);
    drag(container.querySelector("header")!, [400, 300], [380, 260]);
    expect(move.mock.calls).toEqual([[-20, -40]]);
  });

  it("offers no move control and no header drag when not draggable", () => {
    const { move, container } = openPanel(false);
    expect(screen.queryByLabelText(/^Move to/)).toBeNull();
    drag(container.querySelector("header")!, [400, 300], [300, 200]);
    expect(move).not.toHaveBeenCalled();
  });

  it("cycles the corner from the keyboard-reachable move control", () => {
    const { reanchor } = openPanel(true, { corner: "br", dx: 40, dy: 12 });
    fireEvent.click(screen.getByLabelText("Move to bottom left"));
    expect(reanchor).toHaveBeenCalledWith({ corner: "bl", dx: 0, dy: 0 });
  });

  it("re-docks the corner cycle at the configured inset, not the bare corner", () => {
    const { reanchor } = openPanel(true, { corner: "br", dx: 40, dy: 12 }, { offset_x: 12, offset_y: 72 });
    fireEvent.click(screen.getByLabelText("Move to bottom left"));
    expect(reanchor).toHaveBeenCalledWith({ corner: "bl", dx: 12, dy: 72 });
  });

  it("still fires a header button from the keyboard when a drag left no trailing click", () => {
    const { container } = openPanel(true);
    const header = container.querySelector("header")!;
    fireEvent.pointerDown(header, { pointerId: 1, buttons: 1, screenX: 400, screenY: 300 });
    fireEvent.pointerMove(header, { pointerId: 1, buttons: 1, screenX: 340, screenY: 260 });
    // Released off-frame, or an engine that never retargets: no click is dispatched,
    // so the drag flag is still set when the keyboard next reaches a button.
    fireEvent.pointerUp(header, { pointerId: 1, buttons: 0, screenX: 340, screenY: 260 });
    fireEvent.click(screen.getByLabelText("Close"), { detail: 0 });
    expect(screen.getByLabelText(/^Open /)).toBeTruthy();
  });

  it("keeps tracking a drag whose pointer has already left the header", () => {
    const { move, container } = openPanel(true);
    const header = container.querySelector("header")!;
    fireEvent.pointerDown(header, { pointerId: 1, buttons: 1, screenX: 400, screenY: 300 });
    // A brisk drag is off the ~56px header strip before its first move lands. Bound
    // to the element that press is never seen, so it never arms and never captures.
    fireEvent.pointerMove(document.body, { pointerId: 1, buttons: 1, screenX: 400, screenY: 220 });
    fireEvent.pointerMove(document.body, { pointerId: 1, buttons: 1, screenX: 390, screenY: 180 });
    expect(move.mock.calls).toEqual([
      [0, -80],
      [-10, -40],
    ]);
  });

  it("does not fire a header button on the click that ends a drag started over it", () => {
    const { move, container } = openPanel(true);
    const close = screen.getByLabelText("Close");
    fireEvent.pointerDown(close, { pointerId: 1, buttons: 1, screenX: 400, screenY: 300 });
    fireEvent.pointerMove(container.querySelector("header")!, {
      pointerId: 1,
      buttons: 1,
      screenX: 340,
      screenY: 260,
    });
    fireEvent.pointerUp(close, { pointerId: 1, buttons: 0, screenX: 340, screenY: 260 });
    fireEvent.click(close, { detail: 1 });
    expect(move).toHaveBeenCalled();
    expect(screen.getByLabelText("Close")).toBeTruthy();
  });

  it("hides the move control in fullscreen, where there is nothing to move", () => {
    openPanel(true);
    expect(screen.getByLabelText("Move to bottom left")).toBeTruthy();
    fireEvent.click(screen.getByLabelText("Expand"));
    expect(screen.queryByLabelText(/^Move to/)).toBeNull();
  });

  it("puts the resize grip on the corner opposite the anchor", () => {
    const { container } = openPanel(true, { corner: "tl", dx: 0, dy: 0 });
    expect(container.querySelector(".wf-root")!.className).toContain("wf-y-t wf-x-l");
  });
});
