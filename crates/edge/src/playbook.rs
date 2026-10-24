//! Playbook execution for customer conversations.
//!
//! The edge reuses the pure playbook engine (`workforce-playbook`) but drives it
//! conversationally: there are no Slack buttons and no workspace file tools, so
//! step instructions ride the system prompt every turn and the *agent* drives
//! the flow with a `playbook` tool. An agent carries a **catalog** of flows
//! (`playbooks = [..]`): when no flow is active the tool offers `start` and the
//! prompt lists what each flow covers, so the agent starts one when it detects
//! the user wants something a flow guides; `playbook = "<id>"` additionally
//! auto-starts that flow on each new conversation. Definitions are
//! operator-authored TOML loaded at boot (fail closed on a bad reference);
//! per-conversation progress lives on the session like everywhere else.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use serde_json::Value;
use workforce_agent::tools::{Tool, ToolResult};
use workforce_common::playbook_state::PlaybookProgress;
use workforce_playbook::{
    advance, advance_to, current_index, finish, PlaybookDefinition, StepOutcome,
};

use crate::agent::EdgeAgent;

/// Read `playbooks_dir` as `(id, canonical-TOML)` pairs for seeding the store.
/// Fails on a duplicate id (last-file-wins would flip between deploys).
pub fn dir_sources(dir: Option<&str>) -> Result<Vec<(String, String)>> {
    let Some(dir) = dir else {
        return Ok(Vec::new());
    };
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for def in PlaybookDefinition::load_dir(Path::new(dir))
        .wrap_err_with(|| format!("loading playbooks from {dir}"))?
    {
        let id = def.playbook.id.clone();
        if !seen.insert(id.clone()) {
            eyre::bail!("duplicate playbook id {id:?} in {dir}");
        }
        let source = toml::to_string(&def).wrap_err("re-encoding playbook for seed")?;
        out.push((id, source));
    }
    Ok(out)
}

/// Parse stored `(id, source)` pairs into definitions. An unparseable or
/// id-mismatched row is skipped loudly rather than failing the whole set — one
/// bad playbook must not take the catalog (or boot) down.
pub fn parse_sources(sources: Vec<(String, String)>) -> HashMap<String, Arc<PlaybookDefinition>> {
    let mut defs = HashMap::with_capacity(sources.len());
    for (id, source) in sources {
        match PlaybookDefinition::from_toml(&source) {
            Ok(def) if def.playbook.id == id => {
                defs.insert(id, Arc::new(def));
            }
            Ok(def) => tracing::warn!(
                stored = %id,
                parsed = %def.playbook.id,
                "stored playbook id mismatch; skipped"
            ),
            Err(e) => tracing::warn!(id = %id, error = %e, "stored playbook invalid; skipped"),
        }
    }
    defs
}

/// Warn (never bail) on an agent referencing a playbook the catalog doesn't
/// hold — the operator can create it in the admin UI, and per-turn resolution
/// heals it without a restart. Boot resilience beats strictness now that both
/// agents and playbooks are runtime-editable.
pub fn warn_dangling_refs(
    agents: &[Arc<EdgeAgent>],
    defs: &HashMap<String, Arc<PlaybookDefinition>>,
) {
    for agent in agents {
        for id in agent.playbook.iter().chain(agent.playbooks.iter()) {
            if !defs.contains_key(id) {
                tracing::warn!(
                    agent = %agent.id,
                    playbook = %id,
                    "agent references a playbook not currently defined; create it in the admin UI"
                );
            }
        }
    }
}

/// Every playbook this install serves, in one hot-swappable map. Seeded at boot
/// (from `playbooks_dir`, then the DB store) and replaced wholesale whenever the
/// admin UI creates/edits/deletes a flow — the next customer turn resolves
/// against the new set, so edits apply live without a restart.
pub struct PlaybookCatalog {
    defs: std::sync::RwLock<HashMap<String, Arc<PlaybookDefinition>>>,
}

