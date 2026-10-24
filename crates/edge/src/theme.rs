//! Widget theme resolution.
//!
//! Raw `[branding]` config is operator-authored but ships to every visitor's
//! browser inside the widget boot config, so every value is validated here and
//! anything absent or invalid falls back to a safe default — a config typo can
//! restyle the widget, never inject script or CSS.

use serde::Serialize;

use crate::agent::{Branding, DarkPalette, Gradient, GradientStop, SurfacePaint};

/// A fully-resolved, validated theme for one colour mode, serialised into the
/// widget boot config as a JSON object literal. Only safe values reach here — see
/// [`resolve_theme`]. Structural fields (logo, launcher, font, radius, position,
/// draggable, shadow, fullscreen, title, subtitle) are identical across modes; the
/// colour fields differ between the light and dark palettes.
#[derive(Debug, Serialize)]
struct Theme {
    title: String,
    subtitle: String,
    logo_url: String,
    launcher: String,
    primary: String,
    primary_text: String,
    user_bubble: String,
    user_bubble_text: String,
    agent_bubble: String,
    agent_bubble_text: String,
    /// The primary colour used as a FOREGROUND (links, icons, control borders) on
    /// neutral surfaces. Equals `primary` in light and when the operator authored a
    /// dark primary; when the dark primary is unset it is the light primary lifted
    /// toward white so it stays legible on the dark neutrals.
    accent: String,
    surface: String,
    panel: String,
    /// Background of the composer's message box. Defaults to `panel`, which is where
    /// it came from before this was configurable, so unset is a pixel no-op.
    input_bg: String,
    border: String,
    /// Borders that default to `border` but can be split off it.
    input_border: String,
    agent_bubble_border: String,
    muted: String,
    text: String,
    font_family: String,
    /// Stylesheet URL to load a web font (empty ⇒ none). Injected as a `<link>`.
    font_url: String,
    radius: String,
    position: String,
    /// Whether the customer may drag the widget to another viewport corner.
    draggable: bool,
    /// Inset in px of the docked position from its corner, horizontal then
    /// vertical. Already capped.
    offset_x: u32,
    offset_y: u32,
    /// Whether the docked panel casts a drop shadow.
    shadow: bool,
    /// Full-screen content layout: `centered` or `wide`.
    fullscreen: String,
    /// A composed `linear-gradient(…)`, or empty for none. Paint-only: it is applied
    /// as a *background* on the launcher, header, send button, avatar and panel ring,
    /// and never where a `<color>` is required. `primary` stays the flat colour that
    /// every `color-mix()`, focus ring and `accent-color` in the stylesheet consumes,
    /// so a gradient install keeps working focus rings and hover tints.
    gradient: String,
    /// Whether agent replies carry a small round brand avatar.
    avatar: bool,
    /// This surface's OWN paint — a composed gradient or a flat hex — or empty to
    /// inherit the shared brand paint. Independent of each other and of `gradient`,
    /// so one small control can be pinned without touching the rest.
    avatar_bg: String,
    send_bg: String,
    /// Whether the panel adopts the framed presentation (brand paint fills the
    /// panel, conversation inset inside it). Opt-in per agent — every install that
    /// doesn't ask for it keeps the flat panel and hairline border.
    frame: bool,
    /// Frame thickness in px, already clamped. Only meaningful when `frame` is on.
    frame_width: u32,
    /// Send button shape: `rounded` or `circle`.
    send_shape: String,
    /// Send button glyph: `arrow` or `plane`.
    send_icon: String,
    /// `cover` or `contain` — how the logo is fitted where it renders.
    logo_fit: String,
    /// Panel box when it opens, and the floor a customer may drag it to. All four
    /// are already clamped to a usable range and mutually consistent by the time
    /// they reach here, so the widget can trust them without re-validating.
    width: u32,
    height: u32,
    min_width: u32,
    min_height: u32,
}

/// The smallest panel that still works: below roughly this the header, one message
/// and the composer stop coexisting. Operator config is clamped up to it, so no
/// value anyone can write produces an unusable widget.
mod panel {
    pub const FLOOR_W: u32 = 280;
    pub const FLOOR_H: u32 = 360;
    /// Matches the widget's own drag-resize ceiling (`MAX_PANEL` in App.tsx).
    pub const CEIL: u32 = 3000;
    pub const DEFAULT_W: u32 = 400;
    pub const DEFAULT_H: u32 = 660;
    pub const DEFAULT_MIN_W: u32 = 320;
    pub const DEFAULT_MIN_H: u32 = 420;
    /// Frame thickness bounds. A frame thicker than this starts eating the
    /// conversation on a narrow panel; 0 is allowed and sits the body flush.
    pub const FRAME_W_DEFAULT: u32 = 2;
    pub const FRAME_W_MAX: u32 = 24;
    /// Past this a dock inset is a config slip; the loader clamps to the viewport.
    pub const DOCK_OFFSET_MAX: u32 = 1000;
}

/// A configured px size, or the default — clamped into the usable range either way.
/// `clamp` also rescues a floor > ceiling ordering, which cannot happen with these
/// constants but would panic if it ever did.
fn size_or(cfg: Option<u32>, default: u32, floor: u32) -> u32 {
    cfg.unwrap_or(default).clamp(floor, panel::CEIL.max(floor))
}

fn dock_offset(cfg: Option<u32>) -> u32 {
    cfg.unwrap_or(0).min(panel::DOCK_OFFSET_MAX)
}

/// Curated generic-dark neutral defaults, used for any dark neutral the operator
/// leaves unset. Brand colours instead fall back to their light value.
mod dark_default {
    pub const SURFACE: &str = "#1b2027";
    pub const PANEL: &str = "#242b34";
    pub const BORDER: &str = "#333b46";
    pub const MUTED: &str = "#9aa5b4";
    pub const TEXT: &str = "#e6eaf0";
    pub const AGENT_BUBBLE: &str = "#2a323d";
    pub const AGENT_BUBBLE_TEXT: &str = "#e6eaf0";
}

const DEFAULT_FONT: &str = "system-ui, -apple-system, 'Segoe UI', Roboto, sans-serif";

/// A `#rgb` / `#rgba` / `#rrggbb` / `#rrggbbaa` hex colour and nothing else.
fn is_hex_color(s: &str) -> bool {
    match s.strip_prefix('#') {
        Some(rest) => {
            matches!(rest.len(), 3 | 4 | 6 | 8) && rest.bytes().all(|c| c.is_ascii_hexdigit())
        }
        None => false,
    }
}

/// `logo_url` must be an explicit http(s) or `data:image/…` URI. An `<img src>`
/// data image (including SVG) does not execute scripts, so this is a safe sink.
fn is_safe_logo(s: &str) -> bool {
    let l = s.trim();
    l.starts_with("https://") || l.starts_with("http://") || l.starts_with("data:image/")
}

/// A font stylesheet URL: https only (the widget is https, so http would be
/// blocked as mixed content anyway), no quotes/whitespace that could break out
/// of the `<link href>` attribute. It's an href sink (no script execution), but
/// kept tight as defence in depth.
fn is_safe_font_url(s: &str) -> bool {
    let l = s.trim();
    l.starts_with("https://")
        && l.len() <= 500
        && !l.contains(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == '<' || c == '>')
}

