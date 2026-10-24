import { describe, expect, it } from "vitest";

import { usageHint, usageRows, usageText } from "./usage";

const plan = { turns_per_month: 5000, knowledge_chunks: 200, memory_objects: 1000, agents: 5 };
const usage = {
  turns_this_month: 1234,
  active_users_this_month: 7,
  knowledge_chunks: 200,
  memory_objects: null,
  agents: 3,
};

describe("usageRows", () => {
  it("pairs each figure with its cap and says when the cap is reached", () => {
    const rows = usageRows(plan, usage);
    expect(rows.map((r) => r.field)).toEqual([
      "turns_this_month",
      "active_users_this_month",
      "knowledge_chunks",
      "memory_objects",
      "agents",
    ]);
    expect(rows[0]).toMatchObject({ used: 1234, cap: 5000, over: false });
    expect(rows[0].ratio).toBeCloseTo(0.2468);
    expect(rows[1]).toMatchObject({ used: 7, cap: null, over: false, ratio: null });
    expect(rows[2]).toMatchObject({ used: 200, cap: 200, over: true, ratio: 1 });
    expect(rows[3]).toMatchObject({ used: null, cap: 1000, over: false, ratio: null });
    expect(rows[4]).toMatchObject({ used: 3, cap: 5, over: false, ratio: 0.6 });
  });

  it("has no figures without a count", () => {
    for (const row of usageRows(plan, null)) expect(row.used).toBeNull();
  });
});

describe("usageText and usageHint", () => {
  const rows = usageRows(plan, usage);

  it("formats used over cap, used alone when unbounded, a dash when uncounted", () => {
    expect(usageText(rows[0])).toBe("1,234 / 5,000");
    expect(usageText(rows[1])).toBe("7");
    expect(usageText(rows[3])).toBe("—");
  });

  it("hints at the cap, the share of it, or why there is no figure", () => {
    expect(usageHint(rows[0], true)).toBe("25% of the cap");
    expect(usageHint(rows[1], true)).toBe("no limit");
    expect(usageHint(rows[2], true)).toBe("at the plan limit");
    expect(usageHint(rows[3], true)).toBe("could not be counted");
    expect(usageHint(usageRows({}, null)[0], false)).toBe("not served");
    expect(usageHint(usageRows({}, { ...usage, memory_objects: null })[3], true)).toBe("counted only under a cap");
  });
});