impl PlaybookCatalog {
    pub fn new(defs: HashMap<String, Arc<PlaybookDefinition>>) -> Arc<Self> {
        Arc::new(Self {
            defs: std::sync::RwLock::new(defs),
        })
    }

    /// Replace the whole set (called after any admin playbook change).
    pub fn set(&self, defs: Vec<Arc<PlaybookDefinition>>) {
        let map = defs
            .into_iter()
            .map(|d| (d.playbook.id.clone(), d))
            .collect();
        *self.defs.write().expect("catalog lock") = map;
    }

    /// Resolve a playbook by id against the live set.
    pub fn resolve(&self, id: &str) -> Option<Arc<PlaybookDefinition>> {
        self.defs.read().expect("catalog lock").get(id).cloned()
    }
}

/// The agent's guided flows, resolved once at host build: the catalog the tool
/// may start, plus the optional flow auto-started on each new conversation.
#[derive(Clone, Default)]
pub struct AgentPlaybooks {
    catalog: Vec<Arc<PlaybookDefinition>>,
    auto_start: Option<Arc<PlaybookDefinition>>,
}

impl AgentPlaybooks {
    /// Resolve the agent's flows against the LIVE catalog for one turn: the
    /// optional auto-start plus its explicit `playbooks` ids. Missing ids warn
    /// and drop — a referenced flow may have been deleted/renamed in the admin
    /// UI, and resolution is per-turn so it heals on the next edit.
    pub fn for_turn(agent: &EdgeAgent, catalog: &PlaybookCatalog) -> Self {
        let lookup = |id: &String| {
            let def = catalog.resolve(id);
            if def.is_none() {
                tracing::warn!(agent = %agent.id, playbook = %id, "configured playbook not available this turn");
            }
            def
        };
        let auto_start = agent.playbook.as_ref().and_then(lookup);
        let mut flows: Vec<Arc<PlaybookDefinition>> =
            agent.playbooks.iter().filter_map(lookup).collect();
        if let Some(auto) = &auto_start {
            if !flows.iter().any(|d| d.playbook.id == auto.playbook.id) {
                flows.insert(0, auto.clone());
            }
        }
        Self {
            catalog: flows,
            auto_start,
        }
    }

    /// Whether this agent has any flows at all (drives tool registration).
    pub fn is_empty(&self) -> bool {
        self.catalog.is_empty()
    }

    fn by_id(&self, id: &str) -> Option<Arc<PlaybookDefinition>> {
        self.catalog.iter().find(|d| d.playbook.id == id).cloned()
    }

    /// Build the conversation's cursor for this turn: resume saved progress
    /// (dropping it if stale — def swapped or its step edited away), else
    /// auto-start on a brand-new conversation, else idle with the catalog.
    pub fn cursor_for_turn(
        &self,
        saved: Option<PlaybookProgress>,
        new_conversation: bool,
    ) -> Arc<PlaybookCursor> {
        let state = match saved {
            Some(progress) => match self.by_id(&progress.id) {
                // Def present and step still exists: resume in place.
                Some(def) if current_index(&progress, &def).is_some() => {
                    FlowState::Active { def, progress }
                }
                // Def present but the step was edited away: intentional change,
                // drop the stale progress.
                Some(_) => FlowState::idle(),
                // Def not resolvable THIS turn — possibly a hub-synced def the
                // sync hasn't delivered yet (restart race). Preserve the
                // progress so it resumes when the def arrives, rather than
                // wiping a customer's in-flight flow.
                None => FlowState::Idle {
                    orphaned: Some(progress),
                },
            },
            None => match (&self.auto_start, new_conversation) {
                (Some(def), true) => start_state(def.clone()).unwrap_or_else(FlowState::idle),
                _ => FlowState::idle(),
            },
        };
        Arc::new(PlaybookCursor {
            flows: self.clone(),
            state: Mutex::new(state),
        })
    }
}

