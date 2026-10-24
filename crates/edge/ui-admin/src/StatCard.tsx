import type { ReactNode } from "react";

/** One KPI tile: a small label over a large value, with a loading placeholder.
 *  `className` adds a tone to the tile (`over` for a figure at its cap). */
export function StatCard({
  label,
  value,
  hint,
  isLoading,
  className,
}: {
  label: string;
  value: ReactNode;
  hint?: string;
  isLoading?: boolean;
  className?: string;
}) {
  return (
    <div className={className ? `stat-card ${className}` : "stat-card"}>
      <div className="stat-label">{label}</div>
      <div className="stat-value">{isLoading ? <span className="stat-skel" /> : value}</div>
      {hint && <div className="stat-hint">{hint}</div>}
    </div>
  );
}
