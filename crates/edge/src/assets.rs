//! The embedded widget front-end.
//!
//! The chat UI is a TypeScript SPA (`crates/edge/widget-ui`) built to
//! `widget-ui/dist` and baked into the binary with `rust-embed`. Two surfaces:
//!
//! - `GET /embed/{agent}` serves the tiny **loader** script the host page
//!   includes — prefixed with a per-agent boot config (agent id, allowed
//!   origins, validated theme, a per-load nonce). The loader mounts the SPA in
//!   a sandboxed iframe and bridges the token/theme over `postMessage`.
//! - `GET /widget/…` serves the SPA's own assets (its `index.html` + hashed
//!   JS/CSS), loaded inside that iframe.
//!
//! The SPA carries no secret; the customer token is delivered later over the
//! postMessage handshake and every privileged call is authed server-side.

use rust_embed::RustEmbed;

use crate::agent::EmbedSettings;

#[derive(RustEmbed)]
#[folder = "widget-ui/dist/"]
struct WidgetAssets;

/// The operator admin SPA (crates/edge/ui-admin), served under `/admin` behind
/// the admin session guard.
#[derive(RustEmbed)]
#[folder = "ui-admin/dist/"]
struct AdminAssets;

/// The loader script for one agent: a JSON boot config (with the edge origin
/// computed client-side from the script's own URL) followed by the bundled
/// loader. `None` if the UI bundle was not built into this binary.
pub fn loader_script(
    agent_id: &str,
    embed: &EmbedSettings,
    welcome: &str,
    nonce: &str,
    can_stage: bool,
) -> Option<String> {
    let loader = WidgetAssets::get("loader.js")?;
    let body = std::str::from_utf8(&loader.data).ok()?;
    let json = |v: &str| serde_json::to_string(v).unwrap_or_else(|_| "\"\"".into());
    let origins = serde_json::to_string(&embed.allowed_origins).unwrap_or_else(|_| "[]".into());
    let theme = crate::theme::theme_tokens_json(&embed.branding, false);
    let theme_dark = crate::theme::theme_tokens_json(&embed.branding, true);
    let theme_mode = crate::theme::theme_mode(&embed.branding);
    // `base` is a JS expression (the loader's own origin), so a cross-origin
    // embed resolves the edge origin without the server knowing its public URL.
    let config = format!(
        "window.__WF_EDGE__={{\"agent\":{agent},\"origins\":{origins},\"theme\":{theme},\
         \"theme_dark\":{theme_dark},\"theme_mode\":{theme_mode},\"welcome\":{welcome},\
         \"features\":{{\"tables\":{tables},\"charts\":{charts},\"forms\":{forms},\"maps\":{maps},\
         \"diagrams\":{diagrams},\"excalidraw\":{excalidraw},\
         \"inline_images\":{inline_images},\"images\":{images},\"uploads\":{uploads},\
         \"upload_max_mb\":{upload_max_mb},\"drawing\":{drawing}}},\
         \"nonce\":{nonce},\"base\":(document.currentScript?new URL(document.currentScript.src).origin:location.origin)}};\n",
        agent = json(agent_id),
        theme_mode = json(theme_mode),
        welcome = json(welcome),
        tables = embed.data_tables_enabled(),
        charts = embed.charts_enabled(),
        forms = embed.forms_enabled(),
        maps = embed.maps_enabled(),
        diagrams = embed.diagrams_enabled(),
        excalidraw = embed.excalidraw_export_enabled(),
        inline_images = embed.inline_images_enabled(),
        // A `wf-image` block names an artifact the widget must fetch, so without
        // a store there is nothing to fetch and the fence would render as a
        // broken picture. Advertised OFF rather than half-working.
        images = can_stage && embed.images_enabled(),
        // Uploads and the canvas need somewhere to stage bytes. Without an
        // artifact store they are advertised as OFF rather than offered and then
        // refused — a control that cannot work must not appear.
        uploads = serde_json::to_string(match can_stage {
            true => embed.uploads.as_slice(),
            false => &[],
        })
        .unwrap_or_else(|_| "[]".into()),
        upload_max_mb = embed.upload_max_bytes() / (1024 * 1024),
        drawing = can_stage && embed.drawing_enabled(),
        nonce = json(nonce),
    );
    Some(format!("{config}{body}"))
}

