import { describe, expect, it } from "vitest";

import {
  classifyThemeAttrs,
  isDarkColor,
  parseColor,
  resolveMode,
  type ThemeSignals,
} from "./theme-detect";

const NO_ATTRS: ThemeSignals = {
  colorScheme: "",
  dataTheme: null,
  className: "",
  dataBsTheme: null,
  dataMuiColorScheme: null,
  dataColorMode: null,
};

const signals = (o: Partial<ThemeSignals>): ThemeSignals => ({ ...NO_ATTRS, ...o });

describe("parseColor", () => {
  it("parses hex in 3/4/6/8 digit forms", () => {
    expect(parseColor("#000")).toEqual({ r: 0, g: 0, b: 0, a: 1 });
    expect(parseColor("#ffffff")).toEqual({ r: 255, g: 255, b: 255, a: 1 });
    expect(parseColor("#00000080")?.a).toBeCloseTo(128 / 255);
    expect(parseColor("#0f08")).toEqual({ r: 0, g: 255, b: 0, a: 136 / 255 });
  });

  it("parses rgb/rgba with comma and space/slash separators", () => {
    expect(parseColor("rgb(15, 23, 42)")).toEqual({ r: 15, g: 23, b: 42, a: 1 });
    expect(parseColor("rgba(255, 255, 255, 0.5)")).toEqual({ r: 255, g: 255, b: 255, a: 0.5 });
    expect(parseColor("rgb(15 23 42 / 0.5)")).toEqual({ r: 15, g: 23, b: 42, a: 0.5 });
  });

  it("returns null for non-parseable colours", () => {
    for (const bad of ["", "transparent", "rebeccapurple", "linear-gradient(red, blue)", "#12"]) {
      expect(parseColor(bad)).toBeNull();
    }
  });
});

describe("isDarkColor", () => {
  it("classifies by relative luminance", () => {
    expect(isDarkColor("#0f172a")).toBe(true);
    expect(isDarkColor("#ffffff")).toBe(false);
    expect(isDarkColor("rgb(24, 24, 27)")).toBe(true);
    expect(isDarkColor("#f7f9fc")).toBe(false);
  });

  it("returns null for transparent or unparseable colours (never guesses)", () => {
    expect(isDarkColor("transparent")).toBeNull();
    expect(isDarkColor("rgba(0, 0, 0, 0)")).toBeNull();
    expect(isDarkColor("not-a-colour")).toBeNull();
  });
});

describe("classifyThemeAttrs", () => {
  it("reads computed color-scheme first (next-themes on both conventions)", () => {
    // fabrikam: class="dark" AND inline color-scheme; contoso: data-theme + color-scheme.
    expect(classifyThemeAttrs(signals({ colorScheme: "dark", className: "dark" }))).toBe("dark");
    expect(classifyThemeAttrs(signals({ colorScheme: "light", dataTheme: "light" }))).toBe("light");
  });

  it("treats ambiguous color-scheme as inconclusive and falls through", () => {
    expect(classifyThemeAttrs(signals({ colorScheme: "light dark" }))).toBeNull();
    expect(classifyThemeAttrs(signals({ colorScheme: "normal", dataTheme: "dark" }))).toBe("dark");
  });

  it("reads the Tailwind class convention", () => {
    expect(classifyThemeAttrs(signals({ className: "dark antialiased" }))).toBe("dark");
    expect(classifyThemeAttrs(signals({ className: "light" }))).toBe("light");
  });

  it("reads data-theme, Bootstrap, MUI, and Primer attributes", () => {
    expect(classifyThemeAttrs(signals({ dataTheme: "dark" }))).toBe("dark");
    expect(classifyThemeAttrs(signals({ dataBsTheme: "dark" }))).toBe("dark");
    expect(classifyThemeAttrs(signals({ dataMuiColorScheme: "dark" }))).toBe("dark");
    expect(classifyThemeAttrs(signals({ dataColorMode: "dark" }))).toBe("dark");
  });

  it("resolves conflicts by prevalence: color-scheme, then data-theme/class, then framework attrs", () => {
    // color-scheme is authoritative over everything else.
    expect(classifyThemeAttrs(signals({ colorScheme: "light", dataTheme: "dark", className: "dark" }))).toBe("light");
    // A dominant convention (Tailwind class) beats a long-tail framework attr.
    expect(classifyThemeAttrs(signals({ className: "dark", dataBsTheme: "light" }))).toBe("dark");
    // data-theme beats class.
    expect(classifyThemeAttrs(signals({ dataTheme: "dark", className: "light" }))).toBe("dark");
  });

  it("never matches a substring like darkblue and returns null when unset", () => {
    expect(classifyThemeAttrs(signals({ dataTheme: "darkblue" }))).toBeNull();
    expect(classifyThemeAttrs(NO_ATTRS)).toBeNull();
  });
});

describe("resolveMode", () => {
  const base = { attrs: NO_ATTRS, background: null, prefersDark: false } as const;

  it("prefers an explicit host signal over everything", () => {
    expect(
      resolveMode({ ...base, explicit: "light", attrs: signals({ dataTheme: "dark" }), prefersDark: true }),
    ).toBe("light");
  });

  it("uses DOM attributes over luminance and OS", () => {
    expect(
      resolveMode({ ...base, explicit: null, attrs: signals({ className: "dark" }), background: "#ffffff", prefersDark: false }),
    ).toBe("dark");
  });

  it("falls back to background luminance when no attributes are set", () => {
    expect(resolveMode({ ...base, explicit: null, background: "#0f172a", prefersDark: false })).toBe("dark");
    expect(resolveMode({ ...base, explicit: null, background: "#ffffff", prefersDark: true })).toBe("light");
  });

  it("falls back to the OS preference when nothing else is conclusive", () => {
    expect(resolveMode({ ...base, explicit: null, prefersDark: true })).toBe("dark");
    expect(resolveMode({ ...base, explicit: null, background: "transparent", prefersDark: false })).toBe("light");
  });
});
