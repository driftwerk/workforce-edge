// Attaching files to the next message: the picker, the drop/paste handling, the
// pending chips, and the drawing canvas that feeds the same path.
//
// The canvas is not a separate feature — it produces a PNG and hands it to the
// same upload, so a drawn sketch and a chosen photo travel identically.
//
// Client-side checks here only save a round trip and give an instant reason;
// the server re-checks every one of them, because a widget runs on the
// customer's machine.

import { useCallback, useEffect, useRef, useState } from "react";

import type { StagedFile } from "./api";
import type { UploadKind, WidgetFeatures } from "../protocol";

/** Extensions offered in the picker, per kind. The server decides by sniffing
 *  the bytes; this only shapes the file dialog. */
const ACCEPT: Record<UploadKind, string> = {
  image: "image/png,image/jpeg,image/gif,image/webp",
  spreadsheet: ".csv,text/csv,.xlsx,application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
  document:
    ".pdf,application/pdf,.docx,application/vnd.openxmlformats-officedocument.wordprocessingml.document,.txt,.md,text/plain,text/markdown",
};

const KIND_LABEL: Record<UploadKind, string> = {
  image: "images",
  spreadsheet: "spreadsheets",
  document: "documents",
};

export const acceptAttribute = (kinds: UploadKind[]): string =>
  kinds.map((k) => ACCEPT[k]).join(",");

/** What the customer is told they may send, e.g. "images or documents". */
export function attachHint(kinds: UploadKind[]): string {
  const names = kinds.map((k) => KIND_LABEL[k]);
  if (names.length === 0) return "";
  if (names.length === 1) return names[0];
  return `${names.slice(0, -1).join(", ")} or ${names[names.length - 1]}`;
}

/** A file waiting to be sent: uploading, staged, or refused with a reason. */
export interface Pending {
  key: string;
  filename: string;
  staged?: StagedFile;
  error?: string;
}

let nextKey = 0;
export const pendingKey = () => `p${nextKey++}`;

/** Reject what we can name before the round trip. The server is still the
 *  enforcement point; this exists so the customer hears "too big" immediately
 *  rather than after uploading 20 MB. */
export function localRefusal(file: File, features: WidgetFeatures): string | null {
  if (file.size === 0) return "That file is empty.";
  if (file.size > features.upload_max_mb * 1024 * 1024) {
    return `That file is too big — the limit is ${features.upload_max_mb} MB.`;
  }
  return null;
}

export function AttachChips({
  pending,
  onRemove,
}: {
  pending: Pending[];
  onRemove: (key: string) => void;
}) {
  if (pending.length === 0) return null;
  return (
    <div className="wf-chips">
      {pending.map((p) => (
        <span
          key={p.key}
          className={"wf-chip" + (p.error ? " wf-chip-err" : p.staged ? "" : " wf-chip-busy")}
        >
          <span className="wf-chip-name">{p.filename}</span>
          {p.error ? (
            <span className="wf-chip-why" role="alert">
              {p.error}
            </span>
          ) : !p.staged ? (
            <span className="wf-chip-why">uploading…</span>
          ) : null}
          <button type="button" aria-label={`Remove ${p.filename}`} onClick={() => onRemove(p.key)}>
            ×
          </button>
        </span>
      ))}
    </div>
  );
}

/** A freehand canvas whose output is a PNG file. Pointer events cover mouse,
 *  touch and stylus in one path; `touch-action: none` is what stops a drag
 *  scrolling the panel instead of drawing. */
