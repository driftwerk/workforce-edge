import { describe, expect, it } from "vitest";

import type * as api from "./api";
import { pickConversation } from "./Conversations";

/** Minimal `Conversation` rows — only the fields `pickConversation` reads. */
function convo(session: string, conversation: string | null): api.Conversation {
  return {
    session,
    conversation,
    owner: "ext:isssub",
    display_name: null,
    title: "t",
    updated_at: "2026-07-10T00:00:00Z",
    messages: 1,
  };
}

describe("pickConversation", () => {
  const rows = [convo("s1", "c1"), convo("s2", "c2")];

  it("opens the exact conversation the link names", () => {
    expect(pickConversation(rows, "c2")?.session).toBe("s2");
  });

  it("returns null when the named conversation is not in the loaded list", () => {
    // e.g. it fell past the admin list cap, or the session was pruned — the
    // caller then just expands the owner group rather than opening a transcript.
    expect(pickConversation(rows, "c-gone")).toBeNull();
  });

  it("opens the sole conversation when the link names no conversation", () => {
    expect(pickConversation([convo("only", "c9")], null)?.session).toBe("only");
  });

  it("opens nothing when no conversation is named and the customer has several", () => {
    expect(pickConversation(rows, null)).toBeNull();
  });

  it("opens nothing for an owner with no conversations", () => {
    expect(pickConversation([], null)).toBeNull();
    expect(pickConversation([], "c1")).toBeNull();
  });
});
