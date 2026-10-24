// @vitest-environment jsdom
//
// The failure this file exists for (fabrikam staging, 2026-08-26): a turn's socket
// died mid-answer, the widget fell back to polling the transcript, and the user —
// looking at a recovery note with nothing happening — asked their next question.
// That keystroke aborted the previous question's recovery, because the poll shared
// the turn's AbortController. The answer was still being generated and was
// discarded 76 seconds before it landed. It happened twice in one demo.
//
// So: a new turn must never cancel an outstanding recovery, and each recovered
// answer must settle into its own row.
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

import type { TranscriptMessage } from "./api";

/** Transcript rows the fake server will hand back, mutated per test to simulate
 *  answers landing later. */
let transcriptRows: TranscriptMessage[] = [];

/** Resolves the in-flight turn's stream with a dropped socket. */
let dropCurrentTurn: (() => void) | null = null;

vi.mock("./api", async () => {
  const actual = await vi.importActual<typeof import("./api")>("./api");
  return {
    ...actual,
    listConversations: vi.fn(async () => []),
    getConversation: vi.fn(async () => ({
      messages: transcriptRows,
      start: 0,
      has_more: false,
    })),
    postFeedback: vi.fn(async () => {}),
  };
});

vi.mock("./connection", async () => {
  const actual = await vi.importActual<typeof import("./connection")>("./connection");
  class FakeConnection {
    conversation = "conv-1";
    constructor(..._args: unknown[]) {}
    // eslint-disable-next-line require-yield
    async *stream(): AsyncGenerator<string, void> {
      // Park until the test drops the socket, then fail exactly as the real
      // connection does when a reconnect brings no replay.
      await new Promise<void>((resolve) => {
        dropCurrentTurn = resolve;
      });
      throw new Error("connection-lost");
    }
    async *submitForm(): AsyncGenerator<string, void> {}
    setToken() {}
    close() {}
  }
  return { ...actual, EdgeConnection: FakeConnection };
});

const { App } = await import("./App");
const { NO_FEATURES } = await import("../protocol");

function renderWidget() {
  return render(
    <App
      base="https://edge.test"
      agent="fabrikam-assistant"
      token="tok"
      theme={{} as never}
      welcome=""
      features={NO_FEATURES}
      resize={() => {}}
      anchor={{ corner: "br", dx: 0, dy: 0 }}
      move={() => {}}
      reanchor={() => {}}
      emit={() => {}}
      context={null}
    />,
  );
}

/** Open the panel and send `text`, leaving its turn parked in `stream`. */
async function ask(text: string) {
  const box = screen.getByRole("textbox");
  fireEvent.change(box, { target: { value: text } });
  fireEvent.keyDown(box, { key: "Enter" });
  await waitFor(() => expect(dropCurrentTurn).not.toBeNull());
}

// jsdom ships neither, and the panel measures itself on mount.
beforeAll(() => {
  globalThis.ResizeObserver = class {
    observe() {}
    unobserve() {}
    disconnect() {}
  } as unknown as typeof ResizeObserver;
  Element.prototype.scrollTo = () => {};
});

beforeEach(() => {
  transcriptRows = [];
  dropCurrentTurn = null;
  sessionStorage.clear();
  vi.useRealTimers();
});

afterEach(cleanup);

describe("a recovery outlives the turn that started it", () => {
  it("keeps recovering the first answer after the user asks a second question", async () => {
    renderWidget();
    fireEvent.click(screen.getByRole("button", { name: /open|chat|assistant/i }));

    // Q1 goes out, then its socket dies mid-answer.
    await ask("closest deals to exchange?");
    const dropFirst = dropCurrentTurn!;
    dropCurrentTurn = null;
    await act(async () => {
      dropFirst();
    });
    await waitFor(() => expect(screen.getByText(/still working/i)).toBeTruthy());

    // The user, seeing nothing happen, asks the next question. This is the
    // keystroke that used to throw the first answer away.
    await ask("best law firm per office?");

    // Q1's answer finally lands in the transcript, as it did at 09:47:11.
    transcriptRows = [
      { role: "user", content: "closest deals to exchange?", at: "" },
      { role: "assistant", content: "the milestone analysis", at: "" },
    ];

    await waitFor(
      () => expect(screen.getByText(/the milestone analysis/)).toBeTruthy(),
      { timeout: 6000 },
    );
  }, 15000);

  it("settles each recovered answer into its own row, matched by reference", async () => {
    renderWidget();
    fireEvent.click(screen.getByRole("button", { name: /open|chat|assistant/i }));

    await ask("question one");
    const dropFirst = dropCurrentTurn!;
    dropCurrentTurn = null;
    await act(async () => {
      dropFirst();
    });
    await waitFor(() => expect(screen.getByText(/still working/i)).toBeTruthy());

    await ask("question two");
    const dropSecond = dropCurrentTurn!;
    await act(async () => {
      dropSecond();
    });

    // Both answers land together and OUT of ask order — the shape two concurrent
    // turns on one conversation actually produced. Position cannot tell them
    // apart, and neither reference belongs to these turns, so recovery must
    // refuse rather than hand the customer the other question's answer. (It
    // keeps polling until its window closes; the invariant under test is that a
    // wrong answer is never shown, not how it eventually gives up.)
    transcriptRows = [
      { role: "user", content: "question one", at: "" },
      { role: "user", content: "question two", at: "" },
      { role: "assistant", content: "answer to two", at: "", reply_to: "unknown-b" },
      { role: "assistant", content: "answer to one", at: "", reply_to: "unknown-a" },
    ];

    // Long enough for several poll cycles to have seen — and rejected — them.
    await new Promise((r) => setTimeout(r, 6000));

    expect(
      screen.queryByText("answer to two"),
      "an answer stamped for another turn must never be shown",
    ).toBeNull();
    expect(screen.queryByText("answer to one")).toBeNull();
    // Both rows are still waiting rather than settled on something wrong.
    expect(screen.getAllByText(/still working/i).length).toBe(2);
  }, 20000);
});
