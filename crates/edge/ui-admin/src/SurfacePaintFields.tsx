// Paint for one small brand surface (the agent avatar, the send button), independent
// of the shared brand gradient and of each other.
//
// Three states, because that is genuinely what the config expresses: inherit the brand
// paint (the default, and what these did before they were configurable), a flat colour,
// or the surface's own gradient. The mode selector makes that explicit rather than
// leaving an operator to infer it from which field happens to be filled.
import type { Gradient, GradientStop, SurfacePaint } from "./api";
import { GradientBar, stopsOf } from "./GradientBar";
import { HEX, resolveGradient } from "./brandTheme";

type Mode = "inherit" | "color" | "gradient";

function modeOf(p: SurfacePaint | null | undefined): Mode {
  if (resolveGradient(p?.gradient)) return "gradient";
  if (p?.color && HEX.test(p.color)) return "color";
  return "inherit";
}

export function SurfacePaintFields({
  title,
  hint,
  paint,
  brand,
  onChange,
}: {
  title: string;
  hint: string;
  paint?: SurfacePaint | null;
  /** The brand gradient this surface inherits, for the "inherit" swatch. */
  brand?: Gradient | null;
  onChange: (p: SurfacePaint | null) => void;
}) {
  const mode = modeOf(paint);
  const brandCss = resolveGradient(brand);

  const setMode = (m: Mode) => {
    if (m === "inherit") return onChange(null);
    if (m === "color") {
      // Seed from whatever is already on screen so switching modes isn't destructive.
      const seed = paint?.color || stopsOf(paint?.gradient)[0]?.color || "#5b72bc";
      return onChange({ color: seed, gradient: null });
    }
    const from = paint?.color || "#054d5c";
    return onChange({ color: null, gradient: { from, to: "#fbaec3", angle: "to right" } });
  };

  return (
    <div className="field-group">
      <div className="group-title">{title}</div>
      <div className="form-grid">
        <label>
          Paint<span className="hint">{hint}</span>
        </label>
        <select className="select" value={mode} onChange={(e) => setMode(e.target.value as Mode)}>
          <option value="inherit">Inherit the brand paint</option>
          <option value="color">Flat colour</option>
          <option value="gradient">Its own gradient</option>
        </select>
        {mode === "color" && (
          <>
            <label>Colour</label>
            <span className="color-row">
              <input
                type="color"
                value={paint?.color && HEX.test(paint.color) ? paint.color : "#5b72bc"}
                onChange={(e) => onChange({ color: e.target.value, gradient: null })}
              />
              <input
                type="text"
                value={paint?.color ?? ""}
                placeholder="#5b72bc"
                onChange={(e) => onChange({ color: e.target.value, gradient: null })}
              />
            </span>
          </>
        )}
      </div>

      {mode === "inherit" && (
        <div className="paint-inherit">
          <span className="paint-swatch" style={{ background: brandCss || "var(--faint)" }} />
          <span className="hint">
            {brandCss
              ? "Follows the brand gradient above."
              : "Follows the brand colour above."}
          </span>
        </div>
      )}

      {mode === "gradient" && (
        <>
          <div className="form-grid" style={{ marginTop: 8 }}>
            <label>Direction</label>
            <select
              className="select"
              value={paint?.gradient?.angle ?? "to right"}
              onChange={(e) =>
                onChange({ ...paint, gradient: { ...(paint?.gradient ?? {}), angle: e.target.value } })
              }
            >
              <option value="to right">Left → right</option>
              <option value="to left">Right → left</option>
              <option value="to bottom">Top → bottom</option>
              <option value="to top">Bottom → top</option>
              <option value="to bottom right">Diagonal ↘</option>
              <option value="to bottom left">Diagonal ↙</option>
              <option value="to top right">Diagonal ↗</option>
              <option value="to top left">Diagonal ↖</option>
            </select>
          </div>
          <GradientBar
            g={paint?.gradient}
            onChange={(stops: GradientStop[]) =>
              onChange({ ...paint, color: null, gradient: { ...(paint?.gradient ?? {}), stops } })
            }
          />
          <p className="hint" style={{ margin: "8px 2px 0" }}>
            Shown in full rather than zoomed to its middle — a gradient chosen for this
            control is meant to be seen, unlike an inherited brand one on a 30px chip.
          </p>
        </>
      )}
    </div>
  );
}
