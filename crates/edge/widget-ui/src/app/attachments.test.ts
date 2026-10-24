import { describe, expect, it } from "vitest";

import { mergeAttachments, withAttachment } from "./attachments";
import type { Attachment } from "./connection";

const file = (id: string): Attachment => ({
  artifact_id: id,
  filename: `${id}.xlsx`,
  media_type: "application/octet-stream",
});

describe("withAttachment", () => {
  it("appends a new artifact", () => {
    expect(withAttachment(undefined, file("a"))).toEqual([file("a")]);
    expect(withAttachment([file("a")], file("b"))).toEqual([file("a"), file("b")]);
  });

  it("is idempotent for an artifact already present (replay dedupe)", () => {
    const list = [file("a")];
    expect(withAttachment(list, file("a"))).toBe(list);
  });
});

describe("mergeAttachments", () => {
  it("unions live and recovered sets without duplicating shared artifacts", () => {
    expect(mergeAttachments([file("a")], [file("a"), file("b")])).toEqual([file("a"), file("b")]);
  });

  it("returns undefined when both sides are empty", () => {
    expect(mergeAttachments(undefined, undefined)).toBeUndefined();
    expect(mergeAttachments([], undefined)).toBeUndefined();
  });

  it("keeps the live set when there is nothing to recover", () => {
    expect(mergeAttachments([file("a")], undefined)).toEqual([file("a")]);
  });
});
