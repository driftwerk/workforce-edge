import { describe, expect, it } from "vitest";

import { recoveredReply } from "./recovery";
import type { TranscriptMessage } from "./api";

const user = (content: string): TranscriptMessage => ({ role: "user", content, at: "" });
const bot = (content: string): TranscriptMessage => ({ role: "assistant", content, at: "" });

describe("recoveredReply", () => {
  it("returns the assistant message following the sent question", () => {
    const messages = [user("hi"), bot("hello"), user("how many workers?"), bot("42")];
    expect(recoveredReply(messages, "how many workers?")?.content).toBe("42");
  });

  it("returns null while the question is still unanswered", () => {
    const messages = [user("hi"), bot("hello"), user("how many workers?")];
    expect(recoveredReply(messages, "how many workers?")).toBeNull();
  });

  it("returns null when the question is not in the transcript", () => {
    expect(recoveredReply([user("hi"), bot("hello")], "missing")).toBeNull();
  });

  it("matches the newest occurrence of a repeated question", () => {
    const messages = [user("count?"), bot("9"), user("count?"), bot("12")];
    expect(recoveredReply(messages, "count?")?.content).toBe("12");
  });

  it("a repeated question still pending recovers nothing, not the old answer", () => {
    const messages = [user("count?"), bot("9"), user("count?")];
    expect(recoveredReply(messages, "count?")).toBeNull();
  });

  // The poll only ever sees the newest 10 messages. When the matched question is
  // the oldest row in that window, the row that would prove the shape ambiguous
  // may simply be off the page — so a page that does not start at the
  // conversation's beginning cannot be trusted to look unambiguous.
  it("refuses at the page edge, where the row before ours is out of view", () => {
    const page = [user("best law firm per office?"), bot("the milestone analysis")];
    expect(recoveredReply(page, "best law firm per office?", undefined, false)).toBeNull();
    expect(
      recoveredReply(page, "best law firm per office?", undefined, true)?.content,
      "the same page IS trustworthy when it starts at the conversation's beginning",
    ).toBe("the milestone analysis");
  });

  it("handles an empty transcript", () => {
    expect(recoveredReply([], "anything")).toBeNull();
  });

  // The shape a dropped socket produces: two questions outstanding at once, so
  // the transcript holds adjacent user rows. Answers land in completion order,
  // so the row after the run is not reliably the last question's answer.
  it("refuses to guess when two questions were outstanding at once", () => {
    const messages = [
      user("closest deals to exchange?"),
      user("best law firm per office?"),
      bot("here is the milestone analysis"),
    ];
    expect(
      recoveredReply(messages, "best law firm per office?"),
      "must not hand back the other question's answer",
    ).toBeNull();
  });

  it("matches the answer stamped with this turn's reference, whatever its position", () => {
    const messages: TranscriptMessage[] = [
      user("closest deals to exchange?"),
      user("best law firm per office?"),
      { ...bot("milestone analysis"), reply_to: "ref-a" },
      { ...bot("per-office breakdown"), reply_to: "ref-b" },
    ];
    expect(recoveredReply(messages, "best law firm per office?", "ref-b")?.content).toBe(
      "per-office breakdown",
    );
    expect(recoveredReply(messages, "closest deals to exchange?", "ref-a")?.content).toBe(
      "milestone analysis",
    );
  });

  // After a reload the widget recovers the transcript's dangling question, but
  // the reference died with the page — and by then every answer carries one. If a
  // stamped answer were refused here, reload-mid-turn recovery could never
  // succeed again.
  it("recovers a stamped answer positionally when this turn has no reference", () => {
    const messages: TranscriptMessage[] = [
      user("count?"),
      { ...bot("42"), reply_to: "a-turn-from-before-the-reload" },
    ];
    expect(recoveredReply(messages, "count?")?.content).toBe("42");
  });

  it("does not claim a stamped answer by position when the ref does not match", () => {
    const messages: TranscriptMessage[] = [
      user("count?"),
      { ...bot("42"), reply_to: "someone-elses-turn" },
    ];
    expect(recoveredReply(messages, "count?", "my-turn")).toBeNull();
  });

  it("still recovers an unstamped answer saved before the link existed", () => {
    const messages = [user("count?"), bot("42")];
    expect(recoveredReply(messages, "count?", "my-turn")?.content).toBe("42");
  });
});
