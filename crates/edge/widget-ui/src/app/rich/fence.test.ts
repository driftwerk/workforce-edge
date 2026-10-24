import { describe, expect, it } from "vitest";

import { fenceInfo, richKind } from "./fence";

const preNode = (className: unknown, texts: string[] = ['{"a":1}']) => ({
  type: "element",
  tagName: "pre",
  children: [
    {
      type: "element",
      tagName: "code",
      properties: { className },
      children: texts.map((value) => ({ type: "text", value })),
    },
  ],
});

describe("fenceInfo", () => {
  it("extracts the language and joined payload text", () => {
    const info = fenceInfo(preNode(["language-wf-table"], ['{"a"', ":1}"]));
    expect(info).toEqual({ lang: "wf-table", text: '{"a":1}' });
  });

  it("returns null without a language class", () => {
    expect(fenceInfo(preNode(undefined))).toBeNull();
    expect(fenceInfo(preNode([]))).toBeNull();
    expect(fenceInfo(preNode(["hljs"]))).toBeNull();
  });

  it("returns null for non-fence pre shapes", () => {
    expect(fenceInfo(undefined)).toBeNull();
    expect(fenceInfo({ tagName: "pre", children: [] })).toBeNull();
    expect(
      fenceInfo({ tagName: "pre", children: [{ tagName: "span" }, { tagName: "code" }] }),
    ).toBeNull();
  });
});

describe("richKind", () => {
  const all = { tables: true, charts: true, forms: true, maps: true, diagrams: true, excalidraw: true, inline_images: true, images: true, uploads: [], upload_max_mb: 10, drawing: false };
  const without = (off: keyof typeof all) => ({ ...all, [off]: false });

  it("maps enabled fences to their renderer", () => {
    expect(richKind("wf-table", all)).toBe("table");
    expect(richKind("wf-chart", all)).toBe("chart");
    expect(richKind("wf-form", all)).toBe("form");
    expect(richKind("wf-map", all)).toBe("map");
    expect(richKind("wf-diagram", all)).toBe("diagram");
    expect(richKind("wf-image", all)).toBe("image");
  });

  it("returns null for disabled features and other languages", () => {
    expect(richKind("wf-table", without("tables"))).toBeNull();
    expect(richKind("wf-chart", without("charts"))).toBeNull();
    expect(richKind("wf-form", without("forms"))).toBeNull();
    expect(richKind("wf-map", without("maps"))).toBeNull();
    expect(richKind("wf-diagram", without("diagrams"))).toBeNull();
    expect(richKind("wf-image", without("images"))).toBeNull();
    expect(richKind("json", all)).toBeNull();
  });
});
