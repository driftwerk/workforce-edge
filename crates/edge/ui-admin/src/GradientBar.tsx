// A gradient stop editor: the live gradient as a bar, with a draggable handle per
// stop. Drag to reposition, click to select, and edit the selected stop's colour
// below. Add/remove keeps the count between 2 and MAX_STOPS.
//
// It edits `stops` (colour + position). The `from`/`via`/`to` shorthand a config may
// have been written with is converted to stops on first edit, so the two never fight:
// the server prefers stops as soon as two are usable.
import { useCallback, useEffect, useRef, useState } from "react";

import type { Gradient, GradientStop } from "./api";
import { HEX, MAX_STOPS, resolveGradient, usableStops } from "./brandTheme";

/** The stops to edit: explicit ones if present, else the shorthand read as stops so
 *  the first drag has something to move. */
export function stopsOf(g: Gradient | null | undefined): { color: string; at: number }[] {
  const explicit = usableStops(g);
  if (explicit) return explicit;
  const parts = [g?.from, g?.via, g?.to].filter((c): c is string => !!c && HEX.test(c));
  if (parts.length < 2) return [];
  const last = parts.length - 1;
  return parts.map((color, i) => ({ color, at: (i / last) * 100 }));
}

const clamp = (v: number) => Math.min(100, Math.max(0, v));
const round1 = (v: number) => Math.round(v * 10) / 10;

