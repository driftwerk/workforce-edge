//! Passive personal-memory capture for the edge.
//!
//! After every customer turn (off the critical path) an LLM pass extracts the
//! durable things worth remembering about THIS customer — who they are, their
//! role and preferences, and the notable things they've asked about — and writes
//! them scoped to `(agent, customer)` with [`MemorySource::Passive`]. The
//! per-turn recall ([`crate::memory::recall_context`]) then surfaces them into
//! future turns, so a returning customer feels remembered even when they never
//! said "remember this".
//!
//! This mirrors the hub's `passive_memory`, reusing the shared extraction
//! primitives (`remember_with_dedup`, the JSON-salvage helpers, the passive
//! dedup bar) from `workforce_lessons`. It differs in one way that matters: a
//! memory is scoped to a single customer identity and is only ever recalled for
//! THAT customer, so — unlike cross-customer lessons — a customer can at worst
//! shape their own future context, never another's.

use std::sync::Arc;
use workforce_memory::audit::DerivedFigureDetector;

use eyre::Result;
use serde::Deserialize;

use workforce_agent::compaction::extract_text_from_response;
use workforce_agent::llm::provider::LlmProvider;
use workforce_agent::llm::types::LlmMessage;
use workforce_common::session::SessionMessage;
use workforce_core::types::AgentId;
use workforce_lessons::turn_extraction::{
    complete_objects_of_truncated_array, first_bracketed_array, looks_like_recap_request,
    remember_with_dedup, strip_code_fence, DedupConfig, WriteOutcome, PASSIVE_DEDUP_WINDOW,
};
use workforce_memory::store::{MemoryStore, DEFAULT_NEAR_DUPLICATE_SIMILARITY};
use workforce_memory::types::{Memory, MemoryLayer, MemorySource};

use crate::analytics::{UsageMeter, UsagePurpose};
use crate::tenant::TenantId;

/// Single-shot extraction budget. Multi-event turns overflow smaller caps and
/// arrive truncated (then only the object-by-object salvage survives).
const MAX_EXTRACTION_TOKENS: u32 = 2048;

/// Below this many words in the customer's message, skip extraction entirely —
/// a durable personal fact is a multi-word statement, whereas greetings and
/// one-word acks ("hi", "thanks", "ok cool") aren't worth an LLM call.
const MIN_CAPTURE_WORDS: usize = 3;

const EDGE_PASSIVE_PROMPT: &str = "\
You are this assistant's long-term memory of ONE customer. From the customer↔assistant \
exchange below, extract what's worth remembering about THIS customer for future \
conversations, as a JSON array — so next time the assistant already knows them.

Each array element is one memory with this shape:

  {
    \"content\":    \"<first-person narrative from the assistant's perspective: who the \
                     customer is or what they did/asked in THIS turn, and anything they told \
                     you about themselves, their role, or their preferences>\",
    \"facts\":      [\"<concrete fact strings — see FACTS below>\"],
    \"tags\":       [\"<short lowercase kebab-case topic tags>\"],
    \"importance\": <float 0.0-1.0>
  }

Rules:
- CAPTURE two kinds of memory:
  1. WHO THEY ARE — name, role, team, organisation, and any durable preference or \
     recurring need they state (e.g. 'prefers order lists sorted by delivery date', \
     'works in the returns team').
  2. WHAT WE DID — what the customer asked about or worked on in THIS turn, at the \
     topic level, with the entities they engaged with by name ('on 2026-07-08 the \
     customer reviewed delayed bulk orders and asked about order 4815162 and \
     account Sam Carter'). This is what lets the assistant answer 'what have we \
     talked about?' and pick up where they left off — record it even when the turn \
     reveals nothing personal.
- Write `content` about THIS turn only. Do NOT mint memories from prior context being \
  recapped — if the assistant is summarising earlier conversation, return [].
- Anchor time to the conversation date in the payload: write ABSOLUTE dates \
  ('on 2026-07-08'), never bare relative terms ('yesterday', 'this morning').
- Do NOT copy raw tool output, tables, code, or internal implementation detail — name \
  the topic and the key entities, never the data dump itself.
- Do NOT record derived, point-in-time figures — counts, totals, percentages or other \
  metrics computed from a query ('the org has 197 customers', '23 delayed orders'). \
  They go stale the moment the data changes, and a remembered figure gets repeated as \
  fact in later conversations. Record that the customer asked about the topic \
  ('asked how many customers the org has'), never the figure itself.
- importance: 0.3 for a casual preference or routine lookup topic, 0.5 for what they \
  worked on, 0.6 for identity/role, 0.8+ for a strong stated preference or a decision.
