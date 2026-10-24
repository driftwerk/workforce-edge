//! Edge usage/cost analytics — per-call accounting and subject name resolution.
//!
//! Every LLM call a tenant causes lands in `usage_events` with a
//! [`UsagePurpose`]: the customer turn itself, and the background work the turn
//! sets off (grader, lessons, passive memory, consolidation), which reaches the
//! model through a [`UsageMeter`]-wrapped provider that records each call on
//! the turn's own key. The turn KPIs read turns only; `by_purpose` shows the
//! rest, so a tenant sees what its grader costs beside what its customers do.
//!
//! The store rides the shared session pool but owns a separate `edge_analytics`
//! schema with its own refinery history (so a roll-back here can't touch the
//! `sessions` schema). Because that pool pins `search_path = sessions, public`,
//! every runtime query here schema-qualifies `edge_analytics.<table>`.
//!
//! Recording is best-effort and off the turn's critical path (see
//! [`crate::host`]): a failed insert is logged and never fails the turn.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use eyre::{eyre, Result, WrapErr};
use serde::{Deserialize, Serialize};
use tokio_postgres::types::ToSql;
use workforce_agent::llm::provider::LlmProvider;
use workforce_agent::llm::types::{LlmMessage, LlmResponse, LlmToolDef, TokenUsage};
use workforce_common::cost::CostTracker;
use workforce_common::postgres::{
    MigrationRun, MigrationRunner, MigrationSet, PostgresError, PostgresPool,
};

use crate::tenant::{TenantId, TenantPool};

mod embedded {
    refinery::embed_migrations!("migrations/analytics");
}

/// Schema the analytics store owns; its tables and refinery history live here.
const SCHEMA: &str = "edge_analytics";

/// Apply the embedded `edge_analytics` migration set against a pool. Idempotent
/// — refinery records applied versions and skips them. Mirrors
/// `run_session_migrations`, isolated under its own schema.
pub async fn run_edge_analytics_migrations(pool: &PostgresPool) -> Result<()> {
    let set = MigrationSet::new(
        SCHEMA,
        Box::new(EmbeddedAnalyticsMigrations) as Box<dyn MigrationRun>,
    );
    MigrationRunner::run(pool, &[set])
        .await
        .wrap_err("running edge analytics migrations")?;
    Ok(())
}

struct EmbeddedAnalyticsMigrations;

#[async_trait::async_trait]
impl MigrationRun for EmbeddedAnalyticsMigrations {
    async fn run(&self, client: &mut tokio_postgres::Client) -> Result<(), PostgresError> {
        embedded::migrations::runner()
            .run_async(client)
            .await
            .map_err(PostgresError::from)?;
        Ok(())
    }
}

/// A verified end-customer's display identity, captured from the Keycloak token
/// and carried through the turn for attribution.
#[derive(Debug, Clone, Default)]
pub struct TurnIdentity {
    pub display_name: Option<String>,
    pub email: Option<String>,
}

/// A resolved subject identity, for turning session `owner` keys into names in
/// the admin API.
#[derive(Debug, Clone)]
pub struct Identity {
    pub display_name: Option<String>,
    pub email: Option<String>,
}

/// How a turn ended. Only `Ok`/`MaxRounds` carry real tokens/cost; the failure
/// outcomes carry zeros. `Cancelled` (client disconnect) is kept distinct from
/// `Error`/`Timeout` so the failure-rate KPI counts only genuine failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnOutcome {
    Ok,
    MaxRounds,
    Error,
    Timeout,
    Cancelled,
}

impl TurnOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::MaxRounds => "max_rounds",
            Self::Error => "error",
            Self::Timeout => "timeout",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Which LLM call a `usage_events` row meters. `Turn` is the customer turn;
/// the rest are the background calls it sets off, each on its own row so the
/// admin can show spend by purpose. The grader's remediation — the corrective
/// lesson it extracts from a behaviour-gap verdict — is `Grader`: it runs only
/// because the grader is on, so it is part of the grader's share.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsagePurpose {
    Turn,
    Grader,
    Lessons,
    PassiveMemory,
    Consolidation,
}

impl UsagePurpose {
    /// The column's spelling, matching its check constraint and the serde form.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Turn => "turn",
            Self::Grader => "grader",
            Self::Lessons => "lessons",
            Self::PassiveMemory => "passive_memory",
            Self::Consolidation => "consolidation",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "turn" => Ok(Self::Turn),
            "grader" => Ok(Self::Grader),
            "lessons" => Ok(Self::Lessons),
            "passive_memory" => Ok(Self::PassiveMemory),
            "consolidation" => Ok(Self::Consolidation),
            other => Err(eyre!("unknown usage purpose {other:?}")),
        }
    }
}

/// One automated answer-quality verdict for a turn, ready to persist. `quality`
/// is `good` | `poor` | `incorrect` | `unanswerable`; `source` is how it was
/// derived (`llm` | `outcome` | `feedback`); `model` is the concrete grading
/// model (None for a deterministic grade).
#[derive(Debug, Clone)]
pub struct AnswerGrade {
    pub quality: String,
    pub gap_topic: Option<String>,
    pub remedy: Option<String>,
    pub reason: Option<String>,
    pub source: String,
    pub model: Option<String>,
    /// The (truncated) customer question this grade is for, so the gaps export
    /// can show the actual failing questions. `None` for outcome-only grades.
    pub question: Option<String>,
}

/// A process's claim on one in-flight turn, for the cross-process registry.
#[derive(Debug, Clone, Copy)]
pub struct TurnClaim<'a> {
    /// Owner + conversation, the same identity the in-process turn map uses.
    pub turn_key: &'a str,
    pub conversation: &'a str,
    pub owner: &'a str,
    pub agent_id: &'a str,
    /// The claiming process — compared against the reader's own id to tell
    /// "mine" from "running elsewhere".
    pub replica: &'a str,
}

/// One answer's tool evidence, for the audit store.
#[derive(Debug, Clone, Copy)]
pub struct ToolEvidence<'a> {
    pub message_id: &'a str,
    pub conversation: &'a str,
    pub owner: &'a str,
    pub agent_id: &'a str,
    pub org: Option<&'a str>,
    /// Already redacted and bounded by the caller — this store never sees a raw
    /// tool payload.
    pub evidence: &'a str,
}

/// Who is running a conversation's turn right now, per the registry.
#[derive(Debug, Clone)]
pub struct TurnOwner {
    pub replica: String,
    pub started_at: DateTime<Utc>,
}

/// One LLM call's usage row before cost is computed — a terminal turn, or one
/// background call made on its behalf (see [`UsagePurpose`]). Token buckets are
/// the raw counts (stored as-is so cost is always recomputable); a non-success
/// call carries zeros.
#[derive(Debug, Clone)]
pub struct UsageEvent {
    pub purpose: UsagePurpose,
    pub agent_id: String,
    pub owner: String,
    pub conversation: String,
    pub message_id: Option<String>,
    pub model: String,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cache_read_tokens: u32,
    pub cache_write_tokens: u32,
    pub rounds: u32,
    pub tool_count: u32,
    pub tool_failures: u32,
    pub duration_ms: u64,
    pub outcome: TurnOutcome,
}

/// The window + optional equality filters shared by every read/aggregation
/// query. `from`/`to` bound the window (`ts >= from AND ts < to`); `agent`/
/// `owner` are optional equality filters. Every value is bound as a query
/// parameter — a filter value is NEVER interpolated into the SQL text.
#[derive(Debug, Clone)]
pub struct AnalyticsQuery {
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    pub agent: Option<String>,
    pub owner: Option<String>,
}

impl AnalyticsQuery {
    /// Push the shared filter binds (tenant, window, then optional agent/owner)
    /// into `binds` and return the matching `WHERE` fragment over the aliased
    /// `u` table. `$N` are computed from the running `binds.len()`, so a caller
    /// may pre-seed or append further binds; the borrows outlive the query
    /// because they point into `self` and the store's tenant. Values are ALWAYS
    /// bound — never interpolated.
    fn where_clause<'a>(
        &'a self,
        tenant: &'a TenantId,
        binds: &mut Vec<&'a (dyn ToSql + Sync)>,
    ) -> String {
        let mut parts: Vec<String> = Vec::new();
        binds.push(tenant);
        parts.push(format!("u.tenant_id = ${}", binds.len()));
        binds.push(&self.from);
        parts.push(format!("u.ts >= ${}", binds.len()));
        binds.push(&self.to);
        parts.push(format!("u.ts < ${}", binds.len()));
        if let Some(agent) = &self.agent {
            binds.push(agent);
            parts.push(format!("u.agent_id = ${}", binds.len()));
        }
        if let Some(owner) = &self.owner {
            binds.push(owner);
            parts.push(format!("u.owner = ${}", binds.len()));
        }
        parts.join(" AND ")
    }
}

/// The GROUP-BY dimension for [`EdgeAnalyticsStore::by_dimension`], chosen from a
/// FIXED set so no user input ever reaches the SQL column list — an unknown
/// value is rejected (a 400) at the handler before the store is touched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dimension {
    User,
    Agent,
    Model,
    Outcome,
}

impl Dimension {
    /// Parse the path segment; `None` for anything outside the fixed set.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "user" => Some(Self::User),
            "agent" => Some(Self::Agent),
            "model" => Some(Self::Model),
            "outcome" => Some(Self::Outcome),
            _ => None,
        }
    }

    /// The `(from, key_expr, label_expr, group_by)` SQL fragments. All are
    /// `&'static` — derived from this fixed enum, never user input — so they are
    /// safe to embed directly in the query text.
    fn sql(self) -> (&'static str, &'static str, &'static str, &'static str) {
        match self {
            // `user` resolves the display name via a left join, falling back to
            // the `sub` portion of the owner key (after the U+001F separator).
            Self::User => (
                "edge_analytics.usage_events u \
                 LEFT JOIN edge_analytics.chat_identities ci \
                   ON ci.tenant_id = u.tenant_id AND ci.owner = u.owner",
                "u.owner",
                "COALESCE(ci.display_name, split_part(u.owner, chr(31), 2))",
                "u.owner, ci.display_name",
            ),
            Self::Agent => (
                "edge_analytics.usage_events u",
                "u.agent_id",
                "u.agent_id",
                "u.agent_id",
            ),
            Self::Model => (
                "edge_analytics.usage_events u",
                "u.model",
                "u.model",
                "u.model",
            ),
            Self::Outcome => (
                "edge_analytics.usage_events u",
                "u.outcome",
                "u.outcome",
                "u.outcome",
            ),
        }
    }
}

/// The time granularity for [`EdgeAnalyticsStore::timeseries`]. Fixed set → the
/// `date_trunc` unit and `generate_series` step are `&'static` constants, never
/// user input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeBucket {
    Day,
    Hour,
}

impl TimeBucket {
    /// Parse the `bucket` query param; `None` for anything outside the fixed set.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "day" => Some(Self::Day),
            "hour" => Some(Self::Hour),
            _ => None,
        }
    }

    /// `(date_trunc unit, generate_series step interval)` — both `&'static`.
    fn parts(self) -> (&'static str, &'static str) {
        match self {
            Self::Day => ("day", "1 day"),
            Self::Hour => ("hour", "1 hour"),
        }
    }

    /// The bucket width — used to bound the generated-series size before a
    /// timeseries query runs, so a wide window at a fine bucket can't ask
    /// Postgres for millions of rows.
    pub fn step(self) -> chrono::Duration {
        match self {
            Self::Day => chrono::Duration::days(1),
            Self::Hour => chrono::Duration::hours(1),
        }
    }
}

/// KPI roll-up over the filtered window. Every field but `by_purpose` counts
/// customer turns alone; `by_purpose` is every call in the window, one row per
/// purpose that spent anything.
#[derive(Debug, Clone, Serialize)]
pub struct AnalyticsSummary {
    pub turns: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub total_cost_usd: f64,
    pub unique_users: i64,
    /// Share of turns whose outcome is `error`/`timeout` over ALL turns;
    /// `cancelled` (a client disconnect) is excluded from the numerator.
    pub failure_rate: f64,
    pub avg_duration_ms: f64,
    pub by_purpose: Vec<PurposeRow>,
}

/// Spend and tokens for one [`UsagePurpose`] over the window. `calls` is the
/// number of rows: turns for `turn`, model calls for the rest.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PurposeRow {
    pub purpose: UsagePurpose,
    pub cost_usd: f64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub calls: i64,
}

/// One time bucket of the zero-filled series (RFC3339 `ts`, oldest first).
#[derive(Debug, Clone, Serialize)]
pub struct TimeseriesBucket {
    pub ts: String,
    pub turns: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cost_usd: f64,
}

/// One grouped row of a `by/{dimension}` breakdown. `tokens` is input+output.
#[derive(Debug, Clone, Serialize)]
pub struct DimensionRow {
    pub key: String,
    pub label: String,
    pub turns: i64,
    pub tokens: i64,
    pub cost_usd: f64,
}

/// One row of the per-turn drill-down.
#[derive(Debug, Clone, Serialize)]
pub struct TurnRow {
    pub id: i64,
    pub ts: String,
    pub owner: String,
    pub display_name: String,
    pub agent_id: String,
    pub conversation: String,
    pub model: String,
    pub input_tokens: i32,
    pub output_tokens: i32,
    pub cost_usd: f64,
    pub rounds: i32,
    pub tool_count: i32,
    pub tool_failures: i32,
    pub duration_ms: i64,
    pub outcome: String,
}

/// The keyset cursor for the turns drill-down: the `(ts, id)` of the last row a
/// page returned. Opaque to clients (the handler base64-encodes it). Reused
/// verbatim for the feedback drill-down (same `(ts DESC, id DESC)` keyset).
#[derive(Debug, Clone)]
pub struct TurnsCursor {
    pub ts: DateTime<Utc>,
    pub id: i64,
}

/// A page of the turns drill-down plus the cursor for the next page (`None` when
/// the page is the last).
#[derive(Debug, Clone)]
pub struct TurnsResult {
    pub turns: Vec<TurnRow>,
    pub next: Option<TurnsCursor>,
}

fn row_to_turn(r: &tokio_postgres::Row) -> TurnRow {
    TurnRow {
        id: r.get(0),
        ts: r.get::<_, DateTime<Utc>>(1).to_rfc3339(),
        owner: r.get(2),
        display_name: r.get(3),
        agent_id: r.get(4),
        conversation: r.get(5),
        model: r.get(6),
        input_tokens: r.get(7),
        output_tokens: r.get(8),
        cost_usd: r.get(9),
        rounds: r.get(10),
        tool_count: r.get(11),
        tool_failures: r.get(12),
        duration_ms: r.get(13),
        outcome: r.get(14),
    }
}

