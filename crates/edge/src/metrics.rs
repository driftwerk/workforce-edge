//! Prometheus metrics: the process recorder, the `edge_mcp_*` instruments the
//! MCP layer records through, and the bearer-guarded `GET /metrics` route.
//!
//! The recorder is the `metrics` facade's global one, installed with
//! `metrics-exporter-prometheus` exactly as the core crates install theirs, and
//! only when `[metrics]` is enabled — so an install that never asks for
//! metrics records nothing. Every label value comes from configuration (agent
//! and server ids, configured audiences, allowlisted tool names) or a fixed
//! set of outcomes, never from a customer, a token, tool arguments or a URL,
//! which keeps the series count bounded by the config.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use eyre::{eyre, Result};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use serde::Deserialize;

use crate::ingress::{bearer_token, digests_match, token_digest, TokenDigest};
use crate::platform::MIN_SERVICE_TOKEN_LEN;

/// `[metrics]` — the Prometheus endpoint. Off unless `enabled`; when on, a
/// scraper must present `bearer_token` (at least [`MIN_SERVICE_TOKEN_LEN`]
/// characters, typically `${EDGE_METRICS_TOKEN}`).
#[derive(Clone, Default, Deserialize)]
pub struct MetricsConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub bearer_token: Option<String>,
}

// Manual Debug so an accidental `{:?}` of the config never prints the token.
impl std::fmt::Debug for MetricsConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MetricsConfig")
            .field("enabled", &self.enabled)
            .field("has_bearer_token", &self.bearer_token.is_some())
            .finish()
    }
}

static HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();

/// Install the global Prometheus recorder, as the core crates do. A failure
/// is logged, not fatal: the endpoint then says the recorder is missing.
pub fn install_recorder() {
    match PrometheusBuilder::new().install_recorder() {
        Ok(handle) => {
            let _ = HANDLE.set(handle);
        }
        Err(e) => tracing::warn!(error = %e, "failed to install the Prometheus recorder"),
    }
}

/// The enabled `/metrics` route: the digest of the one token that may read it.
pub struct MetricsEndpoint {
    token: TokenDigest,
}

impl MetricsEndpoint {
    /// The endpoint `config` asks for: `None` when absent or disabled (the
    /// route is then not mounted at all), an error when enabled without a
    /// token long enough to stand in for one.
    pub fn from_config(config: Option<&MetricsConfig>) -> Result<Option<Self>> {
        let Some(config) = config.filter(|c| c.enabled) else {
            return Ok(None);
        };
        let token = config
            .bearer_token
            .as_deref()
            .map(str::trim)
            .filter(|t| t.chars().count() >= MIN_SERVICE_TOKEN_LEN)
            .ok_or_else(|| {
                eyre!(
                    "[metrics] is enabled but bearer_token is missing or shorter than \
                     {MIN_SERVICE_TOKEN_LEN} characters; the endpoint is never served \
                     unauthenticated"
                )
            })?;
        Ok(Some(Self {
            token: token_digest(token),
        }))
    }

    fn admits(&self, headers: &HeaderMap) -> bool {
        bearer_token(headers, None).is_some_and(|t| digests_match(&self.token, &token_digest(&t)))
    }
}

/// `GET /metrics`, for merging into the edge's router.
pub fn router<S>(endpoint: Arc<MetricsEndpoint>) -> Router<S> {
    Router::new()
        .route("/metrics", get(render))
        .with_state(endpoint)
}

async fn render(State(endpoint): State<Arc<MetricsEndpoint>>, headers: HeaderMap) -> Response {
    if !endpoint.admits(&headers) {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer")],
        )
            .into_response();
    }
    let body = HANDLE
        .get()
        .map(PrometheusHandle::render)
        .unwrap_or_else(|| {
            "# prometheus recorder not installed; check the log at startup\n".to_string()
        });
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        body,
    )
        .into_response()
}

/// Which discovery an MCP server went through.
#[derive(Debug, Clone, Copy)]
pub(crate) enum DiscoveryPhase {
    /// At build, with the service credential.
    Startup,
    /// On a turn, because nothing was cached yet.
    Lazy,
    /// On a turn, because the cache was past its TTL.
    Refresh,
}

