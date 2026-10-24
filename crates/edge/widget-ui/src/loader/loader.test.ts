// @vitest-environment jsdom
//
// The loader's host-page side: its event relay (a validated `event` message from
// the iframe is surfaced to the host page as `wf-embed-event`; anything else never
// reaches it) and where it docks the iframe on the host viewport.
import { afterEach, describe, expect, it, vi } from "vitest";

import { PROTOCOL, type ThemeTokens } from "../protocol";

const BASE = "https://edge.example";
const HOST = "https://host.example";
const NONCE = "nonce-1";

const theme = { title: "Helper", position: "right", draggable: false } as ThemeTokens;

async function bootLoader(overrides: Partial<ThemeTokens> = {}) {
  const posted = vi.spyOn(window, "postMessage").mockImplementation(() => {});
  const t = { ...theme, ...overrides };
  window.__WF_EDGE__ = {
    agent: "assistant",
    base: BASE,
    origins: [HOST],
    theme: t,
    theme_dark: t,
    welcome: "",
    nonce: NONCE,
  } as never;
  window.__WF_EDGE_LOADED__ = false;
  vi.resetModules();
  await import("./loader");
  return posted;
}

function fromFrame(data: Record<string, unknown>, nonce = NONCE, origin = BASE) {
  window.dispatchEvent(
    new MessageEvent("message", { data: { proto: PROTOCOL, nonce, ...data }, origin }),
  );
}

const hostEvents = (posted: ReturnType<typeof vi.spyOn>) =>
  posted.mock.calls.filter(([m]) => (m as { type?: string })?.type === "wf-embed-event");

afterEach(() => {
  // The loader's own teardown path: removes its listeners and iframe, so each
  // test boots exactly one live loader.
  window.dispatchEvent(
    new MessageEvent("message", { data: { type: "wf-embed-destroy" }, origin: HOST }),
  );
  vi.restoreAllMocks();
});

describe("the loader's usage-event relay", () => {
  it("relays a valid widget event to the host page with the agent attached", async () => {
    const posted = await bootLoader();
    fromFrame({ type: "event", event: "message-sent" });
    expect(hostEvents(posted)).toEqual([
      [{ type: "wf-embed-event", agent: "assistant", event: "message-sent" }, "*"],
    ]);
  });

  it("drops an event name outside the WidgetEvent union", async () => {
    const posted = await bootLoader();
    fromFrame({ type: "event", event: "clicked-everything" });
    fromFrame({ type: "event", event: 42 });
    fromFrame({ type: "event" });
    expect(hostEvents(posted)).toEqual([]);
  });

  it("ignores an event with the wrong nonce or origin", async () => {
    const posted = await bootLoader();
    fromFrame({ type: "event", event: "opened" }, "other-nonce");
    fromFrame({ type: "event", event: "opened" }, NONCE, "https://evil.example");
    expect(hostEvents(posted)).toEqual([]);
  });
});

describe("where the loader docks the iframe", () => {
  const frame = () => document.querySelector("iframe")!.style;

  it("sits flush to the configured corner by default", async () => {
    await bootLoader();
    expect([frame().right, frame().bottom]).toEqual(["0px", "0px"]);
    expect([frame().top, frame().left]).toEqual(["auto", "auto"]);
  });

  it("starts inset by the theme's dock offset, so a host's sticky bar stays clear", async () => {
    await bootLoader({ offset_x: 12, offset_y: 72 });
    expect([frame().right, frame().bottom]).toEqual(["12px", "72px"]);
    expect([frame().top, frame().left]).toEqual(["auto", "auto"]);
  });

  it("keeps the inset on the left dock too", async () => {
    await bootLoader({ position: "left", offset_y: 72 });
    expect([frame().left, frame().bottom]).toEqual(["0px", "72px"]);
  });
});
