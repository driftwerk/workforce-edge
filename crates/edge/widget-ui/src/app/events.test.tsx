// @vitest-environment jsdom
//
// The click-driven usage events (ENG-114): each header/launcher gesture emits
// its event exactly once, at the gesture — no phantom events on mount, none on
// the inverse gesture. Drag-driven `moved` coverage lives in drag.test.tsx.
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

vi.mock("./api", async () => {
  const actual = await vi.importActual<typeof import("./api")>("./api");
  return {
    ...actual,
    listConversations: vi.fn(async () => []),
    getConversation: vi.fn(async () => ({ messages: [], start: 0, has_more: false })),
  };
});

/** When set, turns never finish — so a second send exercises type-to-interrupt. */
let parkStreams = false;

vi.mock("./connection", async () => {
  const actual = await vi.importActual<typeof import("./connection")>("./connection");
  class FakeConnection {
    conversation = "conv-1";
    constructor(..._args: unknown[]) {}
    async *stream(): AsyncGenerator<string, void> {
      if (parkStreams) await new Promise(() => {});
      yield "ok";
    }
    async *submitForm(): AsyncGenerator<string, void> {}
    setToken() {}
    close() {}
  }
  return { ...actual, EdgeConnection: FakeConnection };
});

const { App } = await import("./App");
const { NO_FEATURES } = await import("../protocol");
const { dockedAnchor } = await import("../anchor");
const { theme } = await import("./testTheme");

function renderApp(token: string | null) {
  const emit = vi.fn();
  render(
    <App
      base="https://edge.test"
      agent="assistant"
      token={token}
      theme={theme(true)}
      welcome=""
      features={NO_FEATURES}
      resize={() => {}}
      anchor={dockedAnchor("right")}
      move={() => {}}
      reanchor={() => {}}
      emit={emit}
      context={null}
    />,
  );
  return emit;
}

beforeAll(() => {
  globalThis.ResizeObserver = class {
    observe() {}
    unobserve() {}
    disconnect() {}
  } as unknown as typeof ResizeObserver;
  Element.prototype.scrollTo = () => {};
});

beforeEach(() => {
  sessionStorage.clear();
  parkStreams = false;
});
afterEach(cleanup);

describe("usage events from clicks", () => {
  it("emits opened on launch and closed on close, once each", () => {
    const emit = renderApp(null);
    expect(emit).not.toHaveBeenCalled();
    fireEvent.click(screen.getByLabelText("Open Helper"));
    expect(emit.mock.calls).toEqual([["opened"]]);
    fireEvent.click(screen.getByLabelText("Close"));
    expect(emit.mock.calls).toEqual([["opened"], ["closed"]]);
  });

  it("emits fullscreen on entering it, not on shrinking back", () => {
    const emit = renderApp(null);
    fireEvent.click(screen.getByLabelText("Open Helper"));
    fireEvent.click(screen.getByLabelText("Expand"));
    fireEvent.click(screen.getByLabelText("Shrink"));
    expect(emit.mock.calls).toEqual([["opened"], ["fullscreen"]]);
  });

  it("reports the fullscreen re-entry when reopening a widget closed while full", () => {
    const emit = renderApp(null);
    fireEvent.click(screen.getByLabelText("Open Helper"));
    fireEvent.click(screen.getByLabelText("Expand"));
    fireEvent.click(screen.getByLabelText("Close"));
    // `full` survives the close, so this open lands straight in fullscreen.
    fireEvent.click(screen.getByLabelText("Open Helper"));
    expect(emit.mock.calls).toEqual([
      ["opened"],
      ["fullscreen"],
      ["closed"],
      ["opened"],
      ["fullscreen"],
    ]);
  });

  it("emits conversation-list-opened only when switching to the list", () => {
    const emit = renderApp(null);
    fireEvent.click(screen.getByLabelText("Open Helper"));
    fireEvent.click(screen.getByLabelText("Conversations"));
    fireEvent.click(screen.getByLabelText("Back to chat"));
    fireEvent.click(screen.getByLabelText("Conversations"));
    expect(emit.mock.calls).toEqual([
      ["opened"],
      ["conversation-list-opened"],
      ["conversation-list-opened"],
    ]);
  });

  it("emits moved on the corner cycle", () => {
    const emit = renderApp(null);
    fireEvent.click(screen.getByLabelText("Open Helper"));
    fireEvent.click(screen.getByLabelText(/^Move to /));
    expect(emit.mock.calls).toEqual([["opened"], ["moved"]]);
  });

  it("emits message-sent when a composed message starts a turn", async () => {
    const emit = renderApp("tok");
    fireEvent.click(screen.getByLabelText("Open Helper"));
    const box = await screen.findByRole("textbox");
    fireEvent.change(box, { target: { value: "hi" } });
    fireEvent.keyDown(box, { key: "Enter" });
    await waitFor(() => expect(emit).toHaveBeenCalledWith("message-sent"));
    expect(emit.mock.calls.filter(([e]) => e === "message-sent")).toHaveLength(1);
  });

  it("emits message-sent for each send, including a type-to-interrupt", async () => {
    parkStreams = true;
    const emit = renderApp("tok");
    fireEvent.click(screen.getByLabelText("Open Helper"));
    const box = await screen.findByRole("textbox");
    fireEvent.change(box, { target: { value: "first" } });
    fireEvent.keyDown(box, { key: "Enter" });
    // The first turn never finishes, so this send goes down the interrupt path.
    fireEvent.change(box, { target: { value: "second" } });
    fireEvent.keyDown(box, { key: "Enter" });
    await waitFor(() =>
      expect(emit.mock.calls.filter(([e]) => e === "message-sent")).toHaveLength(2),
    );
  });
});
