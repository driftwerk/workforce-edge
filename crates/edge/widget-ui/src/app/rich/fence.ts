// Inspect a react-markdown `pre` hast node for a wf-* fenced block. Structural
// types only — no dependency on the hast packages.

import type { WidgetFeatures } from "../../protocol";

interface HastNode {
  type?: string;
  tagName?: string;
  properties?: { className?: unknown };
  children?: HastNode[];
  value?: string;
}

export interface FenceInfo {
  lang: string;
  text: string;
}

/** The fence language + raw payload of a `<pre>` whose sole child is a
 *  `<code class="language-…">`, or `null` for anything else. */
export function fenceInfo(node: unknown): FenceInfo | null {
  const pre = node as HastNode | undefined;
  const code = pre?.children?.length === 1 ? pre.children[0] : undefined;
  if (!code || code.tagName !== "code") return null;
  const classes = code.properties?.className;
  if (!Array.isArray(classes)) return null;
  const lang = classes
    .find((c): c is string => typeof c === "string" && c.startsWith("language-"))
    ?.slice("language-".length);
  if (!lang) return null;
  const text = (code.children ?? [])
    .filter((c) => c.type === "text" && typeof c.value === "string")
    .map((c) => c.value)
    .join("");
  return { lang, text };
}

/** Which rich renderer (if any) handles this fence, given the agent's flags. */
export function richKind(
  lang: string,
  features: WidgetFeatures,
): "table" | "chart" | "form" | "map" | "diagram" | "image" | null {
  if (lang === "wf-table" && features.tables) return "table";
  if (lang === "wf-chart" && features.charts) return "chart";
  if (lang === "wf-form" && features.forms) return "form";
  if (lang === "wf-map" && features.maps) return "map";
  if (lang === "wf-diagram" && features.diagrams) return "diagram";
  if (lang === "wf-image" && features.images) return "image";
  return null;
}