/// One row of the operator feedback drill-down. `rating` is `+1`/`-1`;
/// `display_name` resolves via the same `chat_identities` left join + `sub`
/// fallback as the turns view.
#[derive(Debug, Clone, Serialize)]
pub struct FeedbackRow {
    pub id: i64,
    pub ts: String,
    pub owner: String,
    pub display_name: String,
    pub agent_id: String,
    pub conversation: String,
    pub message_id: String,
    pub rating: i16,
    pub comment: Option<String>,
}

/// A page of the feedback drill-down plus the cursor for the next page (`None`
/// when the page is the last). Reuses [`TurnsCursor`] — the keyset is identical.
#[derive(Debug, Clone)]
pub struct FeedbackPage {
    pub feedback: Vec<FeedbackRow>,
    pub next: Option<TurnsCursor>,
}

fn row_to_feedback(r: &tokio_postgres::Row) -> FeedbackRow {
    FeedbackRow {
        id: r.get(0),
        ts: r.get::<_, DateTime<Utc>>(1).to_rfc3339(),
        owner: r.get(2),
        display_name: r.get(3),
        agent_id: r.get(4),
        conversation: r.get(5),
        message_id: r.get(6),
        rating: r.get(7),
        comment: r.get(8),
    }
}

/// Answer-quality distribution over the filtered window — the grader KPIs.
#[derive(Debug, Clone, Serialize)]
pub struct GradesSummary {
    pub graded: i64,
    pub good: i64,
    pub poor: i64,
    pub incorrect: i64,
    pub unanswerable: i64,
}

/// One clustered gap: a `gap_topic` the agent answered badly, with how often and
/// how it split. `remedy`/`example_reason` are the most-common / most-recent
/// grader outputs — UNTRUSTED (LLM-derived from customer/agent text), so the UI
/// must render them as plain text, never markdown/HTML.
#[derive(Debug, Clone, Serialize)]
pub struct GapRow {
    pub topic: String,
    pub count: i64,
    pub unanswerable: i64,
    pub incorrect: i64,
    pub remedy: Option<String>,
    pub example_reason: Option<String>,
    pub last_seen: String,
    /// A few recent failing questions for this topic — what to write docs for.
    /// UNTRUSTED customer text; render as plain text only.
    pub example_questions: Vec<String>,
}

/// One row of the per-grade drill-down. `conversation`/`message_id` let the UI
/// deep-link to the exact transcript (as the feedback view does). `gap_topic`/
/// `reason` are UNTRUSTED grader output — render as plain text.
#[derive(Debug, Clone, Serialize)]
pub struct GradeRow {
    pub id: i64,
    pub ts: String,
    pub owner: String,
    pub display_name: String,
    pub agent_id: String,
    pub conversation: String,
    pub message_id: String,
    pub quality: String,
    pub gap_topic: Option<String>,
    pub remedy: Option<String>,
    pub reason: Option<String>,
}

/// A page of the grade drill-down plus the next-page cursor (`None` on the last
/// page). Reuses [`TurnsCursor`] — the `(ts, id)` keyset is identical.
#[derive(Debug, Clone)]
pub struct GradesPage {
    pub grades: Vec<GradeRow>,
    pub next: Option<TurnsCursor>,
}

fn row_to_grade(r: &tokio_postgres::Row) -> GradeRow {
    GradeRow {
        id: r.get(0),
        ts: r.get::<_, DateTime<Utc>>(1).to_rfc3339(),
        owner: r.get(2),
        display_name: r.get(3),
        agent_id: r.get(4),
        conversation: r.get(5),
        message_id: r.get(6),
        quality: r.get(7),
        gap_topic: r.get(8),
        remedy: r.get(9),
        reason: r.get(10),
    }
}

/// The predicate the turn KPIs append to the shared `WHERE`: background rows
/// share the table but are not turns. A literal of this fixed enum, never
/// user input.
const TURNS_ONLY: &str = " AND u.purpose = 'turn'";

/// A period's turn and user counts, as the plan meters them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PeriodUsage {
    pub turns: u64,
    pub users: u64,
}

/// Per-call usage/cost accounting and subject name resolution, on the edge's
/// own `edge_analytics` schema. Bound to one tenant: every query runs on a
/// connection scoped to it and names it, so no row crosses tenants.
pub struct EdgeAnalyticsStore {
    pool: TenantPool,
    tenant: TenantId,
    cost: CostTracker,
}

impl EdgeAnalyticsStore {
    /// Build a store on an already-migrated pool, with the embedded default
    /// pricing map — the same TOML the hub loads. `CostTracker::new` alone
    /// prices only a three-model minimal fallback, which is what left
    /// `claude-sonnet-5` warning as unpriced on every turn.
    pub fn new(pool: TenantPool, tenant: TenantId) -> Self {
        let cost = CostTracker::new();
        cost.load_pricing_toml(workforce_agent::llm::DEFAULT_PRICING_TOML, None);
        Self { pool, tenant, cost }
    }

    /// Insert one `usage_events` row, computing cost from the normalized model
    /// id. A model with no pricing entry logs once (never per call) and falls to
    /// the Sonnet-tier default — the raw tokens keep the row recomputable. The
    /// one cost formula for every purpose: a background call is priced exactly
    /// as the turn that caused it.
    ///
    /// `org` is `NULL` today: for Harbor, org is a property of the *view*
    /// (org-specific vs cross-org, absent for superadmins), not the token, so it
    /// is attributed from the host-screen context in a later phase — a stale
    /// issuer bucket would be worse than an honest `NULL`.
    pub async fn record_usage(&self, ev: &UsageEvent, org: Option<&str>) -> Result<()> {
        let normalized = normalize_model_id(&ev.model);
        if !self.cost.is_priced(&normalized) {
            warn_unpriced(&normalized);
        }
        let cost = self.cost.calculate_cost(
            &normalized,
            ev.input_tokens as u64,
            ev.output_tokens as u64,
            ev.cache_write_tokens as u64,
            ev.cache_read_tokens as u64,
        );
        let conn = self
            .pool
            .conn(&self.tenant)
            .await
            .wrap_err("analytics pool")?;
        // `$15::float8::numeric` pins the bound param to float8 so tokio-postgres
        // (which has no NUMERIC ToSql) serializes the f64 and the server casts.
        conn.execute(
            "INSERT INTO edge_analytics.usage_events \
             (tenant_id, agent_id, owner, conversation, message_id, model, input_tokens, \
              output_tokens, cache_read_tokens, cache_write_tokens, rounds, tool_count, \
              tool_failures, duration_ms, cost_usd, outcome, org, purpose) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15::float8::numeric(12,6),$16,$17,$18)",
            &[
                &self.tenant,
                &ev.agent_id,
                &ev.owner,
                &ev.conversation,
                &ev.message_id,
                &ev.model,
                &(ev.input_tokens as i32),
                &(ev.output_tokens as i32),
                &(ev.cache_read_tokens as i32),
                &(ev.cache_write_tokens as i32),
                &(ev.rounds as i32),
                &(ev.tool_count as i32),
                &(ev.tool_failures as i32),
                &(ev.duration_ms as i64),
                &cost,
                &ev.outcome.as_str(),
                &org,
                &ev.purpose.as_str(),
            ],
        )
        .await
        .wrap_err("inserting usage_event")?;
        conn.commit().await
    }

    /// Keep the tool evidence behind one answer, so its grounding can be checked
    /// after the turn has ended.
    ///
    /// Upsert on `message_id`: a re-graded or re-recorded turn refreshes its
    /// evidence rather than colliding.
    pub async fn record_tool_evidence(&self, ev: &ToolEvidence<'_>) -> Result<()> {
        let conn = self
            .pool
            .conn(&self.tenant)
            .await
            .wrap_err("analytics pool")?;
        conn.execute(
            "INSERT INTO edge_analytics.turn_tool_evidence \
             (tenant_id, message_id, conversation, owner, agent_id, org, evidence) \
             VALUES ($1,$2,$3,$4,$5,$6,$7) \
             ON CONFLICT (tenant_id, message_id) DO UPDATE SET \
               evidence = EXCLUDED.evidence, ts = now()",
            &[
                &self.tenant,
                &ev.message_id,
                &ev.conversation,
                &ev.owner,
                &ev.agent_id,
                &ev.org,
                &ev.evidence,
            ],
        )
        .await
        .wrap_err("recording tool evidence")?;
        conn.commit().await
    }

    /// The evidence behind one answer, or `None` once it has been swept.
    pub async fn tool_evidence(&self, message_id: &str) -> Result<Option<String>> {
        let conn = self
            .pool
            .conn(&self.tenant)
            .await
            .wrap_err("analytics pool")?;
        let row = conn
            .query_opt(
                "SELECT evidence FROM edge_analytics.turn_tool_evidence \
                 WHERE tenant_id = $1 AND message_id = $2",
                &[&self.tenant, &message_id],
            )
            .await
            .wrap_err("reading tool evidence")?;
        conn.commit().await?;
        Ok(row.map(|r| r.get(0)))
    }

    /// Drop evidence past its retention. Returns how many rows went, so a sweep
    /// that keeps finding work is visible rather than silent.
    pub async fn sweep_tool_evidence(&self, max_age: Duration) -> Result<u64> {
        let conn = self
            .pool
            .conn(&self.tenant)
            .await
            .wrap_err("analytics pool")?;
        let secs = max_age.as_secs_f64();
        let swept = conn
            .execute(
                "DELETE FROM edge_analytics.turn_tool_evidence \
                 WHERE tenant_id = $1 AND ts < now() - ($2::float8 * interval '1 second')",
                &[&self.tenant, &secs],
            )
            .await
            .wrap_err("sweeping tool evidence")?;
        conn.commit().await?;
        Ok(swept)
    }

    /// Claim `turn_key` for this process in the cross-process turn registry.
    ///
    /// Upsert, not insert: the in-process map is keyed by the same `turn_key` and
    /// a new turn replaces the previous one there, so the registry must follow the
    /// same "one live turn per conversation" rule or a stale row would outlive the
    /// turn it describes.
    pub async fn claim_turn(&self, claim: &TurnClaim<'_>) -> Result<()> {
        let conn = self
            .pool
            .conn(&self.tenant)
            .await
            .wrap_err("analytics pool")?;
        conn.execute(
            "INSERT INTO edge_analytics.active_turns \
             (tenant_id, turn_key, conversation, owner, agent_id, replica, started_at) \
             VALUES ($1,$2,$3,$4,$5,$6, now()) \
             ON CONFLICT (tenant_id, turn_key) DO UPDATE SET \
               conversation = EXCLUDED.conversation, owner = EXCLUDED.owner, \
               agent_id = EXCLUDED.agent_id, replica = EXCLUDED.replica, \
               started_at = now(), cancel_requested_at = NULL",
            &[
                &self.tenant,
                &claim.turn_key,
                &claim.conversation,
                &claim.owner,
                &claim.agent_id,
                &claim.replica,
            ],
        )
        .await
        .wrap_err("claiming active turn")?;
        conn.commit().await
    }

    /// Release this process's claim on `turn_key`.
    ///
    /// Guarded on `replica` so a turn superseded by another process cannot have
    /// its successor's claim deleted by the loser's late cleanup — without it, an
    /// interrupt racing across replicas would leave the live turn unregistered.
    pub async fn release_turn(&self, turn_key: &str, replica: &str) -> Result<()> {
        let conn = self
            .pool
            .conn(&self.tenant)
            .await
            .wrap_err("analytics pool")?;
        conn.execute(
            "DELETE FROM edge_analytics.active_turns \
             WHERE tenant_id = $1 AND turn_key = $2 AND replica = $3",
            &[&self.tenant, &turn_key, &replica],
        )
        .await
        .wrap_err("releasing active turn")?;
        conn.commit().await
    }

    /// Ask whoever owns `turn_key`'s turn to stop it.
    ///
    /// A no-op when nothing is registered — the caller has already established
    /// that no local turn exists, so an absent row means there is nothing to
    /// interrupt. Returns whether a live claim was marked, so the caller knows
    /// whether it has a handover to wait for.
    pub async fn request_turn_cancel(&self, turn_key: &str, max_age: Duration) -> Result<bool> {
        let conn = self
            .pool
            .conn(&self.tenant)
            .await
            .wrap_err("analytics pool")?;
        let secs = max_age.as_secs_f64();
        let marked = conn
            .execute(
                "UPDATE edge_analytics.active_turns SET cancel_requested_at = now() \
                 WHERE tenant_id = $1 AND turn_key = $2 \
                   AND started_at > now() - ($3::float8 * interval '1 second')",
                &[&self.tenant, &turn_key, &secs],
            )
            .await
            .wrap_err("requesting turn cancel")?;
        conn.commit().await?;
        Ok(marked > 0)
    }

    /// Whether this process's turn for `turn_key` has been asked to stop.
    ///
    /// Scoped to `replica` so a request aimed at a turn that has since been
    /// superseded cannot cancel its successor.
    pub async fn cancel_requested(&self, turn_key: &str, replica: &str) -> Result<bool> {
        let conn = self
            .pool
            .conn(&self.tenant)
            .await
            .wrap_err("analytics pool")?;
        let row = conn
            .query_opt(
                "SELECT 1 FROM edge_analytics.active_turns \
                 WHERE tenant_id = $1 AND turn_key = $2 AND replica = $3 \
                   AND cancel_requested_at IS NOT NULL",
                &[&self.tenant, &turn_key, &replica],
            )
            .await
            .wrap_err("reading turn cancel request")?;
        conn.commit().await?;
        Ok(row.is_some())
    }

    /// Which replica, if any, is currently running a turn for `turn_key`.
    ///
    /// Rows older than `max_age` are treated as absent: a replica killed mid-turn
    /// never releases its claim, and a stale row must never make a conversation
    /// look permanently busy.
    pub async fn turn_owner(&self, turn_key: &str, max_age: Duration) -> Result<Option<TurnOwner>> {
        let conn = self
            .pool
            .conn(&self.tenant)
            .await
            .wrap_err("analytics pool")?;
        let secs = max_age.as_secs_f64();
        let row = conn
            .query_opt(
                "SELECT replica, started_at FROM edge_analytics.active_turns \
                 WHERE tenant_id = $1 AND turn_key = $2 \
                   AND started_at > now() - ($3::float8 * interval '1 second')",
                &[&self.tenant, &turn_key, &secs],
            )
            .await
            .wrap_err("reading active turn")?;
        conn.commit().await?;
        Ok(row.map(|r| TurnOwner {
            replica: r.get(0),
            started_at: r.get(1),
        }))
    }

