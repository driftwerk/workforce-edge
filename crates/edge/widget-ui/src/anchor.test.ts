import { describe, expect, it } from "vitest";

import {
  anchorInsets,
  clampAnchor,
  dockAt,
  dockedAnchor,
  dragOrigin,
  dragStep,
  nextCorner,
  parseAnchor,
  translateAnchor,
  type Anchor,
  type Corner,
} from "./anchor";

const VIEWPORT = { w: 1000, h: 800 };
const LAUNCHER = { w: 84, h: 84 };
const PANEL = { w: 400, h: 660 };

/** Absolute top-left the anchor resolves to — the invariant every move preserves. */
const topLeft = (a: Anchor, box: { w: number; h: number }, vp = VIEWPORT) => ({
  x: a.corner[1] === "l" ? a.dx : vp.w - box.w - a.dx,
  y: a.corner[0] === "t" ? a.dy : vp.h - box.h - a.dy,
});

describe("dockedAnchor", () => {
  it("starts at the bottom of the configured side", () => {
    expect(dockedAnchor("right")).toEqual({ corner: "br", dx: 0, dy: 0 });
    expect(dockedAnchor("left")).toEqual({ corner: "bl", dx: 0, dy: 0 });
    expect(dockedAnchor("nonsense")).toEqual({ corner: "br", dx: 0, dy: 0 });
  });

  it("starts inset by the configured offset", () => {
    expect(dockedAnchor("right", { offset_x: 12, offset_y: 72 })).toEqual({ corner: "br", dx: 12, dy: 72 });
    expect(dockedAnchor("left", { offset_y: 72 })).toEqual({ corner: "bl", dx: 0, dy: 72 });
  });

  it("treats a missing or junk offset as flush to the corner", () => {
    expect(dockedAnchor("right", {})).toEqual({ corner: "br", dx: 0, dy: 0 });
    expect(dockedAnchor("right", { offset_x: -20, offset_y: NaN })).toEqual({ corner: "br", dx: 0, dy: 0 });
    expect(dockedAnchor("right", { offset_x: "12", offset_y: Infinity })).toEqual({
      corner: "br",
      dx: 0,
      dy: 0,
    });
  });
});

describe("dockAt", () => {
  it("docks to any corner at the same inset, so the corner cycle keeps it", () => {
    expect(dockAt("tl", { offset_x: 12, offset_y: 72 })).toEqual({ corner: "tl", dx: 12, dy: 72 });
    expect(dockAt("bl")).toEqual({ corner: "bl", dx: 0, dy: 0 });
  });
});

describe("clampAnchor", () => {
  it("keeps the whole widget on screen", () => {
    const off = { corner: "tl", dx: 5000, dy: 5000 } as const;
    expect(clampAnchor(off, PANEL, VIEWPORT)).toEqual({ corner: "tl", dx: 600, dy: 140 });
  });

  it("pins a box larger than the viewport to its corner", () => {
    const a = { corner: "br", dx: 30, dy: 30 } as const;
    expect(clampAnchor(a, { w: 1200, h: 900 }, VIEWPORT)).toEqual({ corner: "br", dx: 0, dy: 0 });
  });

  it("returns the input untouched when it already fits", () => {
    const a: Anchor = { corner: "br", dx: 12, dy: 20 };
    expect(clampAnchor(a, PANEL, VIEWPORT)).toBe(a);
  });
});

describe("translateAnchor", () => {
  it("moves the widget by the drag delta", () => {
    const start: Anchor = { corner: "br", dx: 0, dy: 0 };
    const moved = translateAnchor(start, LAUNCHER, VIEWPORT, -100, -50);
    expect(topLeft(moved, LAUNCHER)).toEqual({ x: 816, y: 666 });
  });

  it("re-anchors to whichever corner the box centre is nearest", () => {
    // Bottom-right launcher dragged up and left, past both midpoints.
    const moved = translateAnchor({ corner: "br", dx: 0, dy: 0 }, LAUNCHER, VIEWPORT, -600, -500);
    expect(moved.corner).toBe("tl");
    // The absolute position is unchanged by the re-anchor, so a mid-drag corner
    // flip is continuous rather than a jump.
    expect(topLeft(moved, LAUNCHER)).toEqual({ x: 316, y: 216 });
  });

  it("re-anchors exactly at the viewport midpoint", () => {
    const vp = { w: 1000, h: 800 };
    const box = { w: 100, h: 100 };
    // Centre at x=499 (left of the midline) then x=501 (right of it).
    expect(translateAnchor({ corner: "tl", dx: 449, dy: 0 }, box, vp, 0, 0).corner).toBe("tl");
    expect(translateAnchor({ corner: "tl", dx: 451, dy: 0 }, box, vp, 0, 0).corner).toBe("tr");
  });

  it("cannot bank an offset by dragging past an edge", () => {
    const start: Anchor = { corner: "br", dx: 0, dy: 0 };
    // Shove it far past the bottom-right, then nudge back by one pixel.
    const shoved = translateAnchor(start, LAUNCHER, VIEWPORT, 9000, 9000);
    expect(shoved).toEqual({ corner: "br", dx: 0, dy: 0 });
    expect(translateAnchor(shoved, LAUNCHER, VIEWPORT, -1, -1)).toEqual({
      corner: "br",
      dx: 1,
      dy: 1,
    });
  });

  it("round-trips a delta and its inverse", () => {
    const start: Anchor = { corner: "br", dx: 100, dy: 100 };
    const there = translateAnchor(start, LAUNCHER, VIEWPORT, -60, -40);
    expect(translateAnchor(there, LAUNCHER, VIEWPORT, 60, 40)).toEqual(start);
  });

  it("treats a non-finite delta as no movement", () => {
    const start: Anchor = { corner: "br", dx: 40, dy: 40 };
    expect(translateAnchor(start, LAUNCHER, VIEWPORT, NaN, Infinity)).toEqual(start);
    expect(translateAnchor(start, LAUNCHER, VIEWPORT, "12", null)).toEqual(start);
  });
});

