import type { ReactNode } from "react";

export interface Column<T> {
  key: string;
  header: ReactNode;
  cell: (row: T) => ReactNode;
  align?: "left" | "right";
}

interface Props<T> {
  columns: Column<T>[];
  rows: T[];
  rowKey: (row: T) => string | number;
  onRowClick?: (row: T) => void;
  empty?: ReactNode;
  isLoading?: boolean;
  /** Optional footer slot — typically a "Load more" button driving cursor
   *  pagination from the calling page. */
  footer?: ReactNode;
  /** A minimum table width forcing horizontal scroll inside the wrapper (never
   *  the page body) when the columns don't fit. */
  minWidth?: number;
}

/** A generic, config-driven table matching the admin's `table.audit` styling,
 *  with row-click drill-down, a footer slot, and loading/empty states. */
export function DataTable<T>({
  columns,
  rows,
  rowKey,
  onRowClick,
  empty,
  isLoading,
  footer,
  minWidth,
}: Props<T>) {
  if (isLoading && rows.length === 0) {
    return <p className="spinner">Loading…</p>;
  }
  if (rows.length === 0) {
    return <p className="muted table-empty">{empty ?? "No data in range."}</p>;
  }
  return (
    <div className="table-wrap">
      <div className="table-scroll">
        <table className="audit" style={minWidth ? { minWidth } : undefined}>
          <thead>
            <tr>
              {columns.map((c) => (
                <th key={c.key} className={c.align === "right" ? "num" : undefined}>
                  {c.header}
                </th>
              ))}
            </tr>
          </thead>
          <tbody>
            {rows.map((row) => (
              <tr
                key={rowKey(row)}
                className={onRowClick ? "clickable" : undefined}
                onClick={onRowClick ? () => onRowClick(row) : undefined}
              >
                {columns.map((c) => (
                  <td key={c.key} className={c.align === "right" ? "num" : undefined}>
                    {c.cell(row)}
                  </td>
                ))}
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      {footer && <div className="table-foot">{footer}</div>}
    </div>
  );
}
