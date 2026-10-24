//! Tenancy: the tenants one deployment serves, and how every row is kept to
//! its tenant.
//!
//! A self-hosted install serves one tenant, `default`, created at boot; a
//! cloud deployment serves many, created through the platform API. Every
//! edge table carries `tenant_id`, every query names it, and row-level
//! security in Postgres is the backstop for a query that does not: a store
//! reaches the database only through [`TenantPool`], whose connections set
//! `app.tenant_id` for the transaction they hold, so a policy reading that
//! setting sees nothing when it is missing rather than everything.
//!
//! The edge's tables (agents, playbooks, operators, audit, revisions,
//! knowledge, the tenant registry) live in the `sessions` schema beside the
//! session store's, which the hub shares. They are versioned by this crate's
//! own migration set, with its own history table, so the two sets never
//! contend over one history.

use std::error::Error;
use std::fmt;
use std::net::SocketAddr;
use std::ops::Deref;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use bytes::BytesMut;
use chrono::{DateTime, Datelike, Utc};
use deadpool_postgres::Object;
use eyre::{eyre, Result, WrapErr};
use serde::{Deserialize, Serialize};
use tokio_postgres::types::{to_sql_checked, IsNull, ToSql, Type};
use workforce_common::postgres::{
    MigrationRun, MigrationRunner, MigrationSet, PostgresError, PostgresPool,
};
use workforce_memory::store::MemoryStore;
use workforce_memory::types::MemoryFilter;

use crate::admin::{AdminAuth, AdminConfig, OidcMode};
use crate::agent::EdgeAgent;
use crate::analytics::{EdgeAnalyticsStore, PeriodUsage};
use crate::boot::SessionRoot;
use crate::config::{
    AuditConfig, DemoConfig, EdgeConfig, GitConfig, GraderConfig, LlmConfig, TokenExchangeConfig,
};
use crate::ingress::{client_ip, request_host};
use crate::outbound::OutboundPolicy;
use crate::runtime::TenantRuntime;
use crate::server::EdgeState;
use crate::settings::{LlmStatus, StoredSettings};

mod embedded {
    refinery::embed_migrations!("migrations/edge");
}

/// A tenant's id: a DNS-label-shaped slug, as the `tenants.id` check
/// constraint requires, so it can name a hostname and a vellev tenant.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct TenantId(String);

impl TenantId {
    /// The one tenant a self-hosted install serves.
    pub const DEFAULT: &'static str = "default";

    const MAX_LEN: usize = 63;

    /// Ids the platform keeps for itself: names a product hostname or a
    /// route is built on, and [`Self::DEFAULT`], which only boot creates.
    /// Each is a valid slug; the platform API refuses to create them.
    const RESERVED: &'static [&'static str] = &[
        "app",
        "api",
        "auth",
        "edge",
        "www",
        "mail",
        "admin",
        "platform",
        "status",
        "docs",
        "help",
        "support",
        "billing",
        "portal",
        "login",
        "signup",
        "marrilo",
        Self::DEFAULT,
    ];

    pub fn default_tenant() -> Self {
        Self(Self::DEFAULT.to_string())
    }

    /// A slug of at most 63 lowercase letters, digits and hyphens, not
    /// starting with a hyphen.
    pub fn parse(slug: &str) -> Result<Self> {
        let shaped = !slug.is_empty()
            && slug.len() <= Self::MAX_LEN
            && !slug.starts_with('-')
            && slug
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if shaped {
            Ok(Self(slug.to_string()))
        } else {
            Err(eyre!(
                "tenant id {slug:?} is not a slug of up to {} lowercase letters, digits and hyphens",
                Self::MAX_LEN
            ))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_default(&self) -> bool {
        self.0 == Self::DEFAULT
    }

    /// Whether the id is one of [`Self::RESERVED`]: a slug the platform
    /// will not hand to a tenant.
    pub fn is_reserved(&self) -> bool {
        Self::RESERVED.contains(&self.0.as_str())
    }
}

impl fmt::Display for TenantId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for TenantId {
    type Error = eyre::Report;

    fn try_from(slug: String) -> Result<Self> {
        Self::parse(&slug)
    }
}

impl From<TenantId> for String {
    fn from(id: TenantId) -> Self {
        id.0
    }
}

/// Binds as its slug, so a store names its tenant in a parameter list as it
/// is.
impl ToSql for TenantId {
    fn to_sql(
        &self,
        ty: &Type,
        out: &mut BytesMut,
    ) -> Result<IsNull, Box<dyn Error + Sync + Send>> {
        self.0.to_sql(ty, out)
    }

    fn accepts(ty: &Type) -> bool {
        <String as ToSql>::accepts(ty)
    }

    to_sql_checked!();
}

/// The pool the edge's stores query through. It hands out connections only
/// inside a transaction that names a tenant, so no store can run a query
/// without the row-level-security setting in place.
#[derive(Clone)]
pub struct TenantPool {
    pool: PostgresPool,
}

impl TenantPool {
    pub fn new(pool: PostgresPool) -> Self {
        Self { pool }
    }

    /// A connection inside a transaction scoped to `tenant`. Queries run on
    /// it through [`Deref`]; [`TenantConn::commit`] ends it. Dropping it
    /// uncommitted rolls the transaction back by closing the connection,
    /// so a half-done write and the setting never reach another checkout.
    pub async fn conn(&self, tenant: &TenantId) -> Result<TenantConn> {
        let conn = self.pool.get().await?;
        conn.batch_execute("BEGIN")
            .await
            .wrap_err("beginning a tenant transaction")?;
        let scoped = async {
            conn.execute(
                "SELECT set_config('app.tenant_id', $1, true)",
                &[&tenant.as_str()],
            )
            .await
            .wrap_err("scoping a transaction to its tenant")
        }
        .await;
        match scoped {
            Ok(_) => Ok(TenantConn { conn: Some(conn) }),
            Err(e) => {
                drop(Object::take(conn));
                Err(e)
            }
        }
    }
}

/// A pooled connection inside a tenant-scoped transaction.
pub struct TenantConn {
    conn: Option<Object>,
}

impl TenantConn {
    /// Commit the transaction and return the connection to the pool.
    pub async fn commit(mut self) -> Result<()> {
        let conn = self.conn.take().expect("a live connection until commit");
        match conn.batch_execute("COMMIT").await {
            Ok(()) => Ok(()),
            Err(e) => {
                drop(Object::take(conn));
                Err(e).wrap_err("committing a tenant transaction")
            }
        }
    }
}

impl Deref for TenantConn {
    type Target = Object;

    fn deref(&self) -> &Object {
        self.conn.as_ref().expect("a live connection until commit")
    }
}

impl Drop for TenantConn {
    fn drop(&mut self) {
        // An uncommitted transaction must not be recycled into the pool:
        // taking the client out of the pool closes it, which rolls back.
        if let Some(conn) = self.conn.take() {
            drop(Object::take(conn));
        }
    }
}

/// How a tenant is served.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TenantStatus {
    Active,
    Suspended,
}

impl TenantStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Suspended => "suspended",
        }
    }

    fn parse(status: &str) -> Result<Self> {
        match status {
            "active" => Ok(Self::Active),
            "suspended" => Ok(Self::Suspended),
            other => Err(eyre!("unknown tenant status {other:?}")),
        }
    }
}

/// The caps a tenant's plan sets, each absent when the plan leaves it
/// unbounded. Set through the platform API by the billing package; held on
/// the registry row. `max_concurrent_turns`, `rate_limit_per_min` and
/// `user_rate_limit_per_min` are ceilings over the tenant's own limits
/// ([`TenantLimits`]): the tenant's settings may ask for less, never more.
/// The monthly turn and user caps, the knowledge and memory caps bind
/// through [`PlanUsage`]; the agent cap binds where an agent is created or
/// imported, on the count of mounted agents ([`Plan::admit_agents`]).
/// `upload_mb` is stored but not metered: nothing counts a tenant's upload
/// bytes yet, so the per-agent `upload_max_mb` stays the only cap on
/// uploads.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turns_per_month: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_users: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub knowledge_chunks: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_objects: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upload_mb: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrent_turns: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit_per_min: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_rate_limit_per_min: Option<u32>,
}

impl Plan {
    /// `limits` held under this plan's ceilings: each limit the plan caps is
    /// the lesser of the two, and never below 1.
    pub fn cap_limits(&self, limits: TenantLimits) -> TenantLimits {
        TenantLimits {
            rate_limit_per_min: match self.rate_limit_per_min {
                Some(cap) => limits.rate_limit_per_min.min(cap.max(1)),
                None => limits.rate_limit_per_min,
            },
            user_rate_limit_per_min: match self.user_rate_limit_per_min {
                Some(cap) => limits.user_rate_limit_per_min.min(cap.max(1)),
                None => limits.user_rate_limit_per_min,
            },
            max_concurrent_turns: match self.max_concurrent_turns {
                Some(cap) => limits.max_concurrent_turns.min(cap.max(1)),
                None => limits.max_concurrent_turns,
            },
        }
    }

    /// The first limit in `limits` above this plan's ceiling for it, by
    /// field name — what a settings save is refused for.
    pub fn limit_over_ceiling(&self, limits: &TenantLimits) -> Option<&'static str> {
        if self
            .rate_limit_per_min
            .is_some_and(|cap| limits.rate_limit_per_min > cap)
        {
            return Some("rate_limit_per_min");
        }
        if self
            .user_rate_limit_per_min
            .is_some_and(|cap| limits.user_rate_limit_per_min > cap)
        {
            return Some("user_rate_limit_per_min");
        }
        if self
            .max_concurrent_turns
            .is_some_and(|cap| limits.max_concurrent_turns > cap)
        {
            return Some("max_concurrent_turns");
        }
        None
    }

    /// Whether the tenant may hold `total` agents under the agent cap — the
    /// count it would have once a create or import lands, not the writes:
    /// an agent an import replaces in place is not a new one. The error is
    /// the sentence the admin API refuses with; it names the cap, which the
    /// operator (unlike a customer) is entitled to see.
    pub fn admit_agents(&self, total: usize) -> std::result::Result<(), String> {
        match self.agents {
            Some(cap) if total > cap as usize => Err(format!(
                "the plan allows {cap} agent(s); the workspace would have {total}"
            )),
            _ => Ok(()),
        }
    }

    /// The plan fields `usage` has reached, by name, for the admin banner.
    /// A cap is reached at the number, not past it: that is when the next
    /// turn, user, document or agent is refused. Upload bytes are not
    /// metered, so `upload_mb` never appears; `max_concurrent_turns` binds
    /// on the semaphore and is not a counter.
    pub fn over(&self, usage: &Usage) -> Vec<&'static str> {
        let reached = |cap: Option<u64>, used: u64| cap.is_some_and(|cap| used >= cap);
        let mut over = Vec::new();
        if reached(self.turns_per_month, usage.turns_this_month) {
            over.push(PlanLimit::Turns.field());
        }
        if reached(self.active_users, usage.active_users_this_month) {
            over.push(PlanLimit::ActiveUsers.field());
        }
        if reached(self.knowledge_chunks, usage.knowledge_chunks) {
            over.push(PlanLimit::KnowledgeChunks.field());
        }
        if usage
            .memory_objects
            .is_some_and(|objects| reached(self.memory_objects, objects))
        {
            over.push(PlanLimit::MemoryObjects.field());
        }
        if reached(self.agents.map(u64::from), usage.agents) {
            over.push(PlanLimit::Agents.field());
        }
        over
    }
}