- Emit [] only for turns with nothing to carry forward (greetings, thanks, small talk).

FACTS (narrative alone is lossy under vector recall):
- For each memory, list the concrete facts a future recall needs: \
  'name:Alex', 'role:Operations Lead', 'org:Contoso', \
  'preference:orders sorted by delivery date', 'topic:delayed bulk orders', \
  'order:4815162', 'account:Sam Carter'.
- Entity IDs are facts; aggregates are not. Never mint numeric aggregate facts \
  ('count:23', 'total:197', 'on-time-rate:85%') — see the derived-figures rule above.
- Prefer many short specific facts over one long sentence. Use [] if there are none.

Respond with the JSON array only — no prose, no markdown fences.";

/// One extracted memory as produced by the LLM.
#[derive(Debug, Deserialize)]
struct ExtractedEvent {
    content: String,
    #[serde(default)]
    facts: Vec<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default = "default_importance")]
    importance: f32,
}

fn default_importance() -> f32 {
    0.5
}

/// Prefix the turn with its absolute date so the extractor resolves relative
/// phrasing ("this morning") to a calendar date at write time.
fn extraction_user_payload(
    event_time: chrono::DateTime<chrono::Utc>,
    user_message: &str,
    agent_response: &str,
) -> String {
    let when = event_time.format("%A %Y-%m-%d").to_string();
    format!(
        "## Conversation date\n\n{when} (UTC). Resolve every relative time reference to an \
         absolute calendar date anchored to THIS date.\n\n## Customer message\n\n{user_message}\n\n\
         ## Assistant response\n\n{agent_response}"
    )
}

/// Append `facts` to the narrative as a trailing block so vector recall scores
/// the entity strings directly. Unchanged when `facts` is empty.
fn compose_content_with_facts(content: &str, facts: &[String]) -> String {
    let cleaned: Vec<&str> = facts
        .iter()
        .map(|f| f.trim())
        .filter(|f| !f.is_empty())
        .collect();
    if cleaned.is_empty() {
        return content.to_string();
    }
    let mut out = content.trim_end().to_string();
    out.push_str("\n\nFacts:\n");
    for fact in cleaned {
        out.push_str("- ");
        out.push_str(fact);
        out.push('\n');
    }
    out.trim_end().to_string()
}

/// Tolerant parse of the LLM reply into events (bare/fenced/prose-wrapped array,
/// lone object, or a token-cap-truncated array salvaged object-by-object).
fn parse_events(raw: &str) -> Option<Vec<ExtractedEvent>> {
    let body = strip_code_fence(raw.trim());
    if let Ok(events) = serde_json::from_str::<Vec<ExtractedEvent>>(body) {
        return Some(events);
    }
    if let Some(slice) = first_bracketed_array(body) {
        if let Ok(events) = serde_json::from_str::<Vec<ExtractedEvent>>(slice) {
            return Some(events);
        }
    }
    if let Ok(event) = serde_json::from_str::<ExtractedEvent>(body) {
        return Some(vec![event]);
    }
    let salvaged: Vec<ExtractedEvent> = complete_objects_of_truncated_array(body)
        .into_iter()
        .filter_map(|obj| serde_json::from_str::<ExtractedEvent>(obj).ok())
        .collect();
    (!salvaged.is_empty()).then_some(salvaged)
}

/// Passive narrative embeds densely; keep the strict 0.92 bar (a merge DROPS the
/// new memory, so over-merging is irreversible episodic loss — consolidation
/// handles the missed dups later).
fn passive_dedup_config() -> DedupConfig {
    DedupConfig {
        layer: MemoryLayer::Episodic,
        source: None,
        window: PASSIVE_DEDUP_WINDOW,
        near_duplicate_similarity: DEFAULT_NEAR_DUPLICATE_SIMILARITY,
    }
}

fn build_memory(
    agent: &AgentId,
    customer: &str,
    date: &str,
    event_time: chrono::DateTime<chrono::Utc>,
    session_id: &str,
    message_ids: &[String],
    event: ExtractedEvent,
) -> Memory {
    let mut tags = event.tags;
    tags.push("source:passive".to_string());
    tags.push(format!("date:{date}"));

    let content = compose_content_with_facts(&event.content, &event.facts);
    let mut memory = Memory::new(content, MemoryLayer::Episodic, agent.clone())
        .with_source(MemorySource::Passive)
        .with_importance(event.importance)
        .with_event_time(event_time)
        .with_identity_id(customer)
        .with_tags(tags);
    if !session_id.is_empty() {
        memory = memory.with_source_session(session_id.to_string(), message_ids.to_vec());
    }
    memory
}

