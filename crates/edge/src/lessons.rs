//! Agent self-improvement: lessons recalled into every turn and extracted from
//! completed turns (`workforce-lessons` pipeline over the edge's own Weaviate).
//!
//! Scope is the crux. Per-customer memory (see [`crate::memory`]) is keyed by
//! the customer's identity so it can never cross customers. Lessons are the
//! opposite — operational knowledge the agent should apply for *everyone* — so
//! all lessons live under one fixed sentinel identity, [`LESSON_IDENTITY`]:
//! writes and recalls both use it, sharing lessons across customers while the
//! store's identity gate keeps them out of every per-customer read. Because
//! lesson content crosses customer boundaries, the extraction prompt hard-bans
//! personal data — a lesson describes the situation, never the person.

use std::sync::Arc;
use std::time::Duration;

use workforce_agent::llm::provider::LlmProvider;
use workforce_agent::runner::CURRENT_USER_ID;
use workforce_common::session::SessionMessage;
use workforce_core::types::AgentId;
use workforce_lessons::lessons::{
    extract_and_store, filter_for_prompt, format_for_prompt, ExtractionPolicy, InjectFilter,
    InjectedLesson, LessonSelection, OVERPULL_FACTOR, RELATIVE_SCORE_BAND,
};
use workforce_memory::store::MemoryStore;
use workforce_memory::types::{Memory, MemoryFilter, MemoryId, MemorySource};

use crate::analytics::{UsageMeter, UsagePurpose};

pub(crate) use workforce_memory::edge_owner::LESSON_IDENTITY;

/// Cap on the admin lesson list.
const LESSON_LIST_MAX: usize = 500;

/// The agent's shared-lesson filter (scope for both list and delete).
fn lesson_filter(agent: &AgentId, limit: usize) -> MemoryFilter {
    MemoryFilter {
        source: Some(MemorySource::Lesson),
        owner: Some(agent.clone()),
        identity_id: Some(LESSON_IDENTITY.to_string()),
        limit: Some(limit),
        ..Default::default()
    }
}

/// Every lesson the agent has learned (newest first), for the admin viewer.
/// Propagates a store error so the handler can 500 rather than show a store
/// outage as "no lessons".
pub async fn list_lessons(store: &dyn MemoryStore, agent: &AgentId) -> eyre::Result<Vec<Memory>> {
    let mut ms = store.list(lesson_filter(agent, LESSON_LIST_MAX)).await?;
    ms.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Ok(ms)
}

/// Delete a lesson by id — but only if it is one of THIS agent's shared lessons,
/// so an operator can never delete a customer's private memory by guessing an
/// id. Returns whether a lesson was removed.
pub async fn forget_lesson(
    store: &dyn MemoryStore,
    agent: &AgentId,
    id: &str,
) -> eyre::Result<bool> {
    let scoped = store.list(lesson_filter(agent, LESSON_LIST_MAX)).await?;
    if !scoped.iter().any(|m| m.id.0 == id) {
        return Ok(false);
    }
    store.forget(&MemoryId(id.to_string())).await?;
    Ok(true)
}

/// Delete EVERY lesson for an agent. Used when the agent is removed, so a later
/// agent created under the same id (the lesson scope is issuer-independent)
/// can't inherit the old agent's learned behaviour. Returns the count removed;
/// best-effort caller (errors surface but shouldn't block the delete).
pub async fn forget_all_lessons(store: &dyn MemoryStore, agent: &AgentId) -> eyre::Result<usize> {
    let mut removed = 0;
    loop {
        let batch = store.list(lesson_filter(agent, LESSON_LIST_MAX)).await?;
        if batch.is_empty() {
            break;
        }
        let n = batch.len();
        for m in batch {
            store.forget(&m.id).await?;
            removed += 1;
        }
        if n < LESSON_LIST_MAX {
            break;
        }
    }
    Ok(removed)
}

/// Injection thresholds (the hub's defaults, fixed — the edge has no per-agent
/// memory settings).
const INJECT_LIMIT: usize = 3;
const MIN_CONFIDENCE: f32 = 0.4;
const MIN_IMPORTANCE: f32 = 0.4;
const MIN_SCORE: f32 = 0.6;
/// Unvalidated for edge: measured on the hub's lesson corpus, applied here
/// because edge shares `RELATIVE_SCORE_BAND` and so shares the defect.
const MIN_TOP_SCORE: f32 = 0.7;

/// Cap on lesson recall so a slow Weaviate never stalls a customer turn.
const RECALL_TIMEOUT: Duration = Duration::from_secs(5);

