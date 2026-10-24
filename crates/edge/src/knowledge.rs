//! Training data — a per-agent knowledge corpus the agent answers from.
//!
//! Operators add documents (FAQs, policies, product facts) in the admin UI. Each
//! document's text is chunked and embedded into the edge's Weaviate; on every
//! turn the most relevant chunks are recalled and injected into the system
//! prompt, so the agent grounds its answers in the corpus rather than guessing.
//!
//! Scope, like lessons, is agent-global (not per-customer) but uses its OWN
//! sentinel identity [`KNOWLEDGE_IDENTITY`] and [`MemorySource::Curated`], so
//! knowledge recall, lesson recall, and per-customer memory never bleed into one
//! another. The authored documents (the source of truth for the UI + re-chunking
//! on edit) live in Postgres; Weaviate holds only the derived chunks.

use std::sync::Arc;
use std::time::Duration;

use eyre::{Result, WrapErr};
use workforce_core::types::AgentId;
use workforce_memory::store::MemoryStore;
use workforce_memory::types::{Memory, MemoryFilter, MemoryLayer, MemorySource, Visibility};

use crate::tenant::{PlanUsage, TenantId, TenantPool};

/// The fixed identity every knowledge chunk is stored and recalled under.
pub(crate) const KNOWLEDGE_IDENTITY: &str = "edge:knowledge";

/// Chunking target: pack paragraphs up to this many chars per chunk.
const CHUNK_CHARS: usize = 1200;
/// Hard cap on chunks per document, so one giant upload can't flood Weaviate.
const MAX_CHUNKS: usize = 300;
/// How many chunks to inject per turn.
const RECALL_LIMIT: usize = 4;
const RECALL_MIN_CERTAINTY: f32 = 0.7;
/// Best-effort recall cap so a slow Weaviate never stalls a customer turn.
const RECALL_TIMEOUT: Duration = Duration::from_secs(5);

/// The tag that binds a chunk to its source document (for re-chunk + delete).
fn doc_tag(doc_id: &str) -> String {
    format!("knowdoc:{doc_id}")
}

/// Split text into embedding-sized chunks on paragraph boundaries, greedily
/// packing paragraphs up to `CHUNK_CHARS`. A single oversized paragraph is hard
/// split. Bounded to `MAX_CHUNKS`.
fn chunk(content: &str) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut cur = String::new();
    for para in content
        .split("\n\n")
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
        if cur.chars().count() + para.chars().count() + 2 > CHUNK_CHARS && !cur.is_empty() {
            chunks.push(std::mem::take(&mut cur));
        }
        if para.chars().count() > CHUNK_CHARS {
            // Oversized single paragraph: hard-split by chars.
            let mut rest = para;
            while rest.chars().count() > CHUNK_CHARS {
                let cut = rest
                    .char_indices()
                    .nth(CHUNK_CHARS)
                    .map(|(i, _)| i)
                    .unwrap_or(rest.len());
                chunks.push(rest[..cut].to_string());
                rest = &rest[cut..];
            }
            cur = rest.to_string();
        } else {
            if !cur.is_empty() {
                cur.push_str("\n\n");
            }
            cur.push_str(para);
        }
        if chunks.len() >= MAX_CHUNKS {
            break;
        }
    }
    if !cur.is_empty() && chunks.len() < MAX_CHUNKS {
        chunks.push(cur);
    }
    chunks.truncate(MAX_CHUNKS);
    chunks
}

/// One knowledge document as the operator manages it.
#[derive(Debug, serde::Serialize)]
pub struct KnowledgeDoc {
    pub id: String,
    pub title: String,
    pub content: String,
    pub chunks: usize,
    pub updated_at: String,
}

/// The corpus store: authored documents in Postgres, derived chunks in the
/// vector store. Bound to one tenant, whose documents are all it reads or
/// writes; chunk owners stay per agent, as each tenant has a vector-store
/// tenant of its own.
#[derive(Clone)]
pub struct KnowledgeStore {
    pool: TenantPool,
    tenant: TenantId,
    memory: Arc<dyn MemoryStore>,
    /// The tenant's plan, when the corpus is capped by one: a document that
    /// would take the corpus past its chunk cap is refused before anything
    /// is written, and every change drops the plan's usage snapshot.
    plan: Option<Arc<PlanUsage>>,
}