/// Fresh progress on a definition's first step.
fn start_state(def: Arc<PlaybookDefinition>) -> Option<FlowState> {
    let first = def.steps.first()?;
    let progress = PlaybookProgress::new(
        &def.playbook.id,
        &first.id,
        uuid::Uuid::new_v4().to_string(),
        // No workspace on the edge — customers have no file tools, so no
        // scratch dir and no pinned artifacts.
        "",
    );
    Some(FlowState::Active { def, progress })
}

enum FlowState {
    /// No flow running — the catalog is on offer. `orphaned` carries saved
    /// progress whose definition wasn't resolvable this turn (e.g. a hub-synced
    /// def before the first successful poll): it is persisted untouched so the
    /// flow resumes when the def arrives, and dropped the moment another flow
    /// starts or this one is explicitly ended.
    Idle { orphaned: Option<PlaybookProgress> },
    /// A flow is mid-run.
    Active {
        def: Arc<PlaybookDefinition>,
        progress: PlaybookProgress,
    },
}

impl FlowState {
    fn idle() -> Self {
        FlowState::Idle { orphaned: None }
    }
}

/// A conversation's live playbook position, shared between the turn driver and
/// the `playbook` tool (the tool mutates it mid-run; the driver persists it on
/// the session afterwards).
pub struct PlaybookCursor {
    flows: AgentPlaybooks,
    state: Mutex<FlowState>,
}

impl PlaybookCursor {
    /// The per-turn system-prompt block. Active: playbook position, the current
    /// step's full instructions, and how the agent advances — present every
    /// turn so a step can span many chat turns without losing the script.
    /// Idle: the catalog, so the agent starts a flow when the user's intent
    /// matches one.
    pub fn system_block(&self) -> String {
        let state = self.state.lock().expect("playbook cursor lock");
        match &*state {
            FlowState::Idle { .. } => self.catalog_block(),
            FlowState::Active { def, progress } => active_block(def, progress),
        }
    }

    fn catalog_block(&self) -> String {
        if self.flows.catalog.is_empty() {
            return String::new();
        }
        let mut out = String::from(
            "\n\n## Guided flows available\n\
             You can walk this user through the following guided flows. When their request \
             clearly matches one, call the `playbook` tool with action=\"start\" and the flow's \
             id — its result gives you the first step's instructions. Suggest a flow when it \
             fits; never force one on a user who wants something else.\n",
        );
        for def in &self.flows.catalog {
            let meta = &def.playbook;
            if meta.description.is_empty() {
                out.push_str(&format!("- `{}`: {}\n", meta.id, meta.name));
            } else {
                out.push_str(&format!(
                    "- `{}`: {} — {}\n",
                    meta.id,
                    meta.name,
                    meta.description.trim()
                ));
            }
        }
        out
    }

    /// Snapshot for persisting on the session after the turn: the progress to
    /// save, or `None` when no flow is running (clear it instead).
    pub fn persistable(&self) -> Option<PlaybookProgress> {
        let state = self.state.lock().expect("playbook cursor lock");
        match &*state {
            FlowState::Active { progress, .. } => Some(progress.clone()),
            FlowState::Idle { orphaned } => orphaned.clone(),
        }
    }

