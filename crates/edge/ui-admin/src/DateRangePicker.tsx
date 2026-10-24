import { useState } from "react";

import {
  PRESETS,
  activePreset,
  dateInputToIso,
  isoToDateInput,
  rangeForDays,
  type DateRange,
} from "./daterange";

/** Preset windows (24h/7d/30d/90d) plus a Custom mode with two date inputs.
 *  Emits `{from,to}` ISO strings; the parent owns the range state. */
export function DateRangePicker({
  value,
  onChange,
}: {
  value: DateRange;
  onChange: (next: DateRange) => void;
}) {
  const [custom, setCustom] = useState(false);
  const active = custom ? "custom" : activePreset(value);

  return (
    <div className="range-picker">
      {PRESETS.map((p) => (
        <button
          key={p.label}
          type="button"
          className={`range-btn${!custom && active === p.label ? " active" : ""}`}
          onClick={() => {
            setCustom(false);
            onChange(rangeForDays(p.days));
          }}
        >
          {p.label}
        </button>
      ))}
      <button
        type="button"
        className={`range-btn${custom ? " active" : ""}`}
        onClick={() => setCustom((c) => !c)}
      >
        Custom
      </button>
      {custom && (
        <div className="range-custom">
          <input
            type="date"
            aria-label="From date"
            value={isoToDateInput(value.from)}
            onChange={(e) => onChange({ ...value, from: dateInputToIso(e.target.value) })}
          />
          <span className="range-sep">→</span>
          <input
            type="date"
            aria-label="To date"
            value={isoToDateInput(value.to)}
            onChange={(e) => onChange({ ...value, to: dateInputToIso(e.target.value, true) })}
          />
        </div>
      )}
    </div>
  );
}
