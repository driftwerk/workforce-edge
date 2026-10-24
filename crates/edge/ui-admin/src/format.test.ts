import { describe, expect, it } from "vitest";

import { formatDuration, formatRating, formatTokens, money, percent } from "./format";

describe("money", () => {
  it("shows 2 dp for normal amounts", () => {
    expect(money(1.2)).toBe("$1.20");
    expect(money(12.3456)).toBe("$12.35");
  });
  it("shows 4 dp for sub-cent amounts", () => {
    expect(money(0.005)).toBe("$0.0050");
    expect(money(0.0001)).toBe("$0.0001");
  });
  it("renders a clean zero", () => {
    expect(money(0)).toBe("$0.00");
    expect(money(Number.NaN)).toBe("$0.00");
  });
});

describe("formatTokens", () => {
  it("passes small counts through", () => {
    expect(formatTokens(0)).toBe("0");
    expect(formatTokens(999)).toBe("999");
  });
  it("uses k and M suffixes", () => {
    expect(formatTokens(1500)).toBe("1.5k");
    expect(formatTokens(2_500_000)).toBe("2.5M");
  });
});

describe("percent", () => {
  it("formats a 0..1 ratio to one dp", () => {
    expect(percent(0)).toBe("0.0%");
    expect(percent(0.1234)).toBe("12.3%");
    expect(percent(1)).toBe("100.0%");
  });
});

describe("formatRating", () => {
  it("maps +1 to a positive pill", () => {
    expect(formatRating(1)).toEqual({ symbol: "👍", label: "Positive", cls: "pill-ok" });
  });
  it("maps -1 (and any non-positive) to a negative pill", () => {
    expect(formatRating(-1)).toEqual({ symbol: "👎", label: "Negative", cls: "pill-bad" });
    expect(formatRating(0).cls).toBe("pill-bad");
  });
});

describe("formatDuration", () => {
  it("handles empty/zero", () => {
    expect(formatDuration(0)).toBe("—");
    expect(formatDuration(-5)).toBe("—");
  });
  it("scales ms → s → m → h", () => {
    expect(formatDuration(500)).toBe("500ms");
    expect(formatDuration(1500)).toBe("1s");
    expect(formatDuration(65_000)).toBe("1m 5s");
    expect(formatDuration(3_600_000)).toBe("1h");
  });
});
