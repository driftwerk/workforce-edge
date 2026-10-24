import { describe, expect, it } from "vitest";

import type { PlanUsage } from "./api";
import { limitLabel, planBanner, usageHint, usageRows, usageText } from "./usage";

const snapshot: PlanUsage = {
  plan: { turns_per_month: 5000, knowledge_chunks: 200, memory_objects: 1000, agents: 5 },
  usage: {
    turns_this_month: 1234,
    active_users_this_month: 7,
    knowledge_chunks: 200,
    memory_objects: null,
    agents: 5,
  },
  over: ["knowledge_chunks", "agents"],
};

describe("planBanner", () => {
  it("is silent when no cap is reached", () => {
    expect(planBanner([])).toBeNull();
  });

  it("names one reached cap", () => {
    expect(planBanner(["turns_per_month"])).toBe(
      "This workspace has reached its monthly turn limit.",
    );
  });

  it("lists every reached cap", () => {
    expect(planBanner(["turns_per_month", "active_users", "memory_objects", "agents"])).toBe(
      "This workspace has reached its monthly turn, monthly active user, memory and agent limits.",
    );
  });

  it("falls back to the field name for a cap it does not know", () => {
    expect(limitLabel("seats_total")).toBe("seats total");
  });
});

describe("usageRows", () => {
  it("pairs each figure with its cap and whether it is reached", () => {
    const rows = usageRows(snapshot);
    expect(rows.map((r) => r.field)).toEqual([
      "turns_this_month",
      "active_users_this_month",
      "knowledge_chunks",
      "memory_objects",
      "agents",
    ]);
    const [turns, users, chunks, memories, agents] = rows;
    expect(usageText(turns)).toBe("1,234 / 5,000");
    expect(usageHint(turns)).toBe("");
    expect(usageText(users)).toBe("7");
    expect(usageHint(users)).toBe("no limit");
    expect(chunks.over).toBe(true);
    expect(usageText(chunks)).toBe("200 / 200");
    expect(usageHint(chunks)).toBe("at the plan limit");
    expect(usageText(memories)).toBe("—");
    expect(usageHint(memories)).toBe("could not be counted");
    expect(usageText(agents)).toBe("5 / 5");
    expect(usageHint(agents)).toBe("at the plan limit");
  });

  it("shows every figure as uncounted when the server had no usage", () => {
    const rows = usageRows({ plan: {}, usage: null, over: [] });
    expect(rows.every((r) => r.used === null && r.cap === null && !r.over)).toBe(true);
    expect(rows.map(usageText)).toEqual(["—", "—", "—", "—", "—"]);
  });
});