impl DiscoveryPhase {
    fn as_str(self) -> &'static str {
        match self {
            Self::Startup => "startup",
            Self::Lazy => "lazy",
            Self::Refresh => "refresh",
        }
    }
}

/// How a turn found a server's cached tool list.
#[derive(Debug, Clone, Copy)]
pub(crate) enum CacheResult {
    Hit,
    Miss,
    Stale,
}

impl CacheResult {
    fn as_str(self) -> &'static str {
        match self {
            Self::Hit => "hit",
            Self::Miss => "miss",
            Self::Stale => "stale",
        }
    }
}

/// How one tool call ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CallOutcome {
    Ok,
    /// The server answered with a tool-level failure (`isError`, or a
    /// JSON-RPC error).
    ToolError,
    /// The call did not get an answer: connection, auth, rate limit, body.
    TransportError,
    Timeout,
}

impl CallOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::ToolError => "tool_error",
            Self::TransportError => "transport_error",
            Self::Timeout => "timeout",
        }
    }
}

pub(crate) fn discovery(
    agent: &str,
    server: &str,
    phase: DiscoveryPhase,
    outcome: &'static str,
    elapsed: Duration,
) {
    let (agent, server, phase) = (agent.to_string(), server.to_string(), phase.as_str());
    metrics::counter!("edge_mcp_discovery_total", "agent" => agent.clone(), "server" => server.clone(), "phase" => phase, "outcome" => outcome)
        .increment(1);
    metrics::histogram!("edge_mcp_discovery_duration_seconds", "agent" => agent, "server" => server, "phase" => phase)
        .record(elapsed.as_secs_f64());
}

pub(crate) fn tools_cache(agent: &str, server: &str, result: CacheResult) {
    metrics::counter!("edge_mcp_tools_cache_total", "agent" => agent.to_string(), "server" => server.to_string(), "result" => result.as_str())
        .increment(1);
}

pub(crate) fn tool_call(
    agent: &str,
    server: &str,
    tool: &str,
    outcome: CallOutcome,
    elapsed: Duration,
) {
    let (agent, server, tool) = (agent.to_string(), server.to_string(), tool.to_string());
    metrics::counter!("edge_mcp_tool_calls_total", "agent" => agent.clone(), "server" => server.clone(), "tool" => tool.clone(), "outcome" => outcome.as_str())
        .increment(1);
    metrics::histogram!("edge_mcp_tool_call_duration_seconds", "agent" => agent, "server" => server, "tool" => tool)
        .record(elapsed.as_secs_f64());
}

pub(crate) fn server_skipped(agent: &str, server: &str) {
    metrics::counter!("edge_mcp_server_skipped_total", "agent" => agent.to_string(), "server" => server.to_string())
        .increment(1);
}

pub(crate) fn skip_window_entered(agent: &str, server: &str) {
    metrics::counter!("edge_mcp_skip_window_entered_total", "agent" => agent.to_string(), "server" => server.to_string())
        .increment(1);
}

/// The label an exchange's audience is counted under: the configured value,
/// or `none` for an exchange that asks for no audience.
fn audience_label(audience: Option<&str>) -> String {
    audience.unwrap_or("none").to_string()
}

pub(crate) fn token_exchange(
    agent: &str,
    audience: Option<&str>,
    outcome: &'static str,
    elapsed: Duration,
) {
    let (agent, audience) = (agent.to_string(), audience_label(audience));
    metrics::counter!("edge_mcp_token_exchanges_total", "agent" => agent.clone(), "audience" => audience.clone(), "outcome" => outcome)
        .increment(1);
    metrics::histogram!("edge_mcp_token_exchange_duration_seconds", "agent" => agent, "audience" => audience)
        .record(elapsed.as_secs_f64());
}

