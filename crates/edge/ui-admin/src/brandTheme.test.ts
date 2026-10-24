// These pin brandTheme.ts to `crates/edge/src/theme.rs`. The preview is only
// worth having if it agrees with the server, so the expectations here are copied
// from the Rust tests rather than from the TS implementation.
import { describe, expect, it } from "vitest";

import {
  contrast,
  gradientAngle,
  lighten,
  resolveBrand,
  resolveGradient,
  themeMode,
} from "./brandTheme";
import type { Branding } from "./api";

const GRAD = { from: "#054D5C", via: "#828AFA", to: "#FBAEC3", angle: "to right" };

describe("gradient resolution matches theme.rs", () => {
  it("composes three stops, and two when via is absent", () => {
    expect(resolveGradient(GRAD)).toBe(
      "linear-gradient(to right, #054D5C 0%, #828AFA 50%, #FBAEC3 100%)",
    );
    expect(resolveGradient({ from: "#054D5C", to: "#FBAEC3" })).toBe(
      "linear-gradient(to right, #054D5C 0%, #FBAEC3 100%)",
    );
  });

  it("needs both ends, but drops only a bad via", () => {
    expect(resolveGradient({ from: "#054D5C" })).toBe("");
    expect(resolveGradient({ from: "nope", to: "#FBAEC3" })).toBe("");
    expect(resolveGradient(null)).toBe("");
    expect(resolveGradient({ from: "#054D5C", via: "url(x)", to: "#FBAEC3" })).toBe(
      "linear-gradient(to right, #054D5C 0%, #FBAEC3 100%)",
    );
  });

  it("accepts side/corner keywords and bounded degrees only", () => {
    for (const ok of ["to left", "to bottom right", "45deg", "0deg", "360deg"]) {
      expect(gradientAngle(ok)).toBe(ok);
    }
    for (const bad of ["to nowhere", "400deg", "-10deg", "var(--x)", "calc(1deg * 90)"]) {
      expect(gradientAngle(bad)).toBe("to right");
    }
  });
});

describe("light resolution matches theme.rs defaults", () => {
  it("uses the server's defaults, not the preview's own", () => {
    const t = resolveBrand({});
    expect(t.primary).toBe("#2563eb");
    expect(t.agentBubble).toBe("#f1f5f9");
    expect(t.agentText).toBe("#0f172a");
    expect(t.surface).toBe("#f7f9fc");
    expect(t.panel).toBe("#ffffff");
    expect(t.radius).toBe("12px");
  });

  it("defaults the customer bubble to the primary and honours accent_color", () => {
    expect(resolveBrand({ primary_color: "#123abc" }).userBubble).toBe("#123abc");
    expect(resolveBrand({ accent_color: "#abcdef" }).primary).toBe("#abcdef");
  });

  it("falls back on junk rather than passing it through", () => {
    const t = resolveBrand({
      primary_color: "red; background:url(javascript:alert(1))",
      launcher: "iframe",
    });
    expect(t.primary).toBe("#2563eb");
    expect(t.launcher).toBe("bubble");
  });

  it("accepts expand as a launcher and defaults the avatar off", () => {
    expect(resolveBrand({ launcher: "expand" }).launcher).toBe("expand");
    expect(resolveBrand({}).avatar).toBe(false);
    expect(resolveBrand({ avatar: true }).avatar).toBe(true);
  });
});