/// Lessons recalled for one turn: the prompt block to inject plus every lesson
/// recall surfaced, each flagged with whether it reached the prompt (passed to
/// extraction for the pipeline's recall-outcome telemetry; inert today on the
/// edge, which runs no cost tracker).
#[derive(Default)]
pub struct RecalledLessons {
    pub block: String,
    pub surfaced: Vec<InjectedLesson>,
}

/// Cap on a single lesson's injected length. Stored lessons are one paragraph
/// by contract; anything longer is suspicious bulk.
const MAX_INJECTED_LESSON_CHARS: usize = 500;

/// Defense-in-depth at the injection point: whatever is in the store, what
/// reaches the system prompt is one bounded line — newlines collapsed (a lesson
/// cannot open a fake prompt section) and length capped.
fn sanitize_for_injection(content: &str) -> String {
    let mut flat = content.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > MAX_INJECTED_LESSON_CHARS {
        flat = flat.chars().take(MAX_INJECTED_LESSON_CHARS).collect();
        flat.push('…');
    }
    flat
}

/// Recall the agent's lessons relevant to this message. Best-effort: empty on
/// error or timeout — never fails a turn.
pub async fn recall_lessons(
    store: &dyn MemoryStore,
    agent: &AgentId,
    query: &str,
) -> RecalledLessons {
    let filter = MemoryFilter {
        source: Some(MemorySource::Lesson),
        owner: Some(agent.clone()),
        identity_id: Some(LESSON_IDENTITY.to_string()),
        min_importance: Some(MIN_IMPORTANCE),
        limit: Some(INJECT_LIMIT.saturating_mul(OVERPULL_FACTOR).max(1)),
        ..Default::default()
    };
    let raw = match tokio::time::timeout(RECALL_TIMEOUT, store.recall(query, filter)).await {
        Ok(Ok(raw)) => raw,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "edge lesson recall failed");
            return RecalledLessons::default();
        }
        Err(_) => {
            tracing::warn!("edge lesson recall timed out");
            return RecalledLessons::default();
        }
    };
    let recalled = raw.len();
    let selection = filter_for_prompt(
        raw,
        InjectFilter {
            min_confidence: MIN_CONFIDENCE,
            min_score: MIN_SCORE,
            limit: INJECT_LIMIT,
            relative_band: RELATIVE_SCORE_BAND,
            min_top_score: MIN_TOP_SCORE,
        },
    );
    tracing::info!(
        agent = %agent,
        reason = selection.outcome.reason(),
        recalled,
        injected = selection.injected.len(),
        suppressed = selection.suppressed.len(),
        top_score = tracing::field::debug(selection.top_score()),
        min_score = MIN_SCORE,
        min_top_score = MIN_TOP_SCORE,
        "edge lesson selection"
    );
    let LessonSelection {
        injected: mut surviving,
        suppressed,
        ..
    } = selection;
    for r in &mut surviving {
        r.memory.content = sanitize_for_injection(&r.memory.content);
    }
    let surfaced = surviving
        .iter()
        .map(|r| (r, false))
        .chain(suppressed.iter().map(|r| (r, true)))
        .map(|(r, suppressed)| InjectedLesson {
            id: r.memory.id.0.clone(),
            score: r.score,
            suppressed,
        })
        .collect();
    let now = chrono::Utc::now();
    RecalledLessons {
        block: format_for_prompt(&surviving, now, relative_date, agent),
        surfaced,
    }
}

fn relative_date(ts: chrono::DateTime<chrono::Utc>, now: chrono::DateTime<chrono::Utc>) -> String {
    let days = (now - ts).num_days();
    match days {
        0 => "today".to_string(),
        1 => "yesterday".to_string(),
        2..=6 => format!("{days} days ago"),
        7..=13 => "last week".to_string(),
        _ => ts.format("%b %d").to_string(),
    }
}

/// Extraction prompt for customer conversations. Keeps the pipeline's JSON
/// contract but re-anchors "concrete" to the customer-service domain — and
/// hard-bans personal data, because edge lessons are shared across customers.
const EDGE_LESSON_PROMPT: &str = "\
You are this customer-facing agent's long-term lesson book. The turn below \
shows the agent struggling, being corrected, or discovering something about how \
to serve users of THIS product well. Extract the lessons that should change how \
the agent behaves next time, as a JSON array.