    /// Drop claims older than `max_age` — orphans left by a process that died
    /// mid-turn. Returns how many were removed, so a sweep that keeps finding
    /// work is visible rather than silent.
    pub async fn sweep_stale_turns(&self, max_age: Duration) -> Result<u64> {
        let conn = self
            .pool
            .conn(&self.tenant)
            .await
            .wrap_err("analytics pool")?;
        let secs = max_age.as_secs_f64();
        let swept = conn
            .execute(
                "DELETE FROM edge_analytics.active_turns \
                 WHERE tenant_id = $1 AND started_at < now() - ($2::float8 * interval '1 second')",
                &[&self.tenant, &secs],
            )
            .await
            .wrap_err("sweeping stale active turns")?;
        conn.commit().await?;
        Ok(swept)
    }

    /// Upsert the subject's identity: insert on first sight, refresh
    /// name/email/org and `last_seen` thereafter. Each field `COALESCE`s so a
    /// later token that omits a claim keeps the previously-captured value rather
    /// than wiping it; a present claim still overwrites. `issuer` is retained in
    /// its own column; `org` is `None` in Phase 1 (attributed from view context
    /// later).
    pub async fn upsert_identity(
        &self,
        owner: &str,
        issuer: &str,
        identity: &TurnIdentity,
        org: Option<&str>,
    ) -> Result<()> {
        let conn = self
            .pool
            .conn(&self.tenant)
            .await
            .wrap_err("analytics pool")?;
        conn.execute(
            "INSERT INTO edge_analytics.chat_identities \
             (tenant_id, owner, display_name, email, org, issuer, first_seen, last_seen) \
             VALUES ($1,$2,$3,$4,$5,$6, now(), now()) \
             ON CONFLICT (tenant_id, owner) DO UPDATE SET \
                 display_name = COALESCE(EXCLUDED.display_name, chat_identities.display_name), \
                 email = COALESCE(EXCLUDED.email, chat_identities.email), \
                 org = COALESCE(EXCLUDED.org, chat_identities.org), \
                 last_seen = now()",
            &[
                &self.tenant,
                &owner,
                &identity.display_name,
                &identity.email,
                &org,
                &issuer,
            ],
        )
        .await
        .wrap_err("upserting chat_identity")?;
        conn.commit().await
    }

    /// Resolve a batch of session `owner` keys to their captured identities in
    /// one query. Unknown owners are simply absent from the map.
    pub async fn resolve_identities(&self, owners: &[String]) -> Result<HashMap<String, Identity>> {
        if owners.is_empty() {
            return Ok(HashMap::new());
        }
        let conn = self
            .pool
            .conn(&self.tenant)
            .await
            .wrap_err("analytics pool")?;
        let rows = conn
            .query(
                "SELECT owner, display_name, email FROM edge_analytics.chat_identities \
                 WHERE tenant_id = $1 AND owner = ANY($2)",
                &[&self.tenant, &owners],
            )
            .await
            .wrap_err("resolving identities")?;
        conn.commit().await?;
        Ok(rows
            .iter()
            .map(|r| {
                (
                    r.get::<_, String>(0),
                    Identity {
                        display_name: r.get(1),
                        email: r.get(2),
                    },
                )
            })
            .collect())
    }

    /// Every agent holding anything about `owner`: a conversation, a turn, a
    /// rating, a grade or evidence. Rows outlive a removed agent, so this
    /// reaches agents the install no longer hosts.
    pub async fn agents_holding(&self, owner: &str) -> Result<Vec<String>> {
        let conn = self
            .pool
            .conn(&self.tenant)
            .await
            .wrap_err("analytics pool")?;
        let rows = conn
            .query(
                "SELECT agent_id FROM sessions.sessions \
                   WHERE tenant_id = $2 AND owner = $1 \
                 UNION SELECT agent_id FROM edge_analytics.usage_events \
                   WHERE tenant_id = $2 AND owner = $1 \
                 UNION SELECT agent_id FROM edge_analytics.feedback \
                   WHERE tenant_id = $2 AND owner = $1 \
                 UNION SELECT agent_id FROM edge_analytics.answer_grades \
                   WHERE tenant_id = $2 AND owner = $1 \
                 UNION SELECT agent_id FROM edge_analytics.turn_tool_evidence \
                   WHERE tenant_id = $2 AND owner = $1",
                &[&owner, &self.tenant],
            )
            .await
            .wrap_err("finding the agents holding an owner")?;
        conn.commit().await?;
        Ok(rows.iter().map(|r| r.get(0)).collect())
    }

    /// Whether `owner` had a conversation saved, or a turn claimed, in the last
    /// `window`, on any agent and any replica.
    pub async fn active_within(&self, owner: &str, window: Duration) -> Result<bool> {
        let conn = self
            .pool
            .conn(&self.tenant)
            .await
            .wrap_err("analytics pool")?;
        let secs = window.as_secs_f64();
        let row = conn
            .query_one(
                "SELECT EXISTS (SELECT 1 FROM sessions.sessions \
                     WHERE tenant_id = $3 AND owner = $1 \
                     AND updated_at > now() - ($2::float8 * interval '1 second')) \
                 OR EXISTS (SELECT 1 FROM edge_analytics.active_turns \
                     WHERE tenant_id = $3 AND owner = $1 \
                     AND started_at > now() - ($2::float8 * interval '1 second'))",
                &[&owner, &secs, &self.tenant],
            )
            .await
            .wrap_err("reading an owner's recent activity")?;
        conn.commit().await?;
        Ok(row.get(0))
    }

    /// The plan's monthly counters: customer turns recorded since `since`,
    /// and the users seen since then. Every other purpose is a background
    /// call the turn caused, not a turn of its own, so it is not counted.
    pub async fn usage_since(&self, since: DateTime<Utc>) -> Result<PeriodUsage> {
        let conn = self
            .pool
            .conn(&self.tenant)
            .await
            .wrap_err("analytics pool")?;
        let row = conn
            .query_one(
                "SELECT (SELECT count(*) FROM edge_analytics.usage_events \
                     WHERE tenant_id = $1 AND purpose = 'turn' AND ts >= $2), \
                        (SELECT count(*) FROM edge_analytics.chat_identities \
                     WHERE tenant_id = $1 AND last_seen >= $2)",
                &[&self.tenant, &since],
            )
            .await
            .wrap_err("counting the period's turns and users")?;
        conn.commit().await?;
        Ok(PeriodUsage {
            turns: row.get::<_, i64>(0) as u64,
            users: row.get::<_, i64>(1) as u64,
        })
    }

    /// Whether `owner` has been seen since `since` — whether they are one
    /// of the period's users already, or would be a new one.
    pub async fn seen_since(&self, owner: &str, since: DateTime<Utc>) -> Result<bool> {
        let conn = self
            .pool
            .conn(&self.tenant)
            .await
            .wrap_err("analytics pool")?;
        let row = conn
            .query_one(
                "SELECT EXISTS (SELECT 1 FROM edge_analytics.chat_identities \
                     WHERE tenant_id = $1 AND owner = $2 AND last_seen >= $3)",
                &[&self.tenant, &owner, &since],
            )
            .await
            .wrap_err("reading whether an owner was seen in the period")?;
        conn.commit().await?;
        Ok(row.get(0))
    }

    /// Erase what the analytics hold about `owner`, in one transaction: their
    /// name and email and the tool evidence behind their answers go, and the
    /// text of their ratings and grades is cleared. Turns, ratings and grades
    /// stay counted under a fresh key that names no one, so totals and
    /// per-user counts survive while nothing links them back. Turn claims are
    /// left to their sweep: one live now is a turn that started during the
    /// erasure, which must stay visible to report it. Returns the rows
    /// touched.
    pub async fn forget_owner(&self, owner: &str) -> Result<u64> {
        let key = format!("forgotten:{}", uuid::Uuid::new_v4());
        let conn = self
            .pool
            .conn(&self.tenant)
            .await
            .wrap_err("analytics pool")?;
        let erased: &[&(dyn ToSql + Sync)] = &[&self.tenant, &owner];
        let unlinked: &[&(dyn ToSql + Sync)] = &[&self.tenant, &owner, &key];
        let mut touched = 0;
        for (sql, params) in [
            (
                "DELETE FROM edge_analytics.chat_identities \
                 WHERE tenant_id = $1 AND owner = $2",
                erased,
            ),
            (
                "DELETE FROM edge_analytics.turn_tool_evidence \
                 WHERE tenant_id = $1 AND owner = $2",
                erased,
            ),
            (
                "UPDATE edge_analytics.feedback SET owner = $3, comment = NULL \
                 WHERE tenant_id = $1 AND owner = $2",
                unlinked,
            ),
            (
                "UPDATE edge_analytics.answer_grades \
                 SET owner = $3, question = NULL, reason = NULL, gap_topic = NULL \
                 WHERE tenant_id = $1 AND owner = $2",
                unlinked,
            ),
            (
                "UPDATE edge_analytics.usage_events SET owner = $3 \
                 WHERE tenant_id = $1 AND owner = $2",
                unlinked,
            ),
        ] {
            touched += conn
                .execute(sql, params)
                .await
                .wrap_err("forgetting an owner")?;
        }
        conn.commit()
            .await
            .wrap_err("committing an owner's erasure")?;
        Ok(touched)
    }