impl KnowledgeStore {
    pub fn new(pool: TenantPool, tenant: TenantId, memory: Arc<dyn MemoryStore>) -> Self {
        Self {
            pool,
            tenant,
            memory,
            plan: None,
        }
    }

    /// Hold the corpus to `plan`'s chunk cap.
    pub fn with_plan(mut self, plan: Arc<PlanUsage>) -> Self {
        self.plan = Some(plan);
        self
    }

    /// Documents for one agent, newest first.
    pub async fn list(&self, agent_id: &str) -> Result<Vec<KnowledgeDoc>> {
        let conn = self.pool.conn(&self.tenant).await?;
        let rows = conn
            .query(
                "SELECT id, title, content, chunks, updated_at FROM edge_knowledge \
                 WHERE tenant_id = $1 AND agent_id = $2 ORDER BY updated_at DESC",
                &[&self.tenant, &agent_id],
            )
            .await?;
        conn.commit().await?;
        Ok(rows
            .iter()
            .map(|r| {
                let updated: chrono::DateTime<chrono::Utc> = r.get(4);
                KnowledgeDoc {
                    id: r.get(0),
                    title: r.get(1),
                    content: r.get(2),
                    chunks: r.get::<_, i32>(3) as usize,
                    updated_at: updated.to_rfc3339(),
                }
            })
            .collect())
    }

    /// Add or replace a document: persist it, then re-embed its chunks (drop the
    /// old chunks first so an edit never leaves stale ones). Returns the chunk
    /// count. Under a plan, a document that would take the corpus past its
    /// chunk cap fails with a [`PlanLimit`] before any chunk is touched —
    /// the chunker is deterministic, so the count is known up front.
    pub async fn upsert(
        &self,
        agent_id: &str,
        id: &str,
        title: &str,
        content: &str,
    ) -> Result<usize> {
        let agent = AgentId::new(agent_id);
        let chunks = chunk(content);
        if let Some(plan) = &self.plan {
            let after = self.chunks_after_replacing(agent_id, id).await? + chunks.len() as u64;
            plan.admit_chunks(after)?;
        }
        // Re-embed: remove any existing chunks for this doc, then add fresh ones.
        self.delete_chunks(&agent, id).await?;
        for (i, text) in chunks.iter().enumerate() {
            let memory = Memory::new(text, MemoryLayer::Semantic, agent.clone())
                .with_identity_id(KNOWLEDGE_IDENTITY)
                .with_source(MemorySource::Curated)
                .with_visibility(Visibility::Private)
                .with_tags(vec![doc_tag(id), format!("knowchunk:{i}")]);
            if let Err(e) = self.memory.remember(memory).await {
                // A mid-loop failure would strand chunks that still recall into
                // customer turns; best-effort remove the partial set and don't
                // write the PG row, so the doc simply wasn't (re)created.
                let _ = self.delete_chunks(&agent, id).await;
                return Err(e).wrap_err("embedding a knowledge chunk");
            }
        }
        let conn = self.pool.conn(&self.tenant).await?;
        conn.execute(
            "INSERT INTO edge_knowledge (tenant_id, id, agent_id, title, content, chunks, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6, now()) \
             ON CONFLICT (tenant_id, agent_id, id) DO UPDATE SET title = EXCLUDED.title, \
                 content = EXCLUDED.content, chunks = EXCLUDED.chunks, updated_at = now()",
            &[
                &self.tenant,
                &id,
                &agent_id,
                &title,
                &content,
                &(chunks.len() as i32),
            ],
        )
        .await
        .wrap_err("persisting knowledge document")?;
        conn.commit().await?;
        if let Some(plan) = &self.plan {
            plan.invalidate();
        }
        Ok(chunks.len())
    }