/// A CSS length like `12px`, `0.5rem`, `50%` — digits/dot then a known unit.
fn is_length(s: &str) -> bool {
    let s = s.trim();
    let split = s
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    !num.is_empty() && num.parse::<f32>().is_ok() && matches!(unit, "px" | "rem" | "em" | "%")
}

/// A conservative `font-family` stack — letters, digits, spaces, and the few
/// punctuation marks a stack needs. Blocks anything that could carry a CSS
/// payload even though custom-property values are already inert.
fn is_font(s: &str) -> bool {
    !s.trim().is_empty()
        && s.len() <= 200
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || " ,\"'-".contains(c))
}

fn color_or(cfg: Option<&str>, default: &str) -> String {
    match cfg {
        Some(c) if is_hex_color(c) => c.to_string(),
        _ => default.to_string(),
    }
}

/// Lighten a validated hex colour toward white by `amount` (0.0–1.0), returning
/// `#rrggbb`. Used to keep an unset dark accent legible on dark surfaces. Returns
/// the input unchanged if it can't be parsed to RGB.
fn lighten(hex: &str, amount: f32) -> String {
    let h = hex.trim_start_matches('#');
    let rrggbb = match h.len() {
        3 | 4 => h[..3].chars().flat_map(|c| [c, c]).collect::<String>(),
        6 | 8 => h[..6].to_string(),
        _ => return hex.to_string(),
    };
    let byte = |i: usize| u8::from_str_radix(&rrggbb[i..i + 2], 16).ok();
    match (byte(0), byte(2), byte(4)) {
        (Some(r), Some(g), Some(b)) => {
            let lift = |v: u8| (f32::from(v) + (255.0 - f32::from(v)) * amount).round() as u8;
            format!("#{:02x}{:02x}{:02x}", lift(r), lift(g), lift(b))
        }
        _ => hex.to_string(),
    }
}

/// The directions a gradient may run: the CSS side/corner keywords, or `<n>deg` with
/// `n` a plain number in `0..=360`. Anything else falls back to `to right`.
fn gradient_angle(cfg: Option<&str>) -> String {
    const SIDES: &[&str] = &[
        "to right",
        "to left",
        "to top",
        "to bottom",
        "to top right",
        "to top left",
        "to bottom right",
        "to bottom left",
    ];
    let raw = match cfg {
        Some(a) => a.trim(),
        None => return "to right".into(),
    };
    if SIDES.contains(&raw) {
        return raw.to_string();
    }
    // `<n>deg` — parsed as a number, so no expression or custom property can ride in.
    if let Some(num) = raw.strip_suffix("deg") {
        if let Ok(deg) = num.trim().parse::<f32>() {
            if (0.0..=360.0).contains(&deg) {
                return format!("{deg}deg");
            }
        }
    }
    "to right".into()
}

/// The most stops one gradient may carry. Bounds the CSS string; well past what any
/// brand needs, and the admin editor caps lower still.
const MAX_STOPS: usize = 8;

/// A position as CSS, trimmed of pointless zeros: `50`, `12.5`, `0`.
fn percent(v: f32) -> String {
    let s = format!("{:.1}", v.clamp(0.0, 100.0));
    let t = s.trim_end_matches('0').trim_end_matches('.');
    if t.is_empty() {
        "0".into()
    } else {
        t.into()
    }
}

/// Explicit `stops`, validated and ordered — `None` when fewer than two survive, so
/// the caller can fall back to the shorthand. A stop with no position is spread
/// evenly across the range, matching what the shorthand does.
fn explicit_stops(stops: &[GradientStop]) -> Option<Vec<(String, f32)>> {
    let usable: Vec<&GradientStop> = stops
        .iter()
        .filter(|s| s.color.as_deref().is_some_and(is_hex_color))
        .take(MAX_STOPS)
        .collect();
    if usable.len() < 2 {
        return None;
    }
    let last = usable.len() - 1;
    let mut out: Vec<(String, f32)> = usable
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let even = (i as f32 / last as f32) * 100.0;
            let at = s.at.filter(|v| v.is_finite()).unwrap_or(even);
            (s.color.clone().unwrap_or_default(), at.clamp(0.0, 100.0))
        })
        .collect();
    // CSS renders out-of-order stops by clamping each to its predecessor, which turns
    // a mis-ordered config into invisible bands. Sort so the result is what was meant.
    out.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    Some(out)
}

/// Compose a validated `linear-gradient(…)` from discrete stops, or `""` when the
/// gradient is unusable. Prefers explicit `stops` (colour + position); otherwise the
/// `from`/`via`/`to` shorthand, where `from` and `to` are required and `via` is
/// dropped on its own if invalid rather than failing the whole gradient. Because every
/// part is validated and the string is assembled here, the only thing that can reach
/// the page is a gradient — never a `url()`, `image-set()` or any other paint.
fn resolve_gradient(g: &Gradient) -> String {
    fn stop(c: Option<&String>) -> Option<&str> {
        c.map(String::as_str).filter(|s| is_hex_color(s))
    }
    let angle = gradient_angle(g.angle.as_deref());
    if let Some(stops) = g.stops.as_deref().and_then(explicit_stops) {
        let body = stops
            .iter()
            .map(|(c, at)| format!("{c} {}%", percent(*at)))
            .collect::<Vec<_>>()
            .join(", ");
        return format!("linear-gradient({angle}, {body})");
    }
    let (from, to) = match (stop(g.from.as_ref()), stop(g.to.as_ref())) {
        (Some(f), Some(t)) => (f, t),
        _ => return String::new(),
    };
    match stop(g.via.as_ref()) {
        Some(via) => format!("linear-gradient({angle}, {from} 0%, {via} 50%, {to} 100%)"),
        None => format!("linear-gradient({angle}, {from} 0%, {to} 100%)"),
    }
}

/// A surface's own paint: its gradient when that resolves, else its flat colour, else
/// EMPTY meaning "inherit the shared brand paint". Gradient wins because it is the
/// more specific statement — someone who set both meant the gradient.
fn resolve_paint(p: Option<&SurfacePaint>) -> String {
    let Some(p) = p else {
        return String::new();
    };
    if let Some(css) = p.gradient.as_ref().map(resolve_gradient) {
        if !css.is_empty() {
            return css;
        }
    }
    p.color
        .as_deref()
        .filter(|c| is_hex_color(c))
        .unwrap_or("")
        .to_string()
}

fn enum_or(cfg: Option<&str>, allowed: &[&str], default: &str) -> String {
    match cfg {
        Some(v) if allowed.contains(&v) => v.to_string(),
        _ => default.to_string(),
    }
}

/// Resolve raw [`Branding`] into a validated [`Theme`] for the requested mode.
/// The light theme comes straight from the branding fields; the dark theme applies
/// the `[embed.branding.dark]` overrides on top of it — brand colours fall back to
/// their light value, neutrals to the curated generic-dark defaults. Structural
/// fields are shared across modes. Every value is validated, so a typo can only
/// restyle, never inject.
fn resolve_theme(b: &Branding, dark: bool) -> Theme {
    let light = light_theme(b);
    if dark {
        dark_theme(b, light)
    } else {
        light
    }
}

