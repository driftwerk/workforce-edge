//! Agent memory, backed by the edge's Weaviate.
//!
//! Every turn recalls what the agent knows about the current customer and
//! prepends it to the prompt; the agent can also store/recall via a `memory`
//! tool. All of it is scoped by `owner = <agent id>` **and**
//! `identity_id = <customer>` at `Visibility::Private`, so one customer's
//! memory is never recalled for another and never surfaces in a global read.

use std::sync::Arc;
use std::time::{Duration, Instant};
use workforce_memory::audit::DerivedFigureDetector;

use async_trait::async_trait;
use eyre::Result;
use serde_json::Value;
use workforce_agent::tools::{Tool, ToolResult};
use workforce_core::types::AgentId;
use workforce_memory::store::MemoryStore;
use workforce_memory::types::{Memory, MemoryFilter, MemoryLayer, MemorySource, Visibility};

const RECALL_LIMIT: usize = 8;
/// Minimum `nearText` certainty for a memory to be recalled. text2vec-transformers
/// scores even a strong topical match (e.g. "what is my name?" vs "the user's name
/// is Kingsley") at only ~0.55-0.60, so a high bar silently recalls NOTHING. Recall
/// is already tightly scoped to one agent + one customer, so a permissive floor
/// surfaces that customer's own facts without any risk of cross-customer bleed.
const RECALL_MIN_CERTAINTY: f32 = 0.45;
/// Cap on a recall so a slow Weaviate never stalls a customer turn — memory is
/// best-effort context, not on the critical path.
const RECALL_TIMEOUT: Duration = Duration::from_secs(5);

/// Filter that scopes memory to one agent + one customer. `identity_id` is
/// REQUIRED by the store's scope gate — without it `recall` returns empty.
fn scoped_filter(agent: &AgentId, customer: &str) -> MemoryFilter {
    MemoryFilter {
        owner: Some(agent.clone()),
        identity_id: Some(customer.to_string()),
        limit: Some(RECALL_LIMIT),
        min_certainty: Some(RECALL_MIN_CERTAINTY),
        ..Default::default()
    }
}

async fn recall(
    store: &dyn MemoryStore,
    agent: &AgentId,
    customer: &str,
    query: &str,
) -> Vec<Memory> {
    let filter = scoped_filter(agent, customer);
    match tokio::time::timeout(RECALL_TIMEOUT, store.recall(query, filter)).await {
        Ok(Ok(hits)) => hits.into_iter().map(|h| h.memory).collect(),
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "edge memory recall failed");
            Vec::new()
        }
        Err(_) => {
            tracing::warn!("edge memory recall timed out");
            Vec::new()
        }
    }
}

/// Recall relevant memories for this customer and render them as a context
/// block to prepend to the system prompt. Empty when there's nothing (or on
/// error/timeout — never fails a turn).
///
/// Each memory carries its age, and the header tells the model these are
/// recollections rather than current data — an undated assertion in the
/// system prompt otherwise reads as present-tense fact and gets repeated as
/// one (ENG-105).
pub async fn recall_context(
    store: &dyn MemoryStore,
    agent: &AgentId,
    customer: &str,
    query: &str,
) -> String {
    let hits = recall(store, agent, customer, query).await;
    if hits.is_empty() {
        return String::new();
    }
    let mut block = String::from(
        "\n\n## Remembered about this user\n\
         Recollections from previous conversations, with their age. They are \
         context, not current data: re-derive any figure, count or status from \
         the tools before repeating it as fact.\n",
    );
    for memory in hits {
        block.push_str("- ");
        block.push_str(memory.content.trim());
        block.push_str(&format!(" (remembered {})", memory.age_label()));
        block.push('\n');
    }
    block
}

/// A customer-scoped memory tool the agent can call to remember a durable fact
/// or recall what it knows. Writes land in the edge's own Weaviate, scoped so
/// they can never reach another customer's memory.
pub struct EdgeMemoryTool {
    store: Arc<dyn MemoryStore>,
    agent: AgentId,
    customer: String,
    derived_figures: Arc<DerivedFigureDetector>,
}

impl EdgeMemoryTool {
    pub fn new(
        store: Arc<dyn MemoryStore>,
        agent: AgentId,
        customer: String,
        derived_figures: Arc<DerivedFigureDetector>,
    ) -> Self {
        Self {
            store,
            agent,
            customer,
            derived_figures,
        }
    }
}

