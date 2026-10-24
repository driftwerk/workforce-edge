import { describe, expect, it } from "vitest";

import type { GapRow, GradesSummary } from "./api";
import { buildGapReport } from "./gapReport";

function gap(topic: string, remedy: string | null, count = 1): GapRow {
  return {
    topic,
    count,
    unanswerable: 1,
    incorrect: 0,
    remedy,
    example_reason: `why ${topic}`,
    last_seen: "2026-07-11T00:00:00Z",
    example_questions: [`a question about ${topic}`],
  };
}

const range = { from: "2026-07-04T09:00:00Z", to: "2026-07-11T09:00:00Z" };
const summary: GradesSummary = {
  graded: 10,
  good: 6,
  poor: 2,
  incorrect: 1,
  unanswerable: 1,
};
const agent = { name: "Fabrikam Assistant", id: "fabrikam-assistant" };

describe("buildGapReport", () => {
  it("partitions gaps by remedy into labelled sections", () => {
    const md = buildGapReport(
      [
        gap("sessional shifts", "knowledge"),
        gap("wrong tool used", "behaviour"),
        gap("cancel a shift", "tool"),
        gap("mystery topic", null),
      ],
      summary,
      range,
      agent,
    );
    expect(md).toContain("Knowledge gaps");
    expect(md).toContain("sessional shifts");
    expect(md).toContain("Behaviour gaps");
    expect(md).toContain("wrong tool used");
    expect(md).toContain("Tool gaps");
    expect(md).toContain("cancel a shift");
    expect(md).toContain("Other / unclassified");
    expect(md).toContain("mystery topic");
    // the example reason is carried through for Claude Code to research
    expect(md).toContain("why sessional shifts");
    // the actual failing questions are included so Claude Code knows what to cover
    expect(md).toContain("Example questions");
    expect(md).toContain("a question about sessional shifts");
  });

  it("includes the grading summary and the date range", () => {
    const md = buildGapReport([gap("x", "knowledge")], summary, range, agent);
    expect(md).toContain("Graded: 10");
    expect(md).toContain("2026-07-04 → 2026-07-11");
  });

  it("omits sections with no gaps", () => {
    const md = buildGapReport([gap("x", "knowledge")], summary, range, agent);
    expect(md).toContain("Knowledge gaps");
    expect(md).not.toContain("Tool gaps");
    expect(md).not.toContain("Behaviour gaps");
  });

  it("handles no summary and no gaps without throwing", () => {
    const md = buildGapReport([], null, range, agent);
    expect(md).toContain("no grading summary");
    expect(md).not.toContain("### ");
  });

  it("labels the brief with the selected agent and its training path", () => {
    const md = buildGapReport([gap("x", "knowledge")], summary, range, agent);
    expect(md).toContain("# Fabrikam Assistant — answer gaps to fix");
    expect(md).toContain("**Fabrikam Assistant** agent");
    expect(md).toContain("agents/fabrikam-assistant/training/*.md");
    // no hard-coded customer name leaks into other installs' briefs
    expect(md).not.toContain("Harbor");
  });

  it("falls back to neutral wording when no agent is selected", () => {
    const md = buildGapReport([gap("x", "knowledge")], summary, range, {
      name: "",
      id: "",
    });
    expect(md).toContain("# the assistant — answer gaps to fix");
    expect(md).toContain("the agent's training docs");
    expect(md).not.toContain("Harbor");
  });
});
