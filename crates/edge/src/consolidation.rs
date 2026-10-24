//! Per-customer memory consolidation for the edge (P3 of memory continuity).
//!
//! Memory (both passively-captured AND the agent's own tool-written "remember"
//! facts) accretes near-duplicates over time — a customer ends up with "Alex's
//! role is Operations Lead" alongside "Alex is an Operations Lead" —
//! plus stale one-offs. This runs a cheap, rate-limited LLM pass over ONE
//! customer's Episodic memories that merges duplicates into a single canonical
//! fact and drops the redundant ones, keeping recall clean and the corpus bounded.
//!
//! The canonical merge is written at the `Semantic` layer; recall's filter is
//! layer-agnostic, so it's still surfaced — the merge is not silent loss.
//!
//! Safety: `forget` is a soft delete (rows stay, flagged), so a bad drop is
//! recoverable; the plan can only ever touch ids that were in the customer's own
//! listed set (no hallucinated/foreign ids); and a plan that would drop more than
//! half the corpus is rejected wholesale. Scope is (agent, customer) throughout —
//! it can never read or write another customer's memories.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};
use workforce_memory::audit::DerivedFigureDetector;

use serde::Deserialize;

use std::sync::Arc;
use workforce_agent::compaction::extract_text_from_response;
use workforce_agent::llm::provider::LlmProvider;
use workforce_agent::llm::types::LlmMessage;
use workforce_core::types::AgentId;
use workforce_memory::store::MemoryStore;
use workforce_memory::types::{
    Memory, MemoryFilter, MemoryId, MemoryLayer, MemorySource, Visibility,
};

use crate::analytics::{UsageMeter, UsagePurpose};
use crate::tenant::TenantId;

/// Consolidate only once a customer's memory count exceeds this — below it,
/// duplication isn't worth an LLM call.
const CONSOLIDATE_THRESHOLD: usize = 40;
/// Never consolidate the same customer more than once per this window (bounds
/// the extra LLM cost; a process-local limiter, reset on restart, which is fine).
const CONSOLIDATE_MIN_INTERVAL: Duration = Duration::from_secs(20 * 60);
/// Cap on how many memories we pull for one pass.
const LIST_MAX: usize = 200;
/// Reject a plan that would drop more than this fraction of the corpus — a
/// runaway "forget everything" plan is a bug, not a consolidation.
const MAX_FORGET_FRACTION: f32 = 0.5;
const MAX_PLAN_TOKENS: u32 = 2048;
/// Bound on the rate-limit map so it tracks ~active customers, not every one ever.
const RATE_MAP_MAX: usize = 4096;

/// One customer's memory as the gate sees it: the same customer identity
/// recurs across tenants and agents, and each corpus is its own.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct CorpusKey {
    tenant: TenantId,
    agent: AgentId,
    customer: String,
}

/// Last consolidation time per corpus — the rate-limit gate.
static LAST_RUN: LazyLock<Mutex<HashMap<CorpusKey, Instant>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// True if `corpus` hasn't been consolidated within the interval; records
/// "now" when it returns true so concurrent turns don't both proceed.
fn rate_gate_open(corpus: CorpusKey) -> bool {
    let mut map = LAST_RUN.lock().expect("consolidation rate map");
    let now = Instant::now();
    if let Some(prev) = map.get(&corpus) {
        if now.duration_since(*prev) < CONSOLIDATE_MIN_INTERVAL {
            return false;
        }
    }
    // Bound growth: once large, drop entries past the interval — they no longer
    // gate anything, so the map stays ~proportional to active customers.
    if map.len() > RATE_MAP_MAX {
        map.retain(|_, t| now.duration_since(*t) < CONSOLIDATE_MIN_INTERVAL);
    }
    map.insert(corpus, now);
    true
}