export function DrawingCanvas({
  onDone,
  onCancel,
}: {
  onDone: (file: File) => void;
  onCancel: () => void;
}) {
  const canvas = useRef<HTMLCanvasElement>(null);
  const drawing = useRef(false);
  const [strokes, setStrokes] = useState(0);

  useEffect(() => {
    const el = canvas.current;
    if (!el) return;
    const ctx = el.getContext("2d");
    if (!ctx) return;
    // A signature on a transparent PNG is invisible on a dark background, so the
    // sheet is painted white once and drawn on in ink.
    ctx.fillStyle = "#ffffff";
    ctx.fillRect(0, 0, el.width, el.height);
    ctx.lineWidth = 2.5;
    ctx.lineCap = "round";
    ctx.lineJoin = "round";
    ctx.strokeStyle = "#1e1e1e";
  }, []);

  const at = (e: React.PointerEvent<HTMLCanvasElement>): [number, number] => {
    const el = e.currentTarget;
    const box = el.getBoundingClientRect();
    // The backing store is fixed-size while the element is fluid, so pointer
    // coordinates must be scaled or the ink lands away from the cursor.
    return [
      ((e.clientX - box.left) / box.width) * el.width,
      ((e.clientY - box.top) / box.height) * el.height,
    ];
  };

  const start = (e: React.PointerEvent<HTMLCanvasElement>) => {
    const ctx = e.currentTarget.getContext("2d");
    if (!ctx) return;
    e.currentTarget.setPointerCapture(e.pointerId);
    drawing.current = true;
    const [x, y] = at(e);
    ctx.beginPath();
    ctx.moveTo(x, y);
  };

  const move = (e: React.PointerEvent<HTMLCanvasElement>) => {
    if (!drawing.current) return;
    const ctx = e.currentTarget.getContext("2d");
    if (!ctx) return;
    const [x, y] = at(e);
    ctx.lineTo(x, y);
    ctx.stroke();
  };

  const end = () => {
    if (!drawing.current) return;
    drawing.current = false;
    setStrokes((n) => n + 1);
  };

  const clear = () => {
    const el = canvas.current;
    const ctx = el?.getContext("2d");
    if (!el || !ctx) return;
    ctx.fillStyle = "#ffffff";
    ctx.fillRect(0, 0, el.width, el.height);
    setStrokes(0);
  };

  const attach = () => {
    const el = canvas.current;
    if (!el) return;
    el.toBlob((blob) => {
      if (blob) onDone(new File([blob], "drawing.png", { type: "image/png" }));
    }, "image/png");
  };

  return (
    <div className="wf-draw" role="group" aria-label="Drawing">
      <canvas
        ref={canvas}
        width={560}
        height={280}
        className="wf-draw-pad"
        aria-label="Drawing area"
        onPointerDown={start}
        onPointerMove={move}
        onPointerUp={end}
        onPointerCancel={end}
      />
      <div className="wf-draw-bar">
        <button type="button" className="wf-rt-btn" onClick={clear} disabled={strokes === 0}>
          Clear
        </button>
        <span className="wf-draw-spacer" />
        <button type="button" className="wf-rt-btn" onClick={onCancel}>
          Cancel
        </button>
        <button type="button" className="wf-rt-btn" onClick={attach} disabled={strokes === 0}>
          Attach drawing
        </button>
      </div>
    </div>
  );
}

/** Wire drop-and-paste onto a container. Returns whether a drag is over it, so
 *  the panel can show it is a target. */
export function useDropTarget(enabled: boolean, onFiles: (files: File[]) => void) {
  const [over, setOver] = useState(false);

  const onDragOver = useCallback(
    (e: React.DragEvent) => {
      if (!enabled) return;
      e.preventDefault();
      setOver(true);
    },
    [enabled],
  );
  const onDragLeave = useCallback(() => setOver(false), []);
  const onDrop = useCallback(
    (e: React.DragEvent) => {
      if (!enabled) return;
      e.preventDefault();
      setOver(false);
      const files = [...e.dataTransfer.files];
      if (files.length) onFiles(files);
    },
    [enabled, onFiles],
  );
  const onPaste = useCallback(
    (e: React.ClipboardEvent) => {
      if (!enabled) return;
      const files = [...e.clipboardData.files];
      if (files.length) {
        e.preventDefault();
        onFiles(files);
      }
    },
    [enabled, onFiles],
  );

  return { over, onDragOver, onDragLeave, onDrop, onPaste };
}
