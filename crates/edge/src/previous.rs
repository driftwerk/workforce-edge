//! The class an edge kept its memories in before it named its own.
//!
//! An edge with a Weaviate of its own kept every memory, lesson and
//! knowledge chunk in the class it used by default, `WorkforceMemory`, the
//! hub's name. It now names its own (`[weaviate] collection`), and
//! `[weaviate] previous_collection` holds the old class beside it: memories
//! are moved into the edge's own class while replicas serve
//! ([`workforce_memory::switch`]), one replica copying at a time
//! ([`PgCopier`]), and forgetting and erasure reach both classes until the
//! old one is given up:
//!
//! 1. once no replica runs a release that writes the old class,
//!    `workforce-edge memory reconcile`, then with `--apply`;
//! 2. remove `previous_collection` and deploy;
//! 3. `workforce-edge memory drop-previous --collection <old>`, then with
//!    `--apply`.
//!
//! A sync carries forgetting over, but not an erasure, which purges: a
//! customer erased in the old class by a replica on the old release, or
//! while the first copy ran, would keep a copy in the edge's class. The
//! customers the edge still knows (`edge_analytics.chat_identities`, whose
//! row an erasure deletes) say which: `reconcile` purges every other
//! customer's memories.
//!
//! The old class must hold nothing but the edge's memories: a hub's in the
//! same Weaviate would otherwise be copied into the edge's class. Boot
//! refuses one that holds any, and one that does not exist.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use eyre::{eyre, Result, WrapErr};
use workforce_common::postgres::PostgresPool;
use workforce_core::types::AgentId;
use workforce_memory::admin::{Cursor, MemoryAdmin, SyncTarget, Turn};
use workforce_memory::edge_owner::{is_customer_owner, is_edge_identity};
use workforce_memory::store::MemoryStore;
use workforce_memory::switch::{Copier, Switch, Timing};
use workforce_memory::weaviate::WeaviateStore;

use crate::config::{EdgeConfig, MemoryBackend, WeaviateConfig};
use crate::tenant::TenantId;

/// `workforce-edge memory …`.
pub const COMMAND: &str = "memory";

/// `workforce-edge memory --help`.
pub const USAGE: &str = "\
workforce-edge memory — the class the edge kept its memories in before it
named its own ([weaviate] previous_collection).

USAGE:
    workforce-edge memory check-previous --config edge.toml
    workforce-edge memory reconcile --config edge.toml [--apply]
    workforce-edge memory drop-previous --config edge.toml --collection <class> [--apply]

COMMANDS:
    check-previous   Read-only: both classes' counts, whether memories have
                     switched to the edge's own class, whether the previous
                     one holds anything but the edge's memories, and how many
                     customers the edge no longer knows still have memories.
    reconcile        Purge the memories of customers the edge no longer knows
                     (erased while the previous class was held) from the
                     edge's class, and from the previous one while it is
                     configured. Run once no replica is on the old release.
                     A dry run without --apply.
    drop-previous    Delete <class>, once memories have switched to the
                     edge's own class, reconcile finds nothing, and <class>
                     is no longer configured as previous_collection (remove
                     it and deploy first: while it is configured, every
                     erasure reaches it). A dry run without --apply.
";

/// Hold `previous` beside `current`, the edge's own class, already ensured:
/// serve from `previous` until a mirror fills `current`, then from
/// `current`, keeping `previous` for forgetting, erasure and a rollback.
pub async fn open(
    current: Arc<WeaviateStore>,
    previous: WeaviateStore,
    pool: PostgresPool,
) -> Result<Arc<dyn MemoryStore>> {
    let switched = current.switched().await?;
    let held = survey(&previous).await?;
    held.refusal(switched)?;
    tracing::info!(
        collection = current.collection(),
        previous = previous.collection(),
        previous_objects = held.objects,
        switched,
        "holding the previous memory collection beside the edge's own"
    );
    let copier = PgCopier::new(pool, previous.collection(), current.collection());
    Switch {
        current,
        previous: Arc::new(previous),
        keep_previous: true,
        copier: Arc::new(copier),
        timing: Timing::default(),
    }
    .open()
    .await
}

