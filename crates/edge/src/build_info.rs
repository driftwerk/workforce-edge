//! Build provenance for the running edge binary.
//!
//! `COMMIT_SHA` and `BUILD_DATE` are baked into the image as env vars at
//! image-build time (`Dockerfile.edge`, fed by CI build-args) and read here at
//! *runtime* — the binary is compiled before either value exists, so a
//! compile-time `env!` would always see them unset.
//!
//! Deliberately a sibling of the hub's `build_info` rather than a shared
//! helper: `crate_version()` must report the version of the crate that is
//! actually running, and the edge cannot depend on the hub.
//!
//! Consumed by the public `/version` endpoint and the `edge.version` tool, so
//! "which build is live?" can be answered from a browser, from ops tooling, or
//! by asking the assistant — the question every deploy in this repo has needed
//! and none could answer without shelling onto the host.

/// Full git commit SHA the image was built from, or `"dev"` outside Docker.
pub fn commit_sha() -> String {
    non_empty_env("COMMIT_SHA").unwrap_or_else(|| "dev".to_string())
}

/// The first 7 characters of [`commit_sha`] — what a human quotes.
pub fn short_sha() -> String {
    let sha = commit_sha();
    sha.chars().take(7).collect()
}

/// UTC ISO-8601 timestamp the image was built at, or `"unknown"` outside Docker.
pub fn build_date() -> String {
    non_empty_env("BUILD_DATE").unwrap_or_else(|| "unknown".to_string())
}

/// Crate version from Cargo.toml (compile-time).
pub fn crate_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// The Docker image tag CI publishes for this commit (`sha-<full sha>`), or
/// `None` for non-Docker builds. Lets ops check the *running* build against the
/// registry — the silent-rollback case where a deploy reports success while the
/// previous image keeps serving.
pub fn image_tag() -> Option<String> {
    non_empty_env("COMMIT_SHA").map(|sha| format!("sha-{sha}"))
}

/// Identity of THIS process among the replicas of this install.
///
/// Docker (and Swarm) set `HOSTNAME` to the container id, which is stable for a
/// task's lifetime and differs between replicas — exactly the granularity turn
/// ownership needs. Not build provenance strictly speaking, but it answers the
/// same operational question ("which thing served this?") and shares the env
/// plumbing.
///
/// Falls back to `"local"` outside a container, so a single-process dev run has
/// one consistent id rather than an empty string.
pub fn replica_id() -> String {
    static ID: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
        non_empty_env("HOSTNAME").unwrap_or_else(|| "local".to_string())
    });
    ID.clone()
}

/// Build info as the canonical JSON shape shared by the endpoint and the tool.
/// Carries both the full and short SHA — callers display whichever they need.
pub fn as_json() -> serde_json::Value {
    serde_json::json!({
        "version": crate_version(),
        "commit_sha": commit_sha(),
        "short_sha": short_sha(),
        "build_date": build_date(),
        "image_tag": image_tag(),
        "replica": replica_id(),
    })
}

/// Env var, treating unset, empty, and the Dockerfile `unknown` default as
/// absent (a local `docker build` without build-args bakes the defaults in).
fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty() && v != "unknown")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_carries_every_field() {
        let json = as_json();
        for key in [
            "version",
            "commit_sha",
            "short_sha",
            "build_date",
            "replica",
        ] {
            assert!(json.get(key).is_some(), "missing {key}");
        }
    }

    #[test]
    fn short_sha_is_a_prefix_of_the_full_sha() {
        assert!(commit_sha().starts_with(&short_sha()));
        assert!(short_sha().chars().count() <= 7);
    }
}