describe("per-surface paint mirrors theme.rs", () => {
  it("is empty (inherit) until given a colour or its own gradient", () => {
    expect(resolveBrand({ gradient: GRAD }).avatarBg).toBe("");
    expect(resolveBrand({ gradient: GRAD }).sendBg).toBe("");
  });

  it("takes each surface independently, gradient winning over colour", () => {
    const t = resolveBrand({
      gradient: GRAD,
      avatar_paint: { color: "#5b72bc" },
      send_paint: { gradient: { from: "#111111", to: "#222222" } },
    });
    expect(t.avatarBg).toBe("#5b72bc");
    expect(t.sendBg).toBe("linear-gradient(to right, #111111 0%, #222222 100%)");
    // The shared brand paint is untouched by either.
    expect(t.gradient).toBe(resolveGradient(GRAD));
    const both = resolveBrand({
      avatar_paint: { color: "#5b72bc", gradient: { from: "#111111", to: "#222222" } },
    });
    expect(both.avatarBg.startsWith("linear-gradient(")).toBe(true);
  });

  it("falls back rather than shipping junk", () => {
    expect(resolveBrand({ avatar_paint: { color: "url(x)" } }).avatarBg).toBe("");
    expect(
      resolveBrand({ avatar_paint: { color: "#5b72bc", gradient: { from: "nope" } } }).avatarBg,
    ).toBe("#5b72bc");
  });

  it("reuses the light override in dark unless overridden", () => {
    const b = { avatar_paint: { color: "#5b72bc" } };
    expect(resolveBrand(b, true).avatarBg).toBe("#5b72bc");
    expect(resolveBrand({ ...b, dark: { avatar_paint: { color: "#bd93f9" } } }, true).avatarBg).toBe(
      "#bd93f9",
    );
  });
});

describe("split borders mirror theme.rs", () => {
  it("default to the shared border and split off it", () => {
    const base = resolveBrand({ border: "#e6eaf0" });
    expect([base.inputBorder, base.agentBorder]).toEqual(["#e6eaf0", "#e6eaf0"]);
    const split = resolveBrand({
      border: "#e6eaf0",
      input_border: "#c7d2fe",
      agent_bubble_border: "#d1d5db",
    });
    expect([split.inputBorder, split.agentBorder, split.border]).toEqual([
      "#c7d2fe",
      "#d1d5db",
      "#e6eaf0",
    ]);
  });

  it("follow the dark border, and take dark overrides", () => {
    expect(resolveBrand({ input_border: "#c7d2fe" }, true).inputBorder).toBe("#333b46");
    expect(
      resolveBrand({ dark: { input_border: "#445566" } }, true).inputBorder,
    ).toBe("#445566");
  });
});

describe("frame thickness mirrors theme.rs", () => {
  it("defaults to 2px and caps at 24", () => {
    expect(resolveBrand({}).frameWidth).toBe(2);
    expect(resolveBrand({ frame_width: 8 }).frameWidth).toBe(8);
    expect(resolveBrand({ frame_width: 0 }).frameWidth).toBe(0);
    expect(resolveBrand({ frame_width: 999 }).frameWidth).toBe(24);
  });

  it("is structural — shared with the dark palette", () => {
    expect(resolveBrand({ frame_width: 6 }, true).frameWidth).toBe(6);
  });
});

describe("composer background mirrors theme.rs", () => {
  it("defaults to the panel in each mode", () => {
    const t = resolveBrand({ panel: "#fafbfc" });
    expect(t.inputBg).toBe(t.panel);
    expect(t.inputBg).toBe("#fafbfc");
    // Dark follows the DARK panel, not the light input background.
    expect(resolveBrand({ panel: "#fafbfc" }, true).inputBg).toBe("#242b34");
    expect(resolveBrand({ dark: { panel: "#101418" } }, true).inputBg).toBe("#101418");
  });

  it("can be set per mode, and junk falls back to the panel", () => {
    const b = { panel: "#ffffff", input_bg: "#f1f5f9", dark: { input_bg: "#1b2027" } };
    expect(resolveBrand(b).inputBg).toBe("#f1f5f9");
    expect(resolveBrand(b).panel).toBe("#ffffff");
    expect(resolveBrand(b, true).inputBg).toBe("#1b2027");
    expect(resolveBrand({ panel: "#ffffff", input_bg: "transparent;}x{" }).inputBg).toBe("#ffffff");
  });
});