/// What a tenant has consumed of what its plan caps, as the admin's usage
/// card and the platform view show it. Counted from the tenant's own stores
/// by [`PlanUsage`]; the monthly figures restart on the first of each UTC
/// month.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Usage {
    /// Customer turns answered since the start of the month: `usage_events`
    /// rows with purpose `turn`, whatever their outcome.
    pub turns_this_month: u64,
    /// Distinct end-users seen this month, by `chat_identities.last_seen`.
    pub active_users_this_month: u64,
    /// Embedded chunks across every agent's knowledge corpus.
    pub knowledge_chunks: u64,
    /// Objects in the tenant's vector store — customer memories, lessons and
    /// knowledge chunks alike. Counted only for a plan that caps them;
    /// `None` otherwise, and when the store could not be counted, which the
    /// log reports — a cap cannot bind on an unknown count.
    pub memory_objects: Option<u64>,
    /// Agents mounted on the tenant. Hosts are the runtime's, not a store's,
    /// so the runtime hands the count to [`PlanUsage::usage`].
    pub agents: u64,
}

/// The plan cap that refused a write. Names which, so the widget's message,
/// the admin API's response and the log all say the same thing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanLimit {
    Turns,
    ActiveUsers,
    KnowledgeChunks,
    MemoryObjects,
    Agents,
}

impl PlanLimit {
    /// The error code every surface carries a refusal under: the widget's
    /// error frame and the admin API's 422.
    pub const CODE: &'static str = "plan_limit";

    /// The plan field, as `/admin/api/me` lists it under `over`.
    pub fn field(self) -> &'static str {
        match self {
            Self::Turns => "turns_per_month",
            Self::ActiveUsers => "active_users",
            Self::KnowledgeChunks => "knowledge_chunks",
            Self::MemoryObjects => "memory_objects",
            Self::Agents => "agents",
        }
    }

    /// What the person refused is told: what happened and that the
    /// workspace, not they, must act — never the number.
    pub fn message(self) -> &'static str {
        match self {
            Self::Turns => "This workspace has used its monthly turn allowance.",
            Self::ActiveUsers => "This workspace has reached its monthly limit of active users.",
            Self::KnowledgeChunks => {
                "This workspace has reached its knowledge limit. Remove a document or raise \
                 the plan before adding more."
            }
            Self::MemoryObjects => "This workspace has reached its memory limit.",
            Self::Agents => "This workspace has reached its plan's limit of agents.",
        }
    }
}

impl fmt::Display for PlanLimit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "plan limit reached: {}", self.field())
    }
}

impl Error for PlanLimit {}

/// How long a usage snapshot is served before it is recounted. Short enough
/// that a figure the admin reads is current; the writes that move a counter
/// drop the snapshot anyway, so a cap binds on the write after it is
/// reached rather than a minute later.
const USAGE_CACHE_TTL: Duration = Duration::from_secs(60);

/// Ceiling on counting the vector store, which a turn's admission waits on:
/// a slow store leaves the memory figure unknown rather than holding the
/// customer.
const MEMORY_COUNT_TIMEOUT: Duration = Duration::from_secs(5);

/// A tenant's plan beside its usage, counted from the tenant's own stores
/// and cached for a minute: what admits a turn, a knowledge document and a
/// memory under the plan. Shared by the tenant's runtime, its hosts and its
/// knowledge store, each of which drops the snapshot after the write it
/// makes. An empty plan admits everything without a query.
///
/// The figures are counted apart — the month's turns and users in one
/// query, the corpus in another, the vector store over the network — so a
/// caller pays for the ones it asks about: a turn's admission reads the
/// month alone, and the vector store is counted only for a plan that caps
/// it. Each count is single-flight, so a burst of turns ending together
/// recounts once.
pub struct PlanUsage {
    plan: Plan,
    tenant: TenantId,
    pool: TenantPool,
    analytics: Arc<EdgeAnalyticsStore>,
    memory: Arc<dyn MemoryStore>,
    period: Counter<PeriodUsage>,
    chunks: Counter<u64>,
    memory_objects: Counter<Option<u64>>,
}

/// One cached figure: the snapshot, a generation that every invalidation
/// advances so a count that began before a write cannot be stored after
/// it, and the flight lock that lets one count run at a time.
struct Counter<T> {
    cache: std::sync::Mutex<Cached<T>>,
    flight: tokio::sync::Mutex<()>,
}

struct Cached<T> {
    generation: u64,
    snapshot: Option<(Instant, T)>,
}

impl<T: Clone> Counter<T> {
    fn new() -> Self {
        Self {
            cache: std::sync::Mutex::new(Cached {
                generation: 0,
                snapshot: None,
            }),
            flight: tokio::sync::Mutex::new(()),
        }
    }

    /// The snapshot while it is fresh, else the generation a recount
    /// would store under.
    fn fresh(&self) -> std::result::Result<T, u64> {
        let cache = self.cache.lock().expect("usage cache");
        match &cache.snapshot {
            Some((counted_at, value)) if counted_at.elapsed() < USAGE_CACHE_TTL => {
                Ok(value.clone())
            }
            _ => Err(cache.generation),
        }
    }

    /// The figure, recounted by `count` when the snapshot is older than a
    /// minute or a write dropped it. A caller that finds a count in flight
    /// waits for it and shares its result.
    async fn get<F, Fut>(&self, count: F) -> Result<T>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        if let Ok(value) = self.fresh() {
            return Ok(value);
        }
        let _flight = self.flight.lock().await;
        let generation = match self.fresh() {
            Ok(value) => return Ok(value),
            Err(generation) => generation,
        };
        let value = count().await?;
        let mut cache = self.cache.lock().expect("usage cache");
        if cache.generation == generation {
            cache.snapshot = Some((Instant::now(), value.clone()));
        }
        Ok(value)
    }

    fn invalidate(&self) {
        let mut cache = self.cache.lock().expect("usage cache");
        cache.generation += 1;
        cache.snapshot = None;
    }
}

impl PlanUsage {
    pub fn new(
        plan: Plan,
        tenant: TenantId,
        pool: TenantPool,
        analytics: Arc<EdgeAnalyticsStore>,
        memory: Arc<dyn MemoryStore>,
    ) -> Self {
        Self {
            plan,
            tenant,
            pool,
            analytics,
            memory,
            period: Counter::new(),
            chunks: Counter::new(),
            memory_objects: Counter::new(),
        }
    }

    pub fn plan(&self) -> &Plan {
        &self.plan
    }

    /// The tenant's usage for the admin's card and the platform view: every
    /// figure, recounted when its snapshot is older than a minute or a
    /// write dropped it. The vector store is counted only when the plan
    /// caps it; otherwise `memory_objects` is `None`. `agents` is the
    /// caller's count of mounted hosts, which no store holds.
    pub async fn usage(&self, agents: usize) -> Result<Usage> {
        let period = self.period().await?;
        let knowledge_chunks = self
            .chunks
            .get(|| crate::knowledge::total_chunks(&self.pool, &self.tenant))
            .await?;
        let memory_objects = if self.plan.memory_objects.is_some() {
            self.memory_objects().await?
        } else {
            None
        };
        Ok(Usage {
            turns_this_month: period.turns,
            active_users_this_month: period.users,
            knowledge_chunks,
            memory_objects,
            agents: agents as u64,
        })
    }

    /// Drop every snapshot: the next read recounts. Called by the writes
    /// that move a counter — a turn ending, a knowledge document changing.
    pub fn invalidate(&self) {
        self.period.invalidate();
        self.chunks.invalidate();
        self.memory_objects.invalidate();
    }

    /// The month's turns and users.
    async fn period(&self) -> Result<PeriodUsage> {
        self.period
            .get(|| self.analytics.usage_since(month_start(Utc::now())))
            .await
    }

    /// The vector store's object count; `None` when it could not be
    /// counted, which the log reports.
    async fn memory_objects(&self) -> Result<Option<u64>> {
        self.memory_objects
            .get(|| async {
                let counted = tokio::time::timeout(
                    MEMORY_COUNT_TIMEOUT,
                    self.memory.count(MemoryFilter::system_unscoped()),
                )
                .await;
                Ok(match counted {
                    Ok(Ok(objects)) => Some(objects as u64),
                    Ok(Err(e)) => {
                        tracing::warn!(
                            tenant = %self.tenant, error = %e,
                            "plan usage: the vector store could not be counted; its cap cannot bind"
                        );
                        None
                    }
                    Err(_) => {
                        tracing::warn!(
                            tenant = %self.tenant,
                            "plan usage: counting the vector store timed out; its cap cannot bind"
                        );
                        None
                    }
                })
            })
            .await
    }

    /// Whether `owner` may start a turn under the monthly caps: refused once
    /// the month's turns are used, or when they are a user the month has
    /// not seen and the user cap is full. Only the month is counted — the
    /// corpus and the vector store have no say in a turn's admission. A
    /// count that fails admits the turn and says so in the log — the
    /// database is also what would record the turn, so refusing on its
    /// failure would stop every customer for a fault that is not the plan's.
    pub async fn admit_turn(&self, owner: &str) -> std::result::Result<(), PlanLimit> {
        if self.plan.turns_per_month.is_none() && self.plan.active_users.is_none() {
            return Ok(());
        }
        let period = match self.period().await {
            Ok(period) => period,
            Err(e) => {
                tracing::warn!(
                    tenant = %self.tenant, error = %e,
                    "plan usage could not be counted; admitting the turn"
                );
                return Ok(());
            }
        };
        if self
            .plan
            .turns_per_month
            .is_some_and(|cap| period.turns >= cap)
        {
            return Err(PlanLimit::Turns);
        }
        if self
            .plan
            .active_users
            .is_some_and(|cap| period.users >= cap)
        {
            match self
                .analytics
                .seen_since(owner, month_start(Utc::now()))
                .await
            {
                Ok(true) => {}
                Ok(false) => return Err(PlanLimit::ActiveUsers),
                Err(e) => tracing::warn!(
                    tenant = %self.tenant, error = %e,
                    "plan usage: the user's activity could not be read; admitting the turn"
                ),
            }
        }
        Ok(())
    }

    /// Whether a knowledge corpus of `chunks_after` chunks fits the plan.
    pub fn admit_chunks(&self, chunks_after: u64) -> std::result::Result<(), PlanLimit> {
        match self.plan.knowledge_chunks {
            Some(cap) if chunks_after > cap => Err(PlanLimit::KnowledgeChunks),
            _ => Ok(()),
        }
    }

    /// Whether the vector store holds as many objects as the plan allows,
    /// so a turn keeps no new memory or lesson. False without a cap, and
    /// when the store could not be counted.
    pub async fn memory_at_cap(&self) -> bool {
        let Some(cap) = self.plan.memory_objects else {
            return false;
        };
        match self.memory_objects().await {
            Ok(objects) => objects.is_some_and(|objects| objects >= cap),
            Err(e) => {
                tracing::warn!(
                    tenant = %self.tenant, error = %e,
                    "plan usage could not be counted; memory is not capped this turn"
                );
                false
            }
        }
    }
}

/// Midnight UTC on the first of `now`'s month: where the monthly counters
/// restart.
pub fn month_start(now: DateTime<Utc>) -> DateTime<Utc> {
    now.date_naive()
        .with_day(1)
        .expect("every month has a first day")
        .and_hms_opt(0, 0, 0)
        .expect("midnight exists")
        .and_utc()
}

/// One tenant of the deployment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Tenant {
    pub id: TenantId,
    pub display_name: String,
    pub status: TenantStatus,
    pub hostnames: Vec<String>,
    pub plan: Plan,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// The edge tables that reference `tenants (id)` without a cascade, in an
