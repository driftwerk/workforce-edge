import { afterEach, describe, expect, it, vi } from "vitest";

import { analyticsQs, describeErasure, erasureComplete, forgetCustomer, type Erasure } from "./api";

describe("analyticsQs", () => {
  it("is empty when nothing is set", () => {
    expect(analyticsQs({})).toBe("");
    expect(analyticsQs({ agent: "", owner: undefined })).toBe("");
  });

  it("omits empty values but keeps set ones", () => {
    expect(analyticsQs({ from: "2026-07-01T00:00:00Z", agent: "", owner: undefined })).toBe(
      "?from=2026-07-01T00%3A00%3A00Z",
    );
  });

  it("serialises the full filter", () => {
    const qs = analyticsQs({
      from: "2026-07-01T00:00:00Z",
      to: "2026-07-10T00:00:00Z",
      agent: "support",
    });
    const p = new URLSearchParams(qs.slice(1));
    expect(p.get("from")).toBe("2026-07-01T00:00:00Z");
    expect(p.get("to")).toBe("2026-07-10T00:00:00Z");
    expect(p.get("agent")).toBe("support");
  });

  it("encodes owner keys with separators and reserved chars", () => {
    const owner = "ext:https://kc.example/realmabc-123";
    const qs = analyticsQs({ owner });
    expect(qs).toContain("%1F"); // U+001F separator is percent-encoded
    expect(new URLSearchParams(qs.slice(1)).get("owner")).toBe(owner);
  });

  it("appends extras and drops null/undefined ones", () => {
    expect(analyticsQs({}, { bucket: "day" })).toBe("?bucket=day");
    expect(analyticsQs({}, { limit: 50, cursor: null })).toBe("?limit=50");
    expect(new URLSearchParams(analyticsQs({ agent: "a" }, { limit: 25 }).slice(1)).get("limit")).toBe("25");
  });
});

describe("describeErasure", () => {
  const part = (erased: number, failures: string[] = []) => ({ erased, failures });
  const done: Erasure = {
    memories: part(3),
    conversations: part(2),
    analytics: part(5),
    files: part(0),
  };

  it("counts every part when all of it went", () => {
    expect(erasureComplete(done)).toBe(true);
    expect(describeErasure(done)).toBe(
      "Forgotten: 3 memories, 2 conversations, 5 analytics rows, 0 files.",
    );
  });

  it("names only the parts that failed, and never calls it done", () => {
    const partial = { ...done, memories: part(0, ["agent help: timed out"]) };
    expect(erasureComplete(partial)).toBe(false);
    expect(describeErasure(partial)).toBe(
      "Not fully forgotten. Failed: memories (agent help: timed out). Try again.",
    );
  });
});

describe("forgetCustomer", () => {
  afterEach(() => vi.unstubAllGlobals());
  const answer = (status: number, body: unknown) =>
    vi.stubGlobal("fetch", vi.fn(async () => new Response(JSON.stringify(body), { status })));
  const partial: Erasure = {
    memories: { erased: 0, failures: ["agent help: timed out"] },
    conversations: { erased: 2, failures: [] },
    analytics: { erased: 5, failures: [] },
    files: { erased: 0, failures: [] },
  };

  it("hands back a partial erasure so its failures can be shown", async () => {
    answer(500, partial);
    await expect(forgetCustomer("ext:iss\u001fsub")).resolves.toEqual(partial);
  });

  it("rejects with the server's reason when nothing was erased", async () => {
    answer(409, { error: "this customer used an agent in the last 15 minutes" });
    await expect(forgetCustomer("ext:iss\u001fsub")).rejects.toThrow(
      "this customer used an agent in the last 15 minutes",
    );
  });

  it("falls back to the status when the server gave no reason", async () => {
    answer(500, {});
    await expect(forgetCustomer("ext:iss\u001fsub")).rejects.toThrow("Forget failed (500)");
  });
});
