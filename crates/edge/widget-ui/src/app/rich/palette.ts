// Chart color roles. Series identity uses a validated categorical palette
// (fixed slot order — the ordering is the CVD-safety mechanism, never cycle or
// re-derive it); chart chrome reads the widget's neutral theme variables so
// axes and labels follow the operator's branding.

export const CATEGORICAL: readonly string[] = [
  "#2a78d6", // blue
  "#1baf7a", // aqua
  "#eda100", // yellow
  "#008300", // green
  "#4a3aa7", // violet
  "#e34948", // red
  "#e87ba4", // magenta
  "#eb6834", // orange
];

// Magnitude uses a validated single-hue sequential ramp (blue, light→dark) — the
// choropleth encoding, never the categorical slots. Given low→high to a
// continuous visualMap, which interpolates between the stops.
export const SEQUENTIAL: readonly string[] = [
  "#cde2fb",
  "#9ec5f4",
  "#6da7ec",
  "#3987e5",
  "#256abf",
  "#184f95",
  "#0d366b",
];

// Diagram lane hues, light mode. Colour on a diagram encodes ONE thing — which
// group owns a step — so this is a categorical identity palette, assigned by the
// group's first appearance and never cycled.
//
// Five, not eight: the chart palette's 7th↔8th pair sits at ΔE 12.9 (below the
// 15 normal-vision floor), and green had to go because darkening it for a dark
// ground collapses yellow↔green to ΔE 0.8 under protanopia. Both mode sets are
// validated by the dataviz palette checker; the DARK steps are SELECTED, not
// derived — the dark lightness band (0.48–0.67) is narrower and lower than
// light's (0.43–0.77), so blue, yellow and violet are all different values.
//
// The renderer paints from the `--wf-lane-*` CSS variables (so a theme switch
// costs no re-render); this list is the same palette for the standalone
// Excalidraw export, and a test asserts the two never drift apart.
export const LANE_HUES: readonly string[] = [
  "#2a78d6", // blue
  "#1baf7a", // aqua
  "#eda100", // yellow
  "#4a3aa7", // violet
  "#e34948", // red
];

export interface ChartInk {
  text: string;
  muted: string;
  grid: string;
  axis: string;
  surface: string;
  font: string;
}

const FALLBACK: ChartInk = {
  text: "#0f172a",
  muted: "#898781",
  grid: "#e1e0d9",
  axis: "#c3c2b7",
  surface: "#ffffff",
  font: 'system-ui, -apple-system, "Segoe UI", Roboto, sans-serif',
};

/** Resolve the chrome ink from the element's computed theme variables. */
export function readInk(el: HTMLElement): ChartInk {
  const style = getComputedStyle(el);
  const cssVar = (name: string, fallback: string) =>
    style.getPropertyValue(name).trim() || fallback;
  return {
    text: cssVar("--wf-agent-text", FALLBACK.text),
    muted: cssVar("--wf-muted", FALLBACK.muted),
    grid: cssVar("--wf-border", FALLBACK.grid),
    axis: cssVar("--wf-border", FALLBACK.axis),
    surface: cssVar("--wf-agent", FALLBACK.surface),
    font: style.fontFamily || FALLBACK.font,
  };
}