/// order that satisfies their own references; a tenant's rows in each go
/// before its registry row does. The session and analytics tables are the
/// shared schemas' and carry no foreign key, but their rows are the
/// tenant's too and would otherwise outlive it.
const TENANT_OWNED_TABLES: &[&str] = &[
    "edge_knowledge",
    "edge_revisions",
    "edge_audit",
    "edge_operators",
    "edge_playbooks",
    "edge_agents",
    "turn_journal",
    "turn_claims",
    "session_archive",
    "sessions",
    "edge_analytics.turn_tool_evidence",
    "edge_analytics.active_turns",
    "edge_analytics.answer_grades",
    "edge_analytics.feedback",
    "edge_analytics.usage_events",
    "edge_analytics.chat_identities",
];

/// The tenant registry: `tenants` and `tenant_hostnames`. Read before a
/// request's tenant is known, so it queries the pool directly and its tables
/// carry no row-level policy.
#[derive(Clone)]
pub struct TenantStore {
    pool: PostgresPool,
}

impl TenantStore {
    pub fn new(pool: PostgresPool) -> Self {
        Self { pool }
    }

    /// The `default` tenant, created if the registry lacks it.
    pub async fn ensure_default(&self) -> Result<()> {
        let conn = self.pool.get().await?;
        conn.execute(
            "INSERT INTO tenants (id, display_name) VALUES ($1, 'Default') \
             ON CONFLICT (id) DO NOTHING",
            &[&TenantId::DEFAULT],
        )
        .await
        .wrap_err("ensuring the default tenant")?;
        Ok(())
    }

    pub async fn create(&self, id: &TenantId, display_name: &str) -> Result<Tenant> {
        let conn = self.pool.get().await?;
        conn.execute(
            "INSERT INTO tenants (id, display_name) VALUES ($1, $2)",
            &[&id.as_str(), &display_name],
        )
        .await
        .wrap_err_with(|| format!("creating tenant {id}"))?;
        drop(conn);
        self.get(id)
            .await?
            .ok_or_else(|| eyre!("tenant {id} vanished after creation"))
    }

    pub async fn get(&self, id: &TenantId) -> Result<Option<Tenant>> {
        let conn = self.pool.get().await?;
        let row = conn
            .query_opt(
                &format!("{TENANT_COLUMNS} FROM tenants t WHERE id = $1"),
                &[&id.as_str()],
            )
            .await
            .wrap_err_with(|| format!("reading tenant {id}"))?;
        row.map(tenant_from_row).transpose()
    }

    pub async fn list(&self) -> Result<Vec<Tenant>> {
        let conn = self.pool.get().await?;
        conn.query(&format!("{TENANT_COLUMNS} FROM tenants t ORDER BY id"), &[])
            .await
            .wrap_err("listing tenants")?
            .into_iter()
            .map(tenant_from_row)
            .collect()
    }

    pub async fn set_display_name(&self, id: &TenantId, display_name: &str) -> Result<bool> {
        let conn = self.pool.get().await?;
        let changed = conn
            .execute(
                "UPDATE tenants SET display_name = $2, updated_at = now() WHERE id = $1",
                &[&id.as_str(), &display_name],
            )
            .await
            .wrap_err_with(|| format!("renaming tenant {id}"))?;
        Ok(changed == 1)
    }

    pub async fn set_plan(&self, id: &TenantId, plan: &Plan) -> Result<bool> {
        let conn = self.pool.get().await?;
        let plan = serde_json::to_value(plan).wrap_err("encoding a plan")?;
        let changed = conn
            .execute(
                "UPDATE tenants SET plan = $2, updated_at = now() WHERE id = $1",
                &[&id.as_str(), &plan],
            )
            .await
            .wrap_err_with(|| format!("setting the plan of tenant {id}"))?;
        Ok(changed == 1)
    }

    /// Remove a tenant and every row of its own, in one transaction: its
    /// rows in each edge, session and analytics table go first, through a
    /// connection scoped to it so the policies see them, then its registry
    /// row, which cascades to its settings and hostnames. Returns whether
    /// there was one. `default` is never removed: it is the tenant a
    /// self-hosted install is, and the one the install's own store serves.
    pub async fn delete(&self, id: &TenantId) -> Result<bool> {
        if id.is_default() {
            return Err(eyre!("the default tenant cannot be deleted"));
        }
        let conn = TenantPool::new(self.pool.clone()).conn(id).await?;
        for table in TENANT_OWNED_TABLES {
            conn.execute(
                &format!("DELETE FROM {table} WHERE tenant_id = $1"),
                &[&id.as_str()],
            )
            .await
            .wrap_err_with(|| format!("deleting tenant {id}'s rows from {table}"))?;
        }
        let deleted = conn
            .execute("DELETE FROM tenants WHERE id = $1", &[&id.as_str()])
            .await
            .wrap_err_with(|| format!("deleting tenant {id}"))?;
        conn.commit().await?;
        Ok(deleted == 1)
    }

    pub async fn set_status(&self, id: &TenantId, status: TenantStatus) -> Result<bool> {
        let conn = self.pool.get().await?;
        let changed = conn
            .execute(
                "UPDATE tenants SET status = $2, updated_at = now() WHERE id = $1",
                &[&id.as_str(), &status.as_str()],
            )
            .await
            .wrap_err_with(|| format!("setting tenant {id} {}", status.as_str()))?;
        Ok(changed == 1)
    }

    /// Register `hostname` as reaching `id`. A hostname names one tenant; a
    /// second registration of it is an error, not a transfer.
    pub async fn add_hostname(&self, id: &TenantId, hostname: &str) -> Result<()> {
        let conn = self.pool.get().await?;
        conn.execute(
            "INSERT INTO tenant_hostnames (hostname, tenant_id) VALUES ($1, $2)",
            &[&hostname.to_ascii_lowercase(), &id.as_str()],
        )
        .await
        .wrap_err_with(|| format!("adding hostname {hostname} to tenant {id}"))?;
        Ok(())
    }

    pub async fn remove_hostname(&self, hostname: &str) -> Result<bool> {
        let conn = self.pool.get().await?;
        let removed = conn
            .execute(
                "DELETE FROM tenant_hostnames WHERE hostname = $1",
                &[&hostname.to_ascii_lowercase()],
            )
            .await
            .wrap_err_with(|| format!("removing hostname {hostname}"))?;
        Ok(removed == 1)
    }

    /// The tenant `hostname` reaches, if any.
    pub async fn resolve_hostname(&self, hostname: &str) -> Result<Option<TenantId>> {
        let conn = self.pool.get().await?;
        let row = conn
            .query_opt(
                "SELECT tenant_id FROM tenant_hostnames WHERE hostname = $1",
                &[&hostname.to_ascii_lowercase()],
            )
            .await
            .wrap_err_with(|| format!("resolving hostname {hostname}"))?;
        row.map(|row| TenantId::parse(row.get(0))).transpose()
    }
}

/// Whether the pool's role is subject to row-level security. A superuser or
/// a `BYPASSRLS` role reads every tenant's rows whatever the policies say,
/// so a deployment serving more than one tenant must not run as one.
pub async fn rls_enforced(pool: &PostgresPool) -> Result<bool> {
    let conn = pool.get().await?;
    let bypasses: bool = conn
        .query_one(
            "SELECT rolsuper OR rolbypassrls FROM pg_roles WHERE rolname = current_user",
            &[],
        )
        .await
        .wrap_err("reading the database role's row-level-security standing")?
        .get(0);
    Ok(!bypasses)
}

/// The registry row as [`tenant_from_row`] reads it.
const TENANT_COLUMNS: &str = "SELECT id, display_name, status, created_at, updated_at, \
        (SELECT coalesce(array_agg(hostname ORDER BY hostname), '{}') \
         FROM tenant_hostnames h WHERE h.tenant_id = t.id), \
        plan";

fn tenant_from_row(row: tokio_postgres::Row) -> Result<Tenant> {
    let plan: serde_json::Value = row.get(6);
    Ok(Tenant {
        id: TenantId::parse(row.get(0))?,
        display_name: row.get(1),
        status: TenantStatus::parse(row.get(2))?,
        created_at: row.get(3),
        updated_at: row.get(4),
        hostnames: row.get(5),
        plan: serde_json::from_value(plan).wrap_err("decoding a tenant's plan")?,
    })
}

/// Everything a tenant is configured with: `edge.toml`'s sections, which
/// every tenant copies, overlaid section by section with the tenant's
/// settings row when the install keeps one (see [`Self::for_tenant`]).
#[derive(Clone)]
pub struct TenantSettings {
    pub id: TenantId,
    pub name: String,
    /// The hostnames that reach this tenant; empty for a self-hosted install,
    /// which answers on any host.
    pub hostnames: Vec<String>,
    pub admin: Option<AdminConfig>,
    pub llm: LlmConfig,
    /// The key's last known state, from the settings row; `None` without one.
    pub llm_status: Option<LlmStatus>,
    pub token_exchange: Option<TokenExchangeConfig>,
    pub grader: Option<GraderConfig>,
    pub audit: Option<AuditConfig>,
    pub git_config: Option<GitConfig>,
    pub demo: DemoConfig,
    /// The tenant's share of the process. `max_concurrent_turns` is already
    /// capped by the plan's, when the plan sets one.
    pub limits: TenantLimits,
    /// The registry row's plan; empty for a self-hosted install.
    pub plan: Plan,
    /// How the tenant's own URLs (issuer, admin IdP, MCP, permissions, token
    /// exchange) are reached: the settings row's `outbound` when the tenant
    /// has one, else the file's `[server] default_outbound`. The file has no
    /// per-tenant field, so a self-hosted install stays `Private`.
    pub outbound: OutboundPolicy,
    /// Agents seeded into the store on the tenant's first boot.
    pub seed_agents: Vec<EdgeAgent>,
    pub playbooks_dir: Option<String>,
}

/// A tenant's share of the process: request and turn budgets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TenantLimits {
    pub rate_limit_per_min: u32,
    pub user_rate_limit_per_min: u32,
    pub max_concurrent_turns: usize,
}

impl TenantSettings {
    /// The one tenant a self-hosted install serves: `default`, reachable on
    /// any host, configured entirely by the file.
    pub fn from_edge_toml(cfg: &EdgeConfig) -> Self {
        Self::from_sections(
            TenantId::default_tenant(),
            "Default".to_string(),
            vec![],
            cfg,
        )
    }

    /// A registered tenant's settings. Identity and hostnames come from its
    /// registry row; every section starts as the file's copy, and a settings
    /// row overrides it section by section: a section the row holds replaces
    /// the file's whole, one it leaves null stays the file's. Secrets come
    /// from the row's sealed columns when set, else from the file, so a
    /// self-hosted install whose row has no key still runs on the file's.
    /// What is never in a row — the git config, the demo page, the seed
    /// agents, the provider allowlist, the break-glass and session secrets —
    /// is the file's alone.
    pub fn for_tenant(tenant: &Tenant, cfg: &EdgeConfig, stored: Option<&StoredSettings>) -> Self {
        let mut settings = Self::from_sections(
            tenant.id.clone(),
            tenant.display_name.clone(),
            tenant.hostnames.clone(),
            cfg,
        );
        if let Some(stored) = stored {
            settings.apply_stored(stored, cfg);
        }
        // The plan is a ceiling over whatever the file or the row asked for,
        // never a raise: a tenant on a small plan stays under it however its
        // own settings read.
        settings.limits = tenant.plan.cap_limits(settings.limits);
        settings.plan = tenant.plan.clone();
        settings
    }

