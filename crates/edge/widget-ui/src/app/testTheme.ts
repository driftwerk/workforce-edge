import type { ThemeTokens } from "../protocol";

/** A minimal valid theme for widget tests; `draggable` is the only knob. */
export const theme = (draggable: boolean): ThemeTokens => ({
  title: "Helper",
  subtitle: "",
  logo_url: "",
  launcher: "bubble",
  primary: "#05405c",
  primary_text: "#ffffff",
  user_bubble: "#05405c",
  user_bubble_text: "#ffffff",
  agent_bubble: "#f1f5f9",
  agent_bubble_text: "#0f172a",
  accent: "#05405c",
  surface: "#f7f9fc",
  panel: "#ffffff",
  border: "#e6eaf0",
  muted: "#64748b",
  text: "#0f172a",
  font_family: "system-ui",
  font_url: "",
  radius: "12px",
  position: "right",
  shadow: true,
  fullscreen: "centered",
  draggable,
});