/// What the previous class holds.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Survey {
    pub exists: bool,
    pub objects: usize,
    /// Objects under an identity the edge never writes under.
    pub not_edge: usize,
}

impl Survey {
    /// `Err` when the class cannot be held: missing (a typo would switch to
    /// an empty class, or fail every erasure once switched), or, before the
    /// switch, holding anything but the edge's memories (a mirror would
    /// copy them into the edge's class). Once switched, the marker proves
    /// the class was checked.
    pub fn refusal(&self, switched: bool) -> Result<()> {
        if !self.exists {
            return Err(eyre!(
                "[weaviate] previous_collection names a class that does not exist; name the class the edge kept its memories in, or remove the setting"
            ));
        }
        if !switched && self.not_edge > 0 {
            return Err(eyre!(
                "[weaviate] previous_collection holds {} of {} objects under identities the edge never writes under: is it a hub's class? Nothing was copied",
                self.not_edge,
                self.objects
            ));
        }
        Ok(())
    }
}

/// Count `store`'s objects, and those not under one of the edge's
/// identities. Nothing of an object is read but its identity.
pub async fn survey(store: &WeaviateStore) -> Result<Survey> {
    if !store.collection_exists().await? {
        return Ok(Survey::default());
    }
    let mut survey = Survey {
        exists: true,
        ..Survey::default()
    };
    let mut cursor = Cursor::new(store, false);
    while let Some(object) = cursor.next().await? {
        survey.objects += 1;
        let identity = object["properties"]["identityId"].as_str().unwrap_or("");
        if !is_edge_identity(identity) {
            survey.not_edge += 1;
        }
    }
    Ok(survey)
}

/// One edge replica copies at a time: a transaction-scoped advisory lock on
/// the edge's Postgres, keyed by the two classes, held for the whole sync.
pub struct PgCopier {
    pool: PostgresPool,
    key: i64,
}

impl PgCopier {
    pub fn new(pool: PostgresPool, previous: &str, current: &str) -> Self {
        Self {
            pool,
            key: lock_key(previous, current),
        }
    }
}

/// FNV-1a of the two names: the same in every replica and release.
fn lock_key(previous: &str, current: &str) -> i64 {
    let hash = format!("edge-memory-switch:{previous}->{current}")
        .bytes()
        .fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
        });
    i64::from_ne_bytes(hash.to_ne_bytes())
}

#[async_trait]
impl Copier for PgCopier {
    async fn take(&self) -> Result<Option<Box<dyn Turn>>> {
        // Held from the first, so that a take given up part way closes its
        // connection rather than hand it back inside a transaction.
        let mut turn = PgTurn {
            conn: Some(self.pool.get().await?),
        };
        let conn = turn.conn.as_ref().expect("just taken");
        conn.batch_execute("BEGIN").await?;
        let taken: bool = conn
            .query_one("SELECT pg_try_advisory_xact_lock($1)", &[&self.key])
            .await?
            .get(0);
        if !taken {
            turn.done_now().await?;
            return Ok(None);
        }
        Ok(Some(Box::new(turn)))
    }
}

/// A held [`PgCopier`] lock: its transaction, ended by [`Turn::done`].
struct PgTurn {
    conn: Option<deadpool_postgres::Object>,
}

impl PgTurn {
    async fn done_now(&mut self) -> Result<()> {
        let conn = self.conn.take().expect("held until done");
        match conn.batch_execute("COMMIT").await {
            Ok(()) => Ok(()),
            Err(e) => {
                drop(deadpool_postgres::Object::take(conn));
                Err(eyre!("the memory switch's lock was lost while held: {e}"))
            }
        }
    }
}

#[async_trait]
impl Turn for PgTurn {
    async fn done(mut self: Box<Self>) -> Result<()> {
        self.done_now().await
    }
}

impl Drop for PgTurn {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            drop(deadpool_postgres::Object::take(conn));
        }
    }
}