    /// Overlay the row's sections. A secret the row leaves blank falls back
    /// to the file's for `default` alone — the self-hosted tenant, whose
    /// file is its own. Any other tenant is one of many sharing the file,
    /// and the file's LLM key, token-exchange secret, OIDC client secret
    /// and grader key are never its to inherit: a blank stays blank, and
    /// multi-tenant boot refuses a file that carries them at all
    /// ([`EdgeConfig::shared_file_secret`]).
    fn apply_stored(&mut self, stored: &StoredSettings, cfg: &EdgeConfig) {
        let inherits = self.id.is_default();
        let mut llm = stored.llm.clone();
        if inherits && llm.api_key.as_deref().is_none_or(str::is_empty) {
            llm.api_key = cfg.llm.api_key.clone();
        }
        llm.allowed_providers = cfg.llm.allowed_providers.clone();
        self.llm = llm;
        self.llm_status = stored.llm_status.clone();
        if let Some(grader) = &stored.grader {
            // The grader's own key is not a sealed column yet: a stored grader
            // that routes to another provider runs on the file's key, if any
            // and if this is the tenant the file belongs to.
            let mut grader = grader.clone();
            grader.api_key = cfg
                .grader
                .as_ref()
                .filter(|_| inherits)
                .and_then(|file| file.api_key.clone());
            self.grader = Some(grader);
        }
        if let Some(token_exchange) = &stored.token_exchange {
            let mut token_exchange = token_exchange.clone();
            if inherits && token_exchange.client_secret.is_empty() {
                token_exchange.client_secret = cfg
                    .token_exchange
                    .as_ref()
                    .map(|file| file.client_secret.clone())
                    .unwrap_or_default();
            }
            self.token_exchange = Some(token_exchange);
        }
        if let Some(admin) = &stored.admin {
            let mut admin = admin.clone();
            let file = cfg.admin.as_ref();
            admin.password_hash = file.and_then(|file| file.password_hash.clone());
            admin.session_secret = file
                .and_then(|file| file.session_secret.as_deref())
                .map(|secret| derive_session_secret(secret, &self.id));
            if let Some(oidc) = admin.oidc.as_mut() {
                if inherits && oidc.client_secret.as_deref().is_none_or(str::is_empty) {
                    oidc.client_secret = file
                        .and_then(|file| file.oidc.as_ref())
                        .and_then(|file| file.client_secret.clone());
                }
            }
            self.admin = Some(admin);
        }
        self.limits = stored.limits;
        if let Some(audit) = &stored.audit {
            self.audit = Some(audit.clone());
        }
        self.outbound = stored.outbound;
    }

    fn from_sections(id: TenantId, name: String, hostnames: Vec<String>, cfg: &EdgeConfig) -> Self {
        Self {
            admin: cfg.admin.as_ref().map(|admin| tenant_admin(admin, &id)),
            id,
            name,
            hostnames,
            llm: cfg.llm.clone(),
            llm_status: None,
            token_exchange: cfg.token_exchange.clone(),
            grader: cfg.grader.clone(),
            audit: cfg.audit.clone(),
            git_config: cfg.git_config.clone(),
            demo: cfg.demo.clone(),
            limits: TenantLimits {
                rate_limit_per_min: cfg.server.rate_limit_per_min,
                user_rate_limit_per_min: cfg.server.user_rate_limit_per_min,
                max_concurrent_turns: cfg.server.max_concurrent_turns.max(1),
            },
            plan: Plan::default(),
            outbound: cfg.server.default_outbound,
            seed_agents: cfg.agents.clone(),
            playbooks_dir: cfg.playbooks_dir.clone(),
        }
    }

    /// The `[admin]` the tenant's authenticator is built on: its own, with
    /// the session key settled. The file's `[admin].session_secret` wins
    /// when set (already derived for this tenant); without one the key is
    /// derived for this tenant from the install's session root — the master
    /// key's, so a reload signs under the same key and no save signs the
    /// operator out; without that either, the authenticator's random
    /// per-boot key stands, as it always has for a single-tenant install.
    pub fn admin_with_session_key(
        &self,
        session_root: Option<&SessionRoot>,
    ) -> Option<AdminConfig> {
        let mut admin = self.admin.clone()?;
        if admin.session_secret.is_none() {
            admin.session_secret = session_root.map(|root| root.tenant_session_secret(&self.id));
        }
        Some(admin)
    }

    /// Whether the file's `[admin]` can stand in for every tenant's, which
    /// it does until the settings table carries one per tenant. A break-glass
    /// password or a redirect-mode OIDC client is install-wide by nature —
    /// one password, one registered `redirect_uri` — so sharing it would let
    /// one tenant's operator onto every tenant's admin; the Google-button
    /// mode posts a token the SPA obtained, which the operator allowlist
    /// then binds to the tenant it was posted to.
    pub fn admin_shareable(admin: &AdminConfig) -> Result<()> {
        if admin.password_hash.is_some() {
            return Err(eyre!(
                "server.multi_tenant cannot share [admin].password_hash across tenants: one \
                 break-glass password would open every tenant's admin. Remove it until admin \
                 settings are held per tenant."
            ));
        }
        let auth = AdminAuth::new(admin, TenantId::default_tenant(), OutboundPolicy::default())?;
        if auth.oidc_mode() == Some(OidcMode::Redirect) {
            return Err(eyre!(
                "server.multi_tenant cannot share [admin.oidc] in redirect mode across tenants: \
                 its client_id and public_url name one install. Use mode = \"google\" (the \
                 token is posted by the admin SPA) until admin settings are held per tenant."
            ));
        }
        Ok(())
    }
}

/// The file's `[admin]` as `tenant` gets it. The session key is derived from
/// the install secret and the tenant id, so a session minted for one tenant
/// does not even verify on another — the tenant claim is then a second
/// check, not the only one. Only `default` keeps the file's operator seeds:
/// any other tenant starts with an empty allowlist and gains operators
/// through the API, rather than inheriting the install's.
fn tenant_admin(admin: &AdminConfig, tenant: &TenantId) -> AdminConfig {
    let mut admin = admin.clone();
    admin.session_secret = admin
        .session_secret
        .as_deref()
        .map(|secret| derive_session_secret(secret, tenant));
    if !tenant.is_default() {
        admin.allowed_emails.clear();
        admin.allowed_domains.clear();
    }
    admin
}

/// A per-tenant session secret: HKDF-SHA256 of the install secret under a
/// label naming the tenant, hex-encoded (64 chars, above the 32-byte floor
/// the authenticator requires). An install secret that is empty or short is
/// passed through unchanged so the authenticator still reports it as the
/// operator's mistake, rather than a derived key quietly standing in.
fn derive_session_secret(secret: &str, tenant: &TenantId) -> String {
    if secret.trim().is_empty() || secret.len() < 32 {
        return secret.to_string();
    }
    let info = admin_session_label(tenant);
    hex::encode(workforce_crypto::derive_subkey(secret.as_bytes(), info.as_bytes()).as_slice())
}

/// The HKDF label `tenant`'s admin session key is derived under, whether
/// from the file's install secret or the master key's session root: one
/// label per tenant, so no two tenants' keys come from the same derivation.
pub(crate) fn admin_session_label(tenant: &TenantId) -> String {
    format!("workforce-edge/v1/admin-session/{tenant}")
}

/// How requests map to tenants.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TenantMode {
    /// Every request reaches the one tenant, whatever host it names.
    Single(TenantId),
    /// The request's host picks the tenant from the registry's hostnames.
    Multi,
}

/// What a request's host resolved to.
pub enum Resolved {
    Tenant(Arc<TenantRuntime>),
    /// No tenant answers on this host.
    Unknown,
    /// The tenant exists but is not being served; carries the message its
    /// callers are shown.
    Suspended(String),
}

/// The tenant a request is served for, resolved from its host. Handlers take
/// this first, so nothing below it runs for a host that names no tenant.
#[derive(Clone)]
pub struct ServedTenant(pub Arc<TenantRuntime>);

impl Deref for ServedTenant {
    type Target = Arc<TenantRuntime>;

    fn deref(&self) -> &Arc<TenantRuntime> {
        &self.0
    }
}

/// Why a request could not be served for a tenant. Unknown hosts are 404,
/// indistinguishable from a route that does not exist; a suspended tenant is
/// a 503 that says so, because its operator should learn why from the
/// response and not from a dead widget. Both answers are cheap to provoke
/// and reached by nothing else's limiter, so an address that keeps asking
/// gets a 429 instead.
#[derive(Debug)]
pub enum TenantRejection {
    Unknown,
    Suspended(String),
    RateLimited,
}

impl TenantRejection {
    pub const SUSPENDED_CODE: &'static str = "tenant_suspended";
    /// The error code a socket receives when its tenant's runtime was
    /// replaced under it; the widget reconnects to the replacement.
    pub const RELOADED_CODE: &'static str = "tenant_reloaded";
}

impl IntoResponse for TenantRejection {
    fn into_response(self) -> Response {
        match self {
            Self::Unknown => StatusCode::NOT_FOUND.into_response(),
            Self::Suspended(message) => (
                StatusCode::SERVICE_UNAVAILABLE,
                axum::Json(serde_json::json!({
                    "error": Self::SUSPENDED_CODE,
                    "message": message,
                })),
            )
                .into_response(),
            Self::RateLimited => StatusCode::TOO_MANY_REQUESTS.into_response(),
        }
    }
}

impl FromRequestParts<Arc<EdgeState>> for ServedTenant {
    type Rejection = TenantRejection;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<EdgeState>,
    ) -> Result<Self, Self::Rejection> {
        // A guard that already resolved the tenant leaves it in the extensions.
        if let Some(served) = parts.extensions.get::<ServedTenant>() {
            return Ok(served.clone());
        }
        let peer = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|c| c.0.ip());
        let host = request_host(state.trusted_proxies(), &parts.headers, peer);
        let rejection = match state.resolve(host.as_deref().unwrap_or_default()) {
            Resolved::Tenant(runtime) => return Ok(Self(runtime)),
            Resolved::Unknown => TenantRejection::Unknown,
            Resolved::Suspended(message) => TenantRejection::Suspended(message),
        };
        // No runtime, so no tenant limiter: the process-wide one stands in
        // before anything is sent back.
        let ip = client_ip(state.trusted_proxies(), &parts.headers, peer);
        if !state.admit_unresolved(ip) {
            return Err(TenantRejection::RateLimited);
        }
        Err(rejection)
    }
}

/// Schema the edge's tables live in: the session store's, so the pool's
/// pinned `search_path` reaches them.
const SCHEMA: &str = "sessions";

/// Refinery's history table for this set, apart from the session store's
/// `refinery_schema_history` in the same schema.
const HISTORY_TABLE: &str = "refinery_edge_history";

/// Apply the embedded edge migration set. Idempotent — refinery records
/// applied versions and skips them. Runs after the session set, whose schema
/// this one lives in.
pub async fn run_edge_migrations(pool: &PostgresPool) -> Result<()> {
    let set = MigrationSet::new(
        SCHEMA,
        Box::new(EmbeddedEdgeMigrations) as Box<dyn MigrationRun>,
    );
    MigrationRunner::run(pool, &[set])
        .await
        .wrap_err("running edge migrations")?;
    Ok(())
}

struct EmbeddedEdgeMigrations;

