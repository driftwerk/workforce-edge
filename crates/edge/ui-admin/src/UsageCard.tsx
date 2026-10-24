import * as api from "./api";
import { StatCard } from "./StatCard";
import { usageHint, usageRows, usageText } from "./usage";
import { useAsync } from "./useAsync";

/** The tenant's usage against its plan: one tile per metered figure, each
 *  over its cap (or "no limit"), counted afresh when the page opens. A figure
 *  the server could not count shows as such rather than as zero. */
export function UsageCard() {
  const snapshot = useAsync(() => api.fetchPlanUsage(), []);
  const rows = usageRows(snapshot.data ?? api.planUsage() ?? { plan: {}, usage: null, over: [] });
  return (
    <div className="card usage-card">
      <h3>Usage</h3>
      <p className="muted">
        What this workspace has used of its plan. Monthly figures restart on the first of the
        month (UTC).
      </p>
      {snapshot.error && <p className="err">{snapshot.error}</p>}
      <div className="kpi-grid">
        {rows.map((row) => (
          <StatCard
            key={row.field}
            label={row.label}
            value={usageText(row)}
            hint={usageHint(row) || undefined}
            isLoading={snapshot.loading && !snapshot.data}
            className={row.over ? "over" : undefined}
          />
        ))}
      </div>
    </div>
  );
}
