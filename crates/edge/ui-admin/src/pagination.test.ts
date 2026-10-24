import { describe, expect, it } from "vitest";

import { mergePages } from "./pagination";

interface Row {
  id: number;
}
const id = (r: Row) => r.id;

describe("mergePages", () => {
  it("returns the incoming page when nothing is accumulated", () => {
    expect(mergePages<Row>([], [{ id: 1 }, { id: 2 }], id)).toEqual([{ id: 1 }, { id: 2 }]);
  });

  it("appends a following page in order", () => {
    const acc = mergePages<Row>([{ id: 1 }, { id: 2 }], [{ id: 3 }, { id: 4 }], id);
    expect(acc.map(id)).toEqual([1, 2, 3, 4]);
  });

  it("skips keys already present so an overlapping page never duplicates", () => {
    const acc = mergePages<Row>([{ id: 1 }, { id: 2 }], [{ id: 2 }, { id: 3 }], id);
    expect(acc.map(id)).toEqual([1, 2, 3]);
  });

  it("accumulates across several pages", () => {
    let acc: Row[] = [];
    acc = mergePages(acc, [{ id: 1 }, { id: 2 }], id);
    acc = mergePages(acc, [{ id: 3 }], id);
    acc = mergePages(acc, [{ id: 3 }, { id: 4 }], id);
    expect(acc.map(id)).toEqual([1, 2, 3, 4]);
  });
});