    /// The corpus's chunk count with document `id` of `agent_id` taken out:
    /// what replacing that document starts from, since its old chunks go
    /// before the new ones land.
    async fn chunks_after_replacing(&self, agent_id: &str, id: &str) -> Result<u64> {
        let conn = self.pool.conn(&self.tenant).await?;
        let row = conn
            .query_one(
                "SELECT (SELECT coalesce(sum(chunks), 0) FROM edge_knowledge \
                     WHERE tenant_id = $1)::bigint, \
                        (SELECT coalesce(max(chunks), 0) FROM edge_knowledge \
                     WHERE tenant_id = $1 AND agent_id = $2 AND id = $3)::bigint",
                &[&self.tenant, &agent_id, &id],
            )
            .await
            .wrap_err("counting the corpus")?;
        conn.commit().await?;
        let (total, replaced): (i64, i64) = (row.get(0), row.get(1));
        Ok((total - replaced).max(0) as u64)
    }

    /// Make this agent's corpus exactly `docs` (`(id, title, content)`), for
    /// git-managed installs mirroring the repo's `training/` folder. Skips docs
    /// whose content is unchanged (so a boot doesn't re-embed everything), deletes
    /// docs no longer in git, and is best-effort per doc — one embedding failure
    /// warns and continues rather than bricking boot. Returns `(embedded, kept,
    /// removed)`.
    pub async fn replace_agent_docs(
        &self,
        agent_id: &str,
        docs: &[(String, String, String)],
    ) -> Result<(usize, usize, usize)> {
        let existing: std::collections::HashMap<String, String> = self
            .list(agent_id)
            .await?
            .into_iter()
            .map(|d| (d.id, d.content))
            .collect();
        let keep: std::collections::HashSet<&str> =
            docs.iter().map(|(id, _, _)| id.as_str()).collect();
        let (mut embedded, mut kept, mut removed) = (0, 0, 0);
        for (id, title, content) in docs {
            if existing.get(id).is_some_and(|prev| prev == content) {
                kept += 1;
                continue;
            }
            match self.upsert(agent_id, id, title, content).await {
                Ok(_) => embedded += 1,
                Err(e) => {
                    tracing::warn!(agent = %agent_id, doc = %id, error = %e, "git training ingest failed for a doc")
                }
            }
        }
        for old_id in existing.keys() {
            if !keep.contains(old_id.as_str())
                && self.delete(agent_id, old_id).await.unwrap_or(false)
            {
                removed += 1;
            }
        }
        Ok((embedded, kept, removed))
    }

    /// Cascade-delete: drop the whole corpus (docs + Weaviate chunks) of every
    /// agent whose id is NOT in `keep`. Run after a git mirror so a renamed or
    /// removed agent's training data doesn't linger as orphaned rows + embeddings.
    /// Callers only ever invoke this with a FRESH, non-degenerate git tree (see
    /// server boot / `resync_from_git`), so `keep` is the real current agent set
    /// and a failed/empty fetch never reaches here.
    ///
    /// Scope is deliberately JUST the corpus: a rename does NOT touch the edge's
    /// `sessions` (chat history is kept — it's audit, and shredding it on a folder
    /// rename would be worse than an orphan), nor the appearance overlay (being
    /// retired; a stale overlay could repaint a later agent that reuses the id —
    /// known, low-risk). Returns `(agent_id, docs_removed)` per pruned agent.
    pub async fn prune_agents_not_in(
        &self,
        keep: &std::collections::HashSet<&str>,
    ) -> Result<Vec<(String, usize)>> {
        // Never issue an unbounded delete. An empty keep-set is only reachable when
        // there are genuinely no agents, but guard it explicitly regardless.
        if keep.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.pool.conn(&self.tenant).await?;
        let orphans: Vec<String> = conn
            .query(
                "SELECT DISTINCT agent_id FROM edge_knowledge WHERE tenant_id = $1",
                &[&self.tenant],
            )
            .await
            .wrap_err("listing knowledge agent ids")?
            .into_iter()
            .map(|r| r.get::<_, String>(0))
            .filter(|id| !keep.contains(id.as_str()))
            .collect();
        conn.commit().await?;
        let mut pruned = Vec::new();
        for agent_id in orphans {
            // Empty doc set → replace_agent_docs deletes every doc + its chunks.
            let (_, _, removed) = self.replace_agent_docs(&agent_id, &[]).await?;
            pruned.push((agent_id, removed));
        }
        Ok(pruned)
    }