describe("gradient stops mirror theme.rs", () => {
  const S = (stops: [string, number | null][]) => ({
    stops: stops.map(([color, at]) => ({ color, at })),
  });

  it("carries explicit positions and trims pointless zeros", () => {
    expect(resolveGradient(S([["#054D5C", 0], ["#828AFA", 35], ["#FBAEC3", 100]]))).toBe(
      "linear-gradient(to right, #054D5C 0%, #828AFA 35%, #FBAEC3 100%)",
    );
    expect(resolveGradient(S([["#000000", 12.5], ["#ffffff", 87]]))).toBe(
      "linear-gradient(to right, #000000 12.5%, #ffffff 87%)",
    );
  });

  it("orders, clamps and bounds them", () => {
    expect(resolveGradient(S([["#111111", 80], ["#222222", 10]]))).toBe(
      "linear-gradient(to right, #222222 10%, #111111 80%)",
    );
    expect(resolveGradient(S([["#111111", -50], ["#222222", 999]]))).toBe(
      "linear-gradient(to right, #111111 0%, #222222 100%)",
    );
    const many: [string, number | null][] = Array.from({ length: 20 }, () => ["#123456", null]);
    expect((resolveGradient(S(many)).match(/#/g) ?? []).length).toBe(8);
  });

  it("spreads evenly when positions are omitted, matching the shorthand", () => {
    expect(resolveGradient(S([["#000000", null], ["#888888", null], ["#ffffff", null]]))).toBe(
      "linear-gradient(to right, #000000 0%, #888888 50%, #ffffff 100%)",
    );
  });

  it("falls back to the shorthand until two stops are usable", () => {
    const short = { from: "#054D5C", via: "#828AFA", to: "#FBAEC3" };
    const expected = resolveGradient(short);
    expect(resolveGradient({ ...short, ...S([["#111111", 0]]) })).toBe(expected);
    expect(resolveGradient({ ...short, ...S([["nope", 0], ["bad", 100]]) })).toBe(expected);
    expect(resolveGradient({ ...short, ...S([["#111111", 0], ["#222222", 100]]) })).toBe(
      "linear-gradient(to right, #111111 0%, #222222 100%)",
    );
  });

  it("drops a stop that isn't a colour, so nothing but a gradient reaches CSS", () => {
    expect(resolveGradient(S([["url(https://e/x.png)", 0], ["#fff", 50], ["red;}x{", 100]]))).toBe(
      "",
    );
  });
});

describe("panel size mirrors theme.rs", () => {
  it("uses the server's defaults", () => {
    const t = resolveBrand({});
    expect([t.width, t.height, t.minWidth, t.minHeight]).toEqual([400, 660, 320, 420]);
  });

  it("clamps to a usable range whatever is configured", () => {
    const tiny = resolveBrand({ width: 0, height: 1, min_width: 0, min_height: 0 });
    expect(tiny.width).toBeGreaterThanOrEqual(280);
    expect(tiny.height).toBeGreaterThanOrEqual(360);
    const huge = resolveBrand({ width: 99999, height: 99999 });
    expect([huge.width, huge.height]).toEqual([3000, 3000]);
  });

  it("never lets the floor exceed the opening size", () => {
    const t = resolveBrand({ width: 360, height: 400, min_width: 900, min_height: 900 });
    expect([t.minWidth, t.minHeight]).toEqual([360, 400]);
  });
});

describe("presentation options are per-agent opt-ins", () => {
  it("stays on the pre-existing look by default, gradient or not", () => {
    for (const b of [{}, { gradient: GRAD }] as Branding[]) {
      const t = resolveBrand(b);
      expect(t.frame).toBe(false);
      expect(t.sendShape).toBe("rounded");
      expect(t.sendIcon).toBe("arrow");
      expect(t.logoFit).toBe("cover");
    }
  });

  it("switches each on independently and validates junk away", () => {
    const t = resolveBrand({
      frame: true,
      send_shape: "circle",
      send_icon: "plane",
      logo_fit: "contain",
    });
    expect([t.frame, t.sendShape, t.sendIcon, t.logoFit]).toEqual([
      true,
      "circle",
      "plane",
      "contain",
    ]);
    const junk = resolveBrand({ send_shape: "blob", send_icon: "rocket", logo_fit: "fill" });
    expect([junk.sendShape, junk.sendIcon, junk.logoFit]).toEqual([
      "rounded",
      "arrow",
      "cover",
    ]);
  });

  it("shares them with the dark palette (structural, not colour)", () => {
    const d = resolveBrand({ frame: true, send_shape: "circle" }, true);
    expect(d.frame).toBe(true);
    expect(d.sendShape).toBe("circle");
  });
});

describe("dark resolution matches theme.rs", () => {
  const b: Branding = { primary_color: "#1d4be5", gradient: GRAD };

  it("uses curated neutrals and keeps the light brand when dark is unset", () => {
    const d = resolveBrand(b, true);
    expect(d.surface).toBe("#1b2027");
    expect(d.panel).toBe("#242b34");
    expect(d.agentBubble).toBe("#2a323d");
    expect(d.text).toBe("#e6eaf0");
    expect(d.primary).toBe("#1d4be5");
  });

  it("lifts an unset dark accent but trusts an authored dark primary", () => {
    expect(resolveBrand({ primary_color: "#05405c" }, true).accent).toBe(
      lighten("#05405c", 0.32),
    );
    expect(
      resolveBrand({ primary_color: "#05405c", dark: { primary_color: "#bd93f9" } }, true)
        .accent,
    ).toBe("#bd93f9");
  });

  it("tracks the dark primary with the dark user bubble", () => {
    const d = resolveBrand({ ...b, dark: { primary_color: "#bd93f9" } }, true);
    expect(d.primary).toBe("#bd93f9");
    expect(d.userBubble).toBe("#bd93f9");
  });

  it("validates dark overrides and falls back on junk", () => {
    const d = resolveBrand({ ...b, dark: { surface: "#282a36", text: "not-hex" } }, true);
    expect(d.surface).toBe("#282a36");
    expect(d.text).toBe("#e6eaf0");
  });

  it("reuses the light gradient when the dark one is unset or unusable", () => {
    const light = resolveBrand(b).gradient;
    expect(resolveBrand(b, true).gradient).toBe(light);
    expect(resolveBrand({ ...b, dark: { gradient: { from: "x", to: "#222" } } }, true).gradient).toBe(
      light,
    );
    expect(
      resolveBrand({ ...b, dark: { gradient: { from: "#111111", to: "#222222" } } }, true)
        .gradient,
    ).toBe("linear-gradient(to right, #111111 0%, #222222 100%)");
  });

  it("leaves the light palette untouched", () => {
    const d: Branding = { ...b, dark: { primary_color: "#bd93f9", surface: "#282a36" } };
    expect(resolveBrand(d).primary).toBe("#1d4be5");
    expect(resolveBrand(d).surface).toBe("#f7f9fc");
  });
});

describe("theme mode", () => {
  it("defaults to light and rejects junk", () => {
    expect(themeMode({})).toBe("light");
    expect(themeMode({ theme_mode: "dark" })).toBe("dark");
    expect(themeMode({ theme_mode: "auto" })).toBe("auto");
    expect(themeMode({ theme_mode: "System; :root{}" })).toBe("light");
  });
});

describe("contrast", () => {
  it("measures the header-icon problem the deck's gradient creates", () => {
    // White on the pale end of the Harbor gradient — the reason the editor
    // warns when header text lands there.
    expect(contrast("#FBAEC3", "#ffffff")!).toBeCloseTo(1.76, 1);
    expect(contrast("#054D5C", "#ffffff")!).toBeCloseTo(9.45, 1);
    expect(contrast("not-hex", "#ffffff")).toBeNull();
  });
});
