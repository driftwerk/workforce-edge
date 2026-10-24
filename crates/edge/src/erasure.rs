//! Forgetting a customer: erasing what the edge holds about one person.
//!
//! A customer is one owner key (`ext:{issuer}␟{sub}`), the key their
//! conversations, memory, analytics and files are all stored under, so each
//! store is erased by exact match on it and a neighbour whose key merely starts
//! the same is never touched. The edge keeps memory in its own Weaviate only;
//! it calls no other memory service. Every store is erased on its own and
//! reports its own outcome, so a part that failed is named in the result
//! instead of hiding behind the parts that worked.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use workforce_agent::artifacts::FsArtifactStore;
use workforce_common::session::SessionStore;
use workforce_core::types::AgentId;
use workforce_memory::store::MemoryStore;

use crate::analytics::EdgeAnalyticsStore;

/// Conversations deleted per listing.
const SESSION_PAGE: usize = 500;

/// Where a customer's data lives.
pub struct Stores<'a> {
    pub memory: &'a dyn MemoryStore,
    /// The session store of one agent, by id: an agent the install no longer
    /// hosts still has its conversations on disk.
    pub sessions: &'a (dyn Fn(&str) -> Arc<dyn SessionStore> + Sync),
    pub analytics: &'a EdgeAnalyticsStore,
    /// `None` when the install keeps no files.
    pub files: Option<&'a FsArtifactStore>,
}

/// What one kind of data came to: how many records went, and what could not
/// be erased.
#[derive(Debug, Default, PartialEq, Serialize)]
pub struct Part {
    pub erased: usize,
    pub failures: Vec<String>,
}

impl Part {
    fn tally(outcomes: impl IntoIterator<Item = Result<usize, String>>) -> Self {
        outcomes
            .into_iter()
            .fold(Self::default(), |mut part, outcome| {
                match outcome {
                    Ok(n) => part.erased += n,
                    Err(failure) => part.failures.push(failure),
                }
                part
            })
    }
}

#[derive(Debug, Serialize)]
pub struct Erasure {
    /// Every memory, whatever wrote it, forgotten ones included.
    pub memories: Part,
    /// Conversations with their archives and turn journals.
    pub conversations: Part,
    /// Analytics rows erased, or cleared of content and unlinked.
    pub analytics: Part,
    /// Uploaded and generated files.
    pub files: Part,
}

impl Erasure {
    fn parts(&self) -> [&Part; 4] {
        [
            &self.memories,
            &self.conversations,
            &self.analytics,
            &self.files,
        ]
    }

    pub fn complete(&self) -> bool {
        self.failures() == 0
    }

    pub fn failures(&self) -> usize {
        self.parts().iter().map(|part| part.failures.len()).sum()
    }
}

/// How long a customer must have been idle before they can be forgotten. A
/// turn leaves work running after its answer (passive capture and the
/// consolidation it may start, grading, usage), each bounded by an LLM
/// provider's five-minute request timeout; erasing before it lands would let
/// it write back what was just erased.
pub const SETTLE: Duration = Duration::from_secs(15 * 60);

/// Why an erasure was not started.
#[derive(Debug, PartialEq)]
pub enum Refusal {
    /// The key is not a customer's owner key.
    NotACustomer,
    /// The customer has used an agent within [`SETTLE`].
    Active,
    /// Whether the customer is active could not be read.
    Unchecked(String),
}

/// Erase everything about `customer` from every agent in `hosted` and every
/// agent the stores still hold something of theirs under. Refused while they
/// are active, and reported incomplete if they became active while it ran,
/// so nothing a turn writes back is ever left behind unreported.
pub async fn forget_customer(
    customer: &str,
    hosted: &[String],
    stores: &Stores<'_>,
) -> Result<Erasure, Refusal> {
    if !crate::ingress::is_customer_owner(customer) {
        return Err(Refusal::NotACustomer);
    }
    match stores.analytics.active_within(customer, SETTLE).await {
        Ok(false) => {}
        Ok(true) => return Err(Refusal::Active),
        Err(e) => return Err(Refusal::Unchecked(format!("{e:#}"))),
    }
    let started = Instant::now();
    let mut erasure = erase(customer, hosted, stores).await;
    match stores
        .analytics
        .active_within(customer, started.elapsed())
        .await
    {
        Ok(false) => {}
        Ok(true) => erasure.conversations.failures.push(
            "the customer started a conversation while being forgotten; forget them again once they are idle"
                .to_string(),
        ),
        Err(e) => erasure.conversations.failures.push(format!(
            "whether the customer came back while being forgotten could not be read: {e:#}"
        )),
    }
    Ok(erasure)
}