    fn transition(&self, action: &str, input: &Value) -> Result<Value, String> {
        let mut state = self.state.lock().expect("playbook cursor lock");
        match &mut *state {
            FlowState::Idle { .. } => match action {
                "start" => {
                    let Some(id) = input.get("id").and_then(Value::as_str) else {
                        return Err("start requires the flow `id`".into());
                    };
                    let Some(def) = self.flows.by_id(id) else {
                        let known: Vec<&str> = self
                            .flows
                            .catalog
                            .iter()
                            .map(|d| d.playbook.id.as_str())
                            .collect();
                        return Err(format!("no flow with id {id:?}; available: {known:?}"));
                    };
                    let Some(fresh) = start_state(def) else {
                        return Err(format!("flow {id:?} has no steps"));
                    };
                    *state = fresh;
                    let FlowState::Active { def, progress } = &*state else {
                        unreachable!("start_state only builds Active");
                    };
                    Ok(step_result(def, current_index(progress, def).unwrap_or(0)))
                }
                _ => Err(
                    "no flow is active; use action=\"start\" with a flow id to begin one".into(),
                ),
            },
            FlowState::Active { def, progress } => {
                let nonce = uuid::Uuid::new_v4().to_string();
                let idx = current_index(progress, def);
                let step = idx.and_then(|i| def.steps.get(i));
                let outcome = match action {
                    "start" => {
                        return Err(format!(
                            "flow {:?} is already active; finish or exit it first",
                            def.playbook.id
                        ))
                    }
                    "exit" => {
                        // The user asked to stop: abandon the flow (no completion
                        // implied) and return to the catalog.
                        *state = FlowState::idle();
                        return Ok(serde_json::json!({
                            "exited": true,
                            "note": "The flow is closed. Help the user with whatever they need next.",
                        }));
                    }
                    "advance" => {
                        // A forced-choice step (keep_continue = false) has no
                        // linear edge — the flow must branch via a declared target.
                        if let Some(step) = step {
                            if !step.keep_continue {
                                let branches: Vec<&str> = step
                                    .buttons
                                    .iter()
                                    .filter_map(|b| b.goto.as_deref())
                                    .collect();
                                return Err(format!(
                                    "this step requires choosing a branch: use action=\"goto\" \
                                     with target one of {branches:?}"
                                ));
                            }
                        }
                        advance(progress, def, None, nonce)
                    }
                    "finish" => {
                        // Only the final step (or a step that declares a finish
                        // button) may complete the flow — otherwise "finish" would
                        // be a silent skip-everything escape hatch.
                        let allowed = match idx {
                            Some(i) => {
                                i + 1 == def.steps.len()
                                    || def.steps[i].buttons.iter().any(|b| b.finish)
                            }
                            None => false,
                        };
                        if !allowed {
                            return Err(
                                "finish is only available on the final step; use advance".into()
                            );
                        }
                        finish(progress, None, nonce)
                    }
                    "goto" => {
                        let Some(target) = input.get("target").and_then(Value::as_str) else {
                            return Err("goto requires a `target` step id".into());
                        };
                        // Only the current step's declared branches are legal
                        // edges — otherwise goto would bypass forced-choice gates
                        // and jump the authored graph arbitrarily.
                        let branches: Vec<&str> = step
                            .map(|s| s.buttons.iter().filter_map(|b| b.goto.as_deref()).collect())
                            .unwrap_or_default();
                        if !branches.contains(&target) {
                            return Err(format!(
                                "goto target {target:?} is not a declared branch of this step; \
                                 available: {branches:?}"
                            ));
                        }
                        advance_to(progress, def, target, None, nonce)
                    }
                    other => return Err(format!("unknown action {other:?}")),
                };
                match outcome {
                    StepOutcome::Run(next) => Ok(step_result(def, next)),
                    StepOutcome::Complete => {
                        *state = FlowState::idle();
                        Ok(serde_json::json!({
                            "complete": true,
                            "note": "The flow is complete. Wrap up warmly and answer anything else the user needs.",
                        }))
                    }
                }
            }
        }
    }
}

/// The tool result for landing on a step: position plus the step's instructions
/// so the model can continue guiding immediately.
fn step_result(def: &PlaybookDefinition, idx: usize) -> Value {
    let step = &def.steps[idx];
    serde_json::json!({
        "flow": def.playbook.id,
        "step": step.id,
        "title": step.title,
        "position": format!("{}/{}", idx + 1, def.steps.len()),
        "instructions": step.prompt,
    })
}

