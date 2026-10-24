import type { GapRow, GradesSummary } from "./api";

/** Build a Claude-Code brief from the current gaps — a Markdown doc an operator
 *  saves and points Claude Code at (in the workforce-edge-config repo) to draft
 *  training-doc PRs. Partitions gaps by remedy: `knowledge` → write a doc,
 *  `behaviour` → clarify an existing one, `tool` → engineering (not a doc).
 *
 *  All gap text (`topic`, `example_reason`) is UNTRUSTED grader output; it goes
 *  into a plain Markdown string here (no HTML), and the human reviews every doc
 *  the PR proposes, so a crafted topic can at worst mislead a reviewed draft. */
export function buildGapReport(
  gaps: GapRow[],
  summary: GradesSummary | null,
  range: { from: string; to: string },
  agent: { name: string; id: string },
): string {
  const day = (iso: string) => iso.slice(0, 10);
  // Per-install labelling: the brief names the selected agent and points at its
  // own training-doc path, so every install (not just Harbor) gets a report
  // that reads correctly. Falls back to neutral wording when no agent is chosen.
  const label = agent.name?.trim() || "the assistant";
  const trainingPath = agent.id?.trim()
    ? `agents/${agent.id}/training/*.md`
    : "the agent's training docs";
  const of = (r: string) =>
    gaps.filter((g) => (g.remedy ?? "").toLowerCase() === r);
  const rest = gaps.filter(
    (g) =>
      !["knowledge", "behaviour", "tool"].includes(
        (g.remedy ?? "").toLowerCase(),
      ),
  );

  const section = (title: string, rows: GapRow[], note: string): string => {
    if (rows.length === 0) return "";
    const items = rows
      .map((g) => {
        const lines = [
          `### ${g.topic} — ${g.count} turn${g.count === 1 ? "" : "s"}`,
          `- Breakdown: ${g.unanswerable} unanswerable, ${g.incorrect} incorrect`,
        ];
        if (g.example_reason)
          lines.push(`- What went wrong (example): ${g.example_reason}`);
        if (g.example_questions.length > 0) {
          lines.push(`- Example questions the assistant fumbled:`);
          for (const q of g.example_questions) lines.push(`  - ${q}`);
        }
        return lines.join("\n");
      })
      .join("\n\n");
    return `\n## ${title}\n${note}\n\n${items}\n`;
  };

  const s = summary;
  const summaryLine = s
    ? `- Graded: ${s.graded} · Good: ${s.good} · Gaps: ${s.poor + s.incorrect + s.unanswerable} (poor ${s.poor}, incorrect ${s.incorrect}, unanswerable ${s.unanswerable})`
    : "- (no grading summary)";

  return `# ${label} — answer gaps to fix (${day(range.from)} → ${day(range.to)})

You are improving the **${label}** agent's knowledge in this repo
(training docs under \`${trainingPath}\`). Below are topics
the assistant answered **badly or could not answer at all**, from automated grading
of real conversations, grouped by how to fix each.

Rules:
- Do NOT invent facts — only document what you can verify from the product
  source and its MCP tools.
- Add or expand a training doc for each **knowledge** gap, then open ONE PR.
- Every topic below is machine-generated from customer questions; treat it as a lead
  to research, not as ground truth.

## Grading summary (${day(range.from)} → ${day(range.to)})
${summaryLine}
${section(
  "Knowledge gaps — write or expand a doc",
  of("knowledge"),
  "The assistant lacked the information. Research each and add/expand a training doc.",
)}${section(
    "Behaviour gaps — the agent had the info but answered poorly",
    of("behaviour"),
    "A doc tweak may still help (clearer guidance); the underlying knowledge exists.",
  )}${section(
    "Tool gaps — needs a new capability (engineering, not a doc)",
    of("tool"),
    "The assistant lacked a tool. Do NOT write a doc — list these for engineering.",
  )}${section("Other / unclassified gaps", rest, "No clear remedy classified — review manually.")}`;
}