/// `workforce-edge memory …`.
pub async fn run_cli(args: &[String]) -> Result<()> {
    if args.is_empty() || args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{USAGE}");
        return Ok(());
    }
    let command = Command::from_args(args)?;
    let config = EdgeConfig::load(&command.config)
        .wrap_err_with(|| format!("loading edge config from {}", command.config.display()))?;
    let MemoryBackend::Weaviate(weaviate) = config.memory_backend()? else {
        return Err(eyre!(
            "the edge's memories are in its vellev tenant; there is no previous Weaviate class"
        ));
    };
    let store = |class: &str| {
        WeaviateStore::new(weaviate.url.clone(), weaviate.api_key.as_deref()).in_collection(class)
    };
    let current = store(weaviate.own_collection()?);
    let previous = weaviate.previous_collection()?.map(store);
    let pool = crate::migrate::build_pool(&config.postgres, "workforce-edge-memory")?;
    let known = known_customers(&pool).await?;
    match command.step {
        Step::Check => {
            let previous = previous.ok_or_else(|| {
                eyre!("[weaviate] previous_collection is not set; there is nothing to check")
            })?;
            let report = check(&current, &previous, &known).await?;
            println!("{report}");
            report.refusal()
        }
        Step::Reconcile { apply } => {
            let reconciled = reconcile(&current, previous.as_ref(), &known, apply).await?;
            println!("{reconciled}");
            Ok(())
        }
        Step::Drop { collection, apply } => {
            let old = store(&collection);
            let dropped = drop_previous(weaviate, &current, &old, &known, apply).await?;
            println!("{dropped}");
            Ok(())
        }
    }
}

/// Every customer the edge still knows: an erasure deletes the customer's
/// row. The install's own Weaviate serves the default tenant alone.
pub async fn known_customers(pool: &PostgresPool) -> Result<BTreeSet<String>> {
    let conn = pool.get().await?;
    let rows = conn
        .query(
            "SELECT owner FROM edge_analytics.chat_identities WHERE tenant_id = $1",
            &[&TenantId::DEFAULT],
        )
        .await
        .wrap_err("reading the customers the edge knows")?;
    Ok(rows.iter().map(|row| row.get(0)).collect())
}

/// The customers with memories in `store` that the edge no longer knows,
/// each with its agent and how many objects it holds.
pub async fn unknown_customers(
    store: &WeaviateStore,
    known: &BTreeSet<String>,
) -> Result<BTreeMap<(String, String), usize>> {
    let mut unknown = BTreeMap::new();
    if !store.collection_exists().await? {
        return Ok(unknown);
    }
    let mut cursor = Cursor::new(store, false);
    while let Some(object) = cursor.next().await? {
        let properties = &object["properties"];
        let identity = properties["identityId"].as_str().unwrap_or("");
        if is_customer_owner(identity) && !known.contains(identity) {
            let owner = properties["owner"].as_str().unwrap_or("").to_string();
            *unknown.entry((owner, identity.to_string())).or_default() += 1;
        }
    }
    Ok(unknown)
}

/// What `reconcile` found and did.
#[derive(Debug)]
pub struct Reconciled {
    pub customers: usize,
    pub objects: usize,
    pub purged: usize,
    pub applied: bool,
}

impl std::fmt::Display for Reconciled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.applied {
            true => write!(
                f,
                "{} customers the edge no longer knows: {} objects purged",
                self.customers, self.purged
            ),
            false => write!(
                f,
                "{} customers the edge no longer knows hold {} objects; dry run, add --apply",
                self.customers, self.objects
            ),
        }
    }
}

/// Purge from `current`, and `previous` when held, the memories of every
/// customer the edge no longer knows.
pub async fn reconcile(
    current: &WeaviateStore,
    previous: Option<&WeaviateStore>,
    known: &BTreeSet<String>,
    apply: bool,
) -> Result<Reconciled> {
    let mut found = unknown_customers(current, known).await?;
    if let Some(previous) = previous {
        for (key, objects) in unknown_customers(previous, known).await? {
            *found.entry(key).or_default() += objects;
        }
    }
    let mut reconciled = Reconciled {
        customers: found
            .keys()
            .map(|(_, identity)| identity)
            .collect::<BTreeSet<_>>()
            .len(),
        objects: found.values().sum(),
        purged: 0,
        applied: apply,
    };
    if !apply {
        return Ok(reconciled);
    }
    for (owner, identity) in found.keys() {
        let agent = AgentId::new(owner);
        reconciled.purged += current.purge_identity(&agent, identity).await?;
        if let Some(previous) = previous {
            if previous.collection_exists().await? {
                reconciled.purged += previous.purge_identity(&agent, identity).await?;
            }
        }
    }
    Ok(reconciled)
}

