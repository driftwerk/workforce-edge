// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

import WfTable, { columnStyle, displayCell } from "./WfTable";
import { validateTable, type TablePage, type TableSpec } from "./spec";
import { EdgeApiContext, type EdgeApi } from "./apiContext";
import * as api from "../api";

vi.mock("../api", async (orig) => ({
  ...(await orig<typeof import("../api")>()),
  fetchTablePage: vi.fn(),
}));

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

function validSpec(raw: unknown): TableSpec {
  const v = validateTable(raw);
  if (!v.ok) throw new Error(v.error);
  return v.spec;
}

const cellTexts = () =>
  Array.from(document.querySelectorAll("tbody td")).map((td) => td.textContent);

describe("displayCell / columnStyle", () => {
  it("formats numeric columns and leaves the rest as raw text", () => {
    expect(displayCell(523797.33, "number", { format: "number" })).toBe("523,797.33");
    expect(displayCell("523797.33", "number", { format: "number" })).toBe("523,797.33");
    expect(displayCell(1024773, "number", { format: "id" })).toBe("1024773");
    expect(displayCell("2026-07-01", "date")).toBe("2026-07-01");
    expect(displayCell(true, "string")).toBe("true");
    expect(displayCell(null, "number")).toBe("");
  });

  it("uses the declared format, else infers from label and values", () => {
    expect(columnStyle({ label: "Fee", format: "currency", unit: "£" }, "number", [[1]], 0)).toEqual({
      format: "currency",
      unit: "£",
    });
    expect(columnStyle({ label: "Lead ID" }, "number", [[1024773]], 0)).toEqual({
      format: "id",
      unit: undefined,
    });
    expect(columnStyle({ label: "Year" }, "string", [["2024"]], 0)).toBeUndefined();
    expect(columnStyle(undefined, "number", [[2024], [2025]], 0)).toEqual({ format: "id", unit: undefined });
  });
});

describe("WfTable inline", () => {
  it("renders separators, currency decimals and untouched identifiers", () => {
    render(
      <WfTable
        spec={validSpec({
          columns: [
            "Lead ID",
            { key: "fee", label: "Total fee", format: "currency", unit: "£" },
            "Days to complete",
            "Year",
          ],
          rows: [
            [1024773, 523797.33, 157.3, 2026],
            [1024774, 9400, 74, 2025],
          ],
        })}
      />,
    );
    expect(cellTexts()).toEqual([
      "1024773", "£523,797.33", "157.3", "2026",
      "1024774", "£9,400.00", "74", "2025",
    ]);
  });

  it("still sorts by value, not by formatted text", () => {
    render(
      <WfTable
        spec={validSpec({
          columns: ["Name", "Amount"],
          rows: [["a", 900], ["b", 1200000], ["c", 15000]],
        })}
      />,
    );
    // Numeric columns sort descending first; a text sort would put "900" above "15,000".
    fireEvent.click(screen.getByRole("button", { name: "Sort by Amount" }));
    expect(cellTexts().filter((_, i) => i % 2 === 1)).toEqual(["1,200,000", "15,000", "900"]);
    fireEvent.click(screen.getByRole("button", { name: "Sort by Amount" }));
    expect(cellTexts().filter((_, i) => i % 2 === 1)).toEqual(["900", "15,000", "1,200,000"]);
  });

  it("finds a row by its formatted or raw number", () => {
    const rows = Array.from({ length: 10 }, (_, i) => [`row ${i}`, (i + 1) * 100000]);
    render(<WfTable spec={validSpec({ columns: ["Name", "Amount"], rows })} />);
    const search = screen.getByLabelText("Search table");
    fireEvent.change(search, { target: { value: "300,000" } });
    expect(cellTexts()).toEqual(["row 2", "300,000"]);
    fireEvent.change(search, { target: { value: "300000" } });
    expect(cellTexts()).toEqual(["row 2", "300,000"]);
  });
});

describe("WfTable artifact", () => {
  const ARTIFACT = "art_0123456789abcdef0123456789abcdef";
  const edgeApi: EdgeApi = { base: "http://edge", agent: "a", token: { current: "t" } } as EdgeApi;

  it("applies the column overlay's label and format to the server's columns", async () => {
    const page: TablePage = {
      columns: [
        { key: "lead_id", label: "lead_id", type: "number" },
        { key: "total_fee", label: "total_fee", type: "number" },
      ],
      rows: [[1024773, 523797.33]],
      total: 1,
    };
    vi.mocked(api.fetchTablePage).mockResolvedValue(page);
    render(
      <EdgeApiContext.Provider value={edgeApi}>
        <WfTable
          spec={validSpec({
            artifact_id: ARTIFACT,
            columns: [{ key: "total_fee", label: "Fee", format: "currency", unit: "£" }],
          })}
        />
      </EdgeApiContext.Provider>,
    );
    await waitFor(() => expect(cellTexts()).toEqual(["1024773", "£523,797.33"]));
    expect(screen.getByRole("button", { name: "Sort by Fee" })).toBeTruthy();
  });
});
