import { describe, expect, it } from "vitest";

import { asWidgetEvent } from "./protocol";

describe("asWidgetEvent", () => {
  it("accepts every defined event name", () => {
    const names = [
      "opened",
      "closed",
      "fullscreen",
      "moved",
      "message-sent",
      "conversation-list-opened",
    ];
    for (const name of names) expect(asWidgetEvent(name)).toBe(name);
  });

  it("rejects anything else", () => {
    expect(asWidgetEvent("open")).toBeNull();
    expect(asWidgetEvent("")).toBeNull();
    expect(asWidgetEvent(42)).toBeNull();
    expect(asWidgetEvent(null)).toBeNull();
    expect(asWidgetEvent(undefined)).toBeNull();
    expect(asWidgetEvent({})).toBeNull();
  });
});