export function GradientBar({
  g,
  onChange,
  disabled,
}: {
  g?: Gradient | null;
  /** Receives the full new stop list; the caller writes it back onto the gradient. */
  onChange: (stops: GradientStop[]) => void;
  disabled?: boolean;
}) {
  const stops = stopsOf(g);
  const [selected, setSelected] = useState(0);
  const bar = useRef<HTMLDivElement | null>(null);
  const dragging = useRef<number | null>(null);

  // A removed stop can leave the selection past the end.
  useEffect(() => {
    if (selected > stops.length - 1) setSelected(Math.max(0, stops.length - 1));
  }, [stops.length, selected]);

  const commit = useCallback(
    (next: { color: string; at: number }[]) =>
      onChange(next.map((s) => ({ color: s.color, at: round1(s.at) }))),
    [onChange],
  );

  /** Pointer x → position along the bar, as a percentage. */
  const posFor = (clientX: number): number => {
    const el = bar.current;
    if (!el) return 0;
    const r = el.getBoundingClientRect();
    return r.width ? clamp(((clientX - r.left) / r.width) * 100) : 0;
  };

  // The drag is followed on the WINDOW: the handles are ~14px, so the pointer leaves
  // them almost immediately and a handle-local listener would drop the drag.
  useEffect(() => {
    if (disabled) return;
    const move = (e: PointerEvent) => {
      const i = dragging.current;
      if (i === null) return;
      e.preventDefault();
      const next = stops.map((s, n) => (n === i ? { ...s, at: posFor(e.clientX) } : s));
      commit(next);
    };
    const up = () => {
      dragging.current = null;
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up);
    window.addEventListener("pointercancel", up);
    return () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
      window.removeEventListener("pointercancel", up);
    };
  }, [stops, commit, disabled]);

  const css = resolveGradient({ ...g, stops: stops.map((s) => ({ color: s.color, at: s.at })) });
  const sel = stops[selected];

  const setStop = (i: number, patch: Partial<{ color: string; at: number }>) =>
    commit(stops.map((s, n) => (n === i ? { ...s, ...patch } : s)));

  const addStop = () => {
    if (stops.length >= MAX_STOPS) return;
    // Drop the new stop in the widest gap, which is where someone reaching for "add"
    // almost always means to put it.
    let at = 50;
    let widest = -1;
    for (let i = 0; i < stops.length - 1; i++) {
      const gap = stops[i + 1].at - stops[i].at;
      if (gap > widest) {
        widest = gap;
        at = stops[i].at + gap / 2;
      }
    }
    const next = [...stops, { color: sel?.color ?? "#888888", at }].sort((a, b) => a.at - b.at);
    commit(next);
    setSelected(next.findIndex((s) => s.at === at));
  };

  const removeStop = (i: number) => {
    if (stops.length <= 2) return;
    commit(stops.filter((_, n) => n !== i));
    setSelected(Math.max(0, i - 1));
  };

  if (stops.length < 2) {
    return (
      <p className="hint" style={{ margin: "8px 2px 0" }}>
        Set a start and end colour to enable the stop editor.
      </p>
    );
  }

  return (
    <div className="grad-editor">
      <div
        className="grad-bar"
        ref={bar}
        style={{ background: css }}
        onPointerDown={(e) => {
          if (disabled || e.target !== bar.current) return;
          // A click on the track moves the SELECTED stop there — the common gesture,
          // and it keeps the track from being dead space.
          setStop(selected, { at: posFor(e.clientX) });
        }}
      >
        {stops.map((s, i) => (
          <button
            key={i}
            type="button"
            className={"grad-stop" + (i === selected ? " on" : "")}
            style={{ left: `${s.at}%`, background: s.color }}
            aria-label={`Stop ${i + 1}: ${s.color} at ${round1(s.at)}%`}
            disabled={disabled}
            onPointerDown={(e) => {
              e.stopPropagation();
              setSelected(i);
              dragging.current = i;
            }}
            onKeyDown={(e) => {
              // Keyboard parity with the drag — a slider nobody can reach by keyboard
              // is not an accessible control.
              const step = e.shiftKey ? 10 : 1;
              if (e.key === "ArrowLeft") {
                e.preventDefault();
                setStop(i, { at: clamp(s.at - step) });
              } else if (e.key === "ArrowRight") {
                e.preventDefault();
                setStop(i, { at: clamp(s.at + step) });
              } else if (e.key === "Backspace" || e.key === "Delete") {
                e.preventDefault();
                removeStop(i);
              }
            }}
          />
        ))}
      </div>

      {sel && (
        <div className="grad-controls">
          <span className="grad-swatch-wrap">
            <input
              type="color"
              value={HEX.test(sel.color) ? sel.color : "#888888"}
              disabled={disabled}
              onChange={(e) => setStop(selected, { color: e.target.value })}
            />
            <input
              type="text"
              className="grad-hex"
              value={sel.color}
              disabled={disabled}
              onChange={(e) => setStop(selected, { color: e.target.value })}
            />
          </span>
          <label className="grad-at-label" htmlFor="grad-at">
            at
          </label>
          <input
            id="grad-at"
            type="number"
            className="grad-at"
            min={0}
            max={100}
            step={1}
            value={round1(sel.at)}
            disabled={disabled}
            onChange={(e) => {
              const n = Number(e.target.value);
              if (Number.isFinite(n)) setStop(selected, { at: clamp(n) });
            }}
          />
          <span className="grad-pct">%</span>
          <span className="grad-actions">
            <button
              type="button"
              className="btn-sm ghost"
              onClick={addStop}
              disabled={disabled || stops.length >= MAX_STOPS}
              title={stops.length >= MAX_STOPS ? `Up to ${MAX_STOPS} stops` : "Add a stop"}
            >
              + Stop
            </button>
            <button
              type="button"
              className="btn-sm ghost"
              onClick={() => removeStop(selected)}
              disabled={disabled || stops.length <= 2}
              title={stops.length <= 2 ? "A gradient needs two stops" : "Remove this stop"}
            >
              Remove
            </button>
          </span>
        </div>
      )}
      <p className="hint" style={{ margin: "8px 2px 0" }}>
        Drag a stop, or select one and use ← → (hold shift for 10%). {stops.length} of{" "}
        {MAX_STOPS} stops.
      </p>
    </div>
  );
}