#[async_trait]
impl Tool for EdgeMemoryTool {
    fn name(&self) -> &str {
        "memory"
    }

    fn description(&self) -> &str {
        "Remember a durable fact about the current user, or recall what you already know about \
         them. action=remember with `content` to store a fact; action=recall with `query` to \
         search. Memory is private to this user. Durable facts are who they are, their role and \
         preferences, and the entities they work with — never derived figures (counts, totals, \
         metrics from a query): those go stale, so re-derive them from the tools each time."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["remember", "recall"]},
                "content": {"type": "string", "description": "The fact to remember (action=remember)."},
                "query": {"type": "string", "description": "What to search for (action=recall)."}
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let started = Instant::now();
        let elapsed = || started.elapsed().as_millis() as u64;
        match input.get("action").and_then(Value::as_str) {
            Some("remember") => {
                let content = input
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim();
                if content.is_empty() {
                    return Ok(ToolResult::failure(
                        "remember requires non-empty content",
                        elapsed(),
                    ));
                }
                // The tool path is where the assistant itself stores "the org
                // has 197 workers" as a fact and then repeats it for weeks
                // (ENG-105). Refuse with a teaching error so the model learns
                // what belongs in memory rather than silently dropping.
                if self.derived_figures.matches(content) {
                    return Ok(ToolResult::failure(
                        "not stored: that looks like a derived figure (a count, total or metric \
                         from a query). Such numbers go stale — re-derive them from the tools \
                         when needed. Store durable facts instead: who the user is, their role, \
                         preferences, and the entities they work with.",
                        elapsed(),
                    ));
                }
                let memory = Memory::new(content, MemoryLayer::Episodic, self.agent.clone())
                    .with_identity_id(self.customer.clone())
                    .with_source(MemorySource::Agent)
                    .with_visibility(Visibility::Private);
                match self.store.remember(memory).await {
                    Ok(_) => Ok(ToolResult::success(
                        serde_json::json!({"remembered": true}),
                        elapsed(),
                    )),
                    Err(e) => {
                        tracing::warn!(error = %e, "edge memory write failed");
                        Ok(ToolResult::failure("could not save the memory", elapsed()))
                    }
                }
            }
            Some("recall") => {
                let query = input.get("query").and_then(Value::as_str).unwrap_or("");
                let memories: Vec<Value> =
                    recall(self.store.as_ref(), &self.agent, &self.customer, query)
                        .await
                        .into_iter()
                        .map(|m| {
                            serde_json::json!({
                                "content": m.content,
                                "remembered": m.age_label(),
                            })
                        })
                        .collect();
                Ok(ToolResult::success(
                    serde_json::json!({ "memories": memories }),
                    elapsed(),
                ))
            }
            _ => Ok(ToolResult::failure(
                "action must be 'remember' or 'recall'",
                elapsed(),
            )),
        }
    }
}