describe("anchorInsets", () => {
  it("frees the two sides the anchor does not hold", () => {
    expect(anchorInsets({ corner: "br", dx: 12, dy: 20 })).toEqual({
      top: "auto",
      right: "12px",
      bottom: "20px",
      left: "auto",
    });
    expect(anchorInsets({ corner: "tl", dx: 0, dy: 8 })).toEqual({
      top: "8px",
      right: "auto",
      bottom: "auto",
      left: "0px",
    });
  });
});

describe("nextCorner", () => {
  it("visits all four corners and returns to the start", () => {
    const seen: Corner[] = [];
    let c: Corner = "br";
    for (let i = 0; i < 4; i++) {
      c = nextCorner(c);
      seen.push(c);
    }
    expect(seen).toEqual(["bl", "tl", "tr", "br"]);
  });
});

describe("parseAnchor", () => {
  it("accepts a well-formed anchor", () => {
    expect(parseAnchor({ corner: "tl", dx: 12, dy: 34 })).toEqual({ corner: "tl", dx: 12, dy: 34 });
  });

  it("coerces junk offsets to zero rather than rejecting the corner", () => {
    expect(parseAnchor({ corner: "br", dx: NaN, dy: -20 })).toEqual({ corner: "br", dx: 0, dy: 0 });
    expect(parseAnchor({ corner: "br", dx: Infinity, dy: "30" })).toEqual({
      corner: "br",
      dx: 0,
      dy: 0,
    });
    expect(parseAnchor({ corner: "br" })).toEqual({ corner: "br", dx: 0, dy: 0 });
  });

  it("rejects anything without a known corner", () => {
    for (const bad of [null, undefined, 7, "br", {}, { corner: "middle", dx: 0, dy: 0 }]) {
      expect(parseAnchor(bad)).toBeNull();
    }
  });
});

describe("dragStep", () => {
  it("emits nothing until the press has travelled far enough to be a drag", () => {
    const origin = dragOrigin(100, 100);
    const nudge = dragStep(origin, 102, 103);
    expect(nudge).toEqual({ next: origin, dx: 0, dy: 0 });
    expect(nudge.next.armed).toBe(false);
  });

  it("arms once past the threshold and then reports every delta", () => {
    const armed = dragStep(dragOrigin(100, 100), 100, 106);
    expect(armed.next.armed).toBe(true);
    expect(armed).toMatchObject({ dx: 0, dy: 6 });
    // Sub-threshold movement still counts once the drag is under way.
    expect(dragStep(armed.next, 101, 107)).toMatchObject({ dx: 1, dy: 1 });
  });

  it("arms on either axis alone", () => {
    expect(dragStep(dragOrigin(0, 0), 4, 0).next.armed).toBe(true);
    expect(dragStep(dragOrigin(0, 0), 0, -4).next.armed).toBe(true);
    expect(dragStep(dragOrigin(0, 0), 3, 3).next.armed).toBe(false);
  });

  it("gives a finger far more slop than a mouse, so a shaky tap still opens", () => {
    expect(dragStep(dragOrigin(0, 0, "touch"), 8, 8).next.armed).toBe(false);
    expect(dragStep(dragOrigin(0, 0, "mouse"), 8, 8).next.armed).toBe(true);
    expect(dragStep(dragOrigin(0, 0, "touch"), 12, 0).next.armed).toBe(true);
  });

  it("keeps the threshold it started with for the life of the drag", () => {
    const armed = dragStep(dragOrigin(0, 0, "touch"), 20, 0).next;
    expect(armed.slop).toBe(12);
    expect(dragStep(armed, 21, 0)).toMatchObject({ dx: 1, dy: 0 });
  });
});