fn light_theme(b: &Branding) -> Theme {
    let primary = color_or(
        b.primary_color.as_deref().or(b.accent_color.as_deref()),
        "#2563eb",
    );
    let width = size_or(b.width, panel::DEFAULT_W, panel::FLOOR_W);
    let height = size_or(b.height, panel::DEFAULT_H, panel::FLOOR_H);
    let panel_bg = color_or(b.panel.as_deref(), "#ffffff");
    let border = color_or(b.border.as_deref(), "#e6eaf0");
    Theme {
        title: b.title.clone().unwrap_or_else(|| "Chat".into()),
        subtitle: b.subtitle.clone().unwrap_or_default(),
        logo_url: b
            .logo_url
            .as_deref()
            .filter(|s| is_safe_logo(s))
            .unwrap_or("")
            .to_string(),
        launcher: enum_or(
            b.launcher.as_deref(),
            &["bubble", "pill", "expand"],
            "bubble",
        ),
        gradient: b
            .gradient
            .as_ref()
            .map(resolve_gradient)
            .unwrap_or_default(),
        // Every one of these defaults to the pre-existing look, so an agent that does
        // not ask for them renders exactly as it did before they existed. Brands
        // differ — these are per-agent choices, not consequences of having a gradient.
        avatar: b.avatar.unwrap_or(false),
        avatar_bg: resolve_paint(b.avatar_paint.as_ref()),
        send_bg: resolve_paint(b.send_paint.as_ref()),
        frame: b.frame.unwrap_or(false),
        frame_width: b
            .frame_width
            .unwrap_or(panel::FRAME_W_DEFAULT)
            .min(panel::FRAME_W_MAX),
        send_shape: enum_or(b.send_shape.as_deref(), &["rounded", "circle"], "rounded"),
        send_icon: enum_or(b.send_icon.as_deref(), &["arrow", "plane"], "arrow"),
        logo_fit: enum_or(b.logo_fit.as_deref(), &["cover", "contain"], "cover"),
        width,
        height,
        // A minimum above the opening size would have the panel open smaller than a
        // customer is allowed to shrink it to, so it can never exceed it.
        min_width: size_or(b.min_width, panel::DEFAULT_MIN_W, panel::FLOOR_W).min(width),
        min_height: size_or(b.min_height, panel::DEFAULT_MIN_H, panel::FLOOR_H).min(height),
        user_bubble: color_or(b.user_bubble.as_deref(), &primary),
        user_bubble_text: color_or(b.user_bubble_text.as_deref(), "#ffffff"),
        agent_bubble: color_or(b.agent_bubble.as_deref(), "#f1f5f9"),
        agent_bubble_text: color_or(b.agent_bubble_text.as_deref(), "#0f172a"),
        surface: color_or(b.surface.as_deref(), "#f7f9fc"),
        input_bg: color_or(b.input_bg.as_deref(), &panel_bg),
        panel: panel_bg.clone(),
        input_border: color_or(b.input_border.as_deref(), &border),
        agent_bubble_border: color_or(b.agent_bubble_border.as_deref(), &border),
        border: border.clone(),
        muted: color_or(b.muted.as_deref(), "#64748b"),
        text: color_or(b.text.as_deref(), "#0f172a"),
        font_family: b
            .font_family
            .as_deref()
            .filter(|s| is_font(s))
            .unwrap_or(DEFAULT_FONT)
            .to_string(),
        font_url: b
            .font_url
            .as_deref()
            .map(str::trim)
            .filter(|s| is_safe_font_url(s))
            .unwrap_or("")
            .to_string(),
        radius: b
            .radius
            .as_deref()
            .filter(|s| is_length(s))
            .unwrap_or("12px")
            .to_string(),
        position: enum_or(b.position.as_deref(), &["right", "left"], "right"),
        draggable: b.draggable.unwrap_or(false),
        offset_x: dock_offset(b.offset_x),
        offset_y: dock_offset(b.offset_y),
        // Default `true`: existing installs keep their shadow.
        shadow: b.shadow.unwrap_or(true),
        fullscreen: enum_or(b.fullscreen.as_deref(), &["centered", "wide"], "centered"),
        primary_text: color_or(b.primary_text.as_deref(), "#ffffff"),
        accent: primary.clone(),
        primary,
    }
}

fn dark_theme(b: &Branding, light: Theme) -> Theme {
    let d = b.dark.as_ref();
    let field = |pick: fn(&DarkPalette) -> Option<&String>| {
        d.and_then(pick)
            .map(String::as_str)
            .filter(|s| is_hex_color(s))
    };
    // Brand colours fall back to the resolved LIGHT value; neutrals to the curated
    // generic-dark defaults.
    let brand = |cfg: Option<&str>, fallback: String| cfg.map(String::from).unwrap_or(fallback);
    // Resolve the dark primary first: the user bubble tracks it (mirroring the light
    // theme's linkage), so an operator who recolours only the dark primary gets a
    // matching bubble instead of the stale light primary.
    let dark_border = color_or(field(|p| p.border.as_ref()), dark_default::BORDER);
    let dark_primary = field(|p| p.primary_color.as_ref());
    let primary = brand(dark_primary, light.primary.clone());
    // Foreground accent: an authored dark primary is trusted as-is; an unset one is
    // the light primary lifted toward white so it stays legible on dark neutrals
    // instead of rendering a dark brand colour on a dark surface.
    let accent = match dark_primary {
        Some(p) => p.to_string(),
        None => lighten(&light.primary, 0.32),
    };
    Theme {
        accent,
        primary_text: brand(field(|p| p.primary_text.as_ref()), light.primary_text),
        user_bubble: brand(field(|p| p.user_bubble.as_ref()), primary.clone()),
        user_bubble_text: brand(
            field(|p| p.user_bubble_text.as_ref()),
            light.user_bubble_text,
        ),
        agent_bubble: color_or(
            field(|p| p.agent_bubble.as_ref()),
            dark_default::AGENT_BUBBLE,
        ),
        agent_bubble_text: color_or(
            field(|p| p.agent_bubble_text.as_ref()),
            dark_default::AGENT_BUBBLE_TEXT,
        ),
        surface: color_or(field(|p| p.surface.as_ref()), dark_default::SURFACE),
        input_bg: color_or(
            field(|p| p.input_bg.as_ref()),
            &color_or(field(|p| p.panel.as_ref()), dark_default::PANEL),
        ),
        panel: color_or(field(|p| p.panel.as_ref()), dark_default::PANEL),
        input_border: color_or(field(|p| p.input_border.as_ref()), &dark_border),
        agent_bubble_border: color_or(field(|p| p.agent_bubble_border.as_ref()), &dark_border),
        border: dark_border.clone(),
        muted: color_or(field(|p| p.muted.as_ref()), dark_default::MUTED),
        text: color_or(field(|p| p.text.as_ref()), dark_default::TEXT),
        // The gradient is a BRAND token: an unset dark gradient reuses the light one,
        // exactly like `primary_color`. Without this, every `theme_mode = "auto"`
        // install would lose its gradient the moment the host page went dark.
        gradient: d
            .and_then(|p| p.gradient.as_ref())
            .map(resolve_gradient)
            .filter(|g| !g.is_empty())
            .unwrap_or(light.gradient),
        // Brand tokens: an unset dark override reuses the light one, which may itself
        // be empty — meaning the surface inherits the (already per-mode) brand paint.
        avatar_bg: {
            let d = resolve_paint(d.and_then(|p| p.avatar_paint.as_ref()));
            if d.is_empty() {
                light.avatar_bg
            } else {
                d
            }
        },
        send_bg: {
            let d = resolve_paint(d.and_then(|p| p.send_paint.as_ref()));
            if d.is_empty() {
                light.send_bg
            } else {
                d
            }
        },
        // Structural — shared with the light theme.
        avatar: light.avatar,
        frame: light.frame,
        frame_width: light.frame_width,
        send_shape: light.send_shape,
        send_icon: light.send_icon,
        logo_fit: light.logo_fit,
        width: light.width,
        height: light.height,
        min_width: light.min_width,
        min_height: light.min_height,
        title: light.title,
        subtitle: light.subtitle,
        logo_url: light.logo_url,
        launcher: light.launcher,
        font_family: light.font_family,
        font_url: light.font_url,
        radius: light.radius,
        position: light.position,
        draggable: light.draggable,
        offset_x: light.offset_x,
        offset_y: light.offset_y,
        shadow: light.shadow,
        fullscreen: light.fullscreen,
        primary,
    }
}