/// One built SPA asset by its path under `/widget/` (`""`/`index.html` → the
/// app shell), as `(content_type, bytes)`.
pub fn widget_asset(path: &str) -> Option<(String, std::borrow::Cow<'static, [u8]>)> {
    let rel = match path.trim_start_matches('/') {
        "" => "index.html",
        p => p,
    };
    let file = WidgetAssets::get(rel)?;
    let mime = mime_guess::from_path(rel).first_or_octet_stream();
    Some((mime.as_ref().to_string(), file.data))
}

/// One built admin-SPA asset by its path under `/admin/`. Unknown non-asset
/// paths fall back to `index.html` so client-side routing works on reload.
pub fn admin_asset(path: &str) -> Option<(String, std::borrow::Cow<'static, [u8]>)> {
    let rel = path.trim_start_matches('/');
    let (rel, file) = match AdminAssets::get(rel) {
        Some(f) if !rel.is_empty() => (rel, f),
        _ => ("index.html", AdminAssets::get("index.html")?),
    };
    let mime = mime_guess::from_path(rel).first_or_octet_stream();
    Some((mime.as_ref().to_string(), file.data))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{Branding, McpSettings};

    fn embed() -> EmbedSettings {
        EmbedSettings {
            allowed_origins: vec!["https://app.example".into()],
            allowed_tools: vec![],
            allowed_emails: vec![],
            keycloak_issuer: "https://kc".into(),
            keycloak_audience: None,
            mcp: None::<McpSettings>,
            branding: Branding {
                title: Some("Helper".into()),
                primary_color: Some("#05405C".into()),
                ..Default::default()
            },
            passive_memory: None,
            data_tables: None,
            charts: None,
            forms: None,
            maps: None,
            diagrams: None,
            excalidraw_export: None,
            uploads: vec![],
            upload_max_mb: None,
            drawing: None,
            inline_images: None,
            images: None,
            image_hosts: vec![],
            spill_skip_tools: vec![],
            tool_grants: vec![],
            permissions: None,
            policy: None,
        }
    }

    // These assert the bundle is embedded — they only pass once `npm run build`
    // has produced widget-ui/dist (CI does this before the Rust build).
    #[test]
    fn loader_script_injects_agent_theme_and_nonce() {
        let js = loader_script("assistant", &embed(), "Hi there!", "abc123", true)
            .expect("bundle embedded");
        assert!(js.contains("window.__WF_EDGE__"));
        assert!(js.contains(r#""assistant""#), "agent id injected");
        assert!(js.contains(r#""abc123""#), "nonce injected");
        assert!(js.contains(r#""https://app.example""#), "origins injected");
        assert!(js.contains("#05405C"), "resolved light theme injected");
        assert!(js.contains(r#""theme_dark":{"#), "dark palette injected");
        assert!(
            js.contains(r#""theme_mode":"light""#),
            "theme mode defaults to light"
        );
        assert!(js.contains(r#""Hi there!""#), "welcome injected");
        assert!(
            js.contains(
                r#""features":{"tables":false,"charts":false,"forms":false,"maps":false,"diagrams":false,"excalidraw":false,"inline_images":false,"images":false,"uploads":[],"upload_max_mb":10,"drawing":false}"#
            ),
            "rich-output features default off"
        );
        assert!(
            js.contains(r#""draggable":false"#),
            "repositioning defaults off"
        );
        assert!(
            js.contains(r#""offset_x":0,"offset_y":0"#),
            "the dock sits flush to its corner by default"
        );
        assert!(
            js.contains("document.currentScript"),
            "base computed client-side"
        );
    }

    #[test]
    fn loader_script_injects_draggable_into_both_palettes_when_enabled() {
        let mut e = embed();
        e.branding.draggable = Some(true);
        let js = loader_script("assistant", &e, "", "n", true).expect("bundle embedded");
        // The boot config is the first line; everything after it is the bundle, whose
        // contents this assertion has no business depending on.
        let config = js.lines().next().expect("boot config line");
        assert_eq!(
            config.matches(r#""draggable":true"#).count(),
            2,
            "the light and dark palettes each carry it: {config}"
        );
    }

    #[test]
    fn loader_script_injects_the_dock_offset_into_both_palettes() {
        let mut e = embed();
        e.branding.offset_y = Some(72);
        let js = loader_script("assistant", &e, "", "n", true).expect("bundle embedded");
        let config = js.lines().next().expect("boot config line");
        assert_eq!(
            config.matches(r#""offset_x":0,"offset_y":72"#).count(),
            2,
            "structural, so the light and dark palettes each carry it: {config}"
        );
    }

    /// `wf-image` names an artifact the widget has to fetch. With no store there
    /// is nothing to fetch, so the feature is advertised OFF — the fence would
    /// otherwise render as a broken picture on every reply.
    #[test]
    fn showing_images_is_off_when_the_host_cannot_stage_artifacts() {
        let mut e = embed();
        e.images = Some(true);
        let js = loader_script("assistant", &e, "", "n", false).expect("bundle embedded");
        assert!(js.contains(r#""images":false"#), "{js}");
        let js = loader_script("assistant", &e, "", "n", true).expect("bundle embedded");
        assert!(js.contains(r#""images":true"#), "{js}");
    }

    #[test]
    fn loader_script_injects_enabled_rich_output_features() {
        let mut e = embed();
        e.data_tables = Some(true);
        e.forms = Some(true);
        e.diagrams = Some(true);
        e.excalidraw_export = Some(true);
        e.inline_images = Some(true);
        e.images = Some(true);
        e.uploads = vec![crate::agent::UploadKind::Image];
        e.drawing = Some(true);
        let js = loader_script("assistant", &e, "", "n", true).expect("bundle embedded");
        assert!(js.contains(
            r#""features":{"tables":true,"charts":false,"forms":true,"maps":false,"diagrams":true,"excalidraw":true,"inline_images":true,"images":true,"uploads":["image"],"upload_max_mb":10,"drawing":true}"#
        ));
    }

    /// An install with no `[artifacts]` store cannot stage bytes, so the widget
    /// must not offer an attach control or a canvas. Advertising them and then
    /// refusing the upload is what happened on driftwerk: the flags were on, the
    /// store was absent, and the customer got told the assistant does not accept
    /// uploads — which pointed at a setting that was already correct.
    #[test]
    fn uploads_are_advertised_off_when_there_is_nowhere_to_stage() {
        let mut e = embed();
        e.uploads = vec![crate::agent::UploadKind::Image];
        e.drawing = Some(true);

        let with = loader_script("assistant", &e, "", "n", true).expect("bundle embedded");
        assert!(with.contains(r#""uploads":["image"]"#));
        assert!(with.contains(r#""drawing":true"#));

        let without = loader_script("assistant", &e, "", "n", false).expect("bundle embedded");
        assert!(
            without.contains(r#""uploads":[]"#),
            "no store ⇒ no attach control"
        );
        assert!(
            without.contains(r#""drawing":false"#),
            "no store ⇒ no canvas"
        );
    }

    #[test]
    fn loader_script_injects_the_configured_theme_mode() {
        let mut e = embed();
        e.branding.theme_mode = Some("auto".into());
        let js = loader_script("assistant", &e, "", "n", true).expect("bundle embedded");
        assert!(js.contains(r#""theme_mode":"auto""#));
    }

    #[test]
    fn admin_asset_serves_the_shell_with_spa_fallback() {
        let (ct, _) = admin_asset("").expect("admin index embedded");
        assert!(ct.starts_with("text/html"), "empty path → index.html");
        // Unknown client-route path falls back to the shell (SPA routing).
        let (ct, _) = admin_asset("/some/deep/route").expect("fallback to index");
        assert!(ct.starts_with("text/html"));
    }

    #[test]
    fn widget_asset_serves_the_spa_shell_and_bundle() {
        let (ct, _) = widget_asset("").expect("index embedded");
        assert!(
            ct.starts_with("text/html"),
            "empty path → index.html, got {ct}"
        );
        // Asset filenames are content-hashed (assets/app-<hash>.js), so locate
        // the JS bundle by extension rather than a fixed name.
        let js = WidgetAssets::iter()
            .find(|p| p.starts_with("assets/") && p.ends_with(".js"))
            .expect("a JS bundle is embedded");
        let (ct, _) = widget_asset(&js).expect("app bundle embedded");
        assert!(ct.contains("javascript"), "js content-type, got {ct}");
        assert!(
            widget_asset("../secret").is_none(),
            "no traversal escapes the bundle"
        );
    }
}