/// The "Active guided flow" system block for the current step.
fn active_block(def: &PlaybookDefinition, progress: &PlaybookProgress) -> String {
    let total = def.steps.len();
    let Some(idx) = current_index(progress, def) else {
        return String::new();
    };
    let step = &def.steps[idx];
    let mut out = format!(
        "\n\n## Active guided flow: {} — step {}/{total}: {}\n\
         You are guiding this user through a scripted flow. Instructions for the CURRENT step:\n\n{}\n\n",
        def.playbook.name,
        idx + 1,
        step.title,
        step.prompt.trim(),
    );
    out.push_str("Work only on this step. Do not start or pre-empt later steps. ");
    let branches: Vec<&str> = step
        .buttons
        .iter()
        .filter_map(|b| b.goto.as_deref())
        .collect();
    if step.keep_continue {
        out.push_str(
            "When this step's goal is met and the user is ready, call the `playbook` tool \
             with action=\"advance\" to move on — its result gives you the next step's \
             instructions. ",
        );
        if !branches.is_empty() {
            out.push_str(&format!(
                "This step can also branch: action=\"goto\" with target one of {branches:?}. "
            ));
        }
    } else {
        out.push_str(&format!(
            "This step is a choice point: when the user has decided, call the `playbook` \
             tool with action=\"goto\" and target one of {branches:?} — plain advance is \
             not available here. "
        ));
    }
    match def.steps.get(idx + 1) {
        Some(next) => out.push_str(&format!("The next step is \"{}\". ", next.title)),
        None => out.push_str(
            "This is the final step; when it is done call the tool with action=\"finish\". ",
        ),
    }
    out.push_str("If the user asks to stop the flow, call the tool with action=\"exit\".\n");
    out
}

/// The tool the agent uses to start and drive guided flows. Registered whenever
/// the agent has any flows; all input comes from the model, so actions, ids and
/// targets are validated against the catalog and the authored graph.
pub struct EdgePlaybookTool {
    cursor: Arc<PlaybookCursor>,
}

impl EdgePlaybookTool {
    pub fn new(cursor: Arc<PlaybookCursor>) -> Self {
        Self { cursor }
    }
}

#[async_trait]
impl Tool for EdgePlaybookTool {
    fn name(&self) -> &str {
        "playbook"
    }