#[async_trait::async_trait]
impl MigrationRun for EmbeddedEdgeMigrations {
    async fn run(&self, client: &mut tokio_postgres::Client) -> Result<(), PostgresError> {
        embedded::migrations::runner()
            .set_migration_table_name(HISTORY_TABLE)
            .run_async(client)
            .await
            .map_err(PostgresError::from)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_common::session_postgres::{run_session_migrations, PostgresSessionStore};
    use workforce_postgres::test_support::with_test_pool;

    /// The set creates the edge's tables on a fresh database, adopts them
    /// unchanged where boot already created them, and keeps its history apart
    /// from the session store's.
    #[tokio::test]
    async fn adopts_the_edge_tables_with_a_history_of_its_own() {
        with_test_pool(PostgresSessionStore::build_pool, |pool| async move {
            run_session_migrations(&pool).await.unwrap();
            let conn = pool.get().await.unwrap();
            conn.batch_execute(
                "CREATE TABLE edge_agents (\
                     id TEXT PRIMARY KEY, \
                     config JSONB NOT NULL, \
                     updated_at TIMESTAMPTZ NOT NULL DEFAULT now()); \
                 INSERT INTO edge_agents (id, config) VALUES ('support', '{}')",
            )
            .await
            .unwrap();
            drop(conn);

            run_edge_migrations(&pool).await.unwrap();
            run_edge_migrations(&pool).await.unwrap();

            let conn = pool.get().await.unwrap();
            let kept: i64 = conn
                .query_one("SELECT count(*) FROM edge_agents", &[])
                .await
                .unwrap()
                .get(0);
            assert_eq!(kept, 1, "an adopted table keeps its rows");
            for table in [
                "edge_playbooks",
                "edge_operators",
                "edge_audit",
                "edge_revisions",
                "edge_knowledge",
            ] {
                conn.query(&format!("SELECT 1 FROM {table} LIMIT 0"), &[])
                    .await
                    .unwrap_or_else(|e| panic!("{table} exists: {e}"));
            }
            let histories: Vec<String> = conn
                .query(
                    "SELECT table_name FROM information_schema.tables \
                     WHERE table_schema = 'sessions' AND table_name LIKE 'refinery%' \
                     ORDER BY table_name",
                    &[],
                )
                .await
                .unwrap()
                .into_iter()
                .map(|row| row.get(0))
                .collect();
            assert_eq!(
                histories,
                ["refinery_edge_history", "refinery_schema_history"],
                "two sets, two histories"
            );
            let adopted: Vec<String> = conn
                .query(
                    "SELECT id || '/' || tenant_id FROM edge_agents ORDER BY id",
                    &[],
                )
                .await
                .unwrap()
                .into_iter()
                .map(|row| row.get(0))
                .collect();
            assert_eq!(
                adopted,
                ["support/default"],
                "rows from before belong to default"
            );
        })
        .await;
    }

    /// A settings row overrides the file section by section: a section the
    /// row holds replaces the file's, one it leaves null stays the file's,
    /// secrets come from the row when set — and from the file otherwise for
    /// `default` alone — and what is never in a row stays the file's.
    #[test]
    fn a_settings_row_overrides_the_file_section_by_section() {
        use crate::outbound::OutboundPolicy;
        use crate::settings::{LlmStatus, StoredSettings};

        let mut cfg = crate::settings::tests::full_config();
        cfg.llm.allowed_providers = vec!["anthropic".into(), "vllm".into()];
        let now = Utc::now();
        let row = Tenant {
            id: TenantId::parse("acme").unwrap(),
            display_name: "Acme".into(),
            status: TenantStatus::Active,
            hostnames: vec!["acme.test".into()],
            plan: Plan {
                max_concurrent_turns: Some(2),
                ..Plan::default()
            },
            created_at: now,
            updated_at: now,
        };
        let from_file = TenantSettings::for_tenant(&row, &cfg, None);
        assert_eq!(
            from_file.limits.max_concurrent_turns, 2,
            "the plan caps the file's 3"
        );
        assert_eq!(from_file.llm.default_model.as_deref(), Some("standard"));
        assert!(from_file.admin.as_ref().unwrap().allowed_emails.is_empty());

        let mut stored = StoredSettings::from_settings(&from_file);
        stored.llm.default_model = Some("cheap".into());
        stored.llm.api_key = None;
        stored.llm.allowed_providers = vec!["bedrock".into()];
        stored.llm_status = Some(LlmStatus::ok());
        stored.grader = Some(crate::config::GraderConfig {
            enabled: false,
            model: None,
            api_key: None,
            sample_rate: 0.1,
        });
        stored
            .token_exchange
            .as_mut()
            .unwrap()
            .client_secret
            .clear();
        let admin = stored.admin.as_mut().unwrap();
        admin.allowed_emails = vec!["ada@acme.test".into()];
        admin.session_secret = None;
        admin.oidc.as_mut().unwrap().client_secret = Some("row-oidc-secret".into());
        stored.limits.max_concurrent_turns = 11;
        stored.audit = None;
        stored.outbound = OutboundPolicy::Public;

        let merged = TenantSettings::for_tenant(&row, &cfg, Some(&stored));
        assert_eq!(
            merged.llm.default_model.as_deref(),
            Some("cheap"),
            "the row's section"
        );
        assert_eq!(
            merged.llm.api_key, None,
            "a registered tenant never inherits the file's key"
        );
        assert_eq!(
            merged.llm.allowed_providers,
            ["anthropic", "vllm"],
            "the allowlist is install policy, never the row's"
        );
        assert_eq!(
            merged.llm_status.as_ref().unwrap().state,
            crate::settings::LlmState::Ok
        );
        assert!(!merged.grader.as_ref().unwrap().enabled);
        assert_eq!(
            merged.token_exchange.as_ref().unwrap().client_secret,
            "",
            "nor the file's token-exchange secret"
        );
        let admin = merged.admin.as_ref().unwrap();
        assert_eq!(
            admin.allowed_emails,
            ["ada@acme.test"],
            "the row's operators"
        );
        assert_eq!(
            admin.oidc.as_ref().unwrap().client_secret.as_deref(),
            Some("row-oidc-secret"),
            "a secret the row holds wins"
        );
        assert_eq!(
            admin.session_secret,
            from_file.admin.as_ref().unwrap().session_secret,
            "the session key is still derived from the file's install secret"
        );
        assert_eq!(
            merged.limits.max_concurrent_turns, 2,
            "the row's 11, under the plan's 2"
        );
        assert_eq!(merged.plan, row.plan);
        assert_eq!(
            merged.audit.as_ref().unwrap().retention_days,
            3,
            "a null section stays the file's"
        );
        assert_eq!(merged.outbound, OutboundPolicy::Public);
        assert_eq!(merged.hostnames, ["acme.test"]);

        // The self-hosted tenant is the one the file belongs to: its blank
        // row secrets fall back to the file's.
        let default_row = Tenant {
            id: TenantId::default_tenant(),
            hostnames: vec![],
            plan: Plan::default(),
            ..row.clone()
        };
        let mut own = stored.clone();
        own.admin
            .as_mut()
            .unwrap()
            .oidc
            .as_mut()
            .unwrap()
            .client_secret = None;
        let merged = TenantSettings::for_tenant(&default_row, &cfg, Some(&own));
        assert_eq!(
            merged.llm.api_key.as_deref(),
            Some("sk-ant-file-key-123456"),
            "default inherits the file's key"
        );
        assert_eq!(
            merged.token_exchange.as_ref().unwrap().client_secret,
            "te-secret-abc"
        );
        assert_eq!(
            merged
                .admin
                .as_ref()
                .unwrap()
                .oidc
                .as_ref()
                .unwrap()
                .client_secret
                .as_deref(),
            Some("oidc-secret-xyz")
        );
    }

    /// The plan's rate and concurrency ceilings bind over the tenant's own
    /// limits, and a limit above its ceiling is named.
    #[test]
    fn the_plan_caps_the_rate_limits_and_names_a_limit_over_its_ceiling() {
        let limits = TenantLimits {
            rate_limit_per_min: 600,
            user_rate_limit_per_min: 60,
            max_concurrent_turns: 8,
        };
        assert_eq!(
            Plan::default().cap_limits(limits),
            limits,
            "no plan, no cap"
        );
        assert_eq!(Plan::default().limit_over_ceiling(&limits), None);
        let plan = Plan {
            rate_limit_per_min: Some(100),
            user_rate_limit_per_min: Some(0),
            max_concurrent_turns: Some(2),
            ..Plan::default()
        };
        assert_eq!(
            plan.cap_limits(limits),
            TenantLimits {
                rate_limit_per_min: 100,
                user_rate_limit_per_min: 1,
                max_concurrent_turns: 2,
            },
            "capped, and never below 1"
        );
        assert_eq!(plan.limit_over_ceiling(&limits), Some("rate_limit_per_min"));
        let under = TenantLimits {
            rate_limit_per_min: 50,
            user_rate_limit_per_min: 60,
            max_concurrent_turns: 1,
        };
        assert_eq!(
            plan.limit_over_ceiling(&under),
            Some("user_rate_limit_per_min")
        );
        assert_eq!(
            plan.cap_limits(under).rate_limit_per_min,
            50,
            "less is fine"
        );

        let now = Utc::now();
        let row = Tenant {
            id: TenantId::parse("acme").unwrap(),
            display_name: "Acme".into(),
            status: TenantStatus::Active,
            hostnames: vec![],
            plan: plan.clone(),
            created_at: now,
            updated_at: now,
        };
        let settings =
            TenantSettings::for_tenant(&row, &crate::settings::tests::full_config(), None);
        assert_eq!(settings.limits.rate_limit_per_min, 100);
        assert_eq!(settings.limits.user_rate_limit_per_min, 1);
        assert_eq!(settings.limits.max_concurrent_turns, 2);
    }

    /// The derived session secret is an HKDF subkey of the install secret
    /// per tenant: stable, distinct per tenant, never the input, and a
    /// short or empty input passes through for the authenticator to refuse.
    #[test]
    fn the_session_secret_is_derived_per_tenant_with_hkdf() {
        let secret = "0123456789abcdef0123456789abcdef";
        let acme = derive_session_secret(secret, &TenantId::parse("acme").unwrap());
        let again = derive_session_secret(secret, &TenantId::parse("acme").unwrap());
        let contoso = derive_session_secret(secret, &TenantId::parse("contoso").unwrap());
        assert_eq!(acme, again, "stable across rebuilds");
        assert_ne!(acme, contoso, "distinct per tenant");
        assert_ne!(acme, secret);
        assert_eq!(acme.len(), 64, "32 bytes, hex");
        assert!(acme.bytes().all(|b| b.is_ascii_hexdigit()));
        let expected = hex::encode(
            workforce_crypto::derive_subkey(
                secret.as_bytes(),
                b"workforce-edge/v1/admin-session/acme",
            )
            .as_slice(),
        );
        assert_eq!(acme, expected, "HKDF-SHA256 under the tenant's label");
        assert_eq!(
            derive_session_secret("short", &TenantId::default_tenant()),
            "short"
        );
        assert_eq!(
            derive_session_secret("  ", &TenantId::default_tenant()),
            "  "
        );
    }

    /// Without a file session secret the tenant's admin signs under a key
    /// derived from the install's session root: the same key on every
    /// build, one per tenant — a session minted for acme never verifies
    /// under contoso's — and the file's secret, when set, still wins. With
    /// neither, the key is left for the authenticator to randomise.
    #[test]
    fn an_admin_without_a_file_session_secret_signs_under_the_session_root_per_tenant() {
        use crate::admin::{AdminAuth, AdminConfig, AdminOidc};

        let root = crate::boot::test_install_keys().session_root;
        let admin = AdminConfig {
            oidc: Some(AdminOidc {
                issuer: "https://accounts.google.com".into(),
                audience: Some("edge-admin".into()),
                client_id: Some("edge-admin".into()),
                client_secret: None,
                mode: None,
            }),
            password_hash: None,
            allowed_emails: vec![],
            allowed_domains: vec![],
            session_secret: None,
            public_url: None,
            session_hours: 1,
        };
        let settings_for = |id: &str| TenantSettings {
            id: TenantId::parse(id).unwrap(),
            name: id.to_string(),
            admin: Some(admin.clone()),
            ..TenantSettings::from_edge_toml(&crate::settings::tests::full_config())
        };
        let auth_for = |id: &str| {
            let settings = settings_for(id);
            let keyed = settings
                .admin_with_session_key(Some(&root))
                .expect("the admin section is kept");
            assert_eq!(
                keyed.session_secret.as_deref(),
                Some(root.tenant_session_secret(&settings.id).as_str())
            );
            AdminAuth::new(&keyed, settings.id.clone(), OutboundPolicy::Private).unwrap()
        };

        let acme = auth_for("acme");
        let token = acme.issue_session("ada@acme.test").unwrap();
        assert_eq!(
            auth_for("acme").verify_session(&token),
            Some((
                "ada@acme.test".to_string(),
                TenantId::parse("acme").unwrap()
            )),
            "a second build of the same tenant verifies the first's session"
        );
        assert_eq!(
            auth_for("contoso").verify_session(&token),
            None,
            "another tenant's key never verifies it"
        );
        assert_eq!(auth_for("contoso").session_role(&token), None);

        let from_file = TenantSettings {
            admin: Some(AdminConfig {
                session_secret: Some("0123456789abcdef0123456789abcdef".into()),
                ..admin.clone()
            }),
            ..settings_for("acme")
        };
        assert_eq!(
            from_file
                .admin_with_session_key(Some(&root))
                .unwrap()
                .session_secret
                .as_deref(),
            Some("0123456789abcdef0123456789abcdef"),
            "the file's secret wins over the root"
        );
        assert_eq!(
            settings_for("acme")
                .admin_with_session_key(None)
                .unwrap()
                .session_secret,
            None,
            "no root, no secret: the authenticator's per-boot key stands"
        );
        assert!(TenantSettings {
            admin: None,
            ..settings_for("acme")
        }
        .admin_with_session_key(Some(&root))
        .is_none());
    }

    #[test]
    fn a_tenant_id_is_a_slug() {
        for ok in ["default", "acme", "contoso-prod", "a1", &"x".repeat(63)] {
            assert_eq!(TenantId::parse(ok).unwrap().as_str(), ok);
        }
        for bad in [
            "",
            "-acme",
            "Acme",
            "acme_prod",
            "acme.prod",
            &"x".repeat(64),
        ] {
            assert!(TenantId::parse(bad).is_err(), "{bad:?} is not a slug");
        }
        let parsed: TenantId = serde_json::from_str("\"acme\"").unwrap();
        assert_eq!(parsed, TenantId::parse("acme").unwrap());
        assert!(serde_json::from_str::<TenantId>("\"Not A Slug\"").is_err());
    }

    #[test]
    fn the_platforms_own_names_are_reserved_but_still_slugs() {
        for reserved in ["app", "api", "www", "platform", "marrilo", "default"] {
            let id = TenantId::parse(reserved).expect("a reserved id parses");
            assert!(id.is_reserved(), "{reserved}");
        }
        for free in ["acme", "apps", "api-docs", "my-platform"] {
            assert!(!TenantId::parse(free).unwrap().is_reserved(), "{free}");
        }
    }

    #[tokio::test]
    async fn the_registry_creates_resolves_and_suspends_tenants() {
        with_test_pool(PostgresSessionStore::build_pool, |pool| async move {
            run_session_migrations(&pool).await.unwrap();
            run_edge_migrations(&pool).await.unwrap();
            let store = TenantStore::new(pool);
            store.ensure_default().await.unwrap();
            store.ensure_default().await.unwrap();
            let default = store
                .get(&TenantId::default_tenant())
                .await
                .unwrap()
                .expect("the migration made it");
            assert_eq!(default.status, TenantStatus::Active);
            assert!(default.hostnames.is_empty());

            let acme = TenantId::parse("acme").unwrap();
            let created = store.create(&acme, "Acme Ltd").await.unwrap();
            assert_eq!(created.display_name, "Acme Ltd");
            assert!(
                store.create(&acme, "Again").await.is_err(),
                "ids are unique"
            );

            store.add_hostname(&acme, "Acme.Marrilo.com").await.unwrap();
            assert_eq!(
                store.resolve_hostname("acme.marrilo.com").await.unwrap(),
                Some(acme.clone()),
                "hostnames are matched case-insensitively"
            );
            assert!(
                store
                    .add_hostname(&TenantId::default_tenant(), "acme.marrilo.com")
                    .await
                    .is_err(),
                "a hostname names one tenant"
            );
            assert_eq!(
                store.resolve_hostname("nobody.marrilo.com").await.unwrap(),
                None
            );
            assert_eq!(
                store
                    .list()
                    .await
                    .unwrap()
                    .iter()
                    .map(|t| t.id.as_str())
                    .collect::<Vec<_>>(),
                ["acme", "default"]
            );

            assert!(store
                .set_status(&acme, TenantStatus::Suspended)
                .await
                .unwrap());
            assert_eq!(
                store.get(&acme).await.unwrap().unwrap().status,
                TenantStatus::Suspended
            );
            assert!(
                !store
                    .set_status(&TenantId::parse("ghost").unwrap(), TenantStatus::Active)
                    .await
                    .unwrap(),
                "an unknown tenant is not updated"
            );
            assert!(store.remove_hostname("acme.marrilo.com").await.unwrap());
            assert!(!store.remove_hostname("acme.marrilo.com").await.unwrap());
        })
        .await;
    }

    #[test]
    fn the_month_starts_at_midnight_utc_on_the_first() {
        let mid = DateTime::parse_from_rfc3339("2026-10-17T13:45:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(month_start(mid).to_rfc3339(), "2026-10-01T00:00:00+00:00");
        let first = DateTime::parse_from_rfc3339("2026-10-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(month_start(first), first, "the first is already the start");
    }

    /// A plan stored before the agent cap existed parses with it unbounded;
    /// one that sets it round-trips, admits a count at the cap and refuses
    /// one past it by name.
    #[test]
    fn a_plan_without_an_agent_cap_parses_as_unlimited() {
        let stored: Plan = serde_json::from_str(
            r#"{"turns_per_month":20000,"active_users":500,"knowledge_chunks":20000}"#,
        )
        .unwrap();
        assert_eq!(stored.agents, None);
        assert_eq!(stored.turns_per_month, Some(20000));
        assert_eq!(stored.admit_agents(50), Ok(()), "unbounded");

        let capped: Plan = serde_json::from_str(r#"{"agents":5}"#).unwrap();
        assert_eq!(capped.agents, Some(5));
        assert_eq!(
            serde_json::to_value(&capped).unwrap(),
            serde_json::json!({"agents": 5}),
            "unset caps are not written"
        );
        assert_eq!(capped.admit_agents(5), Ok(()), "at the cap is allowed");
        assert_eq!(
            capped.admit_agents(6).unwrap_err(),
            "the plan allows 5 agent(s); the workspace would have 6"
        );
    }

    /// Which fields a plan reports reached: at the number, not past it; an
    /// empty plan reports none; an uncounted vector store reports none.
    #[test]
    fn over_names_the_caps_usage_has_reached() {
        let usage = Usage {
            turns_this_month: 100,
            active_users_this_month: 4,
            knowledge_chunks: 250,
            memory_objects: Some(1000),
            agents: 5,
        };
        assert!(Plan::default().over(&usage).is_empty(), "no plan, no caps");
        let plan = Plan {
            turns_per_month: Some(100),
            active_users: Some(5),
            knowledge_chunks: Some(200),
            memory_objects: Some(1000),
            agents: Some(5),
            upload_mb: Some(1),
            max_concurrent_turns: Some(1),
            rate_limit_per_min: Some(10),
            user_rate_limit_per_min: Some(1),
        };
        assert_eq!(
            plan.over(&usage),
            [
                "turns_per_month",
                "knowledge_chunks",
                "memory_objects",
                "agents"
            ]
        );
        let uncounted = Usage {
            memory_objects: None,
            ..usage
        };
        assert_eq!(
            plan.over(&uncounted),
            ["turns_per_month", "knowledge_chunks", "agents"],
            "an unknown count cannot reach a cap"
        );
    }

    #[tokio::test]
    async fn a_tenant_connection_sets_the_tenant_for_its_transaction_only() {
        with_test_pool(PostgresSessionStore::build_pool, |pool| async move {
            run_session_migrations(&pool).await.unwrap();
            run_edge_migrations(&pool).await.unwrap();
            let tenants = TenantPool::new(pool.clone());
            let acme = TenantId::parse("acme").unwrap();

            let conn = tenants.conn(&acme).await.unwrap();
            let setting: Option<String> = conn
                .query_one("SELECT current_setting('app.tenant_id', true)", &[])
                .await
                .unwrap()
                .get(0);
            assert_eq!(setting.as_deref(), Some("acme"));
            conn.commit().await.unwrap();

            let fresh = pool.get().await.unwrap();
            let setting: Option<String> = fresh
                .query_one(
                    "SELECT nullif(current_setting('app.tenant_id', true), '')",
                    &[],
                )
                .await
                .unwrap()
                .get(0);
            assert_eq!(setting, None, "the setting ends with the transaction");
            drop(fresh);

            let uncommitted = tenants.conn(&acme).await.unwrap();
            uncommitted
                .execute(
                    "INSERT INTO tenants (id, display_name) VALUES ('acme', 'Acme')",
                    &[],
                )
                .await
                .unwrap();
            drop(uncommitted);
            let count: i64 = pool
                .get()
                .await
                .unwrap()
                .query_one("SELECT count(*) FROM tenants WHERE id = 'acme'", &[])
                .await
                .unwrap()
                .get(0);
            assert_eq!(count, 0, "a dropped connection's transaction never lands");
        })
        .await;
    }
}

#[cfg(test)]
pub(crate) mod plan_tests {
    //! The plan's counters against seeded rows, and what they admit.
    use super::*;
    use crate::analytics::run_edge_analytics_migrations;
    use workforce_common::session_postgres::{run_session_migrations, PostgresSessionStore};
    use workforce_core::types::AgentId;
    use workforce_memory::in_memory::InMemoryStore;
    use workforce_memory::types::{Memory, MemoryLayer};
    use workforce_postgres::test_support::with_test_pool;

    /// Every migration set the counters read, and the `default` tenant.
    pub(crate) async fn migrate(pool: &PostgresPool) {
        run_session_migrations(pool).await.unwrap();
        run_edge_analytics_migrations(pool).await.unwrap();
        run_edge_migrations(pool).await.unwrap();
        TenantStore::new(pool.clone())
            .ensure_default()
            .await
            .unwrap();
    }

    /// A `PlanUsage` for `default` on `pool`, counting `memory`.
    pub(crate) fn plan_usage(
        pool: &PostgresPool,
        plan: Plan,
        memory: Arc<dyn MemoryStore>,
    ) -> Arc<PlanUsage> {
        let tenant = TenantId::default_tenant();
        let tenants = TenantPool::new(pool.clone());
        Arc::new(PlanUsage::new(
            plan,
            tenant.clone(),
            tenants.clone(),
            Arc::new(EdgeAnalyticsStore::new(tenants, tenant)),
            memory,
        ))
    }

    /// One `usage_events` row for `default` at `ts`, under `purpose`.
    pub(crate) async fn seed_turn(pool: &PostgresPool, owner: &str, ts: &str, purpose: &str) {
        let conn = TenantPool::new(pool.clone())
            .conn(&TenantId::default_tenant())
            .await
            .unwrap();
        conn.execute(
            "INSERT INTO edge_analytics.usage_events \
             (tenant_id, ts, agent_id, owner, conversation, message_id, model, input_tokens, \
              output_tokens, cache_read_tokens, cache_write_tokens, rounds, tool_count, \
              tool_failures, duration_ms, cost_usd, outcome, org, purpose) \
             VALUES ('default', $1::text::timestamptz, 'support', $2, 'conv', 'msg', 'm', \
                     1, 1, 0, 0, 1, 0, 0, 10, 0, 'ok', NULL, $3)",
            &[&ts, &owner, &purpose],
        )
        .await
        .unwrap();
        conn.commit().await.unwrap();
    }

    /// One `chat_identities` row for `default`, last seen at `last_seen`.
    pub(crate) async fn seed_identity(pool: &PostgresPool, owner: &str, last_seen: &str) {
        let conn = TenantPool::new(pool.clone())
            .conn(&TenantId::default_tenant())
            .await
            .unwrap();
        conn.execute(
            "INSERT INTO edge_analytics.chat_identities \
             (tenant_id, owner, issuer, first_seen, last_seen) \
             VALUES ('default', $1, 'iss', $2::text::timestamptz, $2::text::timestamptz)",
            &[&owner, &last_seen],
        )
        .await
        .unwrap();
        conn.commit().await.unwrap();
    }

    /// One knowledge document row for `default`, of `chunks` chunks.
    pub(crate) async fn seed_doc(pool: &PostgresPool, agent: &str, id: &str, chunks: i32) {
        let conn = TenantPool::new(pool.clone())
            .conn(&TenantId::default_tenant())
            .await
            .unwrap();
        conn.execute(
            "INSERT INTO edge_knowledge (tenant_id, id, agent_id, title, content, chunks) \
             VALUES ('default', $1, $2, $1, 'text', $3)",
            &[&id, &agent, &chunks],
        )
        .await
        .unwrap();
        conn.commit().await.unwrap();
    }

    fn this_month() -> String {
        Utc::now().to_rfc3339()
    }

    fn last_month() -> String {
        (month_start(Utc::now()) - chrono::Duration::days(1)).to_rfc3339()
    }

    /// A store that records how often it is counted, can refuse to be
    /// (by panicking), and can take its time — for the plan's admission
    /// and single-flight tests. Everything else delegates.
    struct CountingStore {
        inner: InMemoryStore,
        counts: std::sync::atomic::AtomicUsize,
        count_panics: bool,
        count_delay: Duration,
    }

    impl CountingStore {
        fn new(count_panics: bool, count_delay: Duration) -> Self {
            Self {
                inner: InMemoryStore::new(),
                counts: std::sync::atomic::AtomicUsize::new(0),
                count_panics,
                count_delay,
            }
        }

        fn counted(&self) -> usize {
            self.counts.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl MemoryStore for CountingStore {
        async fn remember(
            &self,
            memory: Memory,
        ) -> eyre::Result<workforce_memory::types::MemoryId> {
            self.inner.remember(memory).await
        }

        async fn recall(
            &self,
            query: &str,
            filter: MemoryFilter,
        ) -> eyre::Result<Vec<workforce_memory::types::MemorySearchResult>> {
            self.inner.recall(query, filter).await
        }

        async fn get(
            &self,
            id: &workforce_memory::types::MemoryId,
        ) -> eyre::Result<Option<Memory>> {
            self.inner.get(id).await
        }

        async fn forget(&self, id: &workforce_memory::types::MemoryId) -> eyre::Result<()> {
            self.inner.forget(id).await
        }

        async fn share(
            &self,
            id: &workforce_memory::types::MemoryId,
            visibility: workforce_memory::types::Visibility,
        ) -> eyre::Result<()> {
            self.inner.share(id, visibility).await
        }

        async fn count(&self, filter: MemoryFilter) -> eyre::Result<usize> {
            assert!(
                !self.count_panics,
                "the store was counted for a plan that does not cap it"
            );
            self.counts
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            tokio::time::sleep(self.count_delay).await;
            self.inner.count(filter).await
        }

        async fn list(&self, filter: MemoryFilter) -> eyre::Result<Vec<Memory>> {
            self.inner.list(filter).await
        }

        async fn update_tags(
            &self,
            id: &workforce_memory::types::MemoryId,
            tags: Vec<String>,
        ) -> eyre::Result<()> {
            self.inner.update_tags(id, tags).await
        }

        async fn adjust_importance(
            &self,
            id: &workforce_memory::types::MemoryId,
            delta: f32,
        ) -> eyre::Result<f32> {
            self.inner.adjust_importance(id, delta).await
        }

        async fn identity_memories(
            &self,
            owner: &AgentId,
            identity: &str,
        ) -> eyre::Result<Vec<workforce_memory::types::MemoryId>> {
            self.inner.identity_memories(owner, identity).await
        }

        async fn identity_counts(
            &self,
            owner: &AgentId,
        ) -> eyre::Result<std::collections::BTreeMap<String, usize>> {
            self.inner.identity_counts(owner).await
        }

        async fn reassign_identity(
            &self,
            owner: &AgentId,
            from: &str,
            to: &str,
        ) -> eyre::Result<usize> {
            self.inner.reassign_identity(owner, from, to).await
        }

        async fn purge_identity(&self, owner: &AgentId, identity: &str) -> eyre::Result<usize> {
            self.inner.purge_identity(owner, identity).await
        }

        async fn purge_memories(
            &self,
            ids: &[workforce_memory::types::MemoryId],
        ) -> eyre::Result<usize> {
            self.inner.purge_memories(ids).await
        }

        async fn record_recurrence(
            &self,
            id: &workforce_memory::types::MemoryId,
            additional_message_ids: Vec<String>,
        ) -> eyre::Result<u32> {
            self.inner
                .record_recurrence(id, additional_message_ids)
                .await
        }
    }

    /// A plan that caps turns alone reads the month and nothing else: the
    /// vector store is never counted for a turn's admission, nor for the
    /// usage card of a plan that does not cap it.
    #[tokio::test]
    async fn a_plan_without_a_memory_cap_never_counts_the_store() {
        with_test_pool(PostgresSessionStore::build_pool, |pool| async move {
            migrate(&pool).await;
            seed_turn(&pool, "a", &this_month(), "turn").await;
            let store = Arc::new(CountingStore::new(true, Duration::ZERO));
            let usage = plan_usage(
                &pool,
                Plan {
                    turns_per_month: Some(5),
                    ..Plan::default()
                },
                Arc::clone(&store) as Arc<dyn MemoryStore>,
            );
            assert_eq!(usage.admit_turn("a").await, Ok(()));
            usage.invalidate();
            assert_eq!(usage.admit_turn("b").await, Ok(()));
            let card = usage.usage(0).await.unwrap();
            assert_eq!(card.turns_this_month, 1);
            assert_eq!(card.memory_objects, None, "uncapped, so uncounted");
            assert!(!usage.memory_at_cap().await);
        })
        .await;
    }

    /// Admissions that arrive together while the snapshot is stale share
    /// one count rather than each running their own.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_stale_snapshot_is_recounted_once_for_a_burst() {
        with_test_pool(PostgresSessionStore::build_pool, |pool| async move {
            migrate(&pool).await;
            let store = Arc::new(CountingStore::new(false, Duration::from_millis(200)));
            let usage = plan_usage(
                &pool,
                Plan {
                    memory_objects: Some(10),
                    ..Plan::default()
                },
                Arc::clone(&store) as Arc<dyn MemoryStore>,
            );
            let burst: Vec<_> = (0..8)
                .map(|_| {
                    let usage = Arc::clone(&usage);
                    tokio::spawn(async move { usage.memory_at_cap().await })
                })
                .collect();
            for task in burst {
                assert!(!task.await.unwrap());
            }
            assert_eq!(store.counted(), 1, "one flight served the burst");
            usage.invalidate();
            assert!(!usage.memory_at_cap().await);
            assert_eq!(store.counted(), 2, "an invalidation recounts");
        })
        .await;
    }

    /// Turns are this month's `turn` rows alone (not last month's, not the
    /// grader's); users are the identities seen this month; chunks sum the
    /// corpus; memory is the store's whole count, for a plan that caps it.
    #[tokio::test]
    async fn usage_counts_match_the_seeded_rows() {
        with_test_pool(PostgresSessionStore::build_pool, |pool| async move {
            migrate(&pool).await;
            seed_turn(&pool, "a", &this_month(), "turn").await;
            seed_turn(&pool, "b", &this_month(), "turn").await;
            seed_turn(&pool, "a", &this_month(), "grader").await;
            seed_turn(&pool, "a", &last_month(), "turn").await;
            seed_identity(&pool, "a", &this_month()).await;
            seed_identity(&pool, "b", &this_month()).await;
            seed_identity(&pool, "old", &last_month()).await;
            seed_doc(&pool, "support", "faq", 3).await;
            seed_doc(&pool, "sales", "pricing", 4).await;
            let memory = Arc::new(InMemoryStore::new());
            for text in ["one", "two"] {
                memory
                    .remember(Memory::new(
                        text,
                        MemoryLayer::Semantic,
                        AgentId::new("support"),
                    ))
                    .await
                    .unwrap();
            }

            let usage = plan_usage(
                &pool,
                Plan {
                    memory_objects: Some(100),
                    ..Plan::default()
                },
                memory,
            );
            assert_eq!(
                usage.usage(3).await.unwrap(),
                Usage {
                    turns_this_month: 2,
                    active_users_this_month: 2,
                    knowledge_chunks: 7,
                    memory_objects: Some(2),
                    agents: 3,
                }
            );
        })
        .await;
    }

    /// A snapshot is served until a write drops it: a row that lands
    /// behind the cache is not counted until `invalidate`.
    #[tokio::test]
    async fn the_snapshot_is_served_until_invalidated() {
        with_test_pool(PostgresSessionStore::build_pool, |pool| async move {
            migrate(&pool).await;
            let usage = plan_usage(&pool, Plan::default(), Arc::new(InMemoryStore::new()));
            assert_eq!(usage.usage(0).await.unwrap().turns_this_month, 0);
            seed_turn(&pool, "a", &this_month(), "turn").await;
            assert_eq!(
                usage.usage(0).await.unwrap().turns_this_month,
                0,
                "the snapshot stands"
            );
            usage.invalidate();
            assert_eq!(usage.usage(0).await.unwrap().turns_this_month, 1);
        })
        .await;
    }

    /// A tenant whose month's turns are used is refused; one under the cap
    /// is admitted; no plan admits without a count.
    #[tokio::test]
    async fn the_turn_cap_refuses_once_the_months_turns_are_used() {
        with_test_pool(PostgresSessionStore::build_pool, |pool| async move {
            migrate(&pool).await;
            seed_turn(&pool, "a", &this_month(), "turn").await;
            seed_turn(&pool, "a", &this_month(), "turn").await;
            let memory: Arc<dyn MemoryStore> = Arc::new(InMemoryStore::new());

            let capped = plan_usage(
                &pool,
                Plan {
                    turns_per_month: Some(2),
                    ..Plan::default()
                },
                Arc::clone(&memory),
            );
            assert_eq!(capped.admit_turn("a").await, Err(PlanLimit::Turns));
            assert_eq!(
                capped.admit_turn("newcomer").await,
                Err(PlanLimit::Turns),
                "the turn cap is the workspace's, whoever asks"
            );

            let roomy = plan_usage(
                &pool,
                Plan {
                    turns_per_month: Some(3),
                    ..Plan::default()
                },
                Arc::clone(&memory),
            );
            assert_eq!(roomy.admit_turn("a").await, Ok(()));
            assert_eq!(
                plan_usage(&pool, Plan::default(), memory)
                    .admit_turn("a")
                    .await,
                Ok(())
            );
        })
        .await;
    }

    /// With the user cap full, a user the month has not seen is refused
    /// while one it has proceeds — the cap counts people, not turns.
    #[tokio::test]
    async fn the_user_cap_refuses_a_new_user_and_admits_an_existing_one() {
        with_test_pool(PostgresSessionStore::build_pool, |pool| async move {
            migrate(&pool).await;
            seed_identity(&pool, "regular", &this_month()).await;
            seed_identity(&pool, "lapsed", &last_month()).await;
            let usage = plan_usage(
                &pool,
                Plan {
                    active_users: Some(1),
                    ..Plan::default()
                },
                Arc::new(InMemoryStore::new()),
            );
            assert_eq!(usage.admit_turn("regular").await, Ok(()));
            assert_eq!(
                usage.admit_turn("newcomer").await,
                Err(PlanLimit::ActiveUsers)
            );
            assert_eq!(
                usage.admit_turn("lapsed").await,
                Err(PlanLimit::ActiveUsers),
                "a user last seen before the month counts as new to it"
            );
        })
        .await;
    }

    /// The memory cap binds on the store's count, and not at all when the
    /// store cannot be counted.
    #[tokio::test]
    async fn the_memory_cap_binds_on_the_stores_count() {
        with_test_pool(PostgresSessionStore::build_pool, |pool| async move {
            migrate(&pool).await;
            let memory = Arc::new(InMemoryStore::new());
            memory
                .remember(Memory::new(
                    "a fact",
                    MemoryLayer::Semantic,
                    AgentId::new("support"),
                ))
                .await
                .unwrap();
            let plan = Plan {
                memory_objects: Some(1),
                ..Plan::default()
            };
            assert!(
                plan_usage(&pool, plan.clone(), memory)
                    .memory_at_cap()
                    .await
            );
            assert!(
                !plan_usage(&pool, Plan::default(), Arc::new(InMemoryStore::new()))
                    .memory_at_cap()
                    .await,
                "no cap, never at it"
            );
            let unreachable: Arc<dyn MemoryStore> = Arc::new(
                workforce_memory::weaviate::WeaviateStore::new("http://127.0.0.1:1", None),
            );
            let uncounted = plan_usage(&pool, plan, unreachable);
            assert!(
                !uncounted.memory_at_cap().await,
                "an unknown count cannot bind"
            );
            assert_eq!(uncounted.usage(0).await.unwrap().memory_objects, None);
        })
        .await;
    }
}

#[cfg(test)]
pub(crate) mod rls_tests {
    //! Against a role the policies bind, which the harness's superuser is not.
    //!
    //! The harness owns the tables as the superuser and the bound role is a
    //! plain one, so what these prove is that the policies bind a role that
    //! owns nothing. In production the edge's role owns the tables, and it is
    //! `FORCE ROW LEVEL SECURITY` that makes the owner subject to them too.
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::analytics::run_edge_analytics_migrations;
    use workforce_common::postgres::PostgresConfig;
    use workforce_common::session::SessionStore;
    use workforce_common::session_postgres::{run_session_migrations, PostgresSessionStore};
    use workforce_postgres::test_support::with_test_pool;

    const ROLE: &str = "edge_rls";

    /// Migrate on the superuser pool, then a second pool on the same database
    /// whose connections take a role the policies apply to. Roles are
    /// cluster-wide and tests share the cluster, so a test creating the role
    /// under another's feet is tolerated.
    pub(crate) async fn with_bound_pool<F, Fut>(test: F)
    where
        F: FnOnce(PostgresPool, PostgresPool) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let config: Arc<Mutex<Option<PostgresConfig>>> = Arc::default();
        let captured = Arc::clone(&config);
        with_test_pool(
            move |cfg| {
                *captured.lock().unwrap() = Some(cfg.clone());
                PostgresSessionStore::build_pool(cfg)
            },
            |superuser| async move {
                run_session_migrations(&superuser).await.unwrap();
                run_edge_analytics_migrations(&superuser).await.unwrap();
                run_edge_migrations(&superuser).await.unwrap();
                superuser
                    .get()
                    .await
                    .unwrap()
                    .batch_execute(&format!(
                        "DO $$ BEGIN \
                           IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{ROLE}') THEN \
                             CREATE ROLE {ROLE} NOLOGIN; \
                           END IF; \
                         EXCEPTION WHEN duplicate_object THEN NULL; \
                         END $$; \
                         GRANT USAGE ON SCHEMA sessions, edge_analytics TO {ROLE}; \
                         GRANT ALL ON ALL TABLES IN SCHEMA sessions, edge_analytics TO {ROLE}; \
                         GRANT ALL ON ALL SEQUENCES IN SCHEMA sessions, edge_analytics TO {ROLE}"
                    ))
                    .await
                    .unwrap();
                let cfg = config.lock().unwrap().clone().expect("captured");
                let bound = PostgresPool::from_config_with_session_init(
                    &cfg,
                    &[
                        "SET search_path TO sessions, public",
                        &format!("SET ROLE {ROLE}"),
                    ],
                )
                .unwrap();
                test(superuser, bound).await;
            },
        )
        .await;
    }

    #[tokio::test]
    async fn the_superuser_bypasses_policies_and_a_plain_role_does_not() {
        with_bound_pool(|superuser, bound| async move {
            assert!(!rls_enforced(&superuser).await.unwrap());
            assert!(rls_enforced(&bound).await.unwrap());
        })
        .await;
    }

    /// With the policies alone — no predicate in the query — another tenant's
    /// rows are invisible, a row cannot be written into another tenant, and a
    /// query that never named a tenant sees nothing at all.
    #[tokio::test]
    async fn policies_keep_each_tenant_to_its_own_rows() {
        with_bound_pool(|superuser, bound| async move {
            let registry = TenantStore::new(superuser.clone());
            let a = TenantId::parse("a").unwrap();
            let b = TenantId::parse("b").unwrap();
            registry.create(&a, "A").await.unwrap();
            registry.create(&b, "B").await.unwrap();

            let tenants = TenantPool::new(bound.clone());
            let conn = tenants.conn(&a).await.unwrap();
            conn.execute(
                "INSERT INTO edge_agents (tenant_id, id, config) VALUES ('a', 'support', '{}')",
                &[],
            )
            .await
            .unwrap();
            conn.execute(
                "INSERT INTO edge_analytics.chat_identities (tenant_id, owner) VALUES ('a', 'o')",
                &[],
            )
            .await
            .unwrap();
            conn.commit().await.unwrap();

            async fn count(conn: &TenantConn, table: &str) -> i64 {
                conn.query_one(&format!("SELECT count(*) FROM {table}"), &[])
                    .await
                    .unwrap()
                    .get(0)
            }
            let as_a = tenants.conn(&a).await.unwrap();
            assert_eq!(count(&as_a, "edge_agents").await, 1);
            assert_eq!(count(&as_a, "edge_analytics.chat_identities").await, 1);
            as_a.commit().await.unwrap();

            let as_b = tenants.conn(&b).await.unwrap();
            assert_eq!(
                count(&as_b, "edge_agents").await,
                0,
                "no predicate, no rows"
            );
            assert_eq!(count(&as_b, "edge_analytics.chat_identities").await, 0);
            let forged = as_b
                .execute(
                    "INSERT INTO edge_agents (tenant_id, id, config) VALUES ('a', 'other', '{}')",
                    &[],
                )
                .await
                .expect_err("writing into another tenant is refused");
            assert_eq!(forged.code().map(|c| c.code()), Some("42501"), "{forged}");
            drop(as_b);

            let unscoped = bound.get().await.unwrap();
            let n: i64 = unscoped
                .query_one("SELECT count(*) FROM edge_agents", &[])
                .await
                .unwrap()
                .get(0);
            assert_eq!(n, 0, "a query that named no tenant sees nothing");
            let n: i64 = superuser
                .get()
                .await
                .unwrap()
                .query_one("SELECT count(*) FROM edge_agents", &[])
                .await
                .unwrap()
                .get(0);
            assert_eq!(
                n, 1,
                "the row is there, for the role the policies do not bind"
            );
        })
        .await;
    }

    /// The session store, built on the bound pool and scoped to a tenant,
    /// is held to that tenant by the four session-table policies: another
    /// tenant's store lists nothing, and a query under the other tenant's
    /// connection that names no tenant at all counts nothing in any of the
    /// four tables.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_session_store_is_bound_by_the_session_table_policies() {
        with_bound_pool(|superuser, bound| async move {
            let registry = TenantStore::new(superuser);
            let a = TenantId::parse("a").unwrap();
            let b = TenantId::parse("b").unwrap();
            registry.create(&a, "A").await.unwrap();
            registry.create(&b, "B").await.unwrap();

            let in_a: Arc<dyn SessionStore> =
                PostgresSessionStore::with_shared_pool_in_tenant(bound.clone(), "a", "support");
            let in_b: Arc<dyn SessionStore> =
                PostgresSessionStore::with_shared_pool_in_tenant(bound.clone(), "b", "support");

            let session = in_a.get_or_create("web:1", "o", "support").unwrap();
            in_a.append_to_archive(
                &session.id,
                &workforce_common::session::SessionMessage {
                    id: "m1".into(),
                    role: workforce_common::session::MessageRole::User,
                    content: "hello".into(),
                    tool_data: None,
                    timestamp: Utc::now(),
                    tokens: 1,
                    external_id: None,
                    attachments: None,
                    trust_level: None,
                    actor: None,
                },
            )
            .unwrap();
            assert!(in_a.try_claim_turn(&session.id, "t1", 60).unwrap());
            in_a.journal_append(
                &session.id,
                &workforce_common::session::JournalEntry {
                    call: "c1".into(),
                    at: Utc::now(),
                    step: workforce_common::session::JournalStep::Pending {
                        tool: "t".into(),
                        input: serde_json::json!({}),
                        plan: serde_json::json!({}),
                        actor: None,
                    },
                },
            )
            .unwrap();

            assert_eq!(in_a.list(None, 10).unwrap().len(), 1);
            assert!(in_a.get(&session.id).unwrap().is_some());
            assert!(in_b.list(None, 10).unwrap().is_empty());
            assert!(in_b.get(&session.id).unwrap().is_none());
            assert!(in_b.find("web:1", "o").unwrap().is_none());

            async fn counts(conn: &TenantConn) -> [i64; 4] {
                let mut counts = [0; 4];
                for (n, table) in ["sessions", "session_archive", "turn_claims", "turn_journal"]
                    .into_iter()
                    .enumerate()
                {
                    counts[n] = conn
                        .query_one(&format!("SELECT count(*) FROM {table}"), &[])
                        .await
                        .unwrap()
                        .get(0);
                }
                counts
            }
            let tenants = TenantPool::new(bound);
            let as_a = tenants.conn(&a).await.unwrap();
            assert_eq!(counts(&as_a).await, [1, 1, 1, 1]);
            as_a.commit().await.unwrap();
            let as_b = tenants.conn(&b).await.unwrap();
            assert_eq!(
                counts(&as_b).await,
                [0, 0, 0, 0],
                "no predicate, no rows, in any of the four"
            );
            as_b.commit().await.unwrap();

            // V002 retired the single-tenant unique targets the shared set
            // keeps for the hub, so B may hold the same conversation key and
            // claim a turn on the same session id without colliding with A.
            let own = in_b.get_or_create("web:1", "o", "support").unwrap();
            assert_ne!(own.id, session.id, "one active session per tenant");
            assert!(in_b.try_claim_turn(&session.id, "t2", 60).unwrap());
            assert_eq!(in_a.find("web:1", "o").unwrap().unwrap().id, session.id);
            assert_eq!(in_b.find("web:1", "o").unwrap().unwrap().id, own.id);
        })
        .await;
    }
}