    /// Delete a document and its chunks. Chunks go FIRST so a Weaviate failure
    /// leaves the PG row (still listable + retriable-deletable) rather than
    /// orphaning chunks that keep recalling into turns. Returns whether a row
    /// was removed.
    pub async fn delete(&self, agent_id: &str, id: &str) -> Result<bool> {
        self.delete_chunks(&AgentId::new(agent_id), id).await?;
        let conn = self.pool.conn(&self.tenant).await?;
        let n = conn
            .execute(
                "DELETE FROM edge_knowledge WHERE tenant_id = $1 AND id = $2 AND agent_id = $3",
                &[&self.tenant, &id, &agent_id],
            )
            .await?;
        conn.commit().await?;
        if let Some(plan) = &self.plan {
            plan.invalidate();
        }
        Ok(n > 0)
    }

    /// Remove every Weaviate chunk for one document (scoped to the agent's
    /// knowledge — never touches lessons or customer memory).
    async fn delete_chunks(&self, agent: &AgentId, doc_id: &str) -> Result<()> {
        let filter = MemoryFilter {
            source: Some(MemorySource::Curated),
            owner: Some(agent.clone()),
            identity_id: Some(KNOWLEDGE_IDENTITY.to_string()),
            tags: Some(vec![doc_tag(doc_id)]),
            limit: Some(MAX_CHUNKS),
            ..Default::default()
        };
        let chunks: Vec<_> = self
            .memory
            .list(filter)
            .await?
            .into_iter()
            .map(|m| m.id)
            .collect();
        self.memory.purge_memories(&chunks).await?;
        Ok(())
    }
}

/// Every chunk in the tenant's corpus, across its agents: the plan's
/// knowledge counter.
pub(crate) async fn total_chunks(pool: &TenantPool, tenant: &TenantId) -> Result<u64> {
    let conn = pool.conn(tenant).await?;
    let total: i64 = conn
        .query_one(
            "SELECT coalesce(sum(chunks), 0)::bigint FROM edge_knowledge WHERE tenant_id = $1",
            &[tenant],
        )
        .await
        .wrap_err("counting knowledge chunks")?
        .get(0);
    conn.commit().await?;
    Ok(total.max(0) as u64)
}