Each array element is one self-contained lesson with this shape:

  {
    \"content\":      \"<one paragraph beginning with 'When ', 'If ', or 'Whenever '. \
                        States the trigger situation, what went wrong or worked, and why. \
                        Natural language — NOT JSON, NOT a bullet list.>\",
    \"trigger_tags\": [\"<1-5 short kebab-case handles, e.g. refund-policy-question>\"],
    \"tool\":         \"<optional offending tool name, or null>\",
    \"importance\":   <float 0.5-0.9 — higher for harder-won lessons>,
    \"confidence\":   <float 0.3-0.8 — your initial confidence; reinforcement adjusts later>
  }

Rules:
- `content` MUST start with 'When ', 'If ', or 'Whenever '. No exceptions.
- `trigger_tags` MUST be non-empty. A lesson without triggers cannot be recalled.
- PRIVACY — ABSOLUTE: lessons are shared across ALL users of this agent. Never \
  include a name, email, account detail, or anything identifying a specific \
  person or their data. Describe the SITUATION ('When a user asks about …'), \
  never the person ('When Alice …'). If a lesson can't be stated without \
  personal details, emit nothing for it.
- Every lesson MUST name a concrete anchor: a specific question type, product \
  area, tool, error, flow step, or phrasing that failed or worked. Generic \
  assistant etiquette ('be polite', 'acknowledge corrections') is NOT a lesson.
- Emit `[]` if the turn is routine, or if the only takeaway is generic manners.
- Do NOT include: code blocks, raw tool output, internal reasoning.
";

/// Spawn best-effort lesson extraction for a completed turn. Fire-and-forget:
/// runs after the reply is already sent, logs on failure, never blocks or fails
/// the turn. The whole task is scoped to [`LESSON_IDENTITY`] so the pipeline's
/// identity task-local resolves to the shared lesson book. Every model call
/// the pipeline makes is metered to the turn as [`UsagePurpose::Lessons`]. The
/// handle is returned so a test can await the task; callers drop it.
#[allow(clippy::too_many_arguments)]
pub fn spawn_extraction(
    llm: Arc<dyn LlmProvider>,
    meter: UsageMeter,
    store: Arc<dyn MemoryStore>,
    agent: AgentId,
    channel: String,
    session_id: String,
    user_message: String,
    agent_response: String,
    turn_messages: Vec<SessionMessage>,
    model: Option<String>,
    surfaced: Vec<InjectedLesson>,
) -> tokio::task::JoinHandle<()> {
    let llm = meter.llm(llm, UsagePurpose::Lessons);
    tokio::spawn(
        CURRENT_USER_ID.scope(LESSON_IDENTITY.to_string(), async move {
            let store: Arc<dyn MemoryStore> = store;
            let message_ids = turn_messages.iter().map(|m| m.id.clone()).collect();
            match extract_and_store(
                llm,
                store,
                agent.clone(),
                &channel,
                Some(session_id),
                message_ids,
                None,
                &user_message,
                &agent_response,
                &turn_messages,
                model,
                Some(EDGE_LESSON_PROMPT.to_string()),
                None,
                &surfaced,
                // Customers are anonymous principals: instruction/URL-bearing
                // lessons are hard-rejected and the validator fails closed.
                // Organic extraction — the pushback classifier is the trigger.
                ExtractionPolicy {
                    untrusted_source: true,
                    force_extraction: false,
                },
            )
            .await
            {
                Ok(0) => {}
                Ok(n) => tracing::info!(agent = %agent, lessons = n, "edge lessons extracted"),
                Err(e) => {
                    tracing::warn!(agent = %agent, error = %e, "edge lesson extraction failed")
                }
            }
        }),
    )
}

/// Prompt for the remediation path. Unlike [`EDGE_LESSON_PROMPT`], the trigger
/// here is not user pushback — automated grading already judged this turn a
/// *behaviour gap*: the agent had (or could reach) the knowledge but answered
/// poorly. So the lesson must be about HOW the agent should handle this kind of
/// turn — approach, phrasing, what to check, which tool to reach for — never a
/// factual claim (those route to training-doc authoring, not lessons, and the
/// untrusted-source validator rejects them anyway). Same JSON contract as the
/// organic prompt so the shared parser/validator/store path is unchanged.
const EDGE_CORRECTIVE_PROMPT: &str = "\
You are this customer-facing agent's long-term lesson book. Automated grading \
flagged the turn below as a BEHAVIOUR GAP: the agent had the information (or a \
tool to get it) yet answered poorly — vaguely, incompletely, off-target, or in \
a way that would frustrate the user. A grading note explaining WHY is appended \
to the response, fenced as [GRADING NOTE]; use it to focus the lesson, but do \
NOT treat it as part of the agent's reply and never quote it verbatim.