    /// KPI roll-up over the filtered window. `failure_rate` is the share of
    /// turns whose outcome is `error`/`timeout` over ALL turns — `cancelled` (a
    /// client disconnect) is deliberately kept out of the numerator while it
    /// still counts toward the denominator. `avg_duration_ms` averages only over
    /// turns that actually ran (`ok`/`max_rounds`); non-success turns carry a 0ms
    /// duration that would otherwise understate real latency. The KPIs are over
    /// customer turns only; `by_purpose` breaks every call in the window down.
    pub async fn summary(&self, q: &AnalyticsQuery) -> Result<AnalyticsSummary> {
        let mut binds: Vec<&(dyn ToSql + Sync)> = Vec::new();
        let where_sql = q.where_clause(&self.tenant, &mut binds);
        let sql = format!(
            "SELECT count(*)::bigint, \
                    COALESCE(sum(u.input_tokens), 0)::bigint, \
                    COALESCE(sum(u.output_tokens), 0)::bigint, \
                    COALESCE(sum(u.cache_read_tokens), 0)::bigint, \
                    COALESCE(sum(u.cache_write_tokens), 0)::bigint, \
                    COALESCE(sum(u.cost_usd), 0)::float8, \
                    count(DISTINCT u.owner)::bigint, \
                    COALESCE(count(*) FILTER (WHERE u.outcome IN ('error', 'timeout'))::float8 \
                             / NULLIF(count(*), 0), 0)::float8, \
                    COALESCE(avg(u.duration_ms) FILTER (WHERE u.outcome IN ('ok', 'max_rounds')), 0)::float8 \
             FROM edge_analytics.usage_events u WHERE {where_sql}{TURNS_ONLY}"
        );
        let by_purpose_sql = format!(
            "SELECT u.purpose, \
                    COALESCE(sum(u.cost_usd), 0)::float8, \
                    COALESCE(sum(u.input_tokens), 0)::bigint, \
                    COALESCE(sum(u.output_tokens), 0)::bigint, \
                    count(*)::bigint \
             FROM edge_analytics.usage_events u WHERE {where_sql} \
             GROUP BY u.purpose \
             ORDER BY sum(u.cost_usd) DESC, u.purpose ASC"
        );
        let conn = self
            .pool
            .conn(&self.tenant)
            .await
            .wrap_err("analytics pool")?;
        let row = conn
            .query_one(&sql, &binds)
            .await
            .wrap_err("querying analytics summary")?;
        let by_purpose = conn
            .query(&by_purpose_sql, &binds)
            .await
            .wrap_err("querying analytics by purpose")?
            .iter()
            .map(|r| {
                Ok(PurposeRow {
                    purpose: UsagePurpose::parse(r.get(0))?,
                    cost_usd: r.get(1),
                    input_tokens: r.get(2),
                    output_tokens: r.get(3),
                    calls: r.get(4),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        conn.commit().await?;
        Ok(AnalyticsSummary {
            turns: row.get(0),
            input_tokens: row.get(1),
            output_tokens: row.get(2),
            cache_read_tokens: row.get(3),
            cache_write_tokens: row.get(4),
            total_cost_usd: row.get(5),
            unique_users: row.get(6),
            failure_rate: row.get(7),
            avg_duration_ms: row.get(8),
            by_purpose,
        })
    }

    /// Zero-filled per-bucket totals across the window, oldest first. Empty
    /// buckets are generated (so a chart has no gaps) by left-joining the grouped
    /// rows to a `generate_series` over the window.
    pub async fn timeseries(
        &self,
        q: &AnalyticsQuery,
        bucket: TimeBucket,
    ) -> Result<Vec<TimeseriesBucket>> {
        let mut binds: Vec<&(dyn ToSql + Sync)> = Vec::new();
        let where_sql = q.where_clause(&self.tenant, &mut binds);
        let (unit, step) = bucket.parts();
        // Truncate in UTC explicitly (the `AT TIME ZONE 'UTC'` sandwich) so
        // buckets are stable regardless of the server session timezone — a
        // cost/usage dashboard reports UTC days/hours, not the DB host's locale.
        // `$2`/`$3` are the window bounds `where_clause` bound after the tenant
        // (from, then to). The upper bound truncates `to - 1µs` so a
        // bucket-aligned `to` can't emit a phantom trailing bucket the half-open
        // `ts < to` filter never populates (and an empty/inverted window gives
        // start > stop → no rows).
        let sql = format!(
            "SELECT gs AS bucket, \
                    COALESCE(g.turns, 0)::bigint, \
                    COALESCE(g.input_tokens, 0)::bigint, \
                    COALESCE(g.output_tokens, 0)::bigint, \
                    COALESCE(g.cost_usd, 0)::float8 \
             FROM generate_series( \
                    date_trunc('{unit}', $2::timestamptz AT TIME ZONE 'UTC') AT TIME ZONE 'UTC', \
                    date_trunc('{unit}', ($3::timestamptz AT TIME ZONE 'UTC') - interval '1 microsecond') AT TIME ZONE 'UTC', \
                    interval '{step}' \
                  ) AS gs \
             LEFT JOIN ( \
                    SELECT date_trunc('{unit}', u.ts AT TIME ZONE 'UTC') AT TIME ZONE 'UTC' AS bucket, \
                           count(*) AS turns, \
                           sum(u.input_tokens) AS input_tokens, \
                           sum(u.output_tokens) AS output_tokens, \
                           sum(u.cost_usd) AS cost_usd \
                    FROM edge_analytics.usage_events u \
                    WHERE {where_sql}{TURNS_ONLY} \
                    GROUP BY 1 \
                  ) g ON g.bucket = gs \
             ORDER BY gs ASC"
        );
        let conn = self
            .pool
            .conn(&self.tenant)
            .await
            .wrap_err("analytics pool")?;
        let rows = conn
            .query(&sql, &binds)
            .await
            .wrap_err("querying analytics timeseries")?;
        conn.commit().await?;
        Ok(rows
            .iter()
            .map(|r| TimeseriesBucket {
                ts: r.get::<_, DateTime<Utc>>(0).to_rfc3339(),
                turns: r.get(1),
                input_tokens: r.get(2),
                output_tokens: r.get(3),
                cost_usd: r.get(4),
            })
            .collect())
    }

    /// Cost-ranked rows grouped by `dimension` (top 50). For `user`, `label` is
    /// the resolved display name (fallback: the owner's `sub`); for the other
    /// dimensions `label` mirrors `key`. `tokens` is input+output.
    pub async fn by_dimension(
        &self,
        q: &AnalyticsQuery,
        dimension: Dimension,
    ) -> Result<Vec<DimensionRow>> {
        let mut binds: Vec<&(dyn ToSql + Sync)> = Vec::new();
        let where_sql = q.where_clause(&self.tenant, &mut binds);
        let (from, key, label, group_by) = dimension.sql();
        let sql = format!(
            "SELECT {key} AS key, {label} AS label, \
                    count(*)::bigint, \
                    COALESCE(sum(u.input_tokens::bigint + u.output_tokens), 0)::bigint, \
                    COALESCE(sum(u.cost_usd), 0)::float8 \
             FROM {from} \
             WHERE {where_sql}{TURNS_ONLY} \
             GROUP BY {group_by} \
             ORDER BY sum(u.cost_usd) DESC \
             LIMIT 50"
        );
        let conn = self
            .pool
            .conn(&self.tenant)
            .await
            .wrap_err("analytics pool")?;
        let rows = conn
            .query(&sql, &binds)
            .await
            .wrap_err("querying analytics by-dimension")?;
        conn.commit().await?;
        Ok(rows
            .iter()
            .map(|r| DimensionRow {
                key: r.get(0),
                label: r.get(1),
                turns: r.get(2),
                tokens: r.get(3),
                cost_usd: r.get(4),
            })
            .collect())
    }

    /// One page of the per-turn drill-down, newest first, keyset-paginated on
    /// `(ts, id)`. Fetches `limit + 1` rows to detect a next page without ever
    /// returning a trailing empty one; the surplus row's key becomes `next`.
    pub async fn turns(
        &self,
        q: &AnalyticsQuery,
        cursor: Option<TurnsCursor>,
        limit: i64,
    ) -> Result<TurnsResult> {
        // `fetch` and the cursor fields are owned locals so `binds` can borrow
        // them across the query (the tokio-postgres param slice is all borrows).
        let fetch = limit.saturating_add(1);
        let mut binds: Vec<&(dyn ToSql + Sync)> = Vec::new();
        let mut where_sql = q.where_clause(&self.tenant, &mut binds);
        where_sql.push_str(TURNS_ONLY);
        if let Some(c) = &cursor {
            binds.push(&c.ts);
            let ts_idx = binds.len();
            binds.push(&c.id);
            let id_idx = binds.len();
            where_sql.push_str(&format!(" AND (u.ts, u.id) < (${ts_idx}, ${id_idx})"));
        }
        binds.push(&fetch);
        let limit_idx = binds.len();
        let sql = format!(
            "SELECT u.id, u.ts, u.owner, \
                    COALESCE(ci.display_name, split_part(u.owner, chr(31), 2)) AS display_name, \
                    u.agent_id, u.conversation, u.model, \
                    u.input_tokens, u.output_tokens, u.cost_usd::float8, \
                    u.rounds, u.tool_count, u.tool_failures, u.duration_ms, u.outcome \
             FROM edge_analytics.usage_events u \
             LEFT JOIN edge_analytics.chat_identities ci \
               ON ci.tenant_id = u.tenant_id AND ci.owner = u.owner \
             WHERE {where_sql} \
             ORDER BY u.ts DESC, u.id DESC \
             LIMIT ${limit_idx}"
        );
        let conn = self
            .pool
            .conn(&self.tenant)
            .await
            .wrap_err("analytics pool")?;
        let rows = conn
            .query(&sql, &binds)
            .await
            .wrap_err("querying analytics turns")?;
        conn.commit().await?;
        let has_more = rows.len() as i64 > limit;
        let kept = &rows[..rows.len().min(limit as usize)];
        let next = if has_more {
            kept.last().map(|r| TurnsCursor {
                ts: r.get::<_, DateTime<Utc>>(1),
                id: r.get::<_, i64>(0),
            })
        } else {
            None
        };
        Ok(TurnsResult {
            turns: kept.iter().map(row_to_turn).collect(),
            next,
        })
    }

    /// Record one customer rating of an assistant message: a single fast upsert
    /// keyed on `(owner, message_id)`, so a re-rate overwrites the prior rating
    /// and comment (and bumps `ts`) rather than inserting a duplicate. This runs
    /// on the customer's request path — one bound INSERT, no reads. `org` stays
    /// `NULL` (Phase-1 decision; view-context org lands later, never the issuer).
    pub async fn record_feedback(
        &self,
        agent_id: &str,
        owner: &str,
        conversation: &str,
        message_id: &str,
        rating: i16,
        comment: Option<&str>,
    ) -> Result<()> {
        let conn = self
            .pool
            .conn(&self.tenant)
            .await
            .wrap_err("analytics pool")?;
        conn.execute(
            "INSERT INTO edge_analytics.feedback \
             (tenant_id, agent_id, owner, conversation, message_id, rating, comment) \
             VALUES ($1,$2,$3,$4,$5,$6,$7) \
             ON CONFLICT (tenant_id, owner, message_id) DO UPDATE SET \
                 rating = EXCLUDED.rating, \
                 comment = EXCLUDED.comment, \
                 ts = now()",
            &[
                &self.tenant,
                &agent_id,
                &owner,
                &conversation,
                &message_id,
                &rating,
                &comment,
            ],
        )
        .await
        .wrap_err("upserting feedback")?;
        conn.commit().await
    }

    /// Record one automated answer-quality grade, upserted on `(owner,
    /// message_id)` so a re-grade (e.g. after new training docs land) overwrites
    /// the prior verdict. Off the customer's critical path — one bound INSERT.
    pub async fn record_grade(
        &self,
        agent_id: &str,
        owner: &str,
        conversation: &str,
        message_id: &str,
        grade: &AnswerGrade,
        org: Option<&str>,
    ) -> Result<()> {
        let conn = self
            .pool
            .conn(&self.tenant)
            .await
            .wrap_err("analytics pool")?;
        conn.execute(
            "INSERT INTO edge_analytics.answer_grades \
             (tenant_id, agent_id, owner, conversation, message_id, quality, gap_topic, remedy, \
              reason, source, model, org, question) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13) \
             ON CONFLICT (tenant_id, owner, message_id) DO UPDATE SET \
                 quality = EXCLUDED.quality, gap_topic = EXCLUDED.gap_topic, \
                 remedy = EXCLUDED.remedy, reason = EXCLUDED.reason, \
                 source = EXCLUDED.source, model = EXCLUDED.model, org = EXCLUDED.org, \
                 question = EXCLUDED.question, ts = now()",
            &[
                &self.tenant,
                &agent_id,
                &owner,
                &conversation,
                &message_id,
                &grade.quality.as_str(),
                &grade.gap_topic,
                &grade.remedy,
                &grade.reason,
                &grade.source.as_str(),
                &grade.model,
                &org,
                &grade.question,
            ],
        )
        .await
        .wrap_err("upserting answer grade")?;
        conn.commit().await
    }

    /// One page of the feedback drill-down, newest first, keyset-paginated on
    /// `(ts, id)` — the same shape as [`Self::turns`] (fetch `limit + 1` to
    /// detect a next page; the surplus row's key becomes `next`). The `feedback`
    /// table is aliased `u` so it shares [`AnalyticsQuery::where_clause`]
    /// verbatim (its `ts`/`agent_id`/`owner` columns all exist on `feedback`);
    /// `display_name` resolves via the `chat_identities` left join.
    pub async fn feedback(
        &self,
        q: &AnalyticsQuery,
        cursor: Option<TurnsCursor>,
        limit: i64,
    ) -> Result<FeedbackPage> {
        let fetch = limit.saturating_add(1);
        let mut binds: Vec<&(dyn ToSql + Sync)> = Vec::new();
        let mut where_sql = q.where_clause(&self.tenant, &mut binds);
        if let Some(c) = &cursor {
            binds.push(&c.ts);
            let ts_idx = binds.len();
            binds.push(&c.id);
            let id_idx = binds.len();
            where_sql.push_str(&format!(" AND (u.ts, u.id) < (${ts_idx}, ${id_idx})"));
        }
        binds.push(&fetch);
        let limit_idx = binds.len();
        let sql = format!(
            "SELECT u.id, u.ts, u.owner, \
                    COALESCE(ci.display_name, split_part(u.owner, chr(31), 2)) AS display_name, \
                    u.agent_id, u.conversation, u.message_id, u.rating, u.comment \
             FROM edge_analytics.feedback u \
             LEFT JOIN edge_analytics.chat_identities ci \
               ON ci.tenant_id = u.tenant_id AND ci.owner = u.owner \
             WHERE {where_sql} \
             ORDER BY u.ts DESC, u.id DESC \
             LIMIT ${limit_idx}"
        );
        let conn = self
            .pool
            .conn(&self.tenant)
            .await
            .wrap_err("analytics pool")?;
        let rows = conn
            .query(&sql, &binds)
            .await
            .wrap_err("querying feedback")?;
        conn.commit().await?;
        let has_more = rows.len() as i64 > limit;
        let kept = &rows[..rows.len().min(limit as usize)];
        let next = if has_more {
            kept.last().map(|r| TurnsCursor {
                ts: r.get::<_, DateTime<Utc>>(1),
                id: r.get::<_, i64>(0),
            })
        } else {
            None
        };
        Ok(FeedbackPage {
            feedback: kept.iter().map(row_to_feedback).collect(),
            next,
        })
    }

    /// Answer-quality distribution over the filtered window — the grader KPIs.
    pub async fn grades_summary(&self, q: &AnalyticsQuery) -> Result<GradesSummary> {
        let mut binds: Vec<&(dyn ToSql + Sync)> = Vec::new();
        let where_sql = q.where_clause(&self.tenant, &mut binds);
        let sql = format!(
            "SELECT count(*)::bigint, \
                    count(*) FILTER (WHERE u.quality = 'good')::bigint, \
                    count(*) FILTER (WHERE u.quality = 'poor')::bigint, \
                    count(*) FILTER (WHERE u.quality = 'incorrect')::bigint, \
                    count(*) FILTER (WHERE u.quality = 'unanswerable')::bigint \
             FROM edge_analytics.answer_grades u WHERE {where_sql}"
        );
        let conn = self
            .pool
            .conn(&self.tenant)
            .await
            .wrap_err("analytics pool")?;
        let row = conn
            .query_one(&sql, &binds)
            .await
            .wrap_err("querying grades summary")?;
        conn.commit().await?;
        Ok(GradesSummary {
            graded: row.get(0),
            good: row.get(1),
            poor: row.get(2),
            incorrect: row.get(3),
            unanswerable: row.get(4),
        })
    }

    /// The gaps view: non-`good` grades grouped by the EXACT `gap_topic` string,
    /// most frequent first. `remedy` is the modal fix for the group and
    /// `example_reason` its most-recent justification. NB: grouping is on the raw
    /// LLM-authored topic text, so near-duplicate phrasings ("rate card upload" vs
    /// "rate-card uploads") stay separate rows — embedding-based clustering is a
    /// follow-up. Capped at 100 groups.
    pub async fn gaps(&self, q: &AnalyticsQuery) -> Result<Vec<GapRow>> {
        let mut binds: Vec<&(dyn ToSql + Sync)> = Vec::new();
        let where_sql = q.where_clause(&self.tenant, &mut binds);
        let sql = format!(
            "SELECT u.gap_topic, \
                    count(*)::bigint, \
                    count(*) FILTER (WHERE u.quality = 'unanswerable')::bigint, \
                    count(*) FILTER (WHERE u.quality = 'incorrect')::bigint, \
                    mode() WITHIN GROUP (ORDER BY u.remedy), \
                    (array_agg(u.reason ORDER BY u.ts DESC) FILTER (WHERE u.reason IS NOT NULL))[1], \
                    max(u.ts), \
                    COALESCE((array_agg(u.question ORDER BY u.ts DESC) FILTER (WHERE u.question IS NOT NULL))[1:3], ARRAY[]::text[]) \
             FROM edge_analytics.answer_grades u \
             WHERE {where_sql} AND u.quality <> 'good' AND u.gap_topic IS NOT NULL \
             GROUP BY u.gap_topic \
             ORDER BY count(*) DESC, max(u.ts) DESC \
             LIMIT 100"
        );
        let conn = self
            .pool
            .conn(&self.tenant)
            .await
            .wrap_err("analytics pool")?;
        let rows = conn.query(&sql, &binds).await.wrap_err("querying gaps")?;
        conn.commit().await?;
        Ok(rows
            .iter()
            .map(|r| GapRow {
                topic: r.get(0),
                count: r.get(1),
                unanswerable: r.get(2),
                incorrect: r.get(3),
                remedy: r.get(4),
                example_reason: r.get(5),
                last_seen: r.get::<_, DateTime<Utc>>(6).to_rfc3339(),
                example_questions: r.get(7),
            })
            .collect())
    }

    /// One page of the per-grade drill-down, newest first, keyset-paginated on
    /// `(ts, id)` — the same shape as [`Self::feedback`].
    pub async fn grades(
        &self,
        q: &AnalyticsQuery,
        cursor: Option<TurnsCursor>,
        limit: i64,
    ) -> Result<GradesPage> {
        let fetch = limit.saturating_add(1);
        let mut binds: Vec<&(dyn ToSql + Sync)> = Vec::new();
        let mut where_sql = q.where_clause(&self.tenant, &mut binds);
        if let Some(c) = &cursor {
            binds.push(&c.ts);
            let ts_idx = binds.len();
            binds.push(&c.id);
            let id_idx = binds.len();
            where_sql.push_str(&format!(" AND (u.ts, u.id) < (${ts_idx}, ${id_idx})"));
        }
        binds.push(&fetch);
        let limit_idx = binds.len();
        let sql = format!(
            "SELECT u.id, u.ts, u.owner, \
                    COALESCE(ci.display_name, split_part(u.owner, chr(31), 2)) AS display_name, \
                    u.agent_id, u.conversation, u.message_id, u.quality, u.gap_topic, u.remedy, u.reason \
             FROM edge_analytics.answer_grades u \
             LEFT JOIN edge_analytics.chat_identities ci \
               ON ci.tenant_id = u.tenant_id AND ci.owner = u.owner \
             WHERE {where_sql} \
             ORDER BY u.ts DESC, u.id DESC \
             LIMIT ${limit_idx}"
        );
        let conn = self
            .pool
            .conn(&self.tenant)
            .await
            .wrap_err("analytics pool")?;
        let rows = conn.query(&sql, &binds).await.wrap_err("querying grades")?;
        conn.commit().await?;
        let has_more = rows.len() as i64 > limit;
        let kept = &rows[..rows.len().min(limit as usize)];
        let next = if has_more {
            kept.last().map(|r| TurnsCursor {
                ts: r.get::<_, DateTime<Utc>>(1),
                id: r.get::<_, i64>(0),
            })
        } else {
            None
        };
        Ok(GradesPage {
            grades: kept.iter().map(row_to_grade).collect(),
            next,
        })
    }
}

/// Models already WARNed as unpriced, so a mispriced Bedrock id logs once, not
/// once per turn.
static WARNED_UNPRICED: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

fn warn_unpriced(model: &str) {
    if WARNED_UNPRICED
        .lock()
        .expect("unpriced-warn set")
        .insert(model.to_string())
    {
        tracing::warn!(
            model = %model,
            "edge cost: model has no pricing entry — cost falls back to the Sonnet-tier default; \
             raw tokens are stored so it stays recomputable once priced"
        );
    }
}

/// Normalize a runtime model id to the canonical pricing key. The edge runs
/// Bedrock regional ids (`eu.anthropic.claude-sonnet-4-6`); strip a leading
/// region segment (`eu.`/`us.`/`apac.`) then a leading `anthropic.` so the
/// canonical key (`claude-sonnet-4-6`) hits the pricing map. A canonical id or a
/// non-Anthropic id passes through unchanged.
fn normalize_model_id(model: &str) -> String {
    let mut m = model;
    for region in ["eu.", "us.", "apac."] {
        if let Some(rest) = m.strip_prefix(region) {
            m = rest;
            break;
        }
    }
    if let Some(rest) = m.strip_prefix("anthropic.") {
        m = rest;
    }
    m.to_string()
}

/// Whose turn a background call is billed to: the same key the turn's own row
/// carries, so a purpose's spend can be read beside the turn that caused it.
#[derive(Debug, Clone)]
pub struct UsageAttribution {
    pub agent_id: String,
    pub owner: String,
    pub conversation: String,
    pub message_id: Option<String>,
    pub org: Option<String>,
}

/// Hands background work a provider that records every call it makes under
/// one [`UsagePurpose`], attributed to the turn that set it off. Built once per
/// turn by the host; each module wraps the provider it is given with the
/// purpose it owns, so no call site can forget to meter or mislabel itself.
/// Without a store (a unit-test host) the provider is handed back as it is.
#[derive(Clone)]
pub struct UsageMeter {
    analytics: Option<Arc<EdgeAnalyticsStore>>,
    attribution: UsageAttribution,
}

impl UsageMeter {
    pub fn new(analytics: Option<Arc<EdgeAnalyticsStore>>, attribution: UsageAttribution) -> Self {
        Self {
            analytics,
            attribution,
        }
    }

    /// `inner`, metered under `purpose`.
    pub fn llm(&self, inner: Arc<dyn LlmProvider>, purpose: UsagePurpose) -> Arc<dyn LlmProvider> {
        match &self.analytics {
            Some(analytics) => Arc::new(MeteredProvider {
                inner,
                analytics: Arc::clone(analytics),
                attribution: self.attribution.clone(),
                purpose,
            }),
            None => inner,
        }
    }
}

/// An [`LlmProvider`] that records one `usage_events` row per `create_message`
/// under a fixed purpose, then hands the response (or error) back unchanged.
/// A failed call records an error row with zero tokens, as a failed turn does,
/// so a purpose that keeps failing is visible rather than free. Recording is
/// best-effort: a failed insert is logged and never fails the call. Streaming
/// is not overridden, so the trait's default routes it through here too.
struct MeteredProvider {
    inner: Arc<dyn LlmProvider>,
    analytics: Arc<EdgeAnalyticsStore>,
    attribution: UsageAttribution,
    purpose: UsagePurpose,
}

impl MeteredProvider {
    fn event(
        &self,
        model: &str,
        usage: &TokenUsage,
        duration_ms: u64,
        outcome: TurnOutcome,
    ) -> UsageEvent {
        UsageEvent {
            purpose: self.purpose,
            agent_id: self.attribution.agent_id.clone(),
            owner: self.attribution.owner.clone(),
            conversation: self.attribution.conversation.clone(),
            message_id: self.attribution.message_id.clone(),
            model: model.to_string(),
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_read_tokens: usage.cache_read_input_tokens,
            cache_write_tokens: usage.cache_creation_input_tokens,
            rounds: 1,
            tool_count: 0,
            tool_failures: 0,
            duration_ms,
            outcome,
        }
    }
}

#[async_trait]
impl LlmProvider for MeteredProvider {
    async fn create_message(
        &self,
        system_prompt: Option<&str>,
        messages: &[LlmMessage],
        tools: &[LlmToolDef],
        model: Option<&str>,
        max_tokens: Option<u32>,
        enable_cache: bool,
    ) -> Result<LlmResponse> {
        let started = Instant::now();
        let result = self
            .inner
            .create_message(
                system_prompt,
                messages,
                tools,
                model,
                max_tokens,
                enable_cache,
            )
            .await;
        let duration_ms = started.elapsed().as_millis() as u64;
        let event = match &result {
            Ok(response) => self.event(
                &response.model,
                &response.usage,
                duration_ms,
                TurnOutcome::Ok,
            ),
            Err(_) => self.event(
                model.unwrap_or_default(),
                &TokenUsage::default(),
                duration_ms,
                TurnOutcome::Error,
            ),
        };
        if let Err(e) = self
            .analytics
            .record_usage(&event, self.attribution.org.as_deref())
            .await
        {
            tracing::warn!(
                error = %e,
                purpose = self.purpose.as_str(),
                agent = %self.attribution.agent_id,
                "edge usage recording failed"
            );
        }
        result
    }

    async fn context_floor(&self, model: &str) -> Option<u32> {
        self.inner.context_floor(model).await
    }

    async fn verify_credentials(&self) -> Result<()> {
        self.inner.verify_credentials().await
    }
}

/// Shared by the tests of every module that meters a model call: a store on
/// the embedded test database, a provider that answers with a fixed text and
/// usage, and the rows it leaves behind.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use crate::tenant::TenantConn;
    use workforce_agent::llm::types::{ContentBlock, StopReason};
    use workforce_common::session_postgres::PostgresSessionStore;
    use workforce_postgres::test_support::with_test_pool;

    pub(crate) async fn with_store<F, Fut>(test: F)
    where
        F: FnOnce(EdgeAnalyticsStore) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        with_test_pool(PostgresSessionStore::build_pool, |pool| async move {
            run_edge_analytics_migrations(&pool)
                .await
                .expect("run analytics migrations");
            test(EdgeAnalyticsStore::new(
                TenantPool::new(pool),
                TenantId::default_tenant(),
            ))
            .await;
        })
        .await;
    }

    /// A connection on the store's tenant, to seed and inspect rows directly.
    pub(crate) async fn raw(store: &EdgeAnalyticsStore) -> TenantConn {
        store.pool.conn(&store.tenant).await.unwrap()
    }

    /// The turn a background call is billed to in these tests.
    pub(crate) fn attribution() -> UsageAttribution {
        UsageAttribution {
            agent_id: "help".into(),
            owner: "ext:iss\u{1f}sub-1".into(),
            conversation: "c1".into(),
            message_id: Some("m1".into()),
            org: None,
        }
    }

    /// A provider that answers every call with `text`, reporting `usage` on
    /// the given model, or fails every call when built with [`Self::failing`].
    pub(crate) struct FixedProvider {
        text: String,
        usage: TokenUsage,
        fail: bool,
    }

    impl FixedProvider {
        pub(crate) fn answering(text: &str, input_tokens: u32, output_tokens: u32) -> Arc<Self> {
            Arc::new(Self {
                text: text.to_string(),
                usage: TokenUsage {
                    input_tokens,
                    output_tokens,
                    cache_creation_input_tokens: 0,
                    cache_read_input_tokens: 0,
                },
                fail: false,
            })
        }

        pub(crate) fn failing() -> Arc<Self> {
            Arc::new(Self {
                text: String::new(),
                usage: TokenUsage::default(),
                fail: true,
            })
        }
    }

    #[async_trait]
    impl LlmProvider for FixedProvider {
        async fn create_message(
            &self,
            _system_prompt: Option<&str>,
            _messages: &[LlmMessage],
            _tools: &[LlmToolDef],
            _model: Option<&str>,
            _max_tokens: Option<u32>,
            _enable_cache: bool,
        ) -> Result<LlmResponse> {
            if self.fail {
                return Err(eyre!("provider down"));
            }
            Ok(LlmResponse {
                content: vec![ContentBlock::Text {
                    text: self.text.clone(),
                }],
                stop_reason: StopReason::EndTurn,
                usage: self.usage.clone(),
                model: "eu.anthropic.claude-sonnet-4-6".into(),
            })
        }
    }

    /// Every usage row as `(purpose, outcome, message_id, input, output)`,
    /// oldest first.
    pub(crate) async fn usage_rows(
        store: &EdgeAnalyticsStore,
    ) -> Vec<(String, String, Option<String>, i32, i32)> {
        let conn = raw(store).await;
        let rows = conn
            .query(
                "SELECT purpose, outcome, message_id, input_tokens, output_tokens \
                 FROM edge_analytics.usage_events ORDER BY id",
                &[],
            )
            .await
            .unwrap();
        conn.commit().await.unwrap();
        rows.iter()
            .map(|r| (r.get(0), r.get(1), r.get(2), r.get(3), r.get(4)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::test_support::{attribution, raw, usage_rows, with_store, FixedProvider};
    use super::*;

    #[test]
    fn normalize_strips_region_then_anthropic_prefix() {
        assert_eq!(
            normalize_model_id("eu.anthropic.claude-sonnet-4-6"),
            "claude-sonnet-4-6"
        );
        assert_eq!(
            normalize_model_id("us.anthropic.claude-opus-4-6"),
            "claude-opus-4-6"
        );
        assert_eq!(
            normalize_model_id("apac.anthropic.claude-haiku-4-6"),
            "claude-haiku-4-6"
        );
        // A bare `anthropic.` prefix (no region) is still stripped.
        assert_eq!(
            normalize_model_id("anthropic.claude-sonnet-4-6"),
            "claude-sonnet-4-6"
        );
        // Canonical id passes through unchanged.
        assert_eq!(normalize_model_id("claude-sonnet-4-6"), "claude-sonnet-4-6");
        // A non-Anthropic id is untouched (no region, no anthropic prefix).
        assert_eq!(normalize_model_id("gemini-2.0-flash"), "gemini-2.0-flash");
        // Only the region segment is stripped, not a similarly-prefixed name.
        assert_eq!(normalize_model_id("eureka-model"), "eureka-model");
    }

    #[test]
    fn outcome_as_str_is_stable() {
        assert_eq!(TurnOutcome::Ok.as_str(), "ok");
        assert_eq!(TurnOutcome::MaxRounds.as_str(), "max_rounds");
        assert_eq!(TurnOutcome::Error.as_str(), "error");
        assert_eq!(TurnOutcome::Timeout.as_str(), "timeout");
        assert_eq!(TurnOutcome::Cancelled.as_str(), "cancelled");
    }

    // ─── DB tests (embedded Postgres via the shared test harness) ───────────

    use workforce_common::session_postgres::PostgresSessionStore;
    use workforce_postgres::test_support::{now_micros, with_test_pool};

    #[test]
    fn purpose_spelling_is_stable_and_round_trips() {
        for purpose in [
            UsagePurpose::Turn,
            UsagePurpose::Grader,
            UsagePurpose::Lessons,
            UsagePurpose::PassiveMemory,
            UsagePurpose::Consolidation,
        ] {
            assert_eq!(UsagePurpose::parse(purpose.as_str()).unwrap(), purpose);
            assert_eq!(
                serde_json::to_string(&purpose).unwrap(),
                format!("\"{}\"", purpose.as_str()),
                "the JSON form is the column's"
            );
        }
        assert_eq!(UsagePurpose::PassiveMemory.as_str(), "passive_memory");
        assert!(UsagePurpose::parse("judge").is_err());
    }

    /// A metered provider records one row per call on the turn's key, with
    /// the response's own tokens and model under the meter's purpose; a call
    /// that fails records an error row with zero tokens and still fails.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_metered_call_records_its_purpose_and_a_failed_one_an_error_row() {
        with_store(|store| async move {
            let store = Arc::new(store);
            let meter = UsageMeter::new(Some(Arc::clone(&store)), attribution());
            let ok = meter.llm(
                FixedProvider::answering("fine", 1_000_000, 1_000_000),
                UsagePurpose::Grader,
            );
            let response = ok
                .create_message(None, &[LlmMessage::user("q")], &[], None, Some(8), false)
                .await
                .expect("the response passes through");
            assert_eq!(response.text().as_deref(), Some("fine"));
            let down = meter.llm(FixedProvider::failing(), UsagePurpose::Lessons);
            let err = down
                .create_message(
                    None,
                    &[LlmMessage::user("q")],
                    &[],
                    Some("claude-sonnet-4-6"),
                    Some(8),
                    false,
                )
                .await
                .expect_err("the error passes through");
            assert!(err.to_string().contains("provider down"), "{err}");

            assert_eq!(
                usage_rows(&store).await,
                [
                    (
                        "grader".to_string(),
                        "ok".to_string(),
                        Some("m1".to_string()),
                        1_000_000,
                        1_000_000
                    ),
                    (
                        "lessons".to_string(),
                        "error".to_string(),
                        Some("m1".to_string()),
                        0,
                        0
                    ),
                ]
            );
            let conn = raw(&store).await;
            let row = conn
                .query_one(
                    "SELECT cost_usd::float8, model, agent_id, owner, conversation, rounds \
                     FROM edge_analytics.usage_events WHERE purpose = 'grader'",
                    &[],
                )
                .await
                .unwrap();
            conn.commit().await.unwrap();
            let cost: f64 = row.get(0);
            assert!(
                (cost - 18.0).abs() < 0.001,
                "priced by the turn's own formula (Sonnet: $3/M in + $15/M out), got {cost}"
            );
            assert_eq!(row.get::<_, String>(1), "eu.anthropic.claude-sonnet-4-6");
            assert_eq!(row.get::<_, String>(2), "help");
            assert_eq!(row.get::<_, String>(3), "ext:iss\u{1f}sub-1");
            assert_eq!(row.get::<_, String>(4), "c1");
            assert_eq!(row.get::<_, i32>(5), 1);

            // No store: the provider is handed back unwrapped, nothing recorded.
            let bare = UsageMeter::new(None, attribution());
            bare.llm(FixedProvider::answering("x", 1, 1), UsagePurpose::Turn)
                .create_message(None, &[LlmMessage::user("q")], &[], None, None, false)
                .await
                .unwrap();
            assert_eq!(usage_rows(&store).await.len(), 2);
        })
        .await;
    }

    fn sample_event(outcome: TurnOutcome, tokens: u32) -> UsageEvent {
        UsageEvent {
            purpose: UsagePurpose::Turn,
            agent_id: "help".into(),
            owner: "ext:iss\u{1f}sub-1".into(),
            conversation: "c1".into(),
            message_id: Some("m1".into()),
            model: "eu.anthropic.claude-sonnet-4-6".into(),
            input_tokens: tokens,
            output_tokens: tokens,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            rounds: 2,
            tool_count: 3,
            tool_failures: 1,
            duration_ms: 1234,
            outcome,
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn record_usage_prices_a_normalized_bedrock_model() {
        with_store(|store| async move {
            let ev = sample_event(TurnOutcome::Ok, 1_000_000);
            store.record_usage(&ev, None).await.expect("record usage");

            let conn = raw(&store).await;
            let row = conn
                .query_one(
                    "SELECT input_tokens, output_tokens, tool_count, tool_failures, \
                            duration_ms, cost_usd::float8, outcome, model, org \
                     FROM edge_analytics.usage_events",
                    &[],
                )
                .await
                .unwrap();
            conn.commit().await.unwrap();
            assert_eq!(row.get::<_, i32>(0), 1_000_000);
            assert_eq!(row.get::<_, i32>(1), 1_000_000);
            assert_eq!(row.get::<_, i32>(2), 3);
            assert_eq!(row.get::<_, i32>(3), 1);
            assert_eq!(row.get::<_, i64>(4), 1234);
            // 1M input @ $3/M + 1M output @ $15/M = $18 (Sonnet), via the
            // normalized `claude-sonnet-4-6` key — proving the normalizer wiring.
            let cost: f64 = row.get(5);
            assert!((cost - 18.0).abs() < 0.001, "expected ~18.0, got {cost}");
            assert_eq!(row.get::<_, String>(6), "ok");
            // The model is stored AS RUN (the regional id), not normalized.
            assert_eq!(row.get::<_, String>(7), "eu.anthropic.claude-sonnet-4-6");
            // org is NULL in Phase 1 — attributed from the view context later,
            // never the issuer.
            assert!(row.get::<_, Option<String>>(8).is_none());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn non_success_turn_records_zero_cost() {
        with_store(|store| async move {
            let ev = sample_event(TurnOutcome::Cancelled, 0);
            store.record_usage(&ev, None).await.expect("record usage");
            let conn = raw(&store).await;
            let row = conn
                .query_one(
                    "SELECT cost_usd::float8, outcome FROM edge_analytics.usage_events",
                    &[],
                )
                .await
                .unwrap();
            conn.commit().await.unwrap();
            assert_eq!(row.get::<_, f64>(0), 0.0);
            assert_eq!(row.get::<_, String>(1), "cancelled");
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn upsert_identity_refreshes_on_conflict() {
        with_store(|store| async move {
            let owner = "ext:iss\u{1f}sub-1";
            store
                .upsert_identity(
                    owner,
                    "iss",
                    &TurnIdentity {
                        display_name: Some("Ada".into()),
                        email: Some("ada@example.com".into()),
                    },
                    None,
                )
                .await
                .expect("first upsert");
            store
                .upsert_identity(
                    owner,
                    "iss",
                    &TurnIdentity {
                        display_name: Some("Ada Lovelace".into()),
                        email: Some("ada@example.com".into()),
                    },
                    None,
                )
                .await
                .expect("second upsert");

            let conn = raw(&store).await;
            let row = conn
                .query_one(
                    "SELECT display_name, issuer, first_seen <= last_seen \
                     FROM edge_analytics.chat_identities WHERE owner = $1",
                    &[&owner],
                )
                .await
                .unwrap();
            assert_eq!(row.get::<_, String>(0), "Ada Lovelace", "name refreshed");
            assert_eq!(row.get::<_, String>(1), "iss");
            assert!(row.get::<_, bool>(2), "last_seen advanced past first_seen");
            let count: i64 = conn
                .query_one("SELECT count(*) FROM edge_analytics.chat_identities", &[])
                .await
                .unwrap()
                .get(0);
            conn.commit().await.unwrap();
            assert_eq!(count, 1, "upsert, not a second row");
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn upsert_identity_preserves_name_when_later_claim_is_absent() {
        with_store(|store| async move {
            let owner = "ext:iss\u{1f}sub-1";
            store
                .upsert_identity(
                    owner,
                    "iss",
                    &TurnIdentity {
                        display_name: Some("Grace Hopper".into()),
                        email: Some("grace@example.com".into()),
                    },
                    None,
                )
                .await
                .expect("first upsert");
            // A later turn whose token omits the profile claims must not wipe the
            // captured name/email.
            store
                .upsert_identity(owner, "iss", &TurnIdentity::default(), None)
                .await
                .expect("second upsert with no claims");

            let resolved = store
                .resolve_identities(&[owner.to_string()])
                .await
                .unwrap();
            let id = &resolved[owner];
            assert_eq!(id.display_name.as_deref(), Some("Grace Hopper"));
            assert_eq!(id.email.as_deref(), Some("grace@example.com"));
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn resolve_identities_maps_known_owners_only() {
        with_store(|store| async move {
            store
                .upsert_identity(
                    "ext:iss\u{1f}a",
                    "iss",
                    &TurnIdentity {
                        display_name: Some("Alice".into()),
                        email: Some("alice@example.com".into()),
                    },
                    None,
                )
                .await
                .unwrap();

            assert!(store.resolve_identities(&[]).await.unwrap().is_empty());

            let owners = vec![
                "ext:iss\u{1f}a".to_string(),
                "ext:iss\u{1f}missing".to_string(),
            ];
            let resolved = store.resolve_identities(&owners).await.unwrap();
            assert_eq!(resolved.len(), 1, "only the known owner resolves");
            let alice = &resolved["ext:iss\u{1f}a"];
            assert_eq!(alice.display_name.as_deref(), Some("Alice"));
            assert_eq!(alice.email.as_deref(), Some("alice@example.com"));
        })
        .await;
    }

    // ─── Phase 2 read/aggregation tests ─────────────────────────────────────

    /// Seed one `usage_events` row with an EXPLICIT `ts` (the recorder relies on
    /// `DEFAULT now()`, which would land every row in the current bucket) so the
    /// aggregation/bucketing/pagination tests can span the window deterministically.
    #[allow(clippy::too_many_arguments)]
    async fn insert_event(
        store: &EdgeAnalyticsStore,
        ts: &str,
        agent: &str,
        owner: &str,
        model: &str,
        input: i32,
        output: i32,
        cache_read: i32,
        cache_write: i32,
        duration_ms: i64,
        cost: f64,
        outcome: &str,
    ) {
        let conn = raw(store).await;
        conn.execute(
            "INSERT INTO edge_analytics.usage_events \
             (tenant_id, ts, agent_id, owner, conversation, message_id, model, input_tokens, \
              output_tokens, cache_read_tokens, cache_write_tokens, rounds, tool_count, \
              tool_failures, duration_ms, cost_usd, outcome, org) \
             VALUES ($1, $2::text::timestamptz, $3, $4, 'conv', 'msg', $5, $6, $7, $8, $9, 1, 0, 0, \
                     $10, $11::float8::numeric(12,6), $12, NULL)",
            &[
                &store.tenant,
                &ts,
                &agent,
                &owner,
                &model,
                &input,
                &output,
                &cache_read,
                &cache_write,
                &duration_ms,
                &cost,
                &outcome,
            ],
        )
        .await
        .unwrap();
        conn.commit().await.unwrap();
    }

    /// A window from two RFC3339 bounds, with no agent/owner filter.
    fn window(from: &str, to: &str) -> AnalyticsQuery {
        let parse = |s: &str| DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc);
        AnalyticsQuery {
            from: parse(from),
            to: parse(to),
            agent: None,
            owner: None,
        }
    }

    const MODEL: &str = "eu.anthropic.claude-sonnet-4-6";

    #[tokio::test(flavor = "multi_thread")]
    async fn summary_aggregates_kpis_and_excludes_cancelled_from_failure_rate() {
        with_store(|store| async move {
            // 5 turns / 3 users: 2 successful (with tokens+cost), one error, one
            // timeout, one cancelled (all three failure-shaped carry zero tokens).
            insert_event(
                &store,
                "2026-07-01T10:00:00Z",
                "help",
                "ext:iss\u{1f}a",
                MODEL,
                100,
                50,
                10,
                5,
                200,
                1.0,
                "ok",
            )
            .await;
            insert_event(
                &store,
                "2026-07-01T11:00:00Z",
                "help",
                "ext:iss\u{1f}a",
                MODEL,
                200,
                100,
                20,
                10,
                400,
                2.0,
                "max_rounds",
            )
            .await;
            insert_event(
                &store,
                "2026-07-02T10:00:00Z",
                "help",
                "ext:iss\u{1f}b",
                MODEL,
                0,
                0,
                0,
                0,
                50,
                0.0,
                "error",
            )
            .await;
            insert_event(
                &store,
                "2026-07-02T11:00:00Z",
                "help",
                "ext:iss\u{1f}b",
                MODEL,
                0,
                0,
                0,
                0,
                60,
                0.0,
                "timeout",
            )
            .await;
            insert_event(
                &store,
                "2026-07-03T10:00:00Z",
                "help",
                "ext:iss\u{1f}c",
                MODEL,
                0,
                0,
                0,
                0,
                70,
                0.0,
                "cancelled",
            )
            .await;

            let s = store
                .summary(&window("2026-06-01T00:00:00Z", "2026-08-01T00:00:00Z"))
                .await
                .expect("summary");

            assert_eq!(s.turns, 5);
            assert_eq!(s.input_tokens, 300);
            assert_eq!(s.output_tokens, 150);
            assert_eq!(s.cache_read_tokens, 30);
            assert_eq!(s.cache_write_tokens, 15);
            assert!(
                (s.total_cost_usd - 3.0).abs() < 1e-9,
                "cost {}",
                s.total_cost_usd
            );
            assert_eq!(s.unique_users, 3);
            // 2 failures (error+timeout) over 5 turns. Cancelled is in the
            // denominator but NOT the numerator: 3/5 would prove it counted.
            assert!(
                (s.failure_rate - 0.4).abs() < 1e-9,
                "failure_rate {}",
                s.failure_rate
            );
            // Only the two that ran (ok/max_rounds) count: (200 + 400) / 2 = 300.
            // The error/timeout/cancelled durations (50/60/70) are excluded, so a
            // wrong 156 would prove the FILTER didn't apply.
            assert!(
                (s.avg_duration_ms - 300.0).abs() < 1e-9,
                "avg_ms {}",
                s.avg_duration_ms
            );

            // Same window, now narrowed to one user via the bound `owner` param
            // (exercises the WHERE branch that filters, not interpolates). User
            // `b` has exactly the error + timeout turns.
            let filtered = store
                .summary(&AnalyticsQuery {
                    owner: Some("ext:iss\u{1f}b".into()),
                    ..window("2026-06-01T00:00:00Z", "2026-08-01T00:00:00Z")
                })
                .await
                .expect("filtered summary");
            assert_eq!(filtered.turns, 2, "only user b's turns");
            assert_eq!(filtered.unique_users, 1);
            assert!(
                (filtered.failure_rate - 1.0).abs() < 1e-9,
                "both of b's turns failed: {}",
                filtered.failure_rate
            );
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn by_user_groups_with_name_resolution_and_sub_fallback() {
        with_store(|store| async move {
            // `ada` has a captured identity; `bob` does not (→ sub fallback).
            store
                .upsert_identity(
                    "ext:iss\u{1f}ada",
                    "iss",
                    &TurnIdentity {
                        display_name: Some("Ada Lovelace".into()),
                        email: None,
                    },
                    None,
                )
                .await
                .unwrap();
            // Ada is costlier, so she ranks first (ORDER BY cost DESC).
            insert_event(
                &store,
                "2026-07-01T10:00:00Z",
                "help",
                "ext:iss\u{1f}ada",
                MODEL,
                100,
                50,
                0,
                0,
                100,
                5.0,
                "ok",
            )
            .await;
            insert_event(
                &store,
                "2026-07-01T11:00:00Z",
                "help",
                "ext:iss\u{1f}bob",
                MODEL,
                30,
                20,
                0,
                0,
                100,
                2.0,
                "ok",
            )
            .await;

            let rows = store
                .by_dimension(
                    &window("2026-06-01T00:00:00Z", "2026-08-01T00:00:00Z"),
                    Dimension::User,
                )
                .await
                .expect("by user");

            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0].key, "ext:iss\u{1f}ada");
            assert_eq!(rows[0].label, "Ada Lovelace", "resolved display name");
            assert_eq!(rows[0].turns, 1);
            assert_eq!(rows[0].tokens, 150, "input + output");
            assert!((rows[0].cost_usd - 5.0).abs() < 1e-9);
            assert_eq!(rows[1].key, "ext:iss\u{1f}bob");
            assert_eq!(rows[1].label, "bob", "fallback to the owner's sub");
            assert_eq!(rows[1].tokens, 50);
        })
        .await;
    }

    /// Seed one background-call row (`purpose` other than `turn`) at `ts`.
    async fn insert_purposed(
        store: &EdgeAnalyticsStore,
        ts: &str,
        purpose: UsagePurpose,
        input: i32,
        output: i32,
        cost: f64,
    ) {
        let conn = raw(store).await;
        conn.execute(
            "INSERT INTO edge_analytics.usage_events \
             (tenant_id, ts, agent_id, owner, conversation, message_id, model, input_tokens, \
              output_tokens, cache_read_tokens, cache_write_tokens, rounds, tool_count, \
              tool_failures, duration_ms, cost_usd, outcome, org, purpose) \
             VALUES ($1, $2::text::timestamptz, 'help', 'ext:iss\u{1f}a', 'conv', 'msg', $3, $4, $5, \
                     0, 0, 1, 0, 0, 50, $6::float8::numeric(12,6), 'ok', NULL, $7)",
            &[
                &store.tenant,
                &ts,
                &MODEL,
                &input,
                &output,
                &cost,
                &purpose.as_str(),
            ],
        )
        .await
        .unwrap();
        conn.commit().await.unwrap();
    }

    /// `by_purpose` sums cost, tokens and calls per purpose over the window,
    /// costliest first, while every turn KPI, the series, the breakdowns and
    /// the drill-down keep counting customer turns alone.
    #[tokio::test(flavor = "multi_thread")]
    async fn by_purpose_sums_per_purpose_and_the_turn_kpis_ignore_background_rows() {
        with_store(|store| async move {
            insert_event(
                &store,
                "2026-07-01T10:00:00Z",
                "help",
                "ext:iss\u{1f}a",
                MODEL,
                100,
                50,
                0,
                0,
                200,
                1.0,
                "ok",
            )
            .await;
            insert_purposed(
                &store,
                "2026-07-01T10:00:01Z",
                UsagePurpose::Grader,
                300,
                20,
                0.5,
            )
            .await;
            insert_purposed(
                &store,
                "2026-07-01T10:00:02Z",
                UsagePurpose::Grader,
                300,
                20,
                0.5,
            )
            .await;
            insert_purposed(
                &store,
                "2026-07-01T10:00:03Z",
                UsagePurpose::Lessons,
                40,
                10,
                2.5,
            )
            .await;
            insert_purposed(
                &store,
                "2026-07-01T10:00:04Z",
                UsagePurpose::PassiveMemory,
                20,
                5,
                0.1,
            )
            .await;
            // Outside the window: never counted anywhere.
            insert_purposed(
                &store,
                "2026-09-01T10:00:00Z",
                UsagePurpose::Consolidation,
                9,
                9,
                9.0,
            )
            .await;

            let window = window("2026-06-01T00:00:00Z", "2026-08-01T00:00:00Z");
            let summary = store.summary(&window).await.expect("summary");
            assert_eq!(summary.turns, 1, "background rows are not turns");
            assert_eq!(summary.input_tokens, 100);
            assert_eq!(summary.output_tokens, 50);
            assert!((summary.total_cost_usd - 1.0).abs() < 1e-9);
            assert_eq!(summary.unique_users, 1);
            assert_eq!(
                summary.by_purpose,
                [
                    PurposeRow {
                        purpose: UsagePurpose::Lessons,
                        cost_usd: 2.5,
                        input_tokens: 40,
                        output_tokens: 10,
                        calls: 1,
                    },
                    PurposeRow {
                        purpose: UsagePurpose::Grader,
                        cost_usd: 1.0,
                        input_tokens: 600,
                        output_tokens: 40,
                        calls: 2,
                    },
                    PurposeRow {
                        purpose: UsagePurpose::Turn,
                        cost_usd: 1.0,
                        input_tokens: 100,
                        output_tokens: 50,
                        calls: 1,
                    },
                    PurposeRow {
                        purpose: UsagePurpose::PassiveMemory,
                        cost_usd: 0.1,
                        input_tokens: 20,
                        output_tokens: 5,
                        calls: 1,
                    },
                ]
            );
            let json = serde_json::to_value(&summary).unwrap();
            assert_eq!(json["by_purpose"][0]["purpose"], "lessons");
            assert_eq!(json["by_purpose"][3]["purpose"], "passive_memory");
            assert_eq!(json["by_purpose"][1]["calls"], 2);

            let series = store
                .timeseries(&window, TimeBucket::Day)
                .await
                .expect("timeseries");
            let on_the_day = series.iter().find(|b| b.turns > 0).expect("the turn's day");
            assert_eq!(on_the_day.turns, 1);
            assert!((on_the_day.cost_usd - 1.0).abs() < 1e-9);
            let by_agent = store
                .by_dimension(&window, Dimension::Agent)
                .await
                .expect("by agent");
            assert_eq!(by_agent.len(), 1);
            assert_eq!(by_agent[0].turns, 1);
            assert!((by_agent[0].cost_usd - 1.0).abs() < 1e-9);
            let page = store.turns(&window, None, 10).await.expect("turns");
            assert_eq!(page.turns.len(), 1, "the drill-down lists turns alone");
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn timeseries_buckets_by_day_and_zero_fills_gaps() {
        with_store(|store| async move {
            // Two turns on 07-01, none on 07-02, one on 07-03.
            insert_event(
                &store,
                "2026-07-01T10:00:00Z",
                "help",
                "ext:iss\u{1f}a",
                MODEL,
                100,
                50,
                0,
                0,
                100,
                1.0,
                "ok",
            )
            .await;
            insert_event(
                &store,
                "2026-07-01T15:00:00Z",
                "help",
                "ext:iss\u{1f}a",
                MODEL,
                100,
                50,
                0,
                0,
                100,
                1.0,
                "ok",
            )
            .await;
            insert_event(
                &store,
                "2026-07-03T09:00:00Z",
                "help",
                "ext:iss\u{1f}b",
                MODEL,
                200,
                100,
                0,
                0,
                100,
                2.0,
                "ok",
            )
            .await;

            let buckets = store
                .timeseries(
                    &window("2026-07-01T00:00:00Z", "2026-07-03T12:00:00Z"),
                    TimeBucket::Day,
                )
                .await
                .expect("timeseries");

            assert_eq!(buckets.len(), 3, "07-01, 07-02 (empty), 07-03");
            assert!(buckets[0].ts.starts_with("2026-07-01"));
            assert!(buckets[1].ts.starts_with("2026-07-02"));
            assert!(buckets[2].ts.starts_with("2026-07-03"));
            // Ascending by ts.
            assert!(buckets[0].ts < buckets[1].ts && buckets[1].ts < buckets[2].ts);
            assert_eq!(buckets[0].turns, 2);
            assert_eq!(buckets[0].input_tokens, 200);
            assert!((buckets[0].cost_usd - 2.0).abs() < 1e-9);
            // Zero-filled empty middle bucket.
            assert_eq!(buckets[1].turns, 0);
            assert_eq!(buckets[1].input_tokens, 0);
            assert_eq!(buckets[1].cost_usd, 0.0);
            assert_eq!(buckets[2].turns, 1);
            assert_eq!(buckets[2].output_tokens, 100);
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn timeseries_omits_the_phantom_bucket_when_to_is_aligned() {
        with_store(|store| async move {
            // Half-open window [07-01, 07-03) with a day-ALIGNED `to`. Without the
            // `to - 1µs` upper bound, generate_series would emit an empty 07-03
            // bucket that `ts < to` can never populate.
            insert_event(
                &store,
                "2026-07-01T10:00:00Z",
                "help",
                "ext:iss\u{1f}a",
                MODEL,
                100,
                50,
                0,
                0,
                100,
                1.0,
                "ok",
            )
            .await;
            let buckets = store
                .timeseries(
                    &window("2026-07-01T00:00:00Z", "2026-07-03T00:00:00Z"),
                    TimeBucket::Day,
                )
                .await
                .expect("timeseries");
            assert_eq!(buckets.len(), 2, "07-01 + 07-02 only, no phantom 07-03");
            assert!(buckets[0].ts.starts_with("2026-07-01"));
            assert!(buckets[1].ts.starts_with("2026-07-02"));
            assert_eq!(buckets[0].turns, 1);
            assert_eq!(buckets[1].turns, 0);
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn turns_cursor_paginates_two_pages_without_overlap_or_gap() {
        with_store(|store| async move {
            // Three turns at distinct times; newest first is 12:00, 11:00, 10:00.
            insert_event(
                &store,
                "2026-07-01T10:00:00Z",
                "help",
                "ext:iss\u{1f}alice",
                MODEL,
                10,
                5,
                0,
                0,
                100,
                1.0,
                "ok",
            )
            .await;
            insert_event(
                &store,
                "2026-07-01T11:00:00Z",
                "help",
                "ext:iss\u{1f}alice",
                MODEL,
                10,
                5,
                0,
                0,
                100,
                1.0,
                "ok",
            )
            .await;
            insert_event(
                &store,
                "2026-07-01T12:00:00Z",
                "help",
                "ext:iss\u{1f}alice",
                MODEL,
                10,
                5,
                0,
                0,
                100,
                1.0,
                "ok",
            )
            .await;

            let q = window("2026-06-01T00:00:00Z", "2026-08-01T00:00:00Z");
            let page1 = store.turns(&q, None, 2).await.expect("page 1");
            assert_eq!(page1.turns.len(), 2, "first page is full");
            assert!(page1.next.is_some(), "a second page follows");
            // Newest first, strictly descending.
            assert!(page1.turns[0].ts > page1.turns[1].ts);
            // Fallback display name from the owner's sub.
            assert_eq!(page1.turns[0].display_name, "alice");

            let page2 = store
                .turns(&q, page1.next.clone(), 2)
                .await
                .expect("page 2");
            assert_eq!(page2.turns.len(), 1, "one row remains");
            assert!(page2.next.is_none(), "no third page");
            // No gap across the boundary: page 2's row is older than page 1's last.
            assert!(page1.turns[1].ts > page2.turns[0].ts);

            // No overlap: the three ids across both pages are all distinct.
            let mut ids: Vec<i64> = page1
                .turns
                .iter()
                .chain(page2.turns.iter())
                .map(|t| t.id)
                .collect();
            ids.sort_unstable();
            ids.dedup();
            assert_eq!(ids.len(), 3, "every turn appears exactly once");
        })
        .await;
    }

    // ─── Phase 3 feedback tests ─────────────────────────────────────────────

    /// Seed one `feedback` row with an EXPLICIT `ts` (the recorder relies on
    /// `DEFAULT now()`, which would land every row in the current bucket) so the
    /// pagination tests can span the window deterministically.
    async fn insert_feedback(
        store: &EdgeAnalyticsStore,
        ts: &str,
        owner: &str,
        message_id: &str,
        rating: i16,
        comment: Option<&str>,
    ) {
        let conn = raw(store).await;
        conn.execute(
            "INSERT INTO edge_analytics.feedback \
             (tenant_id, ts, agent_id, owner, conversation, message_id, rating, comment) \
             VALUES ($1, $2::text::timestamptz, 'help', $3, 'conv', $4, $5, $6)",
            &[&store.tenant, &ts, &owner, &message_id, &rating, &comment],
        )
        .await
        .unwrap();
        conn.commit().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn record_feedback_upserts_on_rerate() {
        with_store(|store| async move {
            let owner = "ext:iss\u{1f}sub-1";
            store
                .record_feedback("help", owner, "c1", "m1", 1, None)
                .await
                .expect("first rating");
            // A re-rate on the SAME (owner, message_id) overwrites in place:
            // flipped rating, new comment, bumped ts — never a second row.
            store
                .record_feedback("help", owner, "c1", "m1", -1, Some("unhelpful"))
                .await
                .expect("re-rate");

            let conn = raw(&store).await;
            let row = conn
                .query_one(
                    "SELECT rating, comment FROM edge_analytics.feedback \
                     WHERE owner = $1 AND message_id = 'm1'",
                    &[&owner],
                )
                .await
                .unwrap();
            assert_eq!(row.get::<_, i16>(0), -1, "rating flipped");
            assert_eq!(
                row.get::<_, Option<String>>(1).as_deref(),
                Some("unhelpful")
            );
            let count: i64 = conn
                .query_one("SELECT count(*) FROM edge_analytics.feedback", &[])
                .await
                .unwrap()
                .get(0);
            conn.commit().await.unwrap();
            assert_eq!(count, 1, "upsert on (owner, message_id), not a second row");
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn record_grade_upserts_on_regrade() {
        with_store(|store| async move {
            let owner = "ext:iss\u{1f}sub-g";
            let first = AnswerGrade {
                quality: "unanswerable".into(),
                gap_topic: Some("rate card upload".into()),
                remedy: Some("knowledge".into()),
                reason: Some("no doc covers it".into()),
                source: "llm".into(),
                model: Some("claude-haiku-4-5".into()),
                question: Some("what is a sessional shift?".into()),
            };
            store
                .record_grade("help", owner, "c1", "m1", &first, Some("Trust A"))
                .await
                .expect("first grade");
            // A re-grade on the SAME (owner, message_id) — e.g. after new training
            // docs land — overwrites the verdict in place, never a second row.
            let second = AnswerGrade {
                quality: "good".into(),
                gap_topic: None,
                remedy: Some("none".into()),
                reason: Some("now answered".into()),
                source: "llm".into(),
                model: Some("claude-haiku-4-5".into()),
                question: Some("what is a sessional shift?".into()),
            };
            store
                .record_grade("help", owner, "c1", "m1", &second, Some("Trust A"))
                .await
                .expect("re-grade");

            let conn = raw(&store).await;
            let row = conn
                .query_one(
                    "SELECT quality, gap_topic, org FROM edge_analytics.answer_grades \
                     WHERE owner = $1 AND message_id = 'm1'",
                    &[&owner],
                )
                .await
                .unwrap();
            assert_eq!(row.get::<_, String>(0), "good", "quality overwritten");
            assert_eq!(
                row.get::<_, Option<String>>(1),
                None,
                "gap_topic cleared when the re-grade is good"
            );
            assert_eq!(row.get::<_, Option<String>>(2).as_deref(), Some("Trust A"));
            let count: i64 = conn
                .query_one("SELECT count(*) FROM edge_analytics.answer_grades", &[])
                .await
                .unwrap()
                .get(0);
            conn.commit().await.unwrap();
            assert_eq!(count, 1, "upsert on (owner, message_id), not a second row");
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn grades_summary_and_gaps_aggregate() {
        with_store(|store| async move {
            let g = |quality: &str, topic: Option<&str>, question: &str| AnswerGrade {
                quality: quality.to_string(),
                gap_topic: topic.map(str::to_string),
                remedy: Some("knowledge".to_string()),
                reason: Some("because".to_string()),
                source: "llm".to_string(),
                model: None,
                question: Some(question.to_string()),
            };
            let recs = [
                (
                    "m1",
                    "unanswerable",
                    Some("rate cards"),
                    "how do rate cards work?",
                ),
                (
                    "m2",
                    "unanswerable",
                    Some("rate cards"),
                    "what is a rate card band?",
                ),
                (
                    "m3",
                    "incorrect",
                    Some("rate cards"),
                    "which rate applies at night?",
                ),
                ("m4", "good", None, "who am I logged in as?"),
                ("m5", "poor", Some("exports"), "can I export to csv?"),
            ];
            for (mid, quality, topic, question) in recs {
                store
                    .record_grade(
                        "help",
                        "ext:iss\u{1f}s",
                        "c1",
                        mid,
                        &g(quality, topic, question),
                        None,
                    )
                    .await
                    .expect("record grade");
            }
            let now = now_micros();
            let query = AnalyticsQuery {
                from: now - chrono::Duration::days(1),
                to: now + chrono::Duration::days(1),
                agent: None,
                owner: None,
            };

            let s = store.grades_summary(&query).await.expect("summary");
            assert_eq!(
                (s.graded, s.good, s.poor, s.incorrect, s.unanswerable),
                (5, 1, 1, 1, 2)
            );

            let gaps = store.gaps(&query).await.expect("gaps");
            assert_eq!(gaps.len(), 2, "two non-good topics (good is excluded)");
            assert_eq!(gaps[0].topic, "rate cards", "most frequent first");
            assert_eq!(gaps[0].count, 3);
            assert_eq!(gaps[0].unanswerable, 2);
            assert_eq!(gaps[0].incorrect, 1);
            // Example failing questions for the topic are surfaced (capped at 3).
            assert!(
                !gaps[0].example_questions.is_empty(),
                "example questions present"
            );
            assert!(gaps[0].example_questions.len() <= 3);
            assert!(gaps[0].example_questions.iter().all(|q| [
                "how do rate cards work?",
                "what is a rate card band?",
                "which rate applies at night?"
            ]
            .contains(&q.as_str())));
            assert_eq!(gaps[1].topic, "exports");
            assert_eq!(gaps[1].count, 1);

            let page = store.grades(&query, None, 10).await.expect("grades page");
            assert_eq!(page.grades.len(), 5);
            assert!(page.next.is_none());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn feedback_is_partitioned_by_owner_for_the_same_message_id() {
        with_store(|store| async move {
            // The security property: `owner` is server-derived from the token, so
            // the SAME `message_id` rated by two owners yields two independent
            // rows (the UNIQUE is on the PAIR), and a per-owner read never
            // crosses customers.
            let a = "ext:iss\u{1f}a";
            let b = "ext:iss\u{1f}b";
            store
                .record_feedback("help", a, "c1", "shared-msg", 1, Some("a liked it"))
                .await
                .unwrap();
            store
                .record_feedback("help", b, "c1", "shared-msg", -1, Some("b disliked it"))
                .await
                .unwrap();

            let conn = raw(&store).await;
            let count: i64 = conn
                .query_one("SELECT count(*) FROM edge_analytics.feedback", &[])
                .await
                .unwrap()
                .get(0);
            conn.commit().await.unwrap();
            assert_eq!(count, 2, "distinct rows per owner for one message_id");

            // A read filtered to owner `a` returns only `a`'s rating.
            let only_a = store
                .feedback(
                    &AnalyticsQuery {
                        owner: Some(a.into()),
                        ..window("2000-01-01T00:00:00Z", "2100-01-01T00:00:00Z")
                    },
                    None,
                    50,
                )
                .await
                .expect("owner-filtered feedback");
            assert_eq!(only_a.feedback.len(), 1);
            assert_eq!(only_a.feedback[0].owner, a);
            assert_eq!(only_a.feedback[0].rating, 1);
            assert_eq!(only_a.feedback[0].comment.as_deref(), Some("a liked it"));
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn feedback_paginates_and_resolves_names() {
        with_store(|store| async move {
            // `ada` has a captured identity; `bob` does not (→ sub fallback).
            store
                .upsert_identity(
                    "ext:iss\u{1f}ada",
                    "iss",
                    &TurnIdentity {
                        display_name: Some("Ada Lovelace".into()),
                        email: None,
                    },
                    None,
                )
                .await
                .unwrap();
            // Three ratings at distinct times; newest first is 12:00, 11:00, 10:00.
            insert_feedback(
                &store,
                "2026-07-01T10:00:00Z",
                "ext:iss\u{1f}bob",
                "m1",
                -1,
                Some("nope"),
            )
            .await;
            insert_feedback(
                &store,
                "2026-07-01T11:00:00Z",
                "ext:iss\u{1f}ada",
                "m2",
                1,
                None,
            )
            .await;
            insert_feedback(
                &store,
                "2026-07-01T12:00:00Z",
                "ext:iss\u{1f}ada",
                "m3",
                1,
                Some("great"),
            )
            .await;

            let q = window("2026-06-01T00:00:00Z", "2026-08-01T00:00:00Z");
            let page1 = store.feedback(&q, None, 2).await.expect("page 1");
            assert_eq!(page1.feedback.len(), 2, "first page is full");
            assert!(page1.next.is_some(), "a second page follows");
            // Newest first, strictly descending.
            assert!(page1.feedback[0].ts > page1.feedback[1].ts);
            assert_eq!(page1.feedback[0].message_id, "m3");
            assert_eq!(
                page1.feedback[0].display_name, "Ada Lovelace",
                "resolved name"
            );
            assert_eq!(page1.feedback[0].rating, 1);
            assert_eq!(page1.feedback[0].comment.as_deref(), Some("great"));

            let page2 = store
                .feedback(&q, page1.next.clone(), 2)
                .await
                .expect("page 2");
            assert_eq!(page2.feedback.len(), 1, "one row remains");
            assert!(page2.next.is_none(), "no third page");
            // No gap across the boundary and the sub fallback for the unresolved
            // owner.
            assert!(page1.feedback[1].ts > page2.feedback[0].ts);
            assert_eq!(page2.feedback[0].message_id, "m1");
            assert_eq!(page2.feedback[0].display_name, "bob", "sub fallback");
            assert_eq!(page2.feedback[0].rating, -1);

            // No overlap: all three ids are distinct.
            let mut ids: Vec<i64> = page1
                .feedback
                .iter()
                .chain(page2.feedback.iter())
                .map(|f| f.id)
                .collect();
            ids.sort_unstable();
            ids.dedup();
            assert_eq!(ids.len(), 3, "every rating appears exactly once");
        })
        .await;
    }

    // ─── cross-process turn registry ───────────────────────────────────────

    fn claim<'a>(key: &'a str, replica: &'a str) -> TurnClaim<'a> {
        TurnClaim {
            turn_key: key,
            conversation: "conv-1",
            owner: "owner-1",
            agent_id: "agent-1",
            replica,
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_claim_names_the_replica_running_the_turn() {
        with_store(|store| async move {
            store
                .claim_turn(&claim("k1", "replica-a"))
                .await
                .expect("claim");

            let owner = store
                .turn_owner("k1", Duration::from_secs(240))
                .await
                .expect("lookup")
                .expect("a live claim");
            assert_eq!(owner.replica, "replica-a");

            assert!(
                store
                    .turn_owner("absent", Duration::from_secs(240))
                    .await
                    .expect("lookup")
                    .is_none(),
                "an unclaimed conversation has no owner"
            );
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_new_turn_replaces_the_previous_claim_on_the_same_conversation() {
        with_store(|store| async move {
            store
                .claim_turn(&claim("k1", "replica-a"))
                .await
                .expect("first");
            store
                .claim_turn(&claim("k1", "replica-b"))
                .await
                .expect("second");

            let owner = store
                .turn_owner("k1", Duration::from_secs(240))
                .await
                .expect("lookup")
                .expect("a live claim");
            assert_eq!(
                owner.replica, "replica-b",
                "one live turn per conversation, matching the in-process map"
            );
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn only_the_owning_replica_can_release_a_claim() {
        with_store(|store| async move {
            store
                .claim_turn(&claim("k1", "replica-b"))
                .await
                .expect("claim");

            // The superseded replica's late cleanup must not delete the live
            // claim its successor just wrote.
            store
                .release_turn("k1", "replica-a")
                .await
                .expect("stale release");
            assert!(
                store
                    .turn_owner("k1", Duration::from_secs(240))
                    .await
                    .expect("lookup")
                    .is_some(),
                "a non-owner release is a no-op"
            );

            store
                .release_turn("k1", "replica-b")
                .await
                .expect("release");
            assert!(
                store
                    .turn_owner("k1", Duration::from_secs(240))
                    .await
                    .expect("lookup")
                    .is_none(),
                "the owner's release clears the claim"
            );
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_claim_older_than_the_turn_ceiling_reads_as_absent() {
        with_store(|store| async move {
            store
                .claim_turn(&claim("k1", "replica-a"))
                .await
                .expect("claim");

            // Zero max-age makes every existing row older than the window, which
            // is the orphan case a replica killed mid-turn leaves behind.
            assert!(
                store
                    .turn_owner("k1", Duration::ZERO)
                    .await
                    .expect("lookup")
                    .is_none(),
                "a stale claim must never make a conversation look permanently busy"
            );
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_sweep_removes_only_claims_past_the_window() {
        with_store(|store| async move {
            store
                .claim_turn(&claim("k1", "replica-a"))
                .await
                .expect("claim");

            assert_eq!(
                store
                    .sweep_stale_turns(Duration::from_secs(240))
                    .await
                    .expect("sweep"),
                0,
                "a running turn's claim survives the sweep"
            );

            assert_eq!(
                store
                    .sweep_stale_turns(Duration::ZERO)
                    .await
                    .expect("sweep"),
                1,
                "an orphaned claim is removed"
            );
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_cancel_request_reaches_only_the_replica_running_the_turn() {
        with_store(|store| async move {
            store
                .claim_turn(&claim("k1", "replica-a"))
                .await
                .expect("claim");

            assert!(
                store
                    .request_turn_cancel("k1", Duration::from_secs(240))
                    .await
                    .expect("request"),
                "a live claim can be asked to stop"
            );

            assert!(store
                .cancel_requested("k1", "replica-a")
                .await
                .expect("read"));
            assert!(
                !store
                    .cancel_requested("k1", "replica-b")
                    .await
                    .expect("read"),
                "a request aimed at one replica must never cancel another's turn"
            );
        })
        .await;
    }

    /// The successor turn would otherwise inherit the request that killed its
    /// predecessor and cancel itself the moment its watcher first polled.
    #[tokio::test(flavor = "multi_thread")]
    async fn claiming_a_conversation_clears_an_inherited_cancel_request() {
        with_store(|store| async move {
            store
                .claim_turn(&claim("k1", "replica-a"))
                .await
                .expect("claim");
            store
                .request_turn_cancel("k1", Duration::from_secs(240))
                .await
                .expect("request");

            store
                .claim_turn(&claim("k1", "replica-b"))
                .await
                .expect("successor");

            assert!(
                !store
                    .cancel_requested("k1", "replica-b")
                    .await
                    .expect("read"),
                "the new turn starts uncancelled"
            );
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_orphaned_claim_cannot_be_asked_to_stop() {
        with_store(|store| async move {
            store
                .claim_turn(&claim("k1", "replica-a"))
                .await
                .expect("claim");

            assert!(
                !store
                    .request_turn_cancel("k1", Duration::ZERO)
                    .await
                    .expect("request"),
                "a claim past the turn ceiling is an orphan, not something to interrupt"
            );
            assert!(
                !store
                    .cancel_requested("k1", "replica-a")
                    .await
                    .expect("read"),
                "and it is left unmarked"
            );

            assert!(
                !store
                    .request_turn_cancel("absent", Duration::from_secs(240))
                    .await
                    .expect("request"),
                "nothing registered means nothing to interrupt"
            );
        })
        .await;
    }

    // ─── tenant isolation ──────────────────────────────────────────────────

    /// The explicit tenant predicates alone keep one tenant's turns and claims
    /// out of another's store, before any row-level policy applies.
    #[tokio::test(flavor = "multi_thread")]
    async fn one_tenants_usage_and_claims_are_invisible_to_another() {
        with_test_pool(PostgresSessionStore::build_pool, |pool| async move {
            run_edge_analytics_migrations(&pool)
                .await
                .expect("run analytics migrations");
            let tenants = TenantPool::new(pool);
            let a = EdgeAnalyticsStore::new(tenants.clone(), TenantId::parse("a").unwrap());
            let b = EdgeAnalyticsStore::new(tenants, TenantId::parse("b").unwrap());
            a.record_usage(&sample_event(TurnOutcome::Ok, 100), None)
                .await
                .expect("record usage");
            a.claim_turn(&claim("k1", "replica-a"))
                .await
                .expect("claim");

            let now = now_micros();
            let query = AnalyticsQuery {
                from: now - chrono::Duration::days(1),
                to: now + chrono::Duration::days(1),
                agent: None,
                owner: None,
            };
            assert_eq!(a.summary(&query).await.expect("summary").turns, 1);
            assert_eq!(
                b.summary(&query).await.expect("summary").turns,
                0,
                "another tenant's turns never count"
            );
            assert_eq!(
                a.turn_owner("k1", Duration::from_secs(240))
                    .await
                    .expect("lookup")
                    .expect("a live claim")
                    .replica,
                "replica-a"
            );
            assert!(
                b.turn_owner("k1", Duration::from_secs(240))
                    .await
                    .expect("lookup")
                    .is_none(),
                "a turn key is scoped to its tenant"
            );

            b.claim_turn(&claim("k1", "replica-b"))
                .await
                .expect("claim");
            assert_eq!(
                a.turn_owner("k1", Duration::from_secs(240))
                    .await
                    .expect("lookup")
                    .expect("a live claim")
                    .replica,
                "replica-a",
                "the same key in two tenants is two claims"
            );
        })
        .await;
    }

    // ─── tool-evidence audit store ─────────────────────────────────────────

    fn evidence<'a>(message_id: &'a str, text: &'a str) -> ToolEvidence<'a> {
        ToolEvidence {
            message_id,
            conversation: "conv-1",
            owner: "owner-1",
            agent_id: "agent-1",
            org: None,
            evidence: text,
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn evidence_is_readable_back_for_the_answer_it_belongs_to() {
        with_store(|store| async move {
            store
                .record_tool_evidence(&evidence("m1", "data.query input=... output=1,756 leads"))
                .await
                .expect("record");

            assert_eq!(
                store.tool_evidence("m1").await.expect("read").as_deref(),
                Some("data.query input=... output=1,756 leads"),
                "an answer's grounding must be checkable after the turn has ended"
            );
            assert!(store.tool_evidence("absent").await.expect("read").is_none());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn re_recording_an_answer_refreshes_its_evidence() {
        with_store(|store| async move {
            store
                .record_tool_evidence(&evidence("m1", "first"))
                .await
                .expect("first");
            store
                .record_tool_evidence(&evidence("m1", "second"))
                .await
                .expect("second");

            assert_eq!(
                store.tool_evidence("m1").await.expect("read").as_deref(),
                Some("second"),
                "a re-recorded turn refreshes rather than colliding on the primary key"
            );
        })
        .await;
    }

    /// Retention is the whole reason storing this is acceptable: it is customer
    /// data, and it must not accumulate indefinitely.
    #[tokio::test(flavor = "multi_thread")]
    async fn evidence_past_its_retention_is_swept() {
        with_store(|store| async move {
            store
                .record_tool_evidence(&evidence("m1", "worker and shift data"))
                .await
                .expect("record");

            assert_eq!(
                store
                    .sweep_tool_evidence(Duration::from_secs(14 * 24 * 3600))
                    .await
                    .expect("sweep"),
                0,
                "evidence inside its retention stays"
            );
            assert_eq!(
                store
                    .sweep_tool_evidence(Duration::ZERO)
                    .await
                    .expect("sweep"),
                1,
                "evidence past its retention goes"
            );
            assert!(store.tool_evidence("m1").await.expect("read").is_none());
        })
        .await;
    }
}