/// What `check-previous` found.
#[derive(Debug)]
pub struct Check {
    pub collection: String,
    pub collection_objects: usize,
    pub switched: bool,
    pub previous: String,
    pub held: Survey,
    /// Customers the edge no longer knows with memories in either class.
    pub unknown_customers: usize,
}

impl Check {
    pub fn refusal(&self) -> Result<()> {
        self.held.refusal(self.switched)
    }
}

impl std::fmt::Display for Check {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "{}: {} objects; memories {}",
            self.collection,
            self.collection_objects,
            if self.switched {
                "have switched to it"
            } else {
                "have not switched to it yet"
            }
        )?;
        match self.held.exists {
            true => writeln!(
                f,
                "{}: {} objects, {} under identities the edge never writes under",
                self.previous, self.held.objects, self.held.not_edge
            ),
            false => writeln!(f, "{}: does not exist", self.previous),
        }?;
        write!(
            f,
            "customers the edge no longer knows with memories: {}",
            self.unknown_customers
        )
    }
}

/// Both classes' counts, whether `previous` can be held, and how many
/// customers the edge no longer knows still have memories.
pub async fn check(
    current: &WeaviateStore,
    previous: &WeaviateStore,
    known: &BTreeSet<String>,
) -> Result<Check> {
    Ok(Check {
        collection: current.collection().to_string(),
        collection_objects: survey(current).await?.objects,
        switched: current.switched().await?,
        previous: previous.collection().to_string(),
        held: survey(previous).await?,
        unknown_customers: reconcile(current, Some(previous), known, false)
            .await?
            .customers,
    })
}

/// What `drop-previous` found and did.
#[derive(Debug)]
pub struct Dropped {
    pub collection: String,
    pub objects: usize,
    pub dropped: bool,
}

impl std::fmt::Display for Dropped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.dropped {
            true => write!(f, "{}: dropped ({} objects)", self.collection, self.objects),
            false => write!(
                f,
                "{}: {} objects would be dropped; dry run, add --apply",
                self.collection, self.objects
            ),
        }
    }
}

/// Delete `old`, once memories have switched to the edge's own class,
/// `old` is neither that class nor still held beside it, and no customer the
/// edge no longer knows has memories left in the edge's class.
pub async fn drop_previous(
    weaviate: &WeaviateConfig,
    current: &WeaviateStore,
    old: &WeaviateStore,
    known: &BTreeSet<String>,
    apply: bool,
) -> Result<Dropped> {
    let name = old.collection();
    if name.eq_ignore_ascii_case(current.collection()) {
        return Err(eyre!(
            "{name} is the edge's own class ([weaviate] collection); nothing was dropped"
        ));
    }
    if weaviate
        .previous_collection()?
        .is_some_and(|previous| previous.eq_ignore_ascii_case(name))
    {
        return Err(eyre!(
            "{name} is still [weaviate] previous_collection, so every erasure reaches it: remove the setting and deploy, then drop it. Nothing was dropped"
        ));
    }
    if !current.switched().await? {
        return Err(eyre!(
            "memories have not switched to {} yet; nothing was dropped",
            current.collection()
        ));
    }
    let unknown = reconcile(current, None, known, false).await?;
    if unknown.customers > 0 {
        return Err(eyre!(
            "{} customers the edge no longer knows still have memories in {}: run `workforce-edge memory reconcile --apply` first. Nothing was dropped",
            unknown.customers,
            current.collection()
        ));
    }
    let held = survey(old).await?;
    if !held.exists {
        return Err(eyre!("{name} does not exist; nothing was dropped"));
    }
    if held.not_edge > 0 {
        return Err(eyre!(
            "{name} holds {} of {} objects under identities the edge never writes under: is it a hub's class? Nothing was dropped",
            held.not_edge,
            held.objects
        ));
    }
    if apply {
        old.delete_collection().await?;
    }
    Ok(Dropped {
        collection: name.to_string(),
        objects: held.objects,
        dropped: apply,
    })
}