/// Erase every memory an agent holds about its customers, whichever path wrote
/// it (the memory tool, passive capture, consolidation) and forgotten ones
/// included, so a later agent created under the same id inherits none of it.
/// Only identities shaped as a customer's owner key are reached: lessons and
/// training data sit under their own shared identities and are purged by their
/// own modules. Returns the count removed.
pub async fn forget_all_customer_memory(
    store: &dyn MemoryStore,
    agent: &AgentId,
) -> eyre::Result<usize> {
    let mut removed = 0;
    for customer in store
        .identities(agent)
        .await?
        .iter()
        .filter(|identity| crate::ingress::is_customer_owner(identity))
    {
        removed += store.purge_identity(agent, customer).await?;
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn remember_refuses_derived_figures_before_touching_the_store() {
        // The guard must fire before any store call — the test store points
        // nowhere, so reaching it would error rather than fail cleanly.
        let store: Arc<dyn MemoryStore> = Arc::new(workforce_memory::weaviate::WeaviateStore::new(
            "http://127.0.0.1:1",
            None,
        ));
        let tool = EdgeMemoryTool::new(
            store,
            AgentId::new("help"),
            "ext:realm:sub".into(),
            Arc::new(DerivedFigureDetector::default()),
        );
        let out = tool
            .execute(serde_json::json!({
                "action": "remember",
                "content": "The organisation has 197 workers in total."
            }))
            .await
            .unwrap();
        assert!(!out.success);
        let err = out.error.unwrap_or_default();
        assert!(
            err.contains("derived figure"),
            "teaching error missing: {err}"
        );
    }

    #[tokio::test]
    async fn remember_refuses_derived_figures_in_the_agents_own_words() {
        let store: Arc<dyn MemoryStore> = Arc::new(workforce_memory::weaviate::WeaviateStore::new(
            "http://127.0.0.1:1",
            None,
        ));
        let vocabulary = workforce_common::settings::DerivedFigureVocabulary {
            nouns: vec!["parcel".into()],
            qualifiers: vec!["unshipped".into()],
            fact_keys: vec![],
        };
        let tool = EdgeMemoryTool::new(
            store,
            AgentId::new("help"),
            "ext:realm:sub".into(),
            Arc::new(DerivedFigureDetector::new(&vocabulary)),
        );
        let out = tool
            .execute(serde_json::json!({
                "action": "remember",
                "content": "There were 23 unshipped parcels on Monday."
            }))
            .await
            .unwrap();
        assert!(!out.success);
        assert!(out.error.unwrap_or_default().contains("derived figure"));
    }

    #[tokio::test]
    async fn removing_an_agent_erases_every_customers_memory_and_nothing_shared() {
        use workforce_memory::in_memory::InMemoryStore;
        let store = InMemoryStore::new();
        let agent = AgentId::new("help");
        let other = AgentId::new("other");
        let write = |owner: &AgentId, identity: &str, source: MemorySource| {
            Memory::new("a fact", MemoryLayer::Episodic, owner.clone())
                .with_identity_id(identity.to_string())
                .with_source(source)
        };
        for memory in [
            write(&agent, "ext:iss\u{1f}sub-1", MemorySource::Agent),
            write(&agent, "ext:iss\u{1f}sub-1", MemorySource::Passive),
            write(&agent, "ext:iss\u{1f}sub-2", MemorySource::Passive),
        ] {
            store.remember(memory).await.unwrap();
        }
        let forgotten = store
            .remember(write(&agent, "ext:iss\u{1f}sub-2", MemorySource::Agent))
            .await
            .unwrap();
        store.forget(&forgotten).await.unwrap();
        let lesson = store
            .remember(write(
                &agent,
                crate::lessons::LESSON_IDENTITY,
                MemorySource::Lesson,
            ))
            .await
            .unwrap();
        let chunk = store
            .remember(write(
                &agent,
                crate::knowledge::KNOWLEDGE_IDENTITY,
                MemorySource::Curated,
            ))
            .await
            .unwrap();
        let elsewhere = store
            .remember(write(&other, "ext:iss\u{1f}sub-1", MemorySource::Agent))
            .await
            .unwrap();
        let not_customers = ["6f1c0d2e-hub-user", "ext:cust-9", "hub:ext:iss\u{1f}sub-1"]
            .map(|identity| write(&agent, identity, MemorySource::Passive));
        let mut kept = vec![lesson, chunk, elsewhere];
        for memory in not_customers {
            kept.push(store.remember(memory).await.unwrap());
        }

        assert_eq!(forget_all_customer_memory(&store, &agent).await.unwrap(), 4);
        let mut left = store.identities(&agent).await.unwrap();
        left.sort();
        assert_eq!(
            left,
            [
                "6f1c0d2e-hub-user",
                crate::knowledge::KNOWLEDGE_IDENTITY,
                crate::lessons::LESSON_IDENTITY,
                "ext:cust-9",
                "hub:ext:iss\u{1f}sub-1",
            ]
        );
        for kept in kept {
            assert!(store.get(&kept).await.unwrap().is_some());
        }
    }

    #[test]
    fn scoped_filter_requires_identity_for_the_recall_gate() {
        let f = scoped_filter(&AgentId::new("help"), "ext:realm:sub");
        assert_eq!(f.owner.as_ref().unwrap().0, "help");
        assert_eq!(f.identity_id.as_deref(), Some("ext:realm:sub"));
        assert!(
            !f.system_unscoped,
            "edge memory is per-customer, never unscoped"
        );
        assert_eq!(f.limit, Some(RECALL_LIMIT));
    }
}