/// Extract and store passive memories for one completed turn, scoped to the
/// customer. Returns the number of memories written.
#[allow(clippy::too_many_arguments)]
async fn extract_and_store(
    llm: Arc<dyn LlmProvider>,
    store: Arc<dyn MemoryStore>,
    derived_figures: &DerivedFigureDetector,
    agent: AgentId,
    customer: &str,
    session_id: &str,
    message_ids: &[String],
    user_message: &str,
    agent_response: &str,
    model: Option<String>,
) -> Result<usize> {
    // A recap ("what did we discuss last time?") describes memories that already
    // exist — re-minting them as fresh events would date them wrong.
    if looks_like_recap_request(user_message) {
        tracing::debug!(agent = %agent, "edge passive extraction skipped: recap turn");
        return Ok(0);
    }
    // Cost gate: skip the extraction LLM call on trivial turns (greetings, one-
    // word acknowledgements) — a real personal fact is a multi-word statement.
    // Kept low so short facts ("my name is Kingsley") still get through.
    if user_message.split_whitespace().count() < MIN_CAPTURE_WORDS {
        return Ok(0);
    }

    let event_time = chrono::Utc::now();
    let payload = extraction_user_payload(event_time, user_message, agent_response);

    // Single-shot JSON extraction: suppress extended reasoning (it otherwise
    // burns the whole budget on hidden tokens and returns nothing).
    let response = workforce_agent::llm::provider::with_reasoning_suppressed(llm.create_message(
        Some(EDGE_PASSIVE_PROMPT),
        &[LlmMessage::user(payload)],
        &[],
        model.as_deref(),
        Some(MAX_EXTRACTION_TOKENS),
        true,
    ))
    .await
    .map_err(|e| eyre::eyre!("passive memory extraction LLM call failed: {e}"))?;

    let raw = extract_text_from_response(&response.content);
    let trimmed = raw.trim();
    // Every zero path is logged: a silent drop here is indistinguishable from
    // "capture doesn't run", which has already cost a production goose chase.
    if trimmed.is_empty() {
        tracing::warn!(
            agent = %agent,
            output_tokens = response.usage.output_tokens,
            "edge passive extraction returned EMPTY text (reasoning suppression failed?)"
        );
        return Ok(0);
    }
    if trimmed.eq_ignore_ascii_case("NONE") || trimmed == "[]" {
        tracing::info!(agent = %agent, "edge passive extraction: nothing to remember");
        return Ok(0);
    }
    let Some(events) = parse_events(trimmed) else {
        tracing::warn!(
            agent = %agent,
            raw_len = trimmed.len(),
            "edge passive extraction output did not parse as JSON; dropped"
        );
        return Ok(0);
    };
    if events.is_empty() {
        tracing::info!(agent = %agent, "edge passive extraction: empty event list");
        return Ok(0);
    }

    let date = event_time.format("%Y-%m-%d").to_string();
    let parsed = events.len();
    let mut stored = 0usize;
    for event in events {
        let memory = build_memory(
            &agent,
            customer,
            &date,
            event_time,
            session_id,
            message_ids,
            event,
        );
        // Enforcement backstop behind the prompt rule: a derived figure that
        // slips through extraction must not become a durable "fact" that gets
        // re-served long after it stopped being true (ENG-105). Same detector
        // as the audit sweep, so filter and sweep can't drift apart.
        if derived_figures.matches(&memory.content) {
            tracing::info!(
                agent = %agent,
                "edge passive extraction dropped a derived-figure memory"
            );
            continue;
        }
        match remember_with_dedup(store.as_ref(), memory, passive_dedup_config()).await {
            Ok(WriteOutcome::Inserted) => stored += 1,
            Ok(WriteOutcome::MergedInto { .. }) => {}
            Err(e) => tracing::warn!(error = %e, "edge passive memory write failed"),
        }
    }
    tracing::info!(agent = %agent, parsed, stored, "edge passive extraction done");
    Ok(stored)
}

