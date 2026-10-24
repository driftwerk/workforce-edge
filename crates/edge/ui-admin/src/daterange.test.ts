import { describe, expect, it } from "vitest";

import {
  activePreset,
  bucketFor,
  dateInputToIso,
  defaultRange,
  isoToDateInput,
  rangeForDays,
} from "./daterange";

describe("preset math", () => {
  it("round-trips each preset span back to its label", () => {
    expect(activePreset(rangeForDays(1))).toBe("24h");
    expect(activePreset(rangeForDays(7))).toBe("7d");
    expect(activePreset(rangeForDays(30))).toBe("30d");
    expect(activePreset(rangeForDays(90))).toBe("90d");
  });
  it("reports a non-preset span as custom", () => {
    expect(activePreset(rangeForDays(45))).toBe("custom");
  });
  it("defaults to 30 days", () => {
    expect(activePreset(defaultRange())).toBe("30d");
  });
});

describe("date-input conversion", () => {
  it("ISO → YYYY-MM-DD", () => {
    expect(isoToDateInput("2026-07-10T12:34:56Z")).toBe("2026-07-10");
  });
  it("YYYY-MM-DD → ISO, start vs inclusive end of day", () => {
    expect(dateInputToIso("2026-07-10")).toBe("2026-07-10T00:00:00.000Z");
    expect(dateInputToIso("2026-07-10", true)).toBe("2026-07-10T23:59:59.000Z");
  });
});

describe("bucketFor", () => {
  it("buckets short windows by hour, longer ones by day", () => {
    expect(bucketFor(rangeForDays(1))).toBe("hour");
    expect(bucketFor(rangeForDays(2))).toBe("hour");
    expect(bucketFor(rangeForDays(7))).toBe("day");
    expect(bucketFor(rangeForDays(30))).toBe("day");
  });
});