async fn erase(customer: &str, hosted: &[String], stores: &Stores<'_>) -> Erasure {
    let (agents, unsearched) = match stores.analytics.agents_holding(customer).await {
        Ok(holding) => (
            hosted
                .iter()
                .cloned()
                .chain(holding)
                .collect::<BTreeSet<_>>(),
            None,
        ),
        Err(e) => (
            hosted.iter().cloned().collect(),
            Some(format!("agents no longer hosted were not searched: {e:#}")),
        ),
    };

    let mut memories = Vec::new();
    for agent in &agents {
        memories.push(
            stores
                .memory
                .purge_identity(&AgentId::new(agent), customer)
                .await
                .map_err(|e| format!("agent {agent}: {e:#}")),
        );
    }
    let conversations = agents.iter().map(|agent| {
        erase_conversations((stores.sessions)(agent).as_ref(), customer)
            .map_err(|e| format!("agent {agent}: {e:#}"))
    });
    let unsearched = unsearched.into_iter().map(Err);

    Erasure {
        memories: Part::tally(memories.into_iter().chain(unsearched.clone())),
        conversations: Part::tally(conversations.chain(unsearched)),
        analytics: Part::tally([stores
            .analytics
            .forget_owner(customer)
            .await
            .map(|rows| rows as usize)
            .map_err(|e| format!("{e:#}"))]),
        files: Part::tally(
            stores
                .files
                .map(|files| files.purge_user(customer).map_err(|e| format!("{e:#}"))),
        ),
    }
}