/// Recall the knowledge chunks most relevant to `query` and render them as a
/// system-prompt block. Best-effort: empty on error/timeout, never fails a turn.
pub async fn recall_knowledge(memory: &dyn MemoryStore, agent: &AgentId, query: &str) -> String {
    let filter = MemoryFilter {
        source: Some(MemorySource::Curated),
        owner: Some(agent.clone()),
        identity_id: Some(KNOWLEDGE_IDENTITY.to_string()),
        limit: Some(RECALL_LIMIT),
        min_certainty: Some(RECALL_MIN_CERTAINTY),
        ..Default::default()
    };
    let hits = match tokio::time::timeout(RECALL_TIMEOUT, memory.recall(query, filter)).await {
        Ok(Ok(hits)) => hits,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "knowledge recall failed");
            return String::new();
        }
        Err(_) => {
            tracing::warn!("knowledge recall timed out");
            return String::new();
        }
    };
    if hits.is_empty() {
        return String::new();
    }
    let mut block = String::from(
        "\n\n## Reference knowledge\n\
         _Authoritative excerpts from this product's knowledge base. Prefer these over your own \
         assumptions; if they don't cover the question, say so rather than guessing._\n",
    );
    for h in hits {
        // Flatten newlines so a chunk can't forge a "## ..." prompt section.
        let flat = h
            .memory
            .content
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        block.push_str("\n- ");
        block.push_str(&flat);
        block.push('\n');
    }
    block
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunking_packs_paragraphs_and_bounds() {
        let doc = "Para one.\n\nPara two.\n\nPara three.";
        let cs = chunk(doc);
        assert_eq!(cs.len(), 1, "small paras pack into one chunk");
        assert!(cs[0].contains("Para one.") && cs[0].contains("Para three."));

        // Paragraphs that overflow the target split into multiple chunks.
        let big = (0..50)
            .map(|i| format!("Paragraph number {i} with some filler text to add length."))
            .collect::<Vec<_>>()
            .join("\n\n");
        let cs = chunk(&big);
        assert!(cs.len() > 1, "large doc splits");
        assert!(cs.iter().all(|c| c.chars().count() <= CHUNK_CHARS + 1));
    }

    #[test]
    fn oversized_single_paragraph_is_hard_split() {
        let huge = "x".repeat(CHUNK_CHARS * 3 + 100);
        let cs = chunk(&huge);
        assert!(cs.len() >= 3);
        assert!(cs.iter().all(|c| c.chars().count() <= CHUNK_CHARS));
    }

    #[test]
    fn chunk_count_is_capped() {
        let many = "p\n\n".repeat(MAX_CHUNKS * 2);
        assert!(chunk(&many).len() <= MAX_CHUNKS);
    }

    #[test]
    fn doc_tag_is_stable() {
        assert_eq!(doc_tag("abc"), "knowdoc:abc");
    }

    /// A document that would take the corpus past the plan's chunk cap is
    /// refused with the plan's error before a row or a chunk is written;
    /// replacing a document counts its old chunks as gone, so an edit of
    /// the same size at the cap still lands.
    #[tokio::test]
    async fn a_document_past_the_chunk_cap_is_refused_and_nothing_is_written() {
        use crate::tenant::{plan_tests, Plan, PlanLimit};
        use workforce_common::session_postgres::PostgresSessionStore;
        use workforce_memory::in_memory::InMemoryStore;
        use workforce_postgres::test_support::with_test_pool;

        with_test_pool(PostgresSessionStore::build_pool, |pool| async move {
            plan_tests::migrate(&pool).await;
            let memory = Arc::new(InMemoryStore::new());
            let plan = plan_tests::plan_usage(
                &pool,
                Plan {
                    knowledge_chunks: Some(2),
                    ..Plan::default()
                },
                memory.clone(),
            );
            let store = KnowledgeStore::new(
                TenantPool::new(pool.clone()),
                TenantId::default_tenant(),
                memory.clone(),
            )
            .with_plan(Arc::clone(&plan));
            let two_chunks = "x".repeat(CHUNK_CHARS + 100);
            assert_eq!(
                store
                    .upsert("support", "faq", "FAQ", &two_chunks)
                    .await
                    .unwrap(),
                2,
                "fills the cap exactly"
            );
            assert_eq!(plan.usage(1).await.unwrap().knowledge_chunks, 2);

            let refused = store
                .upsert("support", "policy", "Policy", "one more chunk")
                .await
                .expect_err("a third chunk is over the cap");
            assert_eq!(
                refused.downcast_ref::<PlanLimit>(),
                Some(&PlanLimit::KnowledgeChunks),
                "{refused:#}"
            );
            let docs = store.list("support").await.unwrap();
            assert_eq!(docs.len(), 1, "no row for the refused document");
            assert_eq!(
                memory.count(MemoryFilter::system_unscoped()).await.unwrap(),
                2,
                "no chunk for it either"
            );

            // The same document again, at the same size: its own chunks are
            // replaced, not added.
            assert_eq!(
                store
                    .upsert("support", "faq", "FAQ v2", &two_chunks)
                    .await
                    .unwrap(),
                2
            );
            assert_eq!(
                memory.count(MemoryFilter::system_unscoped()).await.unwrap(),
                2
            );
            assert!(store.delete("support", "faq").await.unwrap());
            assert_eq!(
                plan.usage(1).await.unwrap().knowledge_chunks,
                0,
                "a delete drops the snapshot too"
            );
            assert_eq!(
                store
                    .upsert("support", "policy", "Policy", "fits now")
                    .await
                    .unwrap(),
                1
            );
        })
        .await;
    }
}