const CONSOLIDATE_PROMPT: &str = "\
You are tidying the long-term memory a customer-facing assistant keeps about ONE \
customer. Below is the current set of memories, each as `<id>: <content>`. Produce \
a JSON plan that consolidates them:

  {
    \"merge\":  [ { \"content\": \"<one clear canonical memory that replaces two or more \
                    duplicates/near-duplicates of the SAME fact>\", \"replaces\": [\"<id>\", ...] } ],
    \"forget\": [ \"<id of a memory that is fully redundant (covered by another) or clearly \
                   obsolete>\" ]
  }

Rules:
- Merge ONLY genuine duplicates or paraphrases of the SAME fact. Keep distinct facts separate.
- Every `replaces`/`forget` id MUST be one of the ids listed below. Do not invent ids.
- NEVER drop a unique durable fact — especially the customer's identity (name, role, team, \
  organisation) or a stated preference. If duplicated, merge them into one; never just forget them.
- `forget` is only for a memory that is entirely covered by a merge or by another kept memory.
- Be conservative: when unsure, leave a memory alone. If nothing needs tidying, return \
  {\"merge\":[],\"forget\":[]}.

Respond with the JSON object only — no prose, no markdown fences.";

#[derive(Debug, Deserialize)]
struct Merge {
    content: String,
    #[serde(default)]
    replaces: Vec<String>,
}

#[derive(Debug, Deserialize, Default)]
struct Plan {
    #[serde(default)]
    merge: Vec<Merge>,
    #[serde(default)]
    forget: Vec<String>,
}

fn scoped_filter(agent: &AgentId, customer: &str) -> MemoryFilter {
    MemoryFilter {
        owner: Some(agent.clone()),
        identity_id: Some(customer.to_string()),
        layer: Some(MemoryLayer::Episodic),
        limit: Some(LIST_MAX),
        ..Default::default()
    }
}

fn parse_plan(raw: &str) -> Option<Plan> {
    let body = workforce_lessons::turn_extraction::strip_code_fence(raw.trim());
    if let Ok(plan) = serde_json::from_str::<Plan>(body) {
        return Some(plan);
    }
    // Salvage the first {...} object if the model wrapped it in prose.
    let start = body.find('{')?;
    let end = body.rfind('}')?;
    serde_json::from_str::<Plan>(body.get(start..=end)?).ok()
}

/// Spawn a consolidation pass for one customer, off the critical path and behind
/// the rate gate. `None` when the gate is closed (recently run); otherwise the
/// task's handle, so a test can await it — callers drop it. The planning call
/// is metered to the turn as [`UsagePurpose::Consolidation`].
#[allow(clippy::too_many_arguments)]
pub fn spawn_maybe_consolidate(
    llm: Arc<dyn LlmProvider>,
    meter: &UsageMeter,
    store: Arc<dyn MemoryStore>,
    derived_figures: Arc<DerivedFigureDetector>,
    tenant: TenantId,
    agent: AgentId,
    customer: String,
    model: Option<String>,
) -> Option<tokio::task::JoinHandle<()>> {
    if !rate_gate_open(CorpusKey {
        tenant,
        agent: agent.clone(),
        customer: customer.clone(),
    }) {
        return None;
    }
    let llm = meter.llm(llm, UsagePurpose::Consolidation);
    Some(tokio::spawn(async move {
        let store: Arc<dyn MemoryStore> = store;
        if let Err(e) = consolidate(llm, store, &derived_figures, &agent, &customer, model).await {
            tracing::warn!(agent = %agent, error = %e, "edge memory consolidation failed");
        }
    }))
}

async fn consolidate(
    llm: Arc<dyn LlmProvider>,
    store: Arc<dyn MemoryStore>,
    derived_figures: &DerivedFigureDetector,
    agent: &AgentId,
    customer: &str,
    model: Option<String>,
) -> eyre::Result<()> {
    let memories = store.list(scoped_filter(agent, customer)).await?;
    if memories.len() <= CONSOLIDATE_THRESHOLD {
        return Ok(());
    }

    // id → memory, so the plan can only reference this customer's own memories.
    let by_id: HashMap<String, &Memory> = memories.iter().map(|m| (m.id.to_string(), m)).collect();
    let listing = memories
        .iter()
        .map(|m| format!("{}: {}", m.id, m.content.replace('\n', " ")))
        .collect::<Vec<_>>()
        .join("\n");

    let response = workforce_agent::llm::provider::with_reasoning_suppressed(llm.create_message(
        Some(CONSOLIDATE_PROMPT),
        &[LlmMessage::user(listing)],
        &[],
        model.as_deref(),
        Some(MAX_PLAN_TOKENS),
        true,
    ))
    .await
    .map_err(|e| eyre::eyre!("consolidation LLM call failed: {e}"))?;

    let Some(plan) = parse_plan(extract_text_from_response(&response.content).trim()) else {
        return Ok(());
    };

    // Resolve a plan id to THIS customer's own memory id (foreign/hallucinated
    // ids aren't in `by_id`, so they're silently ignored), deduped into `out`.
    let resolve =
        |id: &str, seen: &mut std::collections::HashSet<MemoryId>, out: &mut Vec<MemoryId>| {
            if let Some(m) = by_id.get(id) {
                if seen.insert(m.id.clone()) {
                    out.push(m.id.clone());
                }
            }
        };
    // A merge is real only with ≥2 sources and non-empty content.
    let is_real_merge = |m: &Merge| !m.content.trim().is_empty() && m.replaces.len() >= 2;

    // Safety valve: reject wholesale if the plan would drop more than half the
    // corpus — checked on the UPPER BOUND (all real merges' sources + explicit
    // forgets) BEFORE writing or deleting anything.
    let mut cand_seen = std::collections::HashSet::new();
    let mut candidate = Vec::new();
    for m in plan.merge.iter().filter(|m| is_real_merge(m)) {
        for id in &m.replaces {
            resolve(id, &mut cand_seen, &mut candidate);
        }
    }
    for id in &plan.forget {
        resolve(id, &mut cand_seen, &mut candidate);
    }
    if candidate.len() as f32 > memories.len() as f32 * MAX_FORGET_FRACTION {
        tracing::warn!(
            agent = %agent,
            forget = candidate.len(),
            total = memories.len(),
            "consolidation plan drops too much — skipping"
        );
        return Ok(());
    }

    // Apply. Write each merge's canonical FIRST; only forget its sources once the
    // replacement is safely stored — a failed write must never orphan the facts.
    let mut forget_seen = std::collections::HashSet::new();
    let mut to_forget = Vec::new();
    let mut merged = 0usize;
    for m in plan.merge.iter().filter(|m| is_real_merge(m)) {
        // A merge must not launder a derived figure into a fresh Semantic
        // "fact" (ENG-105): skip the merge and keep the sources — if they
        // carry the figure themselves, the audit sweep flags them where they
        // are, dated as they were.
        if derived_figures.matches(m.content.trim()) {
            tracing::warn!(
                agent = %agent,
                "consolidation merge skipped: content stores a derived figure"
            );
            continue;
        }
        let importance = m
            .replaces
            .iter()
            .filter_map(|id| by_id.get(id.as_str()))
            .map(|x| x.importance)
            .fold(0.5, f32::max);
        // Carry the newest source's event_time forward. `Memory::new` stamps
        // event_time = now, which would re-date an old fact as brand new —
        // recency-weighted recall then ranks the stalest merged content
        // HIGHER after every consolidation pass.
        let event_time = m
            .replaces
            .iter()
            .filter_map(|id| by_id.get(id.as_str()))
            .map(|x| x.event_time)
            .max();
        let mut canonical = Memory::new(
            m.content.trim().to_string(),
            MemoryLayer::Semantic,
            agent.clone(),
        )
        .with_source(MemorySource::Agent)
        .with_visibility(Visibility::Private)
        .with_importance(importance)
        .with_identity_id(customer)
        .with_tags(vec!["source:consolidated".to_string()]);
        if let Some(t) = event_time {
            canonical = canonical.with_event_time(t);
        }
        match store.remember(canonical).await {
            Ok(_) => {
                merged += 1;
                for id in &m.replaces {
                    resolve(id, &mut forget_seen, &mut to_forget);
                }
            }
            Err(e) => tracing::warn!(error = %e, "consolidation merge write failed — sources kept"),
        }
    }
    // Explicit forgets: memories the plan says are redundant vs a KEPT memory, so
    // there's nothing to write first.
    for id in &plan.forget {
        resolve(id, &mut forget_seen, &mut to_forget);
    }

    let mut forgotten = 0usize;
    for id in &to_forget {
        if store.forget(id).await.is_ok() {
            forgotten += 1;
        }
    }
    if merged > 0 || forgotten > 0 {
        tracing::info!(agent = %agent, merged, forgotten, "edge memory consolidated");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analytics::test_support::{attribution, usage_rows, with_store, FixedProvider};
    use crate::analytics::UsageMeter;
    use workforce_memory::in_memory::InMemoryStore;

    /// The planning call of a pass that runs (corpus over the threshold, gate
    /// open) is metered to the turn under `consolidation`.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_consolidation_pass_is_metered_as_consolidation() {
        with_store(|store| async move {
            let store = Arc::new(store);
            let agent = AgentId::new("help");
            // A customer of this test's own, so the process-wide rate gate is
            // open however the other tests ran.
            let customer = "ext:iss\u{1f}consolidation-metering";
            let memory = Arc::new(InMemoryStore::new());
            for n in 0..=CONSOLIDATE_THRESHOLD {
                memory
                    .remember(
                        Memory::new(format!("fact {n}"), MemoryLayer::Episodic, agent.clone())
                            .with_identity_id(customer),
                    )
                    .await
                    .unwrap();
            }
            let handle = spawn_maybe_consolidate(
                FixedProvider::answering(r#"{"merge": [], "forget": []}"#, 500, 20),
                &UsageMeter::new(Some(Arc::clone(&store)), attribution()),
                memory,
                Arc::new(DerivedFigureDetector::default()),
                TenantId::default_tenant(),
                agent,
                customer.into(),
                None,
            )
            .expect("the gate is open for a fresh corpus");
            handle.await.unwrap();
            assert_eq!(
                usage_rows(&store).await,
                [(
                    "consolidation".to_string(),
                    "ok".to_string(),
                    Some("m1".to_string()),
                    500,
                    20
                )]
            );
        })
        .await;
    }

    #[test]
    fn parse_plan_reads_object_bare_and_fenced_and_prose() {
        let bare = r#"{"merge":[{"content":"c","replaces":["a","b"]}],"forget":["x"]}"#;
        let p = parse_plan(bare).unwrap();
        assert_eq!(p.merge.len(), 1);
        assert_eq!(p.forget, vec!["x"]);

        let fenced = "```json\n{\"merge\":[],\"forget\":[]}\n```";
        assert!(parse_plan(fenced).unwrap().merge.is_empty());

        let prose = "Here is the plan: {\"merge\":[],\"forget\":[\"z\"]} done.";
        assert_eq!(parse_plan(prose).unwrap().forget, vec!["z"]);

        assert!(parse_plan("no json").is_none());
    }

    #[test]
    fn rate_gate_blocks_within_interval_per_corpus() {
        let who = |tenant: &str, agent: &str| CorpusKey {
            tenant: TenantId::parse(tenant).unwrap(),
            agent: AgentId::new(agent),
            customer: "ext:test\u{1f}rate-gate-unique".to_string(),
        };
        assert!(rate_gate_open(who("acme", "support")), "first call opens");
        assert!(
            !rate_gate_open(who("acme", "support")),
            "second call within interval is blocked"
        );
        assert!(
            rate_gate_open(who("contoso", "support")),
            "the same customer under another tenant is another corpus"
        );
        assert!(
            rate_gate_open(who("acme", "sales")),
            "and under another agent"
        );
    }
}