/// Delete every conversation `customer` has with the agent `store` belongs to.
fn erase_conversations(store: &dyn SessionStore, customer: &str) -> eyre::Result<usize> {
    let mut erased = 0;
    loop {
        let page = store.list_for_owner(customer, SESSION_PAGE)?;
        if page.is_empty() {
            return Ok(erased);
        }
        page.iter()
            .try_for_each(|session| store.delete(&session.id))?;
        erased += page.len();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_agent::artifacts::{
        ArtifactBody, ArtifactDescriptor, ArtifactStore, ContentType,
    };
    use workforce_common::session_postgres::{run_session_migrations, PostgresSessionStore};
    use workforce_memory::in_memory::InMemoryStore;
    use workforce_memory::types::{Memory, MemoryLayer, MemorySource};
    use workforce_memory::weaviate::WeaviateStore;
    use workforce_postgres::test_support::with_test_pool;

    const CUSTOMER: &str = "ext:iss\u{1f}sub-1";
    const NEIGHBOUR: &str = "ext:iss\u{1f}sub-10";

    fn file(user: &str) -> ArtifactDescriptor {
        ArtifactDescriptor {
            producer: "test".into(),
            mime: ContentType::ApplicationJson,
            schema: None,
            row_count: None,
            ttl_seconds: 3600,
            agent_id: "help".into(),
            session_id: None,
            user_id: Some(user.into()),
            filename: None,
            media_type: None,
        }
    }

    /// Two customers with a conversation, a memory, analytics and a file each,
    /// on the hosted agent `help`, and a conversation the customer had with
    /// `gone`, an agent since removed.
    async fn seed(
        pool: &workforce_common::postgres::PostgresPool,
        memory: &InMemoryStore,
        analytics: &EdgeAnalyticsStore,
        files: &FsArtifactStore,
    ) {
        for owner in [CUSTOMER, NEIGHBOUR] {
            for agent in ["help", "gone"] {
                PostgresSessionStore::with_shared_pool(pool.clone(), agent)
                    .get_or_create(&format!("edge:{agent}:c1"), owner, agent)
                    .unwrap();
            }
            for source in [MemorySource::Agent, MemorySource::Passive] {
                memory
                    .remember(
                        Memory::new("likes tea", MemoryLayer::Episodic, AgentId::new("help"))
                            .with_identity_id(owner.to_string())
                            .with_source(source),
                    )
                    .await
                    .unwrap();
            }
            analytics
                .record_feedback("help", owner, "c1", "m1", 1, Some("said something"))
                .await
                .unwrap();
            analytics
                .record_usage(
                    &crate::analytics::UsageEvent {
                        purpose: crate::analytics::UsagePurpose::Turn,
                        agent_id: "help".into(),
                        owner: owner.into(),
                        conversation: "c1".into(),
                        message_id: Some("m1".into()),
                        model: "test-model".into(),
                        input_tokens: 10,
                        output_tokens: 5,
                        cache_read_tokens: 0,
                        cache_write_tokens: 0,
                        rounds: 1,
                        tool_count: 0,
                        tool_failures: 0,
                        duration_ms: 100,
                        outcome: crate::analytics::TurnOutcome::Ok,
                    },
                    None,
                )
                .await
                .unwrap();
            analytics
                .record_grade(
                    "help",
                    owner,
                    "c1",
                    "m1",
                    &crate::analytics::AnswerGrade {
                        quality: "poor".into(),
                        gap_topic: Some("their invoice".into()),
                        remedy: Some("knowledge".into()),
                        reason: Some("missed their invoice".into()),
                        source: "llm".into(),
                        model: None,
                        question: Some("where is my invoice".into()),
                    },
                    None,
                )
                .await
                .unwrap();
            analytics
                .upsert_identity(
                    owner,
                    "iss",
                    &crate::analytics::TurnIdentity {
                        display_name: Some("A Person".into()),
                        email: Some("person@example.com".into()),
                    },
                    None,
                )
                .await
                .unwrap();
            files
                .write(
                    "help",
                    file(owner),
                    ArtifactBody::Json(serde_json::json!([1])),
                )
                .unwrap();
        }
        memory
            .remember(
                Memory::new("from before", MemoryLayer::Episodic, AgentId::new("gone"))
                    .with_identity_id(CUSTOMER.to_string()),
            )
            .await
            .unwrap();
    }

    /// What the stores hold after the erasure, for the test to inspect.
    struct Held {
        pool: workforce_common::postgres::PostgresPool,
        memory: InMemoryStore,
        files: Arc<FsArtifactStore>,
    }

    /// What `CUSTOMER` was last doing when their erasure is asked for.
    #[derive(Clone, Copy, PartialEq)]
    enum Last {
        /// Nothing for an hour.
        Idle,
        /// Their conversation was saved just now.
        Saved,
        /// A turn of theirs is running on some replica.
        Running,
    }

    /// Seed both customers as `last` says, erase `customer` with only `help`
    /// hosted, and hand the outcome and the stores to `test`. `memory`
    /// replaces the seeded memory store for the erasure itself.
    async fn erase_with<F, Fut>(
        customer: &str,
        last: Last,
        memory: Option<&(dyn MemoryStore + Sync)>,
        test: F,
    ) where
        F: FnOnce(Result<Erasure, Refusal>, Held) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        with_test_pool(PostgresSessionStore::build_pool, |pool| async move {
            run_session_migrations(&pool).await.unwrap();
            crate::analytics::run_edge_analytics_migrations(&pool)
                .await
                .unwrap();
            let analytics = EdgeAnalyticsStore::new(
                crate::tenant::TenantPool::new(pool.clone()),
                crate::tenant::TenantId::default_tenant(),
            );
            let seeded = InMemoryStore::new();
            let dir = tempfile::tempdir().unwrap();
            let files = FsArtifactStore::new(dir.path().to_path_buf()).unwrap();
            seed(&pool, &seeded, &analytics, &files).await;
            if last != Last::Saved {
                pool.get()
                    .await
                    .unwrap()
                    .execute(
                        "UPDATE sessions.sessions SET updated_at = now() - interval '1 hour'",
                        &[],
                    )
                    .await
                    .unwrap();
            }
            if last == Last::Running {
                analytics
                    .claim_turn(&crate::analytics::TurnClaim {
                        turn_key: "k",
                        conversation: "c2",
                        owner: CUSTOMER,
                        agent_id: "help",
                        replica: "r1",
                    })
                    .await
                    .unwrap();
            }

            let sessions_pool = pool.clone();
            let sessions = move |agent: &str| -> Arc<dyn SessionStore> {
                PostgresSessionStore::with_shared_pool(sessions_pool.clone(), agent)
            };
            let stores = Stores {
                memory: memory.map_or(&seeded as &dyn MemoryStore, |m| m as &dyn MemoryStore),
                sessions: &sessions,
                analytics: &analytics,
                files: Some(&files),
            };
            let erasure = forget_customer(customer, &["help".to_string()], &stores).await;
            test(
                erasure,
                Held {
                    pool,
                    memory: seeded,
                    files,
                },
            )
            .await;
            drop(dir);
        })
        .await;
    }

    /// One line per analytics row: its table, its owner (any forgotten key
    /// under one name) and the content it still carries.
    async fn analytics_rows(pool: &workforce_common::postgres::PostgresPool) -> Vec<String> {
        let conn = pool.get().await.unwrap();
        let mut rows: Vec<String> = conn
            .query(
            "SELECT t || ' ' || CASE WHEN owner LIKE 'forgotten:%' THEN 'forgotten' ELSE owner END \
                 || ' ' || coalesce(content, '-') FROM ( \
               SELECT 'feedback' t, owner, comment content FROM edge_analytics.feedback \
               UNION ALL SELECT 'grade', owner, concat_ws('|', question, reason, gap_topic, remedy) \
                 FROM edge_analytics.answer_grades \
               UNION ALL SELECT 'usage', owner, input_tokens::text FROM edge_analytics.usage_events \
               UNION ALL SELECT 'identity', owner, email FROM edge_analytics.chat_identities) rows",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
        rows.sort();
        rows
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_customer_is_erased_from_every_store_and_agent_and_a_neighbour_is_not() {
        erase_with(CUSTOMER, Last::Idle, None, |erasure, held| async move {
            let erasure = erasure.unwrap();
            assert!(erasure.complete(), "{erasure:?}");
            assert_eq!(
                erasure.memories.erased, 3,
                "both sources, and the removed agent's"
            );
            assert_eq!(erasure.conversations.erased, 2, "the removed agent's too");
            assert_eq!(erasure.analytics.erased, 4);
            assert_eq!(erasure.files.erased, 1);

            for agent in ["help", "gone"] {
                let sessions = PostgresSessionStore::with_shared_pool(held.pool.clone(), agent);
                assert!(sessions.list_for_owner(CUSTOMER, 10).unwrap().is_empty());
                assert_eq!(sessions.list_for_owner(NEIGHBOUR, 10).unwrap().len(), 1);
                assert!(held
                    .memory
                    .identity_memories(&AgentId::new(agent), CUSTOMER)
                    .await
                    .unwrap()
                    .is_empty());
            }
            assert_eq!(
                held.memory
                    .identity_memories(&AgentId::new("help"), NEIGHBOUR)
                    .await
                    .unwrap()
                    .len(),
                2
            );
            assert_eq!(held.files.purge_user(NEIGHBOUR).unwrap(), 1);

            assert_eq!(
                analytics_rows(&held.pool).await,
                [
                    format!("feedback {NEIGHBOUR} said something"),
                    "feedback forgotten -".to_string(),
                    format!("grade {NEIGHBOUR} where is my invoice|missed their invoice|their invoice|knowledge"),
                    "grade forgotten knowledge".to_string(),
                    format!("identity {NEIGHBOUR} person@example.com"),
                    format!("usage {NEIGHBOUR} 10"),
                    "usage forgotten 10".to_string(),
                ],
                "counted under a key naming no one, without their words; the neighbour untouched"
            );
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_store_that_fails_is_reported_and_the_others_are_still_erased() {
        let unreachable = WeaviateStore::new("http://127.0.0.1:1", None);
        erase_with(
            CUSTOMER,
            Last::Idle,
            Some(&unreachable),
            |erasure, _| async move {
                let erasure = erasure.unwrap();
                assert!(!erasure.complete());
                assert_eq!(erasure.memories.erased, 0);
                assert_eq!(erasure.memories.failures.len(), 2, "{:?}", erasure.memories);
                assert!(erasure.memories.failures[0].starts_with("agent gone: "));
                assert_eq!(erasure.failures(), 2);
                assert_eq!(erasure.conversations.erased, 2);
                assert_eq!(erasure.files.erased, 1);
            },
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_customer_who_was_just_active_is_not_erased_yet() {
        for last in [Last::Saved, Last::Running] {
            erase_with(CUSTOMER, last, None, |erasure, held| async move {
                assert_eq!(erasure.unwrap_err(), Refusal::Active);
                let sessions = PostgresSessionStore::with_shared_pool(held.pool.clone(), "help");
                assert_eq!(sessions.list_for_owner(CUSTOMER, 10).unwrap().len(), 1);
            })
            .await;
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn only_a_customers_key_is_erased() {
        for key in [
            "edge:lessons",
            "ext:cust-9",
            "6f1c0d2e-9a5b-4c1d-8e7f-0a1b2c3d4e5f",
        ] {
            erase_with(key, Last::Idle, None, |erasure, _| async move {
                assert_eq!(erasure.unwrap_err(), Refusal::NotACustomer);
            })
            .await;
        }
    }
}
