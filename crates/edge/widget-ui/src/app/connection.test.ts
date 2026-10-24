import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { EdgeConnection } from "./connection";

/** A scriptable stand-in for the browser WebSocket. */
class FakeWebSocket {
  static OPEN = 1;
  static instances: FakeWebSocket[] = [];
  readyState = FakeWebSocket.OPEN;
  sent: string[] = [];
  onmessage: ((e: { data: string }) => void) | null = null;
  onerror: (() => void) | null = null;
  onclose: (() => void) | null = null;

  constructor(
    public url: string,
    public protocols?: string[],
  ) {
    FakeWebSocket.instances.push(this);
  }

  send(data: string) {
    this.sent.push(data);
  }

  close() {
    this.readyState = 3;
    this.onclose?.();
  }

  /** Deliver a server frame. */
  frame(obj: Record<string, unknown>) {
    this.onmessage?.({ data: JSON.stringify(obj) });
  }

  /** The network dropped the socket (not a deliberate client close). */
  drop() {
    this.readyState = 3;
    this.onclose?.();
  }
}

/** Drain the microtask queue so generator plumbing settles — a burst of frames
 *  takes a few ticks per yield to flow through the async generator. */
const settle = async () => {
  for (let i = 0; i < 25; i++) await Promise.resolve();
};

const last = <T>(arr: T[]): T => arr[arr.length - 1];

/** Run the stream to completion in the background, collecting yields/error. */
function collect(gen: AsyncGenerator<string, void>) {
  const out: { yields: string[]; error: Error | null; done: boolean } = {
    yields: [],
    error: null,
    done: false,
  };
  void (async () => {
    try {
      for await (const acc of gen) out.yields.push(acc);
    } catch (e) {
      out.error = e as Error;
    } finally {
      out.done = true;
    }
  })();
  return out;
}

async function connectedStream(onProgress?: (label: string) => void) {
  const conn = new EdgeConnection("http://edge", "helper", "tok");
  const ctrl = new AbortController();
  const out = collect(
    conn.stream("hello", ctrl.signal, undefined, undefined, undefined, onProgress),
  );
  await settle();
  const ws = last(FakeWebSocket.instances);
  ws.frame({ type: "connected", session_id: "c1" });
  await settle();
  ws.frame({ type: "typing" });
  return { conn, ctrl, out, ws };
}

beforeEach(() => {
  vi.useFakeTimers();
  FakeWebSocket.instances = [];
  vi.stubGlobal("WebSocket", FakeWebSocket);
});

afterEach(() => {
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

describe("EdgeConnection stream", () => {
  it("accumulates tokens and completes on done", async () => {
    const { out, ws } = await connectedStream();
    ws.frame({ type: "token", content: "Hel" });
    ws.frame({ type: "token", content: "lo" });
    ws.frame({ type: "done", content: "Hello" });
    await settle();
    expect(last(out.yields)).toBe("Hello");
    expect(out.done).toBe(true);
    expect(out.error).toBeNull();
  });

  it("ignores ping frames", async () => {
    const { out, ws } = await connectedStream();
    ws.frame({ type: "ping" });
    await settle();
    expect(out.yields).toEqual([]);
    expect(out.done).toBe(false);
  });

  it("surfaces progress labels without touching the text", async () => {
    const labels: string[] = [];
    const { out, ws } = await connectedStream((l) => labels.push(l));
    ws.frame({ type: "progress", content: "Invoke query" });
    ws.frame({ type: "token", content: "hi" });
    ws.frame({ type: "done" });
    await settle();
    expect(labels).toEqual(["Invoke query"]);
    expect(last(out.yields)).toBe("hi");
  });

  it("ignores a stray resume that arrives outside recovery", async () => {
    // A `resume` only follows a drop; one arriving mid-stream without a drop is
    // a re-attach to a DIFFERENT turn on this conversation and must not seed our
    // accumulator (its `done` carries the authoritative full text anyway).
    const { out, ws } = await connectedStream();
    ws.frame({ type: "token", content: "real" });
    ws.frame({ type: "resume", content: "STALE OTHER TURN" });
    ws.frame({ type: "token", content: " answer" });
    ws.frame({ type: "done", content: "real answer" });
    await settle();
    expect(last(out.yields)).toBe("real answer");
    expect(out.yields.some((y) => y.includes("STALE"))).toBe(false);
  });

  it("recovers a dropped mid-turn stream through reconnect + resume", async () => {
    const { out, ws } = await connectedStream();
    ws.frame({ type: "token", content: "beg" });
    await settle();
    ws.drop();
    await settle();
    expect(out.done).toBe(false);
    // The backoff reconnect opens a fresh socket; the server replays the turn.
    await vi.advanceTimersByTimeAsync(600);
    const ws2 = last(FakeWebSocket.instances);
    expect(ws2).not.toBe(ws);
    ws2.frame({ type: "connected", session_id: "c1" });
    // The server always leads a replay with `typing`, then `resume`.
    ws2.frame({ type: "typing" });
    ws2.frame({ type: "resume", content: "began" });
    ws2.frame({ type: "token", content: "!" });
    ws2.frame({ type: "done", content: "began!" });
    await settle();
    expect(last(out.yields)).toBe("began!");
    expect(out.error).toBeNull();
    expect(out.done).toBe(true);
  });

  it("keeps a reconnect alive when the replay carries no text yet (tool phase)", async () => {
    // A drop before the first answer token replays `typing` + an EMPTY `resume`
    // — the byte-silent stretch this feature targets. The empty replay still
    // has to end the recovery countdown, or a healthy re-attached socket is
    // abandoned. (`resume` is sent unconditionally for exactly this reason.)
    const { out, ws } = await connectedStream();
    ws.drop();
    await vi.advanceTimersByTimeAsync(600);
    const ws2 = last(FakeWebSocket.instances);
    ws2.frame({ type: "connected", session_id: "c1" });
    ws2.frame({ type: "typing" });
    ws2.frame({ type: "resume", content: "" });
    ws2.frame({ type: "progress", content: "Invoke query" });
    await vi.advanceTimersByTimeAsync(4_100);
    await settle();
    expect(out.done).toBe(false);
    expect(out.error).toBeNull();
    // ...and the live answer still lands on the re-attached socket.
    ws2.frame({ type: "done", content: "the answer" });
    await settle();
    expect(last(out.yields)).toBe("the answer");
    expect(out.done).toBe(true);
  });

  it("fails with connection-lost when the reconnect brings no resume", async () => {
    const { out, ws } = await connectedStream();
    ws.drop();
    await vi.advanceTimersByTimeAsync(600);
    const ws2 = last(FakeWebSocket.instances);
    ws2.frame({ type: "connected", session_id: "c1" });
    // No replay follows the reconnect — the post-connect grace expires.
    await vi.advanceTimersByTimeAsync(4_100);
    await settle();
    expect(out.done).toBe(true);
    expect(out.error?.message).toBe("connection-lost");
  });

  it("fails with connection-lost when no reconnect happens at all", async () => {
    const { out, ws } = await connectedStream();
    ws.drop();
    await vi.advanceTimersByTimeAsync(26_000);
    await settle();
    expect(out.done).toBe(true);
    expect(out.error?.message).toBe("connection-lost");
  });

  it("a deliberate close fails the stream immediately", async () => {
    const { conn, out } = await connectedStream();
    conn.close();
    await settle();
    expect(out.done).toBe(true);
    expect(out.error?.message).toBe("connection-lost");
  });
});
