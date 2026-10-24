// The wf-chart renderer: a tree-shaken echarts instance bound to a validated
// spec. Loaded lazily — this module (and echarts with it) only downloads once a
// conversation actually contains a chart.

import { useEffect, useRef } from "react";

import { initChart } from "./echarts";
import { buildOption, chartAriaLabel } from "./chartOption";
import { readInk } from "./palette";
import type { ChartSpec } from "./spec";

export default function WfChart({ spec }: { spec: ChartSpec }) {
  const plot = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const el = plot.current;
    if (!el) return;
    const reducedMotion = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
    const chart = initChart(el);
    chart.setOption(buildOption(spec, readInk(el), reducedMotion));
    const ro = new ResizeObserver(() => chart.resize());
    ro.observe(el);
    return () => {
      ro.disconnect();
      chart.dispose();
    };
  }, [spec]);

  return (
    <div className="wf-chart">
      {spec.title && <div className="wf-chart-title">{spec.title}</div>}
      <div ref={plot} className="wf-chart-plot" role="img" aria-label={chartAriaLabel(spec)} />
    </div>
  );
}