    fn description(&self) -> &str {
        "Start and drive guided flows. action=start with `id` begins a flow from the catalog \
         when the user's request matches it; action=advance moves to the next step once the \
         current step's goal is met and the user is ready; action=goto with `target` follows a \
         declared branch; action=finish ends the flow from its final step; action=exit stops \
         the flow when the user asks to. Results carry the next step's instructions."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["start", "advance", "goto", "finish", "exit"]},
                "id": {"type": "string", "description": "Flow id to start (action=start)."},
                "target": {"type": "string", "description": "Step id to jump to (action=goto)."}
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let started = Instant::now();
        let action = input.get("action").and_then(Value::as_str).unwrap_or("");
        match self.cursor.transition(action, &input) {
            Ok(body) => Ok(ToolResult::success(
                body,
                started.elapsed().as_millis() as u64,
            )),
            Err(message) => Ok(ToolResult::failure(
                message,
                started.elapsed().as_millis() as u64,
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defs() -> HashMap<String, Arc<PlaybookDefinition>> {
        let tour = r#"
[playbook]
id = "onboarding"
name = "Onboarding"
description = "Walks a new user through setup."

[[step]]
id = "welcome"
title = "Welcome"
prompt = "Greet the user and learn their goal."

[[step]]
id = "setup"
title = "Set up"
prompt = "Walk them through setup."

[[step.button]]
label = "Skip to review"
goto = "review"

[[step]]
id = "review"
title = "Review"
prompt = "Confirm everything works."
"#;
        let refund = r#"
[playbook]
id = "refund"
name = "Request a refund"
description = "Collects what's needed to file a refund."

[[step]]
id = "reason"
title = "Reason"
prompt = "Ask why they want a refund."

[[step]]
id = "confirm"
title = "Confirm"
prompt = "Confirm the request."
"#;
        [tour, refund]
            .into_iter()
            .map(|t| {
                let d = PlaybookDefinition::from_toml(t).unwrap();
                (d.playbook.id.clone(), Arc::new(d))
            })
            .collect()
    }

    fn agent(auto: Option<&str>, catalog: &[&str]) -> EdgeAgent {
        EdgeAgent {
            playbook: auto.map(str::to_string),
            playbooks: catalog.iter().map(|s| s.to_string()).collect(),
            ..EdgeAgent::default()
        }
    }

    fn flows(auto: Option<&str>, catalog: &[&str]) -> AgentPlaybooks {
        let cat = PlaybookCatalog::new(defs());
        AgentPlaybooks::for_turn(&agent(auto, catalog), &cat)
    }

    #[test]
    fn dir_sources_fails_closed_on_duplicate_ids() {
        let dir = tempfile::tempdir().unwrap();
        let toml = r#"
[playbook]
id = "dup"
name = "Dup"
[[step]]
id = "one"
title = "One"
prompt = "p"
"#;
        std::fs::write(dir.path().join("a.toml"), toml).unwrap();
        std::fs::write(dir.path().join("b.toml"), toml).unwrap();
        let err = dir_sources(dir.path().to_str()).unwrap_err();
        assert!(err.to_string().contains("duplicate playbook id"));
        // No dir → empty seed set.
        assert!(dir_sources(None).unwrap().is_empty());
    }

    #[test]
    fn parse_sources_skips_bad_rows_keeps_good() {
        let good = "[playbook]\nid = \"ok\"\nname = \"OK\"\n[[step]]\nid = \"x\"\ntitle = \"X\"\nprompt = \"p\"\n";
        let defs = parse_sources(vec![
            ("ok".into(), good.into()),
            ("broken".into(), "not toml [".into()),
            // id claims "other" but the source says "ok" — dropped.
            ("other".into(), good.into()),
        ]);
        assert_eq!(defs.len(), 1);
        assert!(defs.contains_key("ok"));
    }

    #[test]
    fn warn_dangling_refs_never_bails() {
        // A dangling agent→playbook ref must not panic/error — both are
        // runtime-editable now, so boot stays up and per-turn resolution heals.
        let a = Arc::new(agent(Some("missing"), &["also-missing"]));
        warn_dangling_refs(&[a], &HashMap::new());
    }

    #[test]
    fn catalog_set_replaces_the_whole_live_set() {
        let catalog = PlaybookCatalog::new(defs()); // onboarding + refund
        assert!(catalog.resolve("onboarding").is_some());
        assert!(catalog.resolve("refund").is_some());
        // An admin change replaces the set wholesale — old ids gone, new present.
        let billing = PlaybookDefinition::from_toml(
            "[playbook]\nid = \"billing\"\nname = \"Billing\"\n[[step]]\nid = \"x\"\ntitle = \"X\"\nprompt = \"p\"\n",
        )
        .unwrap();
        catalog.set(vec![Arc::new(billing)]);
        assert!(catalog.resolve("onboarding").is_none(), "old def gone");
        assert!(catalog.resolve("refund").is_none(), "old def gone");
        assert_eq!(catalog.resolve("billing").unwrap().playbook.name, "Billing");
    }

    #[test]
    fn for_turn_resolves_live_against_runtime_defs() {
        let catalog = PlaybookCatalog::new(HashMap::new());
        let a = agent(None, &["billing"]);
        // No runtime def yet: missing id dropped with a warning.
        assert!(AgentPlaybooks::for_turn(&a, &catalog).is_empty());
        // After a sync the same agent picks the flows up — no restart.
        let billing = PlaybookDefinition::from_toml(
            "[playbook]\nid = \"billing\"\nname = \"Billing\"\n[[step]]\nid = \"x\"\ntitle = \"X\"\nprompt = \"p\"\n",
        )
        .unwrap();
        // A runtime-managed def lands (admin UI edit): same agent, next turn.
        catalog.set(vec![Arc::new(billing)]);
        let flows = AgentPlaybooks::for_turn(&a, &catalog);
        assert!(!flows.is_empty());
        let cursor = flows.cursor_for_turn(None, false);
        assert!(cursor.system_block().contains("`billing`: Billing"));
    }

    #[test]
    fn auto_start_flow_joins_the_catalog() {
        let f = flows(Some("onboarding"), &["refund"]);
        assert_eq!(f.catalog.len(), 2);
        // And is not duplicated when listed in both places.
        let f = flows(Some("onboarding"), &["onboarding", "refund"]);
        assert_eq!(f.catalog.len(), 2);
    }

    #[test]
    fn idle_block_lists_the_catalog_and_start_begins_a_flow() {
        let cursor = flows(None, &["onboarding", "refund"]).cursor_for_turn(None, true);
        let block = cursor.system_block();
        assert!(block.contains("Guided flows available"));
        assert!(block.contains("`onboarding`: Onboarding — Walks a new user"));
        assert!(block.contains("`refund`: Request a refund"));
        assert!(block.contains("action=\"start\""));
        // No auto-start: nothing to persist while idle.
        assert!(cursor.persistable().is_none());

        // Wrong / missing ids are rejected with the catalog.
        let err = cursor
            .transition("start", &serde_json::json!({"action":"start","id":"nope"}))
            .unwrap_err();
        assert!(err.contains("onboarding") && err.contains("refund"));
        assert!(cursor
            .transition("start", &serde_json::json!({"action":"start"}))
            .is_err());
        // Driving actions require an active flow.
        assert!(cursor
            .transition("advance", &serde_json::json!({"action":"advance"}))
            .unwrap_err()
            .contains("start"));

        let started = cursor
            .transition(
                "start",
                &serde_json::json!({"action":"start","id":"refund"}),
            )
            .unwrap();
        assert_eq!(started["flow"], "refund");
        assert_eq!(started["step"], "reason");
        assert!(cursor
            .system_block()
            .contains("Request a refund — step 1/2"));
        assert_eq!(cursor.persistable().unwrap().id, "refund");
        // Starting another while active is rejected.
        assert!(cursor
            .transition(
                "start",
                &serde_json::json!({"action":"start","id":"onboarding"})
            )
            .is_err());
    }

    #[test]
    fn auto_start_only_on_new_conversations() {
        let f = flows(Some("onboarding"), &[]);
        let fresh = f.cursor_for_turn(None, true);
        assert_eq!(fresh.persistable().unwrap().id, "onboarding");
        // An existing conversation without progress stays idle (the flow
        // completed or predates the config) — but the catalog remains on offer.
        let old = f.cursor_for_turn(None, false);
        assert!(old.persistable().is_none());
        assert!(old.system_block().contains("Guided flows available"));
    }

    #[test]
    fn completing_a_flow_returns_to_the_catalog_for_the_next_one() {
        let cursor = flows(None, &["refund", "onboarding"]).cursor_for_turn(None, false);
        cursor
            .transition(
                "start",
                &serde_json::json!({"action":"start","id":"refund"}),
            )
            .unwrap();
        cursor
            .transition("advance", &serde_json::json!({"action":"advance"}))
            .unwrap();
        let done = cursor
            .transition("advance", &serde_json::json!({"action":"advance"}))
            .unwrap();
        assert_eq!(done["complete"], true);
        // Back to the catalog: progress cleared, another flow can start.
        assert!(cursor.persistable().is_none());
        assert!(cursor.system_block().contains("Guided flows available"));
        let next = cursor
            .transition(
                "start",
                &serde_json::json!({"action":"start","id":"onboarding"}),
            )
            .unwrap();
        assert_eq!(next["flow"], "onboarding");
    }

    #[test]
    fn exit_abandons_the_flow_from_any_step() {
        let cursor = flows(None, &["onboarding"]).cursor_for_turn(None, false);
        cursor
            .transition(
                "start",
                &serde_json::json!({"action":"start","id":"onboarding"}),
            )
            .unwrap();
        assert!(cursor.system_block().contains("action=\"exit\""));
        let out = cursor
            .transition("exit", &serde_json::json!({"action":"exit"}))
            .unwrap();
        assert_eq!(out["exited"], true);
        assert!(cursor.persistable().is_none());
        assert!(cursor.system_block().contains("Guided flows available"));
    }

    #[test]
    fn goto_is_limited_to_the_current_steps_declared_branches() {
        let cursor = flows(None, &["onboarding"]).cursor_for_turn(None, false);
        cursor
            .transition(
                "start",
                &serde_json::json!({"action":"start","id":"onboarding"}),
            )
            .unwrap();
        // Step 1 declares no branches: any goto is rejected, even to a step
        // that exists — the authored graph has no such edge.
        assert!(cursor
            .transition(
                "goto",
                &serde_json::json!({"action":"goto","target":"review"})
            )
            .is_err());
        cursor
            .transition("advance", &serde_json::json!({"action":"advance"}))
            .unwrap();
        let jumped = cursor
            .transition(
                "goto",
                &serde_json::json!({"action":"goto","target":"review"}),
            )
            .unwrap();
        assert_eq!(jumped["step"], "review");
    }

    #[test]
    fn finish_is_rejected_before_the_final_step() {
        let cursor = flows(None, &["onboarding"]).cursor_for_turn(None, false);
        cursor
            .transition(
                "start",
                &serde_json::json!({"action":"start","id":"onboarding"}),
            )
            .unwrap();
        let err = cursor
            .transition("finish", &serde_json::json!({"action":"finish"}))
            .unwrap_err();
        assert!(err.contains("final step"));
    }

    #[test]
    fn forced_choice_step_rejects_advance_and_advertises_goto() {
        let d = PlaybookDefinition::from_toml(
            r#"
[playbook]
id = "install"
name = "Install"

[[step]]
id = "os"
title = "Pick your OS"
prompt = "Ask which OS they use."
keep_continue = false

[[step.button]]
label = "Mac"
goto = "mac"

[[step.button]]
label = "Linux"
goto = "linux"

[[step]]
id = "mac"
title = "Mac setup"
prompt = "Guide Mac setup."

[[step]]
id = "linux"
title = "Linux setup"
prompt = "Guide Linux setup."
"#,
        )
        .unwrap();
        let catalog = PlaybookCatalog::new(HashMap::from([("install".to_string(), Arc::new(d))]));
        let f = AgentPlaybooks::for_turn(&agent(Some("install"), &[]), &catalog);
        let cursor = f.cursor_for_turn(None, true);
        let block = cursor.system_block();
        assert!(block.contains("choice point"));
        assert!(!block.contains("action=\"advance\""));
        let err = cursor
            .transition("advance", &serde_json::json!({"action":"advance"}))
            .unwrap_err();
        assert!(err.contains("goto"));
        let chosen = cursor
            .transition(
                "goto",
                &serde_json::json!({"action":"goto","target":"linux"}),
            )
            .unwrap();
        assert_eq!(chosen["step"], "linux");
    }

    #[test]
    fn unresolvable_progress_is_preserved_but_stale_steps_drop() {
        let f = flows(None, &["onboarding"]);
        // Progress whose def isn't resolvable THIS turn (e.g. hub-synced def
        // before the first poll): idle, but the progress is PRESERVED so the
        // flow resumes when the def arrives — never wiped by a restart race.
        let orphan = PlaybookProgress::new("refund", "reason", "n", "");
        let cursor = f.cursor_for_turn(Some(orphan.clone()), false);
        assert!(cursor.system_block().contains("Guided flows available"));
        assert_eq!(cursor.persistable().unwrap().id, "refund");
        // …and it is dropped the moment another flow explicitly starts.
        cursor
            .transition(
                "start",
                &serde_json::json!({"action":"start","id":"onboarding"}),
            )
            .unwrap();
        assert_eq!(cursor.persistable().unwrap().id, "onboarding");
        // Def present but the step was edited away: intentional change — drop.
        let stale = PlaybookProgress::new("onboarding", "deleted-step", "n", "");
        let cursor = f.cursor_for_turn(Some(stale), false);
        assert!(cursor.persistable().is_none());
        // Valid progress resumes in place.
        let mine = PlaybookProgress::new("onboarding", "setup", "n", "");
        let cursor = f.cursor_for_turn(Some(mine), false);
        assert!(cursor.system_block().contains("step 2/3"));
    }
}
