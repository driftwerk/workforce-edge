// Where the widget sits on the host viewport, and the pointer-drag arithmetic that
// moves it. The loader is the only code that can see the host viewport, so it owns
// the position; every rule it needs lives here so it stays DOM glue — the same
// split as `theme-detect`.

/** The viewport corner the widget hangs off, as `<vertical><horizontal>`. */
export type Corner = "bl" | "br" | "tl" | "tr";

/** A position: insets in px from the two viewport edges that meet at `corner`.
 *  Corner-relative rather than absolute so the panel always grows inward when it
 *  opens, and a viewport change re-clamps in place instead of throwing the widget
 *  across the screen. */
export interface Anchor {
  corner: Corner;
  dx: number;
  dy: number;
}

/** A size in px — the widget's rendered box, or the host viewport. */
export interface Box {
  w: number;
  h: number;
}

/** CSS inset values for all four sides; `"auto"` on the two the anchor frees. */
export interface Insets {
  top: string;
  right: string;
  bottom: string;
  left: string;
}

const CORNERS: readonly Corner[] = ["br", "bl", "tl", "tr"];

const isCorner = (v: unknown): v is Corner => CORNERS.includes(v as Corner);

/** A finite, non-negative inset, else 0 — every untrusted number (postMessage
 *  payload, stored JSON) passes through this. */
const offset = (v: unknown): number => (typeof v === "number" && Number.isFinite(v) && v > 0 ? v : 0);

/** A finite drag delta, else 0. Unlike an inset it may be negative. */
const delta = (v: unknown): number => (typeof v === "number" && Number.isFinite(v) ? v : 0);

const clamp = (v: number, max: number): number => Math.min(Math.max(v, 0), Math.max(max, 0));

/** The configured inset from a docked corner, as the theme carries it — absent on a
 *  stale server, and untrusted like every other number that crosses the frame. */
export interface DockInset {
  offset_x?: unknown;
  offset_y?: unknown;
}

/** Dock to a corner at the configured inset: where the widget starts, and where the
 *  corner cycle puts it — never the bare corner, which may sit on a host control strip. */
export const dockAt = (corner: Corner, inset?: DockInset): Anchor => ({
  corner,
  dx: offset(inset?.offset_x),
  dy: offset(inset?.offset_y),
});

/** Where the widget sits before anyone moves it: the configured side, at the bottom. */
export const dockedAnchor = (position: string, inset?: DockInset): Anchor =>
  dockAt(position === "left" ? "bl" : "br", inset);

/** Keep the whole widget on screen. A box larger than the viewport pins to the corner. */
export function clampAnchor(a: Anchor, box: Box, viewport: Box): Anchor {
  const dx = clamp(a.dx, viewport.w - box.w);
  const dy = clamp(a.dy, viewport.h - box.h);
  return dx === a.dx && dy === a.dy ? a : { corner: a.corner, dx, dy };
}

/** Absolute top-left of the box an anchor describes. */
const origin = (a: Anchor, box: Box, viewport: Box) => ({
  x: a.corner[1] === "l" ? a.dx : viewport.w - box.w - a.dx,
  y: a.corner[0] === "t" ? a.dy : viewport.h - box.h - a.dy,
});

/** Re-anchor an absolute top-left to whichever corner the box's centre is nearest. */
const anchorAt = (x: number, y: number, box: Box, viewport: Box): Anchor => {
  const vertical = y + box.h / 2 < viewport.h / 2 ? "t" : "b";
  const horizontal = x + box.w / 2 < viewport.w / 2 ? "l" : "r";
  const corner = (vertical + horizontal) as Corner;
  return {
    corner,
    dx: horizontal === "l" ? x : viewport.w - box.w - x,
    dy: vertical === "t" ? y : viewport.h - box.h - y,
  };
};

/** Apply one drag step (screen px, right/down positive), keeping the widget on
 *  screen and re-anchoring to the nearest corner. Bounded by construction: a drag
 *  that runs past an edge cannot bank an offset the user then has to unwind. */
export function translateAnchor(a: Anchor, box: Box, viewport: Box, dx: unknown, dy: unknown): Anchor {
  const from = origin(a, box, viewport);
  return anchorAt(
    clamp(from.x + delta(dx), viewport.w - box.w),
    clamp(from.y + delta(dy), viewport.h - box.h),
    box,
    viewport,
  );
}

/** The four CSS inset values for an already-clamped anchor. */
export const anchorInsets = (a: Anchor): Insets => ({
  top: a.corner[0] === "t" ? a.dy + "px" : "auto",
  bottom: a.corner[0] === "b" ? a.dy + "px" : "auto",
  left: a.corner[1] === "l" ? a.dx + "px" : "auto",
  right: a.corner[1] === "r" ? a.dx + "px" : "auto",
});

/** The next corner in the cycle — the keyboard route to the same four positions. */
export const nextCorner = (c: Corner): Corner => CORNERS[(CORNERS.indexOf(c) + 1) % CORNERS.length];

/** Coerce an untrusted anchor (postMessage payload, stored JSON), else reject it. */
export function parseAnchor(v: unknown): Anchor | null {
  const a = v as Partial<Anchor> | null | undefined;
  return a && isCorner(a.corner) ? { corner: a.corner, dx: offset(a.dx), dy: offset(a.dy) } : null;
}

/** A pointer drag in progress: the last screen point it reported, whether it has
 *  moved far enough to count as a drag, and how far that is for this pointer. */
export interface DragState {
  x: number;
  y: number;
  armed: boolean;
  slop: number;
}

/** Movement before a press becomes a drag, so a click never nudges the widget. A
 *  finger is far shakier than a mouse, and a tap that arms is a tap that failed to
 *  open the widget — so touch gets the platform's own tap slop, not the mouse's. */
const ARM_PX = 4;
const TOUCH_ARM_PX = 12;

export const dragOrigin = (x: number, y: number, pointerType?: string): DragState => ({
  x,
  y,
  armed: false,
  slop: pointerType === "touch" ? TOUCH_ARM_PX : ARM_PX,
});

/** Advance a drag to a new screen point, emitting no delta until it arms. Screen
 *  coordinates are what make this stable: the iframe moves under the cursor, so
 *  client coordinates would fight the drag's own feedback. */
export function dragStep(
  prev: DragState,
  x: number,
  y: number,
): { next: DragState; dx: number; dy: number } {
  if (!prev.armed && Math.abs(x - prev.x) < prev.slop && Math.abs(y - prev.y) < prev.slop) {
    return { next: prev, dx: 0, dy: 0 };
  }
  return { next: { ...prev, x, y, armed: true }, dx: x - prev.x, dy: y - prev.y };
}