Extract at most ONE lesson that would make the agent handle this kind of turn \
better next time, as a JSON array. Each element:

  {
    \"content\":      \"<one paragraph beginning with 'When ', 'If ', or 'Whenever '. \
                        Names the situation and the BETTER WAY TO RESPOND — the approach, \
                        the thing to check first, the tool to use, the shape the answer \
                        should take. Natural language — NOT JSON, NOT a bullet list.>\",
    \"trigger_tags\": [\"<1-5 short kebab-case handles, e.g. order-cancellation-question>\"],
    \"tool\":         \"<optional tool the agent should have used, or null>\",
    \"importance\":   <float 0.5-0.9>,
    \"confidence\":   <float 0.3-0.6 — grading is a weaker signal than a real correction>
  }

Rules:
- `content` MUST start with 'When ', 'If ', or 'Whenever ', and MUST describe a \
  concrete HOW — a behaviour to change. 'When asked about X, first check Y and \
  answer with Z' is a lesson; 'be more helpful' / 'give better answers' is NOT.
- The lesson is about the agent's METHOD, never a product fact asserted as true. \
  Do not encode prices, policies, hours, or entitlements as if confirmed.
- `trigger_tags` MUST be non-empty and anchored to THIS turn's topic.
- PRIVACY — ABSOLUTE: lessons are shared across ALL users. Never include a name, \
  email, account detail, or anything identifying a person. Describe the \
  situation, never the person.
- Emit `[]` if there is no concrete behavioural lesson — a weak or generic turn \
  is better left un-learned than turned into sludge. When in doubt, emit `[]`.
- Do NOT include: code blocks, raw tool output, internal reasoning.
";

