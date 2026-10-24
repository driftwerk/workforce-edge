// @vitest-environment jsdom
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";

import {
  AttachChips,
  DrawingCanvas,
  acceptAttribute,
  attachHint,
  localRefusal,
  pendingKey,
} from "./Attach";
import { NO_FEATURES, type WidgetFeatures } from "../protocol";

const features = (over: Partial<WidgetFeatures> = {}): WidgetFeatures => ({
  ...NO_FEATURES,
  uploads: ["image"],
  upload_max_mb: 10,
  ...over,
});

const file = (name: string, size: number, type = "image/png"): File => {
  const f = new File(["x"], name, { type });
  Object.defineProperty(f, "size", { value: size });
  return f;
};

// jsdom has no 2D context. Stubbing it exercises the canvas's setup path (the
// white sheet and pen) instead of silently taking the null-context early return.
beforeAll(() => {
  HTMLCanvasElement.prototype.getContext = vi.fn(() => ({
    fillRect: vi.fn(),
    beginPath: vi.fn(),
    moveTo: vi.fn(),
    lineTo: vi.fn(),
    stroke: vi.fn(),
    fillStyle: "",
    strokeStyle: "",
    lineWidth: 0,
    lineCap: "",
    lineJoin: "",
  })) as unknown as typeof HTMLCanvasElement.prototype.getContext;
});

afterEach(cleanup);

describe("what the customer is offered", () => {
  it("names the permitted kinds readably", () => {
    expect(attachHint(["image"])).toBe("images");
    expect(attachHint(["image", "document"])).toBe("images or documents");
    expect(attachHint(["image", "spreadsheet", "document"])).toBe(
      "images, spreadsheets or documents",
    );
    expect(attachHint([])).toBe("");
  });

  it("offers only the permitted kinds in the picker", () => {
    const images = acceptAttribute(["image"]);
    expect(images).toContain("image/png");
    expect(images).not.toContain("pdf");
    expect(acceptAttribute(["document"])).toContain("application/pdf");
  });
});

describe("localRefusal", () => {
  // The server re-checks all of this; the point here is an instant reason
  // rather than a 20 MB round trip that ends in a refusal.
  it("rejects oversize and empty before the network", () => {
    expect(localRefusal(file("a.png", 0), features())).toBe("That file is empty.");
    expect(localRefusal(file("a.png", 11 * 1024 * 1024), features())).toContain("10 MB");
    expect(localRefusal(file("a.png", 1024), features())).toBeNull();
  });

  it("follows the agent's configured cap, not a hardcoded one", () => {
    const small = features({ upload_max_mb: 1 });
    expect(localRefusal(file("a.png", 2 * 1024 * 1024), small)).toContain("1 MB");
    expect(localRefusal(file("a.png", 500 * 1024), small)).toBeNull();
  });
});

describe("AttachChips", () => {
  const pending = [
    { key: pendingKey(), filename: "done.png", staged: { artifact_id: "a", filename: "done.png", media_type: "image/png", bytes: 10 } },
    { key: pendingKey(), filename: "busy.png" },
    { key: pendingKey(), filename: "bad.png", error: "That file is too big — the limit is 10 MB." },
  ];

  it("shows each file's state and why a refusal happened", () => {
    render(<AttachChips pending={pending} onRemove={vi.fn()} />);
    expect(screen.getByText("done.png")).toBeDefined();
    expect(screen.getByText("uploading…")).toBeDefined();
    expect(screen.getByRole("alert").textContent).toContain("10 MB");
  });

  it("lets each file be removed by name", () => {
    const onRemove = vi.fn();
    render(<AttachChips pending={pending} onRemove={onRemove} />);
    fireEvent.click(screen.getByLabelText("Remove busy.png"));
    expect(onRemove).toHaveBeenCalledWith(pending[1].key);
  });

  it("renders nothing when there is nothing pending", () => {
    const { container } = render(<AttachChips pending={[]} onRemove={vi.fn()} />);
    expect(container.innerHTML).toBe("");
  });
});

describe("DrawingCanvas", () => {
  it("cannot attach or clear an untouched canvas", () => {
    render(<DrawingCanvas onDone={vi.fn()} onCancel={vi.fn()} />);
    expect(screen.getByText("Attach drawing").hasAttribute("disabled")).toBe(true);
    expect(screen.getByText("Clear").hasAttribute("disabled")).toBe(true);
  });

  it("cancels without producing a file", () => {
    const onCancel = vi.fn();
    const onDone = vi.fn();
    render(<DrawingCanvas onDone={onDone} onCancel={onCancel} />);
    fireEvent.click(screen.getByText("Cancel"));
    expect(onCancel).toHaveBeenCalled();
    expect(onDone).not.toHaveBeenCalled();
  });

  // jsdom has no 2D context, so stroke rendering cannot be exercised here —
  // what IS asserted is that the surface is a labelled, pointer-driven target,
  // which is the part that decides whether a finger draws or scrolls the panel.
  it("exposes a labelled drawing surface", () => {
    render(<DrawingCanvas onDone={vi.fn()} onCancel={vi.fn()} />);
    const pad = screen.getByLabelText("Drawing area");
    expect(pad.tagName).toBe("CANVAS");
    expect(pad.getAttribute("width")).toBe("560");
  });

  // A drawn signature on a transparent PNG is invisible on a dark background,
  // so the sheet is painted before anything is drawn on it.
  it("paints an opaque sheet before drawing", () => {
    render(<DrawingCanvas onDone={vi.fn()} onCancel={vi.fn()} />);
    const ctx = (HTMLCanvasElement.prototype.getContext as ReturnType<typeof vi.fn>).mock
      .results[0].value;
    expect(ctx.fillRect).toHaveBeenCalledWith(0, 0, 560, 280);
  });

  it("enables attach once something has been drawn", () => {
    const onDone = vi.fn();
    render(<DrawingCanvas onDone={onDone} onCancel={vi.fn()} />);
    const pad = screen.getByLabelText("Drawing area");
    (pad as HTMLCanvasElement).setPointerCapture = vi.fn();
    fireEvent.pointerDown(pad, { clientX: 10, clientY: 10, pointerId: 1 });
    fireEvent.pointerMove(pad, { clientX: 40, clientY: 40, pointerId: 1 });
    fireEvent.pointerUp(pad, { pointerId: 1 });
    expect(screen.getByText("Attach drawing").hasAttribute("disabled")).toBe(false);
    expect(screen.getByText("Clear").hasAttribute("disabled")).toBe(false);
  });
});
