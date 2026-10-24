//! `edge.version` tool — reports the build this edge is running: short git SHA
//! and image build date. Lets a customer or an operator ask the assistant "what
//! version are you?" and get the same answer `/version` serves, instead of the
//! model guessing from its training data.

use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use workforce_agent::tools::{Tool, ToolResult};

pub struct EdgeVersionTool;

#[async_trait]
impl Tool for EdgeVersionTool {
    fn name(&self) -> &str {
        "edge.version"
    }

    fn description(&self) -> &str {
        "Show the assistant's own deployed build: short git commit SHA, image build \
         date and crate version. Use when asked what version, build or release is \
         running, or how old the running deployment is. Report the short SHA and the \
         build date; never guess these."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    }

    async fn execute(&self, _input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        Ok(ToolResult::success(
            crate::build_info::as_json(),
            start.elapsed().as_millis() as u64,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reports_the_running_build() {
        let result = EdgeVersionTool
            .execute(serde_json::json!({}))
            .await
            .unwrap();
        assert!(result.success);
        for key in ["short_sha", "build_date", "commit_sha"] {
            assert!(result.output.get(key).is_some(), "missing {key}");
        }
    }
}