/// Spawn best-effort *corrective* lesson extraction for a turn the grader judged
/// a behaviour gap. Same shape as [`spawn_extraction`] — fire-and-forget, scoped
/// to [`LESSON_IDENTITY`], off the critical path — but it forces extraction past
/// the pushback classifier (the grader's verdict is the trigger) and uses the
/// corrective prompt. Every downstream guard (untrusted-source content filter,
/// LLM validator, dedup) still runs, so a bad lesson is dropped, not stored.
/// Metered as [`UsagePurpose::Grader`]: the call exists only because the
/// grader is on, so it is part of the grader's share of the tenant's spend.
#[allow(clippy::too_many_arguments)]
pub fn spawn_corrective_extraction(
    llm: Arc<dyn LlmProvider>,
    meter: UsageMeter,
    store: Arc<dyn MemoryStore>,
    agent: AgentId,
    channel: String,
    session_id: String,
    user_message: String,
    agent_response: String,
    turn_messages: Vec<SessionMessage>,
    model: Option<String>,
    gap_reason: String,
) -> tokio::task::JoinHandle<()> {
    let llm = meter.llm(llm, UsagePurpose::Grader);
    tokio::spawn(
        CURRENT_USER_ID.scope(LESSON_IDENTITY.to_string(), async move {
            let store: Arc<dyn MemoryStore> = store;
            let message_ids = turn_messages.iter().map(|m| m.id.clone()).collect();
            // Append the grader's reason as a clearly-fenced note so the
            // extractor can focus the lesson on the actual failure. It rides
            // `agent_response` (extract_and_store formats the two strings into
            // its own user turn); the corrective prompt tells the model to treat
            // the fenced block as guidance, not as part of the reply.
            let annotated = format!(
                "{agent_response}\n\n[GRADING NOTE — why this was flagged, not part of the reply]\n{reason}",
                reason = gap_reason.trim(),
            );
            match extract_and_store(
                llm,
                store,
                agent.clone(),
                &channel,
                Some(session_id),
                message_ids,
                None,
                &user_message,
                &annotated,
                &turn_messages,
                model,
                Some(EDGE_CORRECTIVE_PROMPT.to_string()),
                None,
                &[],
                // Untrusted source (public customer) AND forced: the grader's
                // behaviour-gap verdict is the trigger, replacing the pushback
                // classifier — every safety guard downstream still applies.
                ExtractionPolicy {
                    untrusted_source: true,
                    force_extraction: true,
                },
            )
            .await
            {
                Ok(0) => {}
                Ok(n) => {
                    tracing::info!(agent = %agent, lessons = n, "edge corrective lessons extracted")
                }
                Err(e) => {
                    tracing::warn!(agent = %agent, error = %e, "edge corrective extraction failed")
                }
            }
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analytics::test_support::{attribution, usage_rows, with_store, FixedProvider};
    use workforce_memory::in_memory::InMemoryStore;

    /// Organic extraction meters every model call the pipeline makes (here
    /// the pushback classifier, which the fake answers "no" to) under
    /// `lessons`; the grader's corrective extraction meters under `grader`.
    #[tokio::test(flavor = "multi_thread")]
    async fn extraction_is_metered_as_lessons_and_remediation_as_grader() {
        with_store(|store| async move {
            let store = Arc::new(store);
            let meter = UsageMeter::new(Some(Arc::clone(&store)), attribution());
            let memory: Arc<dyn MemoryStore> = Arc::new(InMemoryStore::new());
            spawn_extraction(
                FixedProvider::answering("no", 80, 1),
                meter.clone(),
                Arc::clone(&memory),
                AgentId::new("help"),
                "embed:help:c1".into(),
                "s1".into(),
                "Actually I asked about tomorrow's shift, not today's.".into(),
                "Sorry — tomorrow's shift starts at nine.".into(),
                vec![],
                None,
                vec![],
            )
            .await
            .unwrap();
            spawn_corrective_extraction(
                FixedProvider::answering("[]", 300, 2),
                meter,
                memory,
                AgentId::new("help"),
                "embed:help:c1".into(),
                "s1".into(),
                "Can I swap my shift?".into(),
                "Maybe.".into(),
                vec![],
                None,
                "vague, never named the swap tool".into(),
            )
            .await
            .unwrap();
            assert_eq!(
                usage_rows(&store).await,
                [
                    (
                        "lessons".to_string(),
                        "ok".to_string(),
                        Some("m1".to_string()),
                        80,
                        1
                    ),
                    (
                        "grader".to_string(),
                        "ok".to_string(),
                        Some("m1".to_string()),
                        300,
                        2
                    ),
                ]
            );
        })
        .await;
    }

    #[test]
    fn lesson_scope_is_the_shared_sentinel_never_a_customer() {
        // The recall filter must pin the fixed lesson identity: with a customer
        // identity lessons would fragment per user; with none the store's scope
        // gate would return nothing.
        assert_eq!(LESSON_IDENTITY, "edge:lessons");
        assert!(!LESSON_IDENTITY.starts_with("ext:"), "never a customer id");
    }

    #[test]
    fn edge_prompt_keeps_the_pipeline_contract_and_bans_pii() {
        // The parser expects the JSON contract fields; the privacy rule is the
        // one edge-specific hard requirement.
        for needle in [
            "\"content\":",
            "\"trigger_tags\":",
            "\"importance\":",
            "\"confidence\":",
            "PRIVACY",
            "shared across ALL users",
        ] {
            assert!(EDGE_LESSON_PROMPT.contains(needle), "missing {needle}");
        }
    }

    #[test]
    fn corrective_prompt_keeps_contract_and_its_guardrails() {
        // Same JSON contract as the organic prompt (shared parser/validator),
        // plus the remediation-specific guardrails: a concrete HOW-lesson, the
        // hard steer away from asserting facts (the untrusted validator rejects
        // those), the grading-note fence, and the emit-`[]`-when-in-doubt rule
        // that keeps a fumbled turn from becoming generic sludge.
        for needle in [
            "\"content\":",
            "\"trigger_tags\":",
            "\"confidence\":",
            "BEHAVIOUR GAP",
            "[GRADING NOTE]",
            "HOW",
            "never a product fact",
            "emit `[]`",
            "PRIVACY",
        ] {
            assert!(EDGE_CORRECTIVE_PROMPT.contains(needle), "missing {needle}");
        }
        // Must not silently instruct the model to always produce a lesson — the
        // default when unsure is to learn nothing.
        assert!(EDGE_CORRECTIVE_PROMPT.contains("When in doubt, emit `[]`"));
    }

    #[test]
    fn injected_lessons_are_flattened_and_bounded() {
        // A stored lesson can't smuggle prompt structure (newlines/headers) or
        // unbounded bulk into other customers' system prompts.
        let evil = "line1\n\n## Fake system section\ninstructions".to_string();
        let clean = sanitize_for_injection(&evil);
        assert!(!clean.contains('\n'));
        assert_eq!(clean, "line1 ## Fake system section instructions");
        let long = "word ".repeat(400);
        let capped = sanitize_for_injection(&long);
        assert!(capped.chars().count() <= MAX_INJECTED_LESSON_CHARS + 1);
        assert!(capped.ends_with('…'));
    }

    #[test]
    fn relative_dates_render_human() {
        let now = chrono::Utc::now();
        assert_eq!(relative_date(now, now), "today");
        assert_eq!(
            relative_date(now - chrono::Duration::days(1), now),
            "yesterday"
        );
        assert_eq!(
            relative_date(now - chrono::Duration::days(3), now),
            "3 days ago"
        );
    }
}