pub(crate) fn config_refused(reason: &'static str) {
    metrics::counter!("edge_mcp_config_refusals_total", "reason" => reason).increment(1);
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// Every metric `snapshotter` saw, as `name{label=value,…}` (labels
    /// sorted) → its count (a histogram counts its samples).
    pub(crate) fn recorded(
        snapshotter: &metrics_util::debugging::Snapshotter,
    ) -> BTreeMap<String, u64> {
        use metrics_util::debugging::DebugValue;
        snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .map(|(key, _, _, value)| {
                let key = key.key();
                let mut labels: Vec<String> = key
                    .labels()
                    .map(|l| format!("{}={}", l.key(), l.value()))
                    .collect();
                labels.sort();
                let count = match value {
                    DebugValue::Counter(n) => n,
                    DebugValue::Histogram(samples) => samples.len() as u64,
                    DebugValue::Gauge(_) => 1,
                };
                (format!("{}{{{}}}", key.name(), labels.join(",")), count)
            })
            .collect()
    }

    /// The label keys any `edge_mcp_*` metric may carry: none of them can
    /// hold a customer, a token, a tool's arguments or a URL.
    pub(crate) const BOUNDED_LABELS: &[&str] = &[
        "agent", "server", "phase", "outcome", "result", "tool", "audience", "reason",
    ];

    pub(crate) fn assert_bounded(metrics: &BTreeMap<String, u64>) {
        for key in metrics.keys() {
            let labels = &key[key.find('{').unwrap() + 1..key.len() - 1];
            for label in labels.split(',').filter(|l| !l.is_empty()) {
                let name = label.split('=').next().unwrap();
                assert!(
                    BOUNDED_LABELS.contains(&name),
                    "{key}: unbounded label {name}"
                );
            }
        }
    }

    #[test]
    fn the_endpoint_needs_a_long_enough_token_when_enabled() {
        assert!(MetricsEndpoint::from_config(None).unwrap().is_none());
        let disabled = MetricsConfig {
            enabled: false,
            bearer_token: None,
        };
        assert!(MetricsEndpoint::from_config(Some(&disabled))
            .unwrap()
            .is_none());
        for token in [None, Some("short")] {
            let enabled = MetricsConfig {
                enabled: true,
                bearer_token: token.map(str::to_string),
            };
            let err = MetricsEndpoint::from_config(Some(&enabled))
                .err()
                .expect("refused");
            assert!(
                err.to_string().contains("never served unauthenticated"),
                "{err}"
            );
        }
        let printed = format!(
            "{:?}",
            MetricsConfig {
                enabled: true,
                bearer_token: Some("m".repeat(40)),
            }
        );
        assert!(!printed.contains("mmmm"), "{printed}");
    }

    #[test]
    fn instruments_carry_only_bounded_labels() {
        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        metrics::with_local_recorder(&recorder, || {
            discovery(
                "acme-help",
                "docs",
                DiscoveryPhase::Startup,
                "ok",
                Duration::from_millis(5),
            );
            tools_cache("acme-help", "docs", CacheResult::Hit);
            tools_cache("acme-help", "docs", CacheResult::Hit);
            tool_call(
                "acme-help",
                "docs",
                "docs.search",
                CallOutcome::ToolError,
                Duration::from_millis(5),
            );
            server_skipped("acme-help", "docs");
            skip_window_entered("acme-help", "docs");
            token_exchange("acme-help", None, "ok", Duration::from_millis(5));
            config_refused("name_collision");
        });
        let metrics = recorded(&snapshotter);
        assert_bounded(&metrics);
        for expected in [
            "edge_mcp_discovery_total{agent=acme-help,outcome=ok,phase=startup,server=docs}",
            "edge_mcp_discovery_duration_seconds{agent=acme-help,phase=startup,server=docs}",
            "edge_mcp_tool_calls_total{agent=acme-help,outcome=tool_error,server=docs,tool=docs.search}",
            "edge_mcp_tool_call_duration_seconds{agent=acme-help,server=docs,tool=docs.search}",
            "edge_mcp_server_skipped_total{agent=acme-help,server=docs}",
            "edge_mcp_skip_window_entered_total{agent=acme-help,server=docs}",
            "edge_mcp_token_exchanges_total{agent=acme-help,audience=none,outcome=ok}",
            "edge_mcp_token_exchange_duration_seconds{agent=acme-help,audience=none}",
            "edge_mcp_config_refusals_total{reason=name_collision}",
        ] {
            assert_eq!(metrics.get(expected), Some(&1), "{expected} in {metrics:?}");
        }
        assert_eq!(
            metrics.get("edge_mcp_tools_cache_total{agent=acme-help,result=hit,server=docs}"),
            Some(&2),
            "a counter accumulates"
        );
    }
}