/// Fire-and-forget passive capture for a completed turn — never on the critical
/// path (the reply is already on its way). The extraction call is metered to
/// the turn as [`UsagePurpose::PassiveMemory`]; the consolidation pass it may
/// set off meters itself. The handle is returned so a test can await the
/// task; callers drop it.
#[allow(clippy::too_many_arguments)]
pub fn spawn_capture(
    llm: Arc<dyn LlmProvider>,
    meter: UsageMeter,
    store: Arc<dyn MemoryStore>,
    derived_figures: Arc<DerivedFigureDetector>,
    tenant: TenantId,
    agent: AgentId,
    customer: String,
    session_id: String,
    turn_messages: Vec<SessionMessage>,
    user_message: String,
    agent_response: String,
    model: Option<String>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let message_ids: Vec<String> = turn_messages.iter().map(|m| m.id.clone()).collect();
        let mem: Arc<dyn MemoryStore> = store.clone();
        let wrote = match extract_and_store(
            meter.llm(llm.clone(), UsagePurpose::PassiveMemory),
            mem,
            &derived_figures,
            agent.clone(),
            &customer,
            &session_id,
            &message_ids,
            &user_message,
            &agent_response,
            model.clone(),
        )
        .await
        {
            Ok(0) => 0,
            Ok(n) => {
                tracing::info!(agent = %agent, memories = n, "edge passive memories captured");
                n
            }
            Err(e) => {
                tracing::warn!(agent = %agent, error = %e, "edge passive capture failed");
                0
            }
        };
        // Having just added to this customer's memory, occasionally tidy it —
        // rate-limited and gated on corpus size inside, so this is a cheap no-op
        // on most turns.
        if wrote > 0 {
            crate::consolidation::spawn_maybe_consolidate(
                llm,
                &meter,
                store,
                derived_figures,
                tenant,
                agent,
                customer,
                model,
            );
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analytics::test_support::{attribution, usage_rows, with_store, FixedProvider};
    use workforce_memory::in_memory::InMemoryStore;

    /// The capture's extraction call is metered to the turn under
    /// `passive_memory`, whether or not it found anything to remember.
    #[tokio::test(flavor = "multi_thread")]
    async fn capture_is_metered_as_passive_memory() {
        with_store(|store| async move {
            let store = Arc::new(store);
            spawn_capture(
                FixedProvider::answering("[]", 70, 7),
                UsageMeter::new(Some(Arc::clone(&store)), attribution()),
                Arc::new(InMemoryStore::new()),
                Arc::new(DerivedFigureDetector::default()),
                TenantId::default_tenant(),
                AgentId::new("help"),
                "ext:iss\u{1f}sub-1".into(),
                "s1".into(),
                vec![],
                "My name is Ada and I run the night shift.".into(),
                "Nice to meet you, Ada.".into(),
                None,
            )
            .await
            .unwrap();
            assert_eq!(
                usage_rows(&store).await,
                [(
                    "passive_memory".to_string(),
                    "ok".to_string(),
                    Some("m1".to_string()),
                    70,
                    7
                )]
            );
        })
        .await;
    }

    #[test]
    fn parse_events_accepts_array_fenced_and_salvages_truncation() {
        let plain = r#"[{"content":"a","facts":["name:X"],"tags":["t"],"importance":0.6}]"#;
        assert_eq!(parse_events(plain).unwrap().len(), 1);

        let fenced = "```json\n[{\"content\":\"a\"}]\n```";
        assert_eq!(parse_events(fenced).unwrap().len(), 1);

        // Token-cap truncation (no closing ]) — salvage the one complete object.
        let truncated = r#"[{"content":"one"},{"content":"tw"#;
        assert_eq!(parse_events(truncated).unwrap().len(), 1);

        assert!(parse_events("not json at all").is_none());
    }

    #[test]
    fn compose_content_appends_facts_and_is_noop_when_empty() {
        let out = compose_content_with_facts("Narrative.", &["name:Kingsley".into(), "  ".into()]);
        assert!(out.contains("Facts:"));
        assert!(out.contains("- name:Kingsley"));
        assert_eq!(compose_content_with_facts("Just this.", &[]), "Just this.");
    }

    #[test]
    fn build_memory_scopes_to_customer_and_marks_passive() {
        let agent = AgentId::new("harbor-assistant");
        let event = ExtractedEvent {
            content: "The customer is a Principal Developer.".into(),
            facts: vec!["role:Principal Developer".into()],
            tags: vec!["identity".into()],
            importance: 0.6,
        };
        let m = build_memory(
            &agent,
            "ext:issuer\u{1f}sub-123",
            "2026-07-08",
            chrono::Utc::now(),
            "sess-1",
            &["msg-1".into()],
            event,
        );
        assert_eq!(m.identity_id.as_deref(), Some("ext:issuer\u{1f}sub-123"));
        assert_eq!(m.source, MemorySource::Passive);
        assert!(m.content.contains("Facts:"));
        assert!(m.tags.iter().any(|t| t == "source:passive"));
        assert!(m.tags.iter().any(|t| t == "date:2026-07-08"));
    }
}
