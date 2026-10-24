// @vitest-environment jsdom
//
// What a transcript row shows for FILES, on both sides of the conversation.
//
// The customer's own uploads were once gated to `role === "assistant"` and their
// filename faked into the message text, so a photo you sent vanished from your
// own bubble and a caption-less send read as the words "📎 drawing.png". Every
// chat surface shows you what you just sent; these pin that.

import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";

import { MessageRow, type Msg } from "./App";
import { NO_FEATURES, type WidgetFeatures } from "../protocol";
import type { Attachment } from "./connection";

// jsdom has no blob URLs. Without these the inline-image path rejects after the
// test has finished and the failure surfaces as an unrelated unhandled error.
beforeAll(() => {
  URL.createObjectURL = vi.fn(() => "blob:test");
  URL.revokeObjectURL = vi.fn();
});

afterEach(cleanup);

const features = (over: Partial<WidgetFeatures> = {}): WidgetFeatures => ({
  ...NO_FEATURES,
  ...over,
});

const png: Attachment = {
  artifact_id: "art_0123456789abcdef0123456789abcdef",
  filename: "drawing.png",
  media_type: "image/png",
};

const sheet: Attachment = {
  artifact_id: "art_ffffffffffffffffffffffffffffffff",
  filename: "q3.xlsx",
  media_type: "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
};

const msg = (over: Partial<Msg> = {}): Msg => ({
  id: 1,
  role: "user",
  text: "",
  ...over,
});

const row = (m: Msg, f = features(), onLoadFile?: (a: Attachment) => Promise<Blob>) =>
  render(
    <MessageRow m={m} features={f} onDownload={vi.fn(async () => {})} onLoadFile={onLoadFile} />,
  );

describe("the customer's own attachments", () => {
  it("appear in their own row", () => {
    row(msg({ text: "what is this?", attachments: [png] }));
    expect(screen.getByText("what is this?")).toBeTruthy();
    expect(screen.getByRole("button", { name: /drawing\.png/ })).toBeTruthy();
  });

  it("render with no empty bubble when the file was sent without a caption", () => {
    const { container } = row(msg({ attachments: [png] }));
    expect(container.querySelector(".wf-bubble")).toBeNull();
    expect(screen.getByRole("button", { name: /drawing\.png/ })).toBeTruthy();
  });

  it("still show a bubble when there is a caption", () => {
    const { container } = row(msg({ text: "have a look", attachments: [png] }));
    expect(container.querySelector(".wf-bubble-user")).toBeTruthy();
  });

  it("stays a download button when inline images are off", () => {
    const load = vi.fn(async () => new Blob([new Uint8Array([1])], { type: "image/png" }));
    row(msg({ attachments: [png] }), features({ inline_images: false }), load);
    expect(load).not.toHaveBeenCalled();
    expect(screen.getByRole("button", { name: /drawing\.png/ })).toBeTruthy();
  });

  it("renders as a picture when inline images are on", async () => {
    const load = vi.fn(async () => new Blob([new Uint8Array([1])], { type: "image/png" }));
    row(msg({ attachments: [png] }), features({ inline_images: true }), load);
    expect(load).toHaveBeenCalledWith(png);
    await waitFor(() => expect(screen.getByRole("img")).toBeTruthy());
  });

  it("keeps a non-image on the download path even with inline images on", () => {
    const load = vi.fn(async () => new Blob([]));
    row(msg({ attachments: [sheet] }), features({ inline_images: true }), load);
    expect(load).not.toHaveBeenCalled();
    expect(screen.getByRole("button", { name: /q3\.xlsx/ })).toBeTruthy();
  });

  it("is dropped when the host gave no download handler — a button that cannot fetch is worse than none", () => {
    const { container } = render(
      <MessageRow m={msg({ text: "hi", attachments: [png] })} features={features()} />,
    );
    expect(container.querySelector(".wf-attachments")).toBeNull();
    expect(screen.getByText("hi")).toBeTruthy();
  });
});

describe("the agent's produced files", () => {
  it("still render, unchanged by the customer-side path", () => {
    row(msg({ role: "assistant", text: "here you go", attachments: [sheet] }));
    expect(screen.getByRole("button", { name: /q3\.xlsx/ })).toBeTruthy();
  });

  it("keep their bubble even when the reply text is empty", () => {
    // An assistant row with no text is a real state (a file-only reply); unlike
    // the customer side it must keep the bubble, which carries the typing dots.
    const { container } = row(msg({ role: "assistant", attachments: [sheet] }));
    expect(container.querySelector(".wf-bubble-assistant")).toBeTruthy();
  });
});
