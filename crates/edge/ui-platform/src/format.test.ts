import { describe, expect, it } from "vitest";

import { actionLabel, llmPill, principalLabel, relative, statusPill } from "./format";

describe("principalLabel", () => {
  it("tells a service from an operator", () => {
    expect(principalLabel("ops@marrilo.com")).toEqual({ name: "ops@marrilo.com", service: false });
    expect(principalLabel("service:billing")).toEqual({ name: "billing", service: true });
  });
});

describe("actionLabel", () => {
  it("names the known actions and passes an unknown one through", () => {
    expect(actionLabel("tenant.suspend")).toBe("suspended tenant");
    expect(actionLabel("tenant.rescue")).toBe("opened admin (rescue)");
    expect(actionLabel("something.new")).toBe("something.new");
  });
});

describe("relative", () => {
  const now = new Date("2026-10-09T12:00:00Z");

  it("rounds to the nearest unit", () => {
    expect(relative("2026-10-09T11:59:40Z", now)).toBe("just now");
    expect(relative("2026-10-09T11:55:00Z", now)).toBe("5 min ago");
    expect(relative("2026-10-09T09:00:00Z", now)).toBe("3 h ago");
    expect(relative("2026-10-07T12:00:00Z", now)).toBe("2 d ago");
  });

  it("falls back to a date past two weeks, and to the text when unparseable", () => {
    expect(relative("2026-09-01T12:00:00Z", now)).toMatch(/2026/);
    expect(relative("not a date", now)).toBe("not a date");
  });
});

describe("statusPill", () => {
  it("ranks suspended over not-serving over active", () => {
    expect(statusPill({ status: "suspended", serving: false, unavailable: "Billing overdue" })).toMatchObject({
      label: "suspended",
      tone: "bad",
      title: "Billing overdue",
    });
    expect(statusPill({ status: "active", serving: false, unavailable: "boot failed" })).toMatchObject({
      label: "not serving",
      tone: "warn",
      title: "boot failed",
    });
    expect(statusPill({ status: "active", serving: true, unavailable: null })).toMatchObject({ label: "active", tone: "ok" });
  });
});

describe("llmPill", () => {
  it("reads the key state, unknown while not served", () => {
    expect(llmPill(null).label).toBe("unknown");
    expect(llmPill({ state: "ok", checked_at: "2026-10-09T00:00:00Z", message: "" }).tone).toBe("ok");
    expect(llmPill({ state: "failed", checked_at: "", message: "Rejected" })).toMatchObject({ tone: "bad", title: "Rejected" });
    expect(llmPill({ state: "unset", checked_at: "", message: "" }).label).toBe("no key");
  });
});
