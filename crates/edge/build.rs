// The widget UI (crates/edge/widget-ui) builds to widget-ui/dist and is embedded
// into the binary with rust-embed. Ensure the folder exists so a Rust-only build
// (no `npm run build`) still compiles — it just embeds nothing — and re-run the
// build when the bundle changes so a rebuilt UI is actually re-embedded (rust-embed
// has no such trigger of its own; the hub hit exactly this stale-embed bug).
fn main() {
    workforce_build_support::track_dir("migrations");
    for dist in ["widget-ui/dist", "ui-admin/dist", "ui-platform/dist"] {
        let path = std::path::Path::new(dist);
        if !path.exists() {
            let _ = std::fs::create_dir_all(path);
        }
        println!("cargo:rerun-if-changed={dist}");
    }
}