/// The validated theme tokens for one mode as a JSON object literal, injected into
/// the widget's boot config and forwarded to the iframe over the init handshake.
pub fn theme_tokens_json(branding: &Branding, dark: bool) -> String {
    serde_json::to_string(&resolve_theme(branding, dark)).unwrap_or_else(|_| "{}".into())
}

/// The validated colour-scheme mode: `light` (default), `dark`, or `auto`.
/// `light`/`dark` pin the widget; `auto` lets the loader follow the host page's
/// theme. Anything unset or unrecognised falls back to `light`.
pub fn theme_mode(branding: &Branding) -> &'static str {
    match branding.theme_mode.as_deref() {
        Some("dark") => "dark",
        Some("auto") => "auto",
        _ => "light",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn font_url_only_accepts_safe_https_and_new_controls_resolve() {
        let ok = Branding {
            font_url: Some("https://fonts.googleapis.com/css2?family=Poppins:wght@400;600".into()),
            shadow: Some(false),
            fullscreen: Some("wide".into()),
            ..Default::default()
        };
        let t = resolve_theme(&ok, false);
        assert!(t.font_url.starts_with("https://fonts.googleapis.com"));
        assert!(!t.shadow);
        assert_eq!(t.fullscreen, "wide");
        // http, quotes/whitespace, and a bad fullscreen enum are rejected.
        for bad in [
            "http://evil.example/f.css",
            "https://x/\" onload=alert(1)",
            "https://x/a b.css",
        ] {
            let b = Branding {
                font_url: Some(bad.into()),
                ..Default::default()
            };
            assert_eq!(resolve_theme(&b, false).font_url, "", "rejected: {bad}");
        }
        let b = Branding {
            fullscreen: Some("gigantic".into()),
            ..Default::default()
        };
        assert_eq!(
            resolve_theme(&b, false).fullscreen,
            "centered",
            "bad enum → default"
        );
    }

    #[test]
    fn invalid_theme_values_fall_back_to_safe_defaults() {
        let evil = Branding {
            primary_color: Some("red; background:url(javascript:alert(1))".into()),
            logo_url: Some("javascript:alert(1)".into()),
            radius: Some("12px;}body{display:none".into()),
            font_family: Some("Arial;}*{color:red".into()),
            launcher: Some("iframe".into()),
            position: Some("center".into()),
            ..Default::default()
        };
        let theme = resolve_theme(&evil, false);
        assert_eq!(theme.primary, "#2563eb", "bad colour → default");
        assert_eq!(theme.logo_url, "", "non-image url dropped");
        assert_eq!(theme.radius, "12px", "bad radius → default");
        assert_eq!(theme.font_family, DEFAULT_FONT, "bad font → default");
        assert_eq!(theme.launcher, "bubble", "bad launcher → default");
        // New generic controls: safe defaults preserve existing behaviour.
        assert!(theme.shadow, "shadow defaults on");
        assert_eq!(theme.fullscreen, "centered", "fullscreen defaults centered");
        assert_eq!(theme.font_url, "", "empty font_url");
        assert_eq!(theme.position, "right", "bad position → default");
        // And none of the injected payloads survive into the JSON.
        let json = theme_tokens_json(&evil, false);
        assert!(!json.contains("javascript:alert"));
        assert!(!json.contains("body{display:none"));
        assert!(!json.contains("*{color:red"));
    }

    #[test]
    fn valid_theme_values_pass_through_and_user_bubble_defaults_to_primary() {
        let b = Branding {
            primary_color: Some("#123abc".into()),
            logo_url: Some("https://cdn.example.com/logo.png".into()),
            radius: Some("8px".into()),
            launcher: Some("pill".into()),
            position: Some("left".into()),
            ..Default::default()
        };
        let theme = resolve_theme(&b, false);
        assert_eq!(theme.primary, "#123abc");
        assert_eq!(
            theme.user_bubble, "#123abc",
            "user bubble defaults to primary"
        );
        assert_eq!(theme.logo_url, "https://cdn.example.com/logo.png");
        assert_eq!(theme.launcher, "pill");
        assert_eq!(theme.position, "left");
    }

    fn painted(avatar: Option<SurfacePaint>, send: Option<SurfacePaint>) -> Branding {
        Branding {
            avatar_paint: avatar,
            send_paint: send,
            gradient: Some(Gradient {
                from: Some("#054D5C".into()),
                to: Some("#FBAEC3".into()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }
    fn flat(c: &str) -> SurfacePaint {
        SurfacePaint {
            color: Some(c.into()),
            gradient: None,
        }
    }
    fn own_gradient(from: &str, to: &str) -> SurfacePaint {
        SurfacePaint {
            color: None,
            gradient: Some(Gradient {
                from: Some(from.into()),
                to: Some(to.into()),
                ..Default::default()
            }),
        }
    }

    #[test]
    fn avatar_and_send_inherit_the_brand_paint_until_given_their_own() {
        // Empty means inherit, which is what both surfaces did before this existed.
        let t = resolve_theme(&painted(None, None), false);
        assert_eq!(t.avatar_bg, "");
        assert_eq!(t.send_bg, "");
        assert!(t.gradient.starts_with("linear-gradient("));
    }

    #[test]
    fn each_surface_takes_a_flat_colour_or_its_own_gradient_independently() {
        // A solid avatar while the send button keeps its own gradient — and the shared
        // brand gradient untouched by either.
        let b = painted(
            Some(flat("#5b72bc")),
            Some(own_gradient("#111111", "#222222")),
        );
        let t = resolve_theme(&b, false);
        assert_eq!(t.avatar_bg, "#5b72bc");
        assert_eq!(
            t.send_bg,
            "linear-gradient(to right, #111111 0%, #222222 100%)"
        );
        assert_eq!(
            t.gradient, "linear-gradient(to right, #054D5C 0%, #FBAEC3 100%)",
            "the shared brand paint is untouched"
        );
        // A gradient wins over a colour on the same surface — the more specific ask.
        let both = SurfacePaint {
            color: Some("#5b72bc".into()),
            gradient: Some(Gradient {
                from: Some("#111111".into()),
                to: Some("#222222".into()),
                ..Default::default()
            }),
        };
        assert!(resolve_theme(&painted(Some(both), None), false)
            .avatar_bg
            .starts_with("linear-gradient("));
    }

    #[test]
    fn a_surface_paint_falls_back_rather_than_shipping_junk() {
        // An unusable gradient falls through to the colour…
        let mixed = SurfacePaint {
            color: Some("#5b72bc".into()),
            gradient: Some(Gradient {
                from: Some("nope".into()),
                ..Default::default()
            }),
        };
        assert_eq!(
            resolve_theme(&painted(Some(mixed), None), false).avatar_bg,
            "#5b72bc"
        );
        // …and an unusable colour falls through to inheriting.
        let junk = SurfacePaint {
            color: Some("url(https://evil.example/x.png)".into()),
            gradient: None,
        };
        let b = painted(Some(junk), None);
        assert_eq!(resolve_theme(&b, false).avatar_bg, "");
        assert!(!theme_tokens_json(&b, false).contains("evil.example"));
    }

    #[test]
    fn dark_surface_paints_reuse_the_light_override_unless_set() {
        let mut b = painted(Some(flat("#5b72bc")), None);
        assert_eq!(
            resolve_theme(&b, true).avatar_bg,
            "#5b72bc",
            "brand token — unset dark reuses light"
        );
        b.dark = Some(crate::agent::DarkPalette {
            avatar_paint: Some(flat("#bd93f9")),
            ..Default::default()
        });
        assert_eq!(resolve_theme(&b, true).avatar_bg, "#bd93f9");
        assert_eq!(resolve_theme(&b, false).avatar_bg, "#5b72bc", "light kept");
    }

    #[test]
    fn input_and_bubble_borders_default_to_border_then_split_off_it() {
        let base = Branding {
            border: Some("#e6eaf0".into()),
            ..Default::default()
        };
        let t = resolve_theme(&base, false);
        assert_eq!(t.input_border, "#e6eaf0");
        assert_eq!(t.agent_bubble_border, "#e6eaf0");
        // Each splits off independently, leaving the shared border alone.
        let split = Branding {
            border: Some("#e6eaf0".into()),
            input_border: Some("#c7d2fe".into()),
            agent_bubble_border: Some("#d1d5db".into()),
            ..Default::default()
        };
        let s = resolve_theme(&split, false);
        assert_eq!(s.input_border, "#c7d2fe");
        assert_eq!(s.agent_bubble_border, "#d1d5db");
        assert_eq!(s.border, "#e6eaf0");
        // Dark: they follow the DARK border, and take their own overrides.
        let d = resolve_theme(&split, true);
        assert_eq!(d.input_border, dark_default::BORDER);
        assert_eq!(d.agent_bubble_border, dark_default::BORDER);
        let mut with_dark = split.clone();
        with_dark.dark = Some(crate::agent::DarkPalette {
            input_border: Some("#333b46".into()),
            agent_bubble_border: Some("#444c58".into()),
            ..Default::default()
        });
        let dd = resolve_theme(&with_dark, true);
        assert_eq!(dd.input_border, "#333b46");
        assert_eq!(dd.agent_bubble_border, "#444c58");
        // Junk falls back to the border rather than reaching the page.
        let junk = Branding {
            border: Some("#e6eaf0".into()),
            input_border: Some("red;}html{display:none".into()),
            ..Default::default()
        };
        assert_eq!(resolve_theme(&junk, false).input_border, "#e6eaf0");
        assert!(!theme_tokens_json(&junk, false).contains("display:none"));
    }

    #[test]
    fn composer_background_defaults_to_the_panel_in_both_modes() {
        // Unset must be a pixel no-op: the composer took its colour from `panel`
        // before this was configurable.
        let plain = Branding {
            panel: Some("#fafbfc".into()),
            ..Default::default()
        };
        let l = resolve_theme(&plain, false);
        assert_eq!(l.input_bg, l.panel);
        assert_eq!(l.input_bg, "#fafbfc");
        // In dark it follows the DARK panel, not the light input background — it is a
        // neutral, so it tracks its own mode's surface.
        let d = resolve_theme(&plain, true);
        assert_eq!(d.input_bg, d.panel);
        assert_eq!(d.input_bg, dark_default::PANEL);
        let d_panel = Branding {
            dark: Some(crate::agent::DarkPalette {
                panel: Some("#101418".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(resolve_theme(&d_panel, true).input_bg, "#101418");
    }

    #[test]
    fn composer_background_can_be_set_per_mode_and_is_validated() {
        let b = Branding {
            panel: Some("#ffffff".into()),
            input_bg: Some("#f1f5f9".into()),
            dark: Some(crate::agent::DarkPalette {
                input_bg: Some("#1b2027".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(resolve_theme(&b, false).input_bg, "#f1f5f9");
        assert_eq!(resolve_theme(&b, false).panel, "#ffffff", "panel untouched");
        assert_eq!(resolve_theme(&b, true).input_bg, "#1b2027");
        // Junk falls back to the panel rather than reaching the page.
        let junk = Branding {
            panel: Some("#ffffff".into()),
            input_bg: Some("transparent;}html{display:none".into()),
            ..Default::default()
        };
        let t = resolve_theme(&junk, false);
        assert_eq!(t.input_bg, "#ffffff");
        assert!(!theme_tokens_json(&junk, false).contains("display:none"));
    }

    #[test]
    fn light_theme_neutrals_default_to_the_established_palette() {
        let t = resolve_theme(&Branding::default(), false);
        assert_eq!(t.surface, "#f7f9fc");
        assert_eq!(t.panel, "#ffffff");
        assert_eq!(t.border, "#e6eaf0");
        assert_eq!(t.muted, "#64748b");
        assert_eq!(t.text, "#0f172a");
        assert_eq!(t.agent_bubble, "#f1f5f9");
        assert_eq!(t.agent_bubble_text, "#0f172a");
    }

    #[test]
    fn dark_theme_uses_curated_neutrals_and_shares_brand_and_structure() {
        let b = Branding {
            primary_color: Some("#1d4be5".into()),
            title: Some("Helper".into()),
            position: Some("left".into()),
            ..Default::default()
        };
        let d = resolve_theme(&b, true);
        // Unset dark neutrals → curated generic-dark defaults.
        assert_eq!(d.surface, dark_default::SURFACE);
        assert_eq!(d.panel, dark_default::PANEL);
        assert_eq!(d.text, dark_default::TEXT);
        assert_eq!(d.agent_bubble, dark_default::AGENT_BUBBLE);
        // Unset dark brand colours → the light brand value (consistency).
        assert_eq!(d.primary, "#1d4be5", "dark primary falls back to light");
        // Structure is shared with light.
        assert_eq!(d.title, "Helper");
        assert_eq!(d.position, "left");
    }

    #[test]
    fn dark_palette_overrides_win_and_are_validated() {
        let b = Branding {
            primary_color: Some("#1d4be5".into()),
            dark: Some(crate::agent::DarkPalette {
                primary_color: Some("#bd93f9".into()),
                surface: Some("#282a36".into()),
                text: Some("not-a-hex".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let d = resolve_theme(&b, true);
        assert_eq!(d.primary, "#bd93f9", "dark override wins over light brand");
        assert_eq!(d.surface, "#282a36", "dark neutral override honoured");
        assert_eq!(
            d.text,
            dark_default::TEXT,
            "invalid dark colour → dark default"
        );
        // The light theme is unaffected by the dark block.
        assert_eq!(resolve_theme(&b, false).primary, "#1d4be5");
        assert_eq!(resolve_theme(&b, false).surface, "#f7f9fc");
    }

    #[test]
    fn dark_accent_is_lifted_when_unset_but_trusts_an_authored_dark_primary() {
        // Unset dark primary → light primary lifted toward white for legibility.
        let unset = Branding {
            primary_color: Some("#05405c".into()),
            ..Default::default()
        };
        let d = resolve_theme(&unset, true);
        assert_eq!(
            d.primary, "#05405c",
            "background primary still the light value"
        );
        assert_ne!(
            d.accent, "#05405c",
            "foreground accent is lifted, not the raw dark bg"
        );
        assert_eq!(d.accent, lighten("#05405c", 0.32));
        // Light accent is just the primary (no lift).
        assert_eq!(resolve_theme(&unset, false).accent, "#05405c");
        // An authored dark primary is trusted as-is for the accent.
        let authored = Branding {
            primary_color: Some("#05405c".into()),
            dark: Some(crate::agent::DarkPalette {
                primary_color: Some("#bd93f9".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(resolve_theme(&authored, true).accent, "#bd93f9");
    }

    #[test]
    fn lighten_moves_toward_white_and_handles_short_hex() {
        assert_eq!(lighten("#000000", 0.5), "#808080");
        assert_eq!(lighten("#000", 1.0), "#ffffff");
        assert_eq!(lighten("#ffffff", 0.5), "#ffffff");
        // Unparseable input is returned unchanged.
        assert_eq!(lighten("nope", 0.5), "nope");
    }

    #[test]
    fn dark_user_bubble_tracks_the_dark_primary_not_the_light_one() {
        let b = Branding {
            primary_color: Some("#1d4be5".into()),
            dark: Some(crate::agent::DarkPalette {
                primary_color: Some("#bd93f9".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let d = resolve_theme(&b, true);
        assert_eq!(d.primary, "#bd93f9");
        assert_eq!(
            d.user_bubble, "#bd93f9",
            "unset dark user bubble follows the dark primary, not the light one"
        );
    }

    #[test]
    fn draggable_defaults_off_and_is_shared_across_modes() {
        assert!(
            !resolve_theme(&Branding::default(), false).draggable,
            "the widget stays put unless the operator opts in"
        );
        let on = Branding {
            draggable: Some(true),
            ..Default::default()
        };
        assert!(resolve_theme(&on, false).draggable);
        assert!(
            resolve_theme(&on, true).draggable,
            "structural — the dark palette inherits it"
        );
        let off = Branding {
            draggable: Some(false),
            ..Default::default()
        };
        assert!(!resolve_theme(&off, false).draggable);
    }

    #[test]
    fn dock_offset_defaults_flush_is_capped_and_shared_across_modes() {
        let d = resolve_theme(&Branding::default(), false);
        assert_eq!(
            (d.offset_x, d.offset_y),
            (0, 0),
            "flush to the corner unless set"
        );
        let set = Branding {
            offset_x: Some(12),
            offset_y: Some(72),
            ..Default::default()
        };
        let light = resolve_theme(&set, false);
        assert_eq!((light.offset_x, light.offset_y), (12, 72));
        let dark = resolve_theme(&set, true);
        assert_eq!(
            (dark.offset_x, dark.offset_y),
            (12, 72),
            "structural — the dark palette inherits it"
        );
        let huge = Branding {
            offset_y: Some(99_999),
            ..Default::default()
        };
        assert_eq!(
            resolve_theme(&huge, false).offset_y,
            panel::DOCK_OFFSET_MAX,
            "capped"
        );
    }

    #[test]
    fn theme_mode_defaults_to_light_and_validates() {
        assert_eq!(theme_mode(&Branding::default()), "light", "unset → light");
        for (raw, want) in [("light", "light"), ("dark", "dark"), ("auto", "auto")] {
            let b = Branding {
                theme_mode: Some(raw.into()),
                ..Default::default()
            };
            assert_eq!(theme_mode(&b), want);
        }
        let junk = Branding {
            theme_mode: Some("System; :root{}".into()),
            ..Default::default()
        };
        assert_eq!(theme_mode(&junk), "light", "junk → light");
    }

    #[test]
    fn accent_color_is_a_back_compat_alias_for_primary() {
        let b = Branding {
            accent_color: Some("#abcdef".into()),
            ..Default::default()
        };
        assert_eq!(resolve_theme(&b, false).primary, "#abcdef");
    }

    fn grad(from: &str, via: Option<&str>, to: &str, angle: Option<&str>) -> Branding {
        Branding {
            gradient: Some(Gradient {
                from: Some(from.into()),
                via: via.map(Into::into),
                to: Some(to.into()),
                angle: angle.map(Into::into),
                stops: None,
            }),
            ..Default::default()
        }
    }

    #[test]
    fn gradient_composes_from_validated_stops_and_defaults_to_right() {
        let three = grad("#054D5C", Some("#828AFA"), "#FBAEC3", Some("to right"));
        assert_eq!(
            resolve_theme(&three, false).gradient,
            "linear-gradient(to right, #054D5C 0%, #828AFA 50%, #FBAEC3 100%)"
        );
        // `via` is optional — a two-stop gradient omits the 50% stop entirely.
        let two = grad("#054D5C", None, "#FBAEC3", None);
        assert_eq!(
            resolve_theme(&two, false).gradient,
            "linear-gradient(to right, #054D5C 0%, #FBAEC3 100%)",
            "angle defaults to `to right`"
        );
        // No gradient block at all ⇒ empty, and the widget keeps the flat primary.
        assert_eq!(resolve_theme(&Branding::default(), false).gradient, "");
    }

    fn with_stops(stops: &[(&str, Option<f32>)]) -> Branding {
        Branding {
            gradient: Some(Gradient {
                stops: Some(
                    stops
                        .iter()
                        .map(|(c, at)| GradientStop {
                            color: Some((*c).into()),
                            at: *at,
                        })
                        .collect(),
                ),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn explicit_stops_carry_their_own_positions() {
        let b = with_stops(&[
            ("#054D5C", Some(0.0)),
            ("#828AFA", Some(35.0)),
            ("#FBAEC3", Some(100.0)),
        ]);
        assert_eq!(
            resolve_theme(&b, false).gradient,
            "linear-gradient(to right, #054D5C 0%, #828AFA 35%, #FBAEC3 100%)"
        );
        // Fractional positions survive, trimmed of pointless zeros.
        let frac = with_stops(&[("#000000", Some(12.5)), ("#ffffff", Some(87.0))]);
        assert_eq!(
            resolve_theme(&frac, false).gradient,
            "linear-gradient(to right, #000000 12.5%, #ffffff 87%)"
        );
    }

    #[test]
    fn stops_are_ordered_clamped_and_bounded() {
        // CSS renders out-of-order stops by clamping each to its predecessor, which
        // silently collapses bands — so they are sorted rather than passed through.
        let jumbled = with_stops(&[
            ("#111111", Some(80.0)),
            ("#222222", Some(10.0)),
            ("#333333", Some(45.0)),
        ]);
        assert_eq!(
            resolve_theme(&jumbled, false).gradient,
            "linear-gradient(to right, #222222 10%, #333333 45%, #111111 80%)"
        );
        // Out-of-range positions clamp into 0-100 rather than emitting invalid CSS.
        let wild = with_stops(&[("#111111", Some(-50.0)), ("#222222", Some(999.0))]);
        assert_eq!(
            resolve_theme(&wild, false).gradient,
            "linear-gradient(to right, #111111 0%, #222222 100%)"
        );
        // More than MAX_STOPS is truncated, not unbounded.
        let many: Vec<(&str, Option<f32>)> = (0..20).map(|_| ("#123456", None)).collect();
        let g = resolve_theme(&with_stops(&many), false).gradient;
        assert_eq!(g.matches('#').count(), MAX_STOPS);
    }

    #[test]
    fn stops_spread_evenly_when_positions_are_omitted() {
        let b = with_stops(&[("#000000", None), ("#888888", None), ("#ffffff", None)]);
        assert_eq!(
            resolve_theme(&b, false).gradient,
            "linear-gradient(to right, #000000 0%, #888888 50%, #ffffff 100%)",
            "matches what the from/via/to shorthand produces"
        );
    }

    #[test]
    fn stops_win_over_the_shorthand_but_only_when_usable() {
        let mut b = grad("#054D5C", Some("#828AFA"), "#FBAEC3", None);
        let shorthand = resolve_theme(&b, false).gradient;
        // Fewer than two valid stops ⇒ the shorthand still applies, so a half-written
        // stops array can't wipe a working gradient.
        for bad in [
            vec![("#111111", Some(0.0))],
            vec![("nope", Some(0.0)), ("also-bad", Some(100.0))],
        ] {
            let g = b.gradient.as_mut().unwrap();
            g.stops = Some(
                bad.iter()
                    .map(|(c, at)| GradientStop {
                        color: Some((*c).into()),
                        at: *at,
                    })
                    .collect(),
            );
            assert_eq!(resolve_theme(&b, false).gradient, shorthand, "{bad:?}");
        }
        // Two usable stops ⇒ stops win.
        let g = b.gradient.as_mut().unwrap();
        g.stops = Some(vec![
            GradientStop {
                color: Some("#111111".into()),
                at: Some(0.0),
            },
            GradientStop {
                color: Some("#222222".into()),
                at: Some(100.0),
            },
        ]);
        assert_eq!(
            resolve_theme(&b, false).gradient,
            "linear-gradient(to right, #111111 0%, #222222 100%)"
        );
    }

    #[test]
    fn a_stop_can_never_smuggle_in_a_paint() {
        let evil = with_stops(&[
            ("url(https://evil.example/x.png)", Some(0.0)),
            ("#ffffff", Some(50.0)),
            ("red;}html{display:none", Some(100.0)),
        ]);
        // Only the one valid stop survives → under two → no gradient at all.
        assert_eq!(resolve_theme(&evil, false).gradient, "");
        let json = theme_tokens_json(&evil, false);
        assert!(!json.contains("url("));
        assert!(!json.contains("display:none"));
    }

    #[test]
    fn gradient_angle_accepts_sides_corners_and_bounded_degrees_only() {
        for ok in ["to left", "to bottom right", "45deg", "0deg", "360deg"] {
            let b = grad("#000000", None, "#ffffff", Some(ok));
            assert!(
                resolve_theme(&b, false).gradient.contains(ok),
                "accepted: {ok}"
            );
        }
        // Junk, out-of-range degrees, and anything that could carry a CSS payload all
        // fall back to the default direction rather than reaching the page.
        for bad in [
            "to nowhere",
            "400deg",
            "-10deg",
            "var(--x)",
            "90deg, red 0%, url(https://evil.example/p.png)",
            "calc(1deg * 90)",
        ] {
            let b = grad("#000000", None, "#ffffff", Some(bad));
            let g = resolve_theme(&b, false).gradient;
            assert_eq!(
                g, "linear-gradient(to right, #000000 0%, #ffffff 100%)",
                "rejected: {bad}"
            );
        }
    }

    #[test]
    fn gradient_needs_both_ends_and_drops_only_a_bad_via() {
        // A missing or invalid endpoint kills the whole gradient — better a flat brand
        // colour than a half-composed paint.
        for (from, to) in [("#054D5C", "not-a-hex"), ("red", "#FBAEC3")] {
            let b = grad(from, None, to, None);
            assert_eq!(resolve_theme(&b, false).gradient, "", "{from} → {to}");
        }
        let missing = Branding {
            gradient: Some(Gradient {
                from: Some("#054D5C".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(resolve_theme(&missing, false).gradient, "");
        // A bad middle stop degrades to the two-stop gradient, not to nothing.
        let bad_via = grad("#054D5C", Some("javascript:alert(1)"), "#FBAEC3", None);
        assert_eq!(
            resolve_theme(&bad_via, false).gradient,
            "linear-gradient(to right, #054D5C 0%, #FBAEC3 100%)"
        );
    }

    #[test]
    fn dark_gradient_falls_back_to_light_but_an_override_wins() {
        let b = grad("#054D5C", Some("#828AFA"), "#FBAEC3", None);
        let light = resolve_theme(&b, false).gradient;
        assert_eq!(
            resolve_theme(&b, true).gradient,
            light,
            "a brand token — an unset dark gradient reuses the light one"
        );
        let mut with_dark = b.clone();
        with_dark.dark = Some(crate::agent::DarkPalette {
            gradient: Some(Gradient {
                from: Some("#111111".into()),
                to: Some("#222222".into()),
                ..Default::default()
            }),
            ..Default::default()
        });
        assert_eq!(
            resolve_theme(&with_dark, true).gradient,
            "linear-gradient(to right, #111111 0%, #222222 100%)"
        );
        assert_eq!(
            resolve_theme(&with_dark, false).gradient,
            light,
            "light kept"
        );
        // An unusable dark override falls back to light rather than to no gradient.
        let mut junk = b.clone();
        junk.dark = Some(crate::agent::DarkPalette {
            gradient: Some(Gradient {
                from: Some("nope".into()),
                to: Some("#222222".into()),
                ..Default::default()
            }),
            ..Default::default()
        });
        assert_eq!(resolve_theme(&junk, true).gradient, light);
    }

    fn sized(w: Option<u32>, h: Option<u32>, mw: Option<u32>, mh: Option<u32>) -> Branding {
        Branding {
            width: w,
            height: h,
            min_width: mw,
            min_height: mh,
            ..Default::default()
        }
    }

    #[test]
    fn panel_size_defaults_match_the_widget_and_pass_valid_config_through() {
        let d = resolve_theme(&Branding::default(), false);
        assert_eq!((d.width, d.height), (400, 660));
        assert_eq!((d.min_width, d.min_height), (320, 420));
        let set = resolve_theme(&sized(Some(520), Some(720), Some(360), Some(480)), false);
        assert_eq!((set.width, set.height), (520, 720));
        assert_eq!((set.min_width, set.min_height), (360, 480));
        // Structural — the dark palette inherits it.
        let dark = resolve_theme(&sized(Some(520), Some(720), None, None), true);
        assert_eq!((dark.width, dark.height), (520, 720));
    }

    #[test]
    fn no_config_can_produce_an_unusable_panel() {
        // The whole point of the clamp: whatever an operator writes — zero, one px,
        // absurdly large — the panel stays within a range that still works.
        for (w, h) in [(Some(0), Some(0)), (Some(1), Some(1)), (Some(50), Some(90))] {
            let t = resolve_theme(&sized(w, h, w, h), false);
            assert!(t.width >= 280 && t.height >= 360, "floor: {t:?}");
            assert!(
                t.min_width >= 280 && t.min_height >= 360,
                "min floor: {t:?}"
            );
        }
        let huge = resolve_theme(&sized(Some(99_999), Some(99_999), None, None), false);
        assert_eq!((huge.width, huge.height), (3000, 3000), "ceiling");
    }

    #[test]
    fn the_minimum_can_never_exceed_the_opening_size() {
        // Otherwise the panel opens smaller than a customer is allowed to shrink it
        // to, and the resize grip fights the initial size.
        let t = resolve_theme(&sized(Some(360), Some(400), Some(900), Some(900)), false);
        assert_eq!((t.width, t.height), (360, 400));
        assert_eq!(
            (t.min_width, t.min_height),
            (360, 400),
            "min pulled down to the opening size"
        );
        // And it holds when only the opening size is shrunk below the DEFAULT min.
        let small = resolve_theme(&sized(Some(300), Some(380), None, None), false);
        assert!(small.min_width <= small.width && small.min_height <= small.height);
    }

    #[test]
    fn frame_thickness_defaults_to_two_and_is_bounded() {
        assert_eq!(resolve_theme(&Branding::default(), false).frame_width, 2);
        for (cfg, want) in [
            (Some(0), 0),
            (Some(8), 8),
            (Some(24), 24),
            (Some(9_999), 24),
        ] {
            let b = Branding {
                frame_width: cfg,
                ..Default::default()
            };
            assert_eq!(resolve_theme(&b, false).frame_width, want, "{cfg:?}");
        }
        // Structural — the dark palette inherits it.
        let b = Branding {
            frame_width: Some(6),
            ..Default::default()
        };
        assert_eq!(resolve_theme(&b, true).frame_width, 6);
    }

    #[test]
    fn presentation_options_default_to_the_pre_existing_look() {
        // The whole point: a client that asks for none of these renders exactly as it
        // did before they existed. Different brands want different treatments, so none
        // of them may ride on merely having configured a gradient.
        let with_gradient = grad("#054D5C", Some("#828AFA"), "#FBAEC3", None);
        for b in [Branding::default(), with_gradient] {
            let t = resolve_theme(&b, false);
            assert!(!t.frame, "flat panel unless the agent opts in");
            assert_eq!(t.send_shape, "rounded");
            assert_eq!(t.send_icon, "arrow");
            assert_eq!(t.logo_fit, "cover");
            assert!(!t.avatar);
        }
    }

    #[test]
    fn presentation_options_are_validated_and_shared_across_modes() {
        let b = Branding {
            frame: Some(true),
            send_shape: Some("circle".into()),
            send_icon: Some("plane".into()),
            logo_fit: Some("contain".into()),
            ..Default::default()
        };
        let t = resolve_theme(&b, false);
        assert!(t.frame);
        assert_eq!(t.send_shape, "circle");
        assert_eq!(t.send_icon, "plane");
        assert_eq!(t.logo_fit, "contain");
        let d = resolve_theme(&b, true);
        assert!(d.frame, "structural — the dark palette inherits it");
        assert_eq!(d.send_shape, "circle");
        assert_eq!(d.send_icon, "plane");
        assert_eq!(d.logo_fit, "contain");
        // Junk falls back to the default rather than reaching the page.
        let junk = Branding {
            send_shape: Some("blob;}html{display:none".into()),
            send_icon: Some("rocket".into()),
            logo_fit: Some("fill".into()),
            ..Default::default()
        };
        let j = resolve_theme(&junk, false);
        assert_eq!(j.send_shape, "rounded");
        assert_eq!(j.send_icon, "arrow");
        assert_eq!(j.logo_fit, "cover");
        assert!(!theme_tokens_json(&junk, false).contains("display:none"));
    }

    #[test]
    fn expand_is_a_valid_launcher_and_avatar_defaults_off() {
        let b = Branding {
            launcher: Some("expand".into()),
            avatar: Some(true),
            ..Default::default()
        };
        let t = resolve_theme(&b, false);
        assert_eq!(t.launcher, "expand");
        assert!(t.avatar);
        assert!(
            resolve_theme(&b, true).avatar,
            "structural — shared with dark"
        );
        let d = resolve_theme(&Branding::default(), false);
        assert_eq!(d.launcher, "bubble");
        assert!(!d.avatar, "no avatar unless the operator opts in");
    }

    #[test]
    fn a_gradient_never_becomes_the_primary_colour() {
        // The stylesheet feeds `primary` to 17 `color-mix()` calls and `accent-color`,
        // none of which accept a gradient. Adding a gradient must leave it a flat hex.
        let b = grad("#054D5C", Some("#828AFA"), "#FBAEC3", None);
        let t = resolve_theme(&b, false);
        assert!(is_hex_color(&t.primary), "primary stays a <color>");
        assert!(!t.primary.contains("gradient"));
        assert!(is_hex_color(&t.user_bubble) && is_hex_color(&t.accent));
    }

    #[test]
    fn data_image_logo_is_allowed_but_data_html_is_not() {
        assert!(is_safe_logo("data:image/svg+xml;base64,AAAA"));
        assert!(!is_safe_logo("data:text/html,<script>"));
        assert!(is_safe_logo("https://x/y.png"));
        assert!(!is_safe_logo("//evil.example/x.png"));
    }
}