#[derive(Debug, PartialEq, Eq)]
enum Step {
    Check,
    Reconcile { apply: bool },
    Drop { collection: String, apply: bool },
}

#[derive(Debug, PartialEq, Eq)]
struct Command {
    config: PathBuf,
    step: Step,
}

impl Command {
    fn from_args(args: &[String]) -> Result<Self> {
        let mut words = args.iter();
        let step = words.next().map(String::as_str);
        let mut config = None;
        let mut collection = None;
        let mut apply = false;
        while let Some(word) = words.next() {
            match word.as_str() {
                "--config" => config = words.next().map(PathBuf::from),
                "--collection" => collection = words.next().cloned(),
                "--apply" => apply = true,
                other => return Err(eyre!("unknown argument {other:?}\n\n{USAGE}")),
            }
        }
        let config = config.ok_or_else(|| eyre!("--config <edge.toml> is required\n\n{USAGE}"))?;
        let step = match step {
            Some("check-previous") if collection.is_none() && !apply => Step::Check,
            Some("check-previous") => {
                return Err(eyre!("check-previous takes only --config\n\n{USAGE}"))
            }
            Some("reconcile") if collection.is_none() => Step::Reconcile { apply },
            Some("reconcile") => return Err(eyre!("reconcile takes no --collection\n\n{USAGE}")),
            Some("drop-previous") => Step::Drop {
                collection: collection
                    .ok_or_else(|| eyre!("--collection <class> is required\n\n{USAGE}"))?,
                apply,
            },
            other => return Err(eyre!("unknown command {other:?}\n\n{USAGE}")),
        };
        Ok(Self { config, step })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use workforce_memory::edge_owner::{external_owner, KNOWLEDGE_IDENTITY, LESSON_IDENTITY};

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    #[test]
    fn the_commands_parse_and_refuse_what_they_do_not_take() {
        assert_eq!(
            Command::from_args(&args(&["check-previous", "--config", "edge.toml"])).unwrap(),
            Command {
                config: PathBuf::from("edge.toml"),
                step: Step::Check
            }
        );
        assert_eq!(
            Command::from_args(&args(&[
                "drop-previous",
                "--config",
                "edge.toml",
                "--collection",
                "WorkforceMemory",
                "--apply"
            ]))
            .unwrap()
            .step,
            Step::Drop {
                collection: "WorkforceMemory".into(),
                apply: true
            }
        );
        assert_eq!(
            Command::from_args(&args(&["reconcile", "--config", "edge.toml"]))
                .unwrap()
                .step,
            Step::Reconcile { apply: false }
        );
        for refused in [
            &["drop-previous", "--config", "edge.toml"][..],
            &["check-previous", "--config", "edge.toml", "--apply"],
            &["check-previous"],
            &["rename", "--config", "edge.toml"],
            &["check-previous", "--config", "edge.toml", "--force"],
            &["reconcile", "--config", "edge.toml", "--collection", "X"],
        ] {
            assert!(Command::from_args(&args(refused)).is_err(), "{refused:?}");
        }
    }

    #[test]
    fn a_previous_class_is_refused_when_missing_or_not_only_the_edges() {
        let missing = Survey::default();
        assert!(missing.refusal(false).is_err());
        assert!(missing.refusal(true).is_err());
        let shared = Survey {
            exists: true,
            objects: 3,
            not_edge: 1,
        };
        let err = shared.refusal(false).unwrap_err();
        assert!(format!("{err}").contains("1 of 3"), "{err}");
        assert!(shared.refusal(true).is_ok());
        let edges = Survey {
            exists: true,
            objects: 3,
            not_edge: 0,
        };
        assert!(edges.refusal(false).is_ok());
    }

    #[test]
    fn the_lock_key_is_fixed_by_the_two_classes() {
        let key = lock_key("WorkforceMemory", "EdgeMemory");
        assert_eq!(key, lock_key("WorkforceMemory", "EdgeMemory"));
        assert_ne!(key, lock_key("EdgeMemory", "WorkforceMemory"));
        assert_ne!(key, lock_key("WorkforceMemory", "OtherMemory"));
    }

    /// A Weaviate holding classes of objects under identities, a switch
    /// marker when `switched`, and a record of classes deleted.
    struct Weaviate {
        url: String,
        deleted: Arc<Mutex<Vec<String>>>,
    }

    async fn weaviate(classes: &[(&str, Vec<String>)], switched: bool) -> Weaviate {
        use axum::extract::{Path, Query};
        use axum::http::StatusCode;
        use axum::routing::get;
        let classes: Arc<BTreeMap<String, Vec<String>>> = Arc::new(
            classes
                .iter()
                .map(|(name, identities)| (name.to_string(), identities.clone()))
                .collect(),
        );
        let deleted = Arc::new(Mutex::new(Vec::new()));
        let schema_classes = Arc::clone(&classes);
        let gone = Arc::clone(&deleted);
        let listed = Arc::clone(&classes);
        let app = axum::Router::new()
            .route(
                "/v1/schema/{class}",
                get(move |Path(class): Path<String>| {
                    let classes = Arc::clone(&schema_classes);
                    async move {
                        match classes.contains_key(&class) {
                            true => StatusCode::OK,
                            false => StatusCode::NOT_FOUND,
                        }
                    }
                })
                .delete(move |Path(class): Path<String>| {
                    let gone = Arc::clone(&gone);
                    async move {
                        gone.lock().unwrap().push(class);
                        StatusCode::OK
                    }
                }),
            )
            .route(
                "/v1/objects",
                get(move |Query(query): Query<BTreeMap<String, String>>| {
                    let classes = Arc::clone(&listed);
                    async move {
                        let objects: Vec<serde_json::Value> = match query.contains_key("after") {
                            true => Vec::new(),
                            false => classes
                                .get(query.get("class").map(String::as_str).unwrap_or(""))
                                .into_iter()
                                .flatten()
                                .enumerate()
                                .map(|(n, identity)| {
                                    serde_json::json!({
                                        "id": uuid::Uuid::from_u128(n as u128 + 1).to_string(),
                                        "properties": { "identityId": identity, "owner": "assistant" },
                                    })
                                })
                                .collect(),
                        };
                        axum::Json(serde_json::json!({ "objects": objects }))
                    }
                }),
            )
            .route(
                "/v1/objects/{class}/{id}",
                get(move |Path((class, _id)): Path<(String, String)>| {
                    let marked = switched && class == "WorkforceMemorySwitch";
                    async move {
                        match marked {
                            true => StatusCode::OK,
                            false => StatusCode::NOT_FOUND,
                        }
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Weaviate { url, deleted }
    }

    fn edges() -> Vec<String> {
        vec![
            external_owner("https://iss", "sub-1"),
            LESSON_IDENTITY.to_string(),
            KNOWLEDGE_IDENTITY.to_string(),
        ]
    }

    /// The customers the edge knows: the one [`edges`] holds memories of.
    fn known() -> BTreeSet<String> {
        BTreeSet::from([external_owner("https://iss", "sub-1")])
    }

    fn config(url: &str, previous: Option<&str>) -> WeaviateConfig {
        WeaviateConfig {
            url: url.to_string(),
            api_key: None,
            collection: Some("EdgeMemory".into()),
            previous_collection: previous.map(str::to_string),
            connect_budget_secs: 1,
        }
    }

    fn store(url: &str, class: &str) -> WeaviateStore {
        WeaviateStore::new(url, None).in_collection(class)
    }

    #[tokio::test]
    async fn a_survey_counts_objects_not_under_the_edges_identities() {
        let mut identities = edges();
        identities.push("a-hub-user".into());
        let w = weaviate(&[("WorkforceMemory", identities)], false).await;
        assert_eq!(
            survey(&store(&w.url, "WorkforceMemory")).await.unwrap(),
            Survey {
                exists: true,
                objects: 4,
                not_edge: 1
            }
        );
        assert_eq!(
            survey(&store(&w.url, "Missing")).await.unwrap(),
            Survey::default()
        );
    }

    #[tokio::test]
    async fn the_previous_class_is_dropped_only_once_switched_and_no_longer_held() {
        let w = weaviate(
            &[("WorkforceMemory", edges()), ("EdgeMemory", edges())],
            true,
        )
        .await;
        let current = store(&w.url, "EdgeMemory");
        let old = store(&w.url, "WorkforceMemory");
        let held = config(&w.url, Some("WorkforceMemory"));
        let err = drop_previous(&held, &current, &old, &known(), true)
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("still [weaviate] previous_collection"));
        let settled = config(&w.url, None);
        let own = store(&w.url, "edgememory");
        let err = drop_previous(&settled, &current, &own, &known(), true)
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("the edge's own class"));
        let err = drop_previous(&settled, &current, &old, &BTreeSet::new(), true)
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("reconcile --apply"), "{err}");
        let dry = drop_previous(&settled, &current, &old, &known(), false)
            .await
            .unwrap();
        assert!(!dry.dropped);
        assert_eq!(dry.objects, 3);
        assert!(w.deleted.lock().unwrap().is_empty());
        let dropped = drop_previous(&settled, &current, &old, &known(), true)
            .await
            .unwrap();
        assert!(dropped.dropped);
        assert_eq!(
            w.deleted.lock().unwrap().clone(),
            vec!["WorkforceMemory".to_string()]
        );
    }

    #[tokio::test]
    async fn nothing_is_dropped_before_the_switch_or_from_a_shared_class() {
        let unswitched = weaviate(&[("WorkforceMemory", edges())], false).await;
        let err = drop_previous(
            &config(&unswitched.url, None),
            &store(&unswitched.url, "EdgeMemory"),
            &store(&unswitched.url, "WorkforceMemory"),
            &known(),
            true,
        )
        .await
        .unwrap_err();
        assert!(format!("{err}").contains("have not switched"));
        let mut identities = edges();
        identities.push("a-hub-user".into());
        let shared = weaviate(&[("WorkforceMemory", identities)], true).await;
        let err = drop_previous(
            &config(&shared.url, None),
            &store(&shared.url, "EdgeMemory"),
            &store(&shared.url, "WorkforceMemory"),
            &known(),
            true,
        )
        .await
        .unwrap_err();
        assert!(format!("{err}").contains("is it a hub's class"));
        assert!(unswitched.deleted.lock().unwrap().is_empty());
        assert!(shared.deleted.lock().unwrap().is_empty());
    }

    /// Memories of a customer the edge no longer knows are found in both
    /// classes and counted once per customer; lessons and knowledge are
    /// never a customer's.
    #[tokio::test]
    async fn a_reconcile_finds_the_customers_the_edge_no_longer_knows() {
        let erased = external_owner("https://iss", "erased");
        let mut current = edges();
        current.push(erased.clone());
        let previous = vec![erased.clone(), erased, LESSON_IDENTITY.to_string()];
        let w = weaviate(
            &[("EdgeMemory", current), ("WorkforceMemory", previous)],
            true,
        )
        .await;
        let found = reconcile(
            &store(&w.url, "EdgeMemory"),
            Some(&store(&w.url, "WorkforceMemory")),
            &known(),
            false,
        )
        .await
        .unwrap();
        assert_eq!((found.customers, found.objects, found.purged), (1, 3, 0));
        let everyone: BTreeSet<String> = known()
            .into_iter()
            .chain([external_owner("https://iss", "erased")])
            .collect();
        let none = reconcile(&store(&w.url, "EdgeMemory"), None, &everyone, false)
            .await
            .unwrap();
        assert_eq!((none.customers, none.objects), (0, 0));
    }

    #[tokio::test]
    async fn one_copier_holds_the_lock_until_its_turn_is_done() {
        use workforce_common::session_postgres::PostgresSessionStore;
        use workforce_postgres::test_support::with_test_pool;
        with_test_pool(PostgresSessionStore::build_pool, |pool| async move {
            let first = PgCopier::new(pool.clone(), "WorkforceMemory", "EdgeMemory");
            let second = PgCopier::new(pool.clone(), "WorkforceMemory", "EdgeMemory");
            let other = PgCopier::new(pool, "OldMemory", "EdgeMemory");
            let turn = first.take().await.unwrap().expect("a free lock");
            assert!(second.take().await.unwrap().is_none());
            let unrelated = other.take().await.unwrap().expect("another pair's lock");
            unrelated.done().await.unwrap();
            turn.done().await.unwrap();
            let again = second.take().await.unwrap().expect("given back");
            again.done().await.unwrap();
        })
        .await;
    }
}
