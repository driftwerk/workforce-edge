// @vitest-environment jsdom
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";

import { InlineImage, isInlineImage } from "./InlineImage";
import type { Attachment } from "./connection";

const file = (media_type: string, filename = "chart.png"): Attachment => ({
  artifact_id: "art_0123456789abcdef0123456789abcdef",
  filename,
  media_type,
});

beforeAll(() => {
  // jsdom implements neither; the component's whole job is object-URL lifecycle.
  URL.createObjectURL = vi.fn(() => "blob:fake");
  URL.revokeObjectURL = vi.fn();
});

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe("isInlineImage", () => {
  it("accepts the raster types and ignores parameters", () => {
    expect(isInlineImage(file("image/png"))).toBe(true);
    expect(isInlineImage(file("image/jpeg"))).toBe(true);
    expect(isInlineImage(file("IMAGE/WEBP"))).toBe(true);
    expect(isInlineImage(file("image/png; charset=binary"))).toBe(true);
  });

  // `image/*` would sweep in SVG, which is a script container, and these bytes
  // are agent-authored rather than operator-authored.
  it("refuses svg and non-images", () => {
    expect(isInlineImage(file("image/svg+xml"))).toBe(false);
    expect(isInlineImage(file("application/pdf"))).toBe(false);
    expect(isInlineImage(file("text/html"))).toBe(false);
  });
});

describe("InlineImage", () => {
  const setup = (load: () => Promise<Blob>) =>
    render(
      <InlineImage
        file={file("image/png")}
        load={load}
        onDownload={vi.fn()}
        fallback={<button type="button">⬇ chart.png</button>}
      />,
    );

  it("renders the fetched bytes with the filename as alt text", async () => {
    setup(() => Promise.resolve(new Blob(["x"])));
    const img = await screen.findByAltText("chart.png");
    expect(img.getAttribute("src")).toBe("blob:fake");
  });

  // An image that will not load must never cost the customer the file itself.
  it("degrades to the download fallback when the fetch fails", async () => {
    setup(() => Promise.reject(new Error("403")));
    await waitFor(() => expect(screen.getByText("⬇ chart.png")).toBeDefined());
    expect(screen.queryByAltText("chart.png")).toBeNull();
  });

  it("shows a placeholder while the bytes are in flight", () => {
    setup(() => new Promise(() => {}));
    expect(screen.getByRole("status")).toBeDefined();
  });

  it("revokes the object URL on unmount so a long chat leaks no blobs", async () => {
    const { unmount } = setup(() => Promise.resolve(new Blob(["x"])));
    await screen.findByAltText("chart.png");
    unmount();
    expect(URL.revokeObjectURL).toHaveBeenCalledWith("blob:fake");
  });

  it("offers expand and download beside the picture", async () => {
    setup(() => Promise.resolve(new Blob(["x"])));
    await screen.findByAltText("chart.png");
    expect(screen.getByLabelText("Expand chart.png")).toBeDefined();
    expect(screen.getByText("Download")).toBeDefined();
  });
});

// The customer's own attachments were gated to `role === "assistant"`, so a
// photo you sent never appeared in your own bubble — it read as if the file had
// not gone. These pin the two shapes that broke.
describe("a customer's own attachment", () => {
  const own: Attachment = {
    artifact_id: "art_0123456789abcdef0123456789abcdef",
    filename: "drawing.png",
    media_type: "image/png",
  };

  it("is an image the widget will render inline, not a download-only file", () => {
    expect(isInlineImage(own)).toBe(true);
  });

  it("keeps a non-image attachment on the download path", () => {
    expect(isInlineImage({ ...own, filename: "report.pdf", media_type: "application/pdf" })).toBe(
      false,
    );
  });
});
