//! `workforce-edge curate` — the training-data curator (provider-agnostic).
//!
//! Product code changes; the docs that teach the assistant about that product go
//! stale. This keeps them current: each `agents/<id>/training/*.md` declares, in
//! `sources:` front-matter, the product-repo globs it's derived from. When those
//! sources change, an LLM rewrites the doc from the CURRENT source.
//!
//! Grounding (this is the whole point — the PR is a *double-check*, so the curator
//! must be trustworthy, not merely plausible):
//!   1. Behaviour is usually split across repos, so a doc is fed the CURRENT
//!      content of ALL its globs across ALL repos at once, each file labelled
//!      `repo/path`, and asked to reason over them together — never one repo alone.
//!   2. The drafter fixes only what the source CONTRADICTS; it must NOT delete
//!      hand-authored domain knowledge just because the code doesn't mention it
//!      (policies/definitions live outside these repos) — it keeps it and inline-
//!      flags it. Otherwise every code change silently erodes the docs.
//!   3. A strict fact-checker then lists contradictions AND, per behavioural claim,
//!      copies the verbatim source snippet backing it. We `grep` those snippets in
//!      the fed source — a DETERMINISTIC check that kills hallucinated citations an
//!      LLM verifier (sharing the drafter's blind spots) would wave through.
//!   4. Severity: a claim the source CONTRADICTS (adjudicated genuine, not a
//!      polarity misread) → revise → re-check, up to N rounds; still unresolved →
//!      the doc is LEFT UNCHANGED and `flagged` (loud). A claim merely NOT confirmed
//!      by the source (no/paraphrased citation) is UNVERIFIED, not wrong, so it's a
//!      `review_note` for the human — never a reason to quarantine or rewrite.
//!   5. If the source didn't fit the context budget we `flag` rather than curate on
//!      a partial picture (a dropped authoritative file makes "clean" meaningless).
//!
//! It reuses the edge's own `[llm]` provider, so a Bedrock install and an
//! Anthropic install run the exact same code — nothing here is provider-specific.
//! The GitHub Action does the git/gh work (checkout, diff, one PR); this does only
//! the LLM curation and writes a JSON report the workflow surfaces in the PR.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use eyre::{eyre, Result, WrapErr};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use workforce_agent::compaction::extract_text_from_response;
use workforce_agent::llm::provider::{create_provider, LlmProvider, ProviderKind};
use workforce_agent::llm::tiers::TierRegistry;
use workforce_agent::llm::types::LlmMessage;

/// Cap the source context fed per doc. Sized to fit a large-but-reasonable set
/// (a few hundred files) well within a modern model's window while leaving room
/// for the doc, prompt and output; a doc whose globs blow past this is too broad
/// and gets FLAGGED rather than curated on a partial picture (module docs, pt 5).
const MAX_SOURCE_BYTES: usize = 500 * 1024;
const MAX_CURATE_TOKENS: u32 = 8000;
const MAX_CHECK_TOKENS: u32 = 4000;
/// How many revise→re-check rounds before we give up and flag the doc.
const MAX_VERIFY_ROUNDS: usize = 3;
/// A citation shorter than this (after whitespace-normalising) is too generic to
/// prove anything, so it's treated as unsupported rather than grounded.
const MIN_QUOTE_LEN: usize = 16;
/// Sentinel the model returns when the doc is already accurate.
const KEEP_EXISTING: &str = "KEEP_EXISTING";

/// Drafter: rewrite the doc from source, fixing only contradictions and PRESERVING
/// hand-authored domain knowledge the code doesn't cover.
const DRAFT_PROMPT: &str = "\
You maintain ONE training document that teaches a customer-facing AI assistant about a product. \
Below is the CURRENT document body and the CURRENT source code it is derived from. Each source file \
is labelled with its `repo/path`; a single behaviour is often implemented across SEVERAL repos, so \
read them together and reconcile them — never describe one repo's half in isolation.

Rules:
- The source is the only evidence of how the PRODUCT BEHAVES. Correct any statement the source \
  CONTRADICTS (wrong status name, wrong order, wrong rule) — the source wins.
- Do NOT delete or water down a statement merely because the source doesn't mention it. Domain \
  knowledge — policies, definitions, business context — legitimately lives outside this code. Keep \
  such statements verbatim, and append on their own line: \
  `<!-- CURATOR: verify — not evidenced in the provided source -->`.
- Do NOT invent APIs, fields, statuses, screens, or behaviour that isn't evidenced in the source.
- Describe product BEHAVIOUR and domain concepts only — never internal class names, file paths, \
  table names, or implementation detail.
- Preserve the document's structure, headings, and voice. Make the smallest changes that make it \
  correct; don't rewrite wholesale for style.
- If the document is already accurate for the current source, reply with EXACTLY `KEEP_EXISTING` \
  and nothing else.

Output ONLY the updated Markdown body — begin DIRECTLY with the document's own first line. \
Do NOT add any preamble, explanation of your edits, or \"Here is…\" commentary; no front-matter; \
no code fences. Or, if already accurate, output EXACTLY `KEEP_EXISTING`.";

/// Fact-checker: judge the (drafted) doc against source. Returns JSON — the
/// citations are grep-checked in Rust, so they must be VERBATIM copies.
const CHECK_PROMPT: &str = "\
You are a strict fact-checker. Below is a training document and the CURRENT source code it must be \
grounded in (each file labelled `repo/path`; a behaviour may span repos — read them together). \
Judge the document against the source.

Return ONLY a JSON object — no prose, no code fences:
{
  \"contradicted\": [
    {\"claim\": \"<verbatim document statement the source DISPROVES>\",
     \"source_quote\": \"<text COPIED VERBATIM from the source above that disproves it>\"}
  ],
  \"citations\": [
    {\"claim\": \"<a key factual claim the document makes about how the product behaves>\",
     \"quote\": \"<text COPIED VERBATIM from the source above that backs this claim, or \\\"\\\" if the source does not back it>\"}
  ]
}

Rules:
- `contradicted`: report a statement ONLY if the source clearly DISPROVES it AND you can copy the \
  exact source text that does so into `source_quote` (verbatim — no paraphrase). If you cannot copy \
  disproving source text, do NOT report it. A difference of ORDER or WORDING is NOT a contradiction \
  — e.g. the same set of items listed in a different order agrees with the source. Absence from the \
  source is NOT a contradiction; much valid domain knowledge is simply not in the code.
- `citations`: for each important BEHAVIOURAL claim, copy the EXACT source text that supports it \
  (verbatim — do not paraphrase, do not summarise). If nothing in the source supports it, use \"\".
- Ignore style, structure, and pure domain definitions the code would never contain.";

/// Adjudicator: does ONE cited line actually disprove ONE claim? Judged in
/// isolation so a supporting line can't masquerade as a contradiction.
const ADJUDICATE_PROMPT: &str = "\
You decide whether a single piece of source code genuinely CONTRADICTS a documentation statement. \
It contradicts only if the source makes the statement FALSE. If the source AGREES WITH, SUPPORTS, or \
is merely CONSISTENT WITH the statement, that is NOT a contradiction — even if the wording or naming \
looks opposite (e.g. a constant named `NON_EDITABLE_STATES` listing states SUPPORTS \"those states \
cannot be edited\"). Answer with exactly one word: CONTRADICTS or CONSISTENT.";

/// Reviser: fix ONLY the listed grounding problems, keep everything else.
const REVISE_PROMPT: &str = "\
You maintain a training document that has the grounding problems listed below. Fix ONLY these \
problems, using the source code as the only source of truth. Keep everything else unchanged — \
especially statements the source simply doesn't mention (that is legitimate domain knowledge). \
Do not introduce new claims, and do not remove the `<!-- CURATOR: verify … -->` markers.

Output ONLY the corrected Markdown body — begin DIRECTLY with the document's own first line, \
with NO preamble or explanation, no front-matter, and no code fences.";

/// Parsed `curate` invocation.
pub struct CurateArgs {
    /// LLM provider (`bedrock`, `anthropic`, `gemini`, `openrouter`, …) — this
    /// install's, so the curator uses the same model family as the agent. Creds
    /// come from the environment (AWS for Bedrock; `CURATOR_LLM_API_KEY` else).
    pub provider: String,
    /// Config-repo checkout (holds `agents/<id>/training/*.md`).
    pub config_dir: PathBuf,
    /// `repo-name -> checkout path`, e.g. `HarborRails -> ./sources/HarborRails`.
    pub sources: BTreeMap<String, PathBuf>,
    /// File of repo-qualified changed paths (one per line), from the workflow's git diff.
    pub changed_files: PathBuf,
    /// Model override; falls back to `[llm].default_model`.
    pub model: Option<String>,
    /// Where to write the JSON report.
    pub out: PathBuf,
}

impl CurateArgs {
    /// Parse `curate --config-dir X --sources repo=path … --changed F --out O [--model M]`.
    pub fn from_args(args: &[String]) -> Result<Self> {
        let mut provider = None;
        let mut config_dir = None;
        let mut sources = BTreeMap::new();
        let mut changed_files = None;
        let mut model = None;
        let mut out = None;
        let mut it = args.iter();
        while let Some(flag) = it.next() {
            let mut val = || {
                it.next()
                    .cloned()
                    .ok_or_else(|| eyre!("{flag} needs a value"))
            };
            match flag.as_str() {
                "--provider" => provider = Some(val()?),
                "--config-dir" => config_dir = Some(PathBuf::from(val()?)),
                "--changed" => changed_files = Some(PathBuf::from(val()?)),
                "--out" => out = Some(PathBuf::from(val()?)),
                "--model" => model = Some(val()?),
                "--sources" => {
                    let v = val()?;
                    let (repo, path) = v
                        .split_once('=')
                        .ok_or_else(|| eyre!("--sources wants repo=path, got {v:?}"))?;
                    sources.insert(repo.to_string(), PathBuf::from(path));
                }
                other => return Err(eyre!("unknown curate flag {other:?}")),
            }
        }
        Ok(Self {
            provider: provider.ok_or_else(|| eyre!("--provider is required"))?,
            config_dir: config_dir.ok_or_else(|| eyre!("--config-dir is required"))?,
            sources,
            changed_files: changed_files.ok_or_else(|| eyre!("--changed is required"))?,
            model,
            out: out.ok_or_else(|| eyre!("--out is required"))?,
        })
    }
}

/// One training doc parsed off disk: its `sources:` globs and its body (the
/// front-matter is preserved verbatim so the LLM never touches it).
struct TrainingDoc {
    agent: String,
    name: String,
    path: PathBuf,
    front_matter: String,
    sources: Vec<String>,
    body: String,
}

/// The current content of a doc's globs, gathered across every repo.
struct SourceContext {
    /// Concatenated file contents, each headed by `===== repo/path =====`.
    block: String,
    /// Repo-qualified files actually included (for the PR body's citations).
    used: Vec<String>,
    /// The size cap was hit, so `block` is only PART of the doc's sources.
    truncated: bool,
}

/// What became of one doc this run.
enum Outcome {
    /// Already accurate, or the verified rewrite was byte-identical.
    Unchanged,
    /// Verified clean and written. `review_notes` = claims not evidenced in source
    /// (kept + inline-flagged) for the reviewer to eyeball.
    Updated { review_notes: Vec<String> },
    /// Could NOT be verified clean → left unchanged on disk. `flags` = why.
    Flagged {
        flags: Vec<String>,
        review_notes: Vec<String>,
    },
}

#[derive(Serialize)]
struct DocReport {
    agent: String,
    doc: String,
    /// `updated` | `unchanged` | `flagged`.
    status: &'static str,
    /// Repo-qualified source files fed to the model (for the PR body's citations).
    sources_used: Vec<String>,
    /// Claims kept but not evidenced in source — reviewer should verify these.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    review_notes: Vec<String>,
    /// Why a doc was flagged (unresolved contradictions / unparseable check / truncation).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    flags: Vec<String>,
}

/// Fact-checker JSON.
#[derive(Deserialize, Default)]
struct CheckReply {
    #[serde(default)]
    contradicted: Vec<Contradiction>,
    #[serde(default)]
    citations: Vec<Citation>,
}

/// A statement the source disproves — with the verbatim source that disproves it,
/// so a fabricated contradiction (which would quarantine a correct doc) can be
/// dropped when its `source_quote` isn't actually in the fed source.
#[derive(Deserialize)]
struct Contradiction {
    #[serde(default)]
    claim: String,
    #[serde(default)]
    source_quote: String,
}

#[derive(Deserialize)]
struct Citation {
    #[serde(default)]
    claim: String,
    #[serde(default)]
    quote: String,
}

/// Entry point for `workforce-edge curate`.
pub async fn run(args: CurateArgs) -> Result<()> {
    // Creds from the environment — Bedrock uses ambient AWS; others read a key.
    let api_key = std::env::var("CURATOR_LLM_API_KEY").unwrap_or_default();
    let llm = build_provider(&args.provider, args.model.as_deref(), &api_key)?;

    let changed = read_changed(&args.changed_files)?;
    let docs = load_training_docs(&args.config_dir)?;
    tracing::info!(
        docs = docs.len(),
        changed = changed.len(),
        "curator: scanning"
    );

    let mut report = Vec::new();
    for doc in &docs {
        let matched = doc
            .sources
            .iter()
            .any(|g| changed.iter().any(|f| glob_matches(g, f)));
        if !matched {
            continue;
        }
        // Feed the CURRENT content of everything the doc's globs cover — across
        // ALL repos — not just the changed files: the doc must reflect the whole,
        // cross-repo picture, not one side of a split behaviour.
        let ctx = gather_sources(&doc.sources, &args.sources)?;
        if ctx.block.is_empty() && !ctx.truncated {
            tracing::warn!(agent = %doc.agent, doc = %doc.name, "curator: sources matched but no files readable; skipping");
            continue;
        }
        let outcome = curate_one(llm.as_ref(), args.model.as_deref(), doc, &ctx).await?;
        let (status, review_notes, flags) = match outcome {
            Outcome::Unchanged => ("unchanged", Vec::new(), Vec::new()),
            Outcome::Updated { review_notes } => ("updated", review_notes, Vec::new()),
            Outcome::Flagged {
                flags,
                review_notes,
            } => ("flagged", review_notes, flags),
        };
        match status {
            "updated" => {
                tracing::info!(agent = %doc.agent, doc = %doc.name, "curator: rewrote (verified)")
            }
            "flagged" => {
                tracing::warn!(agent = %doc.agent, doc = %doc.name, flags = ?flags, "curator: could NOT verify against source — left unchanged")
            }
            _ => {}
        }
        report.push(DocReport {
            agent: doc.agent.clone(),
            doc: doc.name.clone(),
            status,
            sources_used: ctx.used,
            review_notes,
            flags,
        });
    }

    let json = serde_json::to_string_pretty(&report)?;
    std::fs::write(&args.out, json).wrap_err("writing curate report")?;
    let updated = report.iter().filter(|r| r.status == "updated").count();
    let flagged = report.iter().filter(|r| r.status == "flagged").count();
    tracing::info!(considered = report.len(), updated, flagged, "curator: done");
    if flagged > 0 {
        // Loud, but non-fatal: the workflow still opens the PR for the verified
        // docs and surfaces the flagged ones prominently (see curator.yml).
        tracing::warn!(
            flagged,
            "curator: {flagged} doc(s) could not be verified against source and were left unchanged — the workflow will surface these for manual attention"
        );
    }
    Ok(())
}

fn build_provider(
    provider: &str,
    model_override: Option<&str>,
    api_key: &str,
) -> Result<Box<dyn LlmProvider>> {
    let kind = ProviderKind::from_str_loose(provider)
        .ok_or_else(|| eyre!("unknown llm provider {provider:?}"))?;
    let tiers = TierRegistry::from_defaults().unwrap_or_else(|_| TierRegistry::new(None));
    let tier_key = default_tier_key(provider);
    // A tier name (e.g. `powerful`) resolves; a concrete model id passes through.
    let model = model_override.map(|m| tiers.resolve_model(m, &tier_key));
    create_provider(kind, api_key, model.as_deref(), None, false)
}

fn default_tier_key(provider: &str) -> String {
    match provider {
        "bedrock" => "bedrock.anthropic".to_string(),
        other => other.to_string(),
    }
}

fn read_changed(path: &Path) -> Result<Vec<String>> {
    Ok(std::fs::read_to_string(path)
        .wrap_err_with(|| format!("reading changed-files list {}", path.display()))?
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect())
}

/// Walk `<config_dir>/agents/<id>/training/*.md`.
fn load_training_docs(config_dir: &Path) -> Result<Vec<TrainingDoc>> {
    let mut out = Vec::new();
    let agents_dir = config_dir.join("agents");
    let Ok(agents) = std::fs::read_dir(&agents_dir) else {
        return Ok(out);
    };
    for agent_entry in agents.flatten() {
        if !agent_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let agent = agent_entry.file_name().to_string_lossy().to_string();
        let training = agent_entry.path().join("training");
        let Ok(files) = std::fs::read_dir(&training) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let raw = std::fs::read_to_string(&path)
                .wrap_err_with(|| format!("reading {}", path.display()))?;
            let (front_matter, sources, body) = split_front_matter(&raw);
            out.push(TrainingDoc {
                agent: agent.clone(),
                name: path.file_name().unwrap().to_string_lossy().to_string(),
                path,
                front_matter,
                sources,
                body,
            });
        }
    }
    out.sort_by(|a, b| (&a.agent, &a.name).cmp(&(&b.agent, &b.name)));
    Ok(out)
}

/// Split `(front_matter_raw, sources, body)`. The front matter (between the two
/// `---` fences) is returned verbatim WITH its fences so it can be re-prepended
/// untouched; `sources` is parsed from its YAML.
fn split_front_matter(raw: &str) -> (String, Vec<String>, String) {
    let trimmed = raw.trim_start_matches('\u{feff}');
    let after = trimmed
        .strip_prefix("---\n")
        .or_else(|| trimmed.strip_prefix("---\r\n"));
    let Some(rest) = after else {
        return (String::new(), Vec::new(), raw.to_string());
    };
    // Find the closing fence at a line start.
    let end = ["\n---\n", "\n---\r\n", "\r\n---\r\n"]
        .iter()
        .find_map(|pat| rest.find(pat).map(|i| (i, pat.len())));
    let Some((idx, skip)) = end else {
        return (String::new(), Vec::new(), raw.to_string());
    };
    let yaml = &rest[..idx];
    let body = rest[idx + skip..].to_string();
    let front_matter = format!("---\n{}\n---\n\n", yaml.trim_end());
    #[derive(Deserialize)]
    struct Fm {
        #[serde(default)]
        sources: Vec<String>,
    }
    let sources = serde_yaml::from_str::<Fm>(yaml)
        .map(|f| f.sources)
        .unwrap_or_default();
    (front_matter, sources, body)
}

/// Does repo-qualified `path` match repo-qualified glob `pat`? Uses shell-glob
/// semantics with `**` spanning directories.
fn glob_matches(pat: &str, path: &str) -> bool {
    glob::Pattern::new(pat)
        .map(|p| {
            p.matches_with(
                path,
                glob::MatchOptions {
                    require_literal_separator: false,
                    ..Default::default()
                },
            )
        })
        .unwrap_or(false)
}

/// Read the current content of every file matching the doc's globs, from the
/// checked-out source repos. Files from ALL repos land in one context block.
fn gather_sources(
    globs: &[String],
    checkouts: &BTreeMap<String, PathBuf>,
) -> Result<SourceContext> {
    let mut block = String::new();
    let mut used = Vec::new();
    let mut total = 0usize;
    let mut truncated = false;
    for glob_pat in globs {
        // Repo-qualified: `<repo>/<glob-within-repo>`.
        let Some((repo, within)) = glob_pat.split_once('/') else {
            continue;
        };
        let Some(root) = checkouts.get(repo) else {
            continue; // this doc references a repo this run didn't check out
        };
        // glob(3) gotcha: a TRAILING `**` matches only directories, not the files
        // inside them — `dir/**` enumerates zero files. A doc glob is naturally
        // written `dir/**` ("everything under here"), so rewrite it to `dir/**/*`
        // for the filesystem walk. Change-detection (glob_matches, below) matches
        // the raw glob against changed paths and is unaffected.
        let within = match within.strip_suffix("/**") {
            Some(base) => format!("{base}/**/*"),
            None if within == "**" => "**/*".to_string(),
            None => within.to_string(),
        };
        let full = root.join(&within);
        let Some(full_str) = full.to_str() else {
            continue;
        };
        for entry in glob::glob(full_str).into_iter().flatten().flatten() {
            if !entry.is_file() {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(&entry) else {
                continue; // binary / unreadable
            };
            let rel = entry
                .strip_prefix(root)
                .map(|p| format!("{repo}/{}", p.display()))
                .unwrap_or_else(|_| entry.display().to_string());
            if used.contains(&rel) {
                continue;
            }
            if total + content.len() > MAX_SOURCE_BYTES {
                truncated = true;
                break;
            }
            total += content.len();
            used.push(rel.clone());
            block.push_str(&format!("\n===== {rel} =====\n{content}\n"));
        }
        if truncated {
            break;
        }
    }
    used.sort();
    Ok(SourceContext {
        block,
        used,
        truncated,
    })
}

/// Curate one doc through draft → check → revise, writing ONLY if verified clean.
async fn curate_one(
    llm: &dyn LlmProvider,
    model: Option<&str>,
    doc: &TrainingDoc,
    ctx: &SourceContext,
) -> Result<Outcome> {
    // Point 5: never curate on a partial source picture.
    if ctx.truncated {
        return Ok(Outcome::Flagged {
            flags: vec![format!(
                "source exceeded the {}KB context budget — curation skipped so the doc isn't \
                 rewritten against a partial picture; narrow or split this doc's `sources:` globs",
                MAX_SOURCE_BYTES / 1024
            )],
            review_notes: Vec::new(),
        });
    }

    // DRAFT — fix contradictions, preserve+flag hand-authored knowledge.
    let Some(mut body) = draft_doc(llm, model, doc, &ctx.block).await? else {
        return Ok(Outcome::Unchanged); // KEEP_EXISTING
    };

    // Precompute once for the deterministic verbatim-citation grep.
    let source_norm = normalize_ws(&ctx.block);
    let mut review_notes = Vec::new();
    let mut last_problems: Vec<String> = Vec::new();

    for round in 0..MAX_VERIFY_ROUNDS {
        let mut problems: Vec<String> = Vec::new();
        review_notes.clear();

        match check_doc(llm, model, &body, &ctx.block).await? {
            // Fail-closed: an unparseable verifier reply means we CAN'T verify —
            // treat it as a hard problem rather than waving the doc through.
            None => problems.push(
                "the fact-checker's reply could not be parsed as JSON — the doc cannot be verified"
                    .to_string(),
            ),
            Some(check) => {
                // A contradiction counts only if (a) the checker grounds it in
                // verbatim source (drops fabrications), AND (b) that quote really
                // does disprove the claim. (b) is adjudicated in ISOLATION — just the
                // claim and its one quote, no source haystack — which catches the
                // checker citing a SUPPORTING line as if it refutes (a polarity
                // misread grep can't see, e.g. `NON_EDITABLE_STATES = [...]` cited
                // against "those states cannot be edited"). Both guards protect a
                // CORRECT doc from being quarantined by a false contradiction.
                for c in &check.contradicted {
                    let claim = c.claim.trim();
                    if claim.is_empty() || !contains_norm(&source_norm, &c.source_quote) {
                        continue;
                    }
                    if is_genuine_contradiction(llm, model, claim, &c.source_quote).await? {
                        problems.push(format!(
                            "statement contradicts the source — correct it from source: \"{claim}\" \
                             (source says: {})",
                            snippet(&c.source_quote)
                        ));
                    }
                }

                // Deterministic grounding: a claim is CONFIRMED only if the checker's
                // supporting quote actually appears (verbatim, modulo whitespace) in
                // the fed source. A claim with no quote, or a quote we can't find, is
                // simply UNCONFIRMED — it may well be true (domain knowledge, or the
                // checker paraphrased) — so it's surfaced for review, NOT quarantined.
                // Quarantine is reserved for what's provably wrong (an adjudicated
                // contradiction) or unverifiable (an unparseable checker reply).
                for c in &check.citations {
                    let claim = c.claim.trim();
                    if claim.is_empty() {
                        continue;
                    }
                    if c.quote.trim().is_empty() || !contains_norm(&source_norm, &c.quote) {
                        review_notes.push(format!("not confirmed in source — verify: {claim}"));
                    }
                    // else: grounded verbatim → confirmed, nothing to surface.
                }
            }
        }

        if problems.is_empty() {
            let changed = write_if_changed(doc, &body)?;
            return Ok(if changed {
                Outcome::Updated { review_notes }
            } else {
                Outcome::Unchanged
            });
        }

        last_problems = problems.clone();
        if round + 1 == MAX_VERIFY_ROUNDS {
            break; // out of rounds — flag below
        }
        body = revise_doc(llm, model, &body, &ctx.block, &problems).await?;
    }

    // Couldn't reach a clean, grounded state → leave the doc as-is and flag loudly.
    Ok(Outcome::Flagged {
        flags: last_problems,
        review_notes,
    })
}

/// One LLM turn with reasoning suppressed; returns the plain text.
async fn call_llm(
    llm: &dyn LlmProvider,
    model: Option<&str>,
    system: &str,
    user: String,
    max_tokens: u32,
    who: &str,
) -> Result<String> {
    let response = workforce_agent::llm::provider::with_reasoning_suppressed(llm.create_message(
        Some(system),
        &[LlmMessage::user(user)],
        &[],
        model,
        Some(max_tokens),
        true,
    ))
    .await
    .map_err(|e| eyre!("curator {who} LLM call failed: {e}"))?;
    Ok(extract_text_from_response(&response.content).to_string())
}

/// Draft a rewrite. `Ok(None)` == `KEEP_EXISTING` (already accurate).
async fn draft_doc(
    llm: &dyn LlmProvider,
    model: Option<&str>,
    doc: &TrainingDoc,
    source_block: &str,
) -> Result<Option<String>> {
    let user = format!(
        "## Current document body\n\n{}\n\n## Current source code\n{}",
        doc.body.trim(),
        source_block
    );
    let reply = call_llm(llm, model, DRAFT_PROMPT, user, MAX_CURATE_TOKENS, "draft").await?;
    let reply = strip_fence(reply.trim());
    if reply.is_empty() || reply == KEEP_EXISTING || reply.starts_with(KEEP_EXISTING) {
        return Ok(None);
    }
    Ok(Some(strip_preamble(&doc.body, reply).trim().to_string()))
}

/// Fact-check the (drafted) body against source. `Ok(None)` == the reply couldn't
/// be parsed as JSON — the caller treats that fail-closed (can't verify → problem).
async fn check_doc(
    llm: &dyn LlmProvider,
    model: Option<&str>,
    body: &str,
    source_block: &str,
) -> Result<Option<CheckReply>> {
    let user = format!(
        "## Document\n\n{}\n\n## Source code\n{}",
        body.trim(),
        source_block
    );
    let reply = call_llm(llm, model, CHECK_PROMPT, user, MAX_CHECK_TOKENS, "check").await?;
    Ok(extract_json::<CheckReply>(&reply))
}

/// Adjudicate a single (claim, disproving-quote) pair in isolation: is it a
/// genuine contradiction? A short, cheap call — runs only for contradictions the
/// checker already grounded in real source, so it's rare.
async fn is_genuine_contradiction(
    llm: &dyn LlmProvider,
    model: Option<&str>,
    claim: &str,
    source_quote: &str,
) -> Result<bool> {
    let user = format!(
        "Statement:\n{claim}\n\nCited source code:\n{source_quote}\n\nDoes the cited source make the statement FALSE?"
    );
    let reply = call_llm(llm, model, ADJUDICATE_PROMPT, user, 16, "adjudicate").await?;
    Ok(reply.trim().to_uppercase().starts_with("CONTRADICT"))
}

/// Trim a source snippet for a human-readable problem message.
fn snippet(s: &str) -> String {
    let one_line = normalize_ws(s);
    if one_line.chars().count() > 160 {
        format!("{}…", one_line.chars().take(160).collect::<String>())
    } else {
        one_line
    }
}

/// Revise the body to fix the listed grounding problems only.
async fn revise_doc(
    llm: &dyn LlmProvider,
    model: Option<&str>,
    body: &str,
    source_block: &str,
    problems: &[String],
) -> Result<String> {
    let list = problems
        .iter()
        .map(|p| format!("- {p}"))
        .collect::<Vec<_>>()
        .join("\n");
    let user = format!(
        "## Document\n\n{}\n\n## Grounding problems to fix\n{}\n\n## Source code\n{}",
        body.trim(),
        list,
        source_block
    );
    let reply = call_llm(llm, model, REVISE_PROMPT, user, MAX_CURATE_TOKENS, "revise").await?;
    let reply = strip_fence(reply.trim());
    Ok(strip_preamble(body, reply).trim().to_string())
}

/// Write the doc if its rendered form differs from disk; report whether it did.
fn write_if_changed(doc: &TrainingDoc, body: &str) -> Result<bool> {
    let new_doc = format!("{}{}\n", doc.front_matter, body.trim());
    let current = format!("{}{}\n", doc.front_matter, doc.body.trim());
    if new_doc == current {
        return Ok(false);
    }
    std::fs::write(&doc.path, &new_doc)
        .wrap_err_with(|| format!("writing {}", doc.path.display()))?;
    Ok(true)
}

/// Parse the first JSON value in `s` (tolerating fences and trailing prose).
fn extract_json<T: DeserializeOwned>(s: &str) -> Option<T> {
    let s = strip_fence(s.trim());
    let start = s.find('{')?;
    serde_json::Deserializer::from_str(&s[start..])
        .into_iter::<T>()
        .next()?
        .ok()
}

/// Collapse all whitespace runs to single spaces and trim — so a citation matches
/// the source despite reindentation/reflow, while still requiring the real text.
fn normalize_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Is `quote` present (verbatim, modulo whitespace) in the already-normalised
/// `source_norm`? Too-short quotes prove nothing, so they don't count as grounded.
fn contains_norm(source_norm: &str, quote: &str) -> bool {
    let q = normalize_ws(quote);
    q.len() >= MIN_QUOTE_LEN && source_norm.contains(&q)
}

/// Models sometimes prepend "The only change is… / Here is the updated doc:"
/// commentary ABOVE the document. If the original doc began with a Markdown
/// heading (these training docs do), drop any lines before the first heading in
/// the reply. Never touch a reply with no heading — don't risk deleting real
/// content — so this only ever removes leading non-heading preamble.
fn strip_preamble<'a>(original_body: &str, reply: &'a str) -> &'a str {
    let orig_starts_heading = original_body
        .lines()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.trim_start().starts_with('#'))
        .unwrap_or(false);
    if !orig_starts_heading {
        return reply;
    }
    // Byte offset of the first line that starts (modulo indent) with `#`.
    let mut offset = 0usize;
    for line in reply.split_inclusive('\n') {
        if line.trim_start().starts_with('#') {
            return &reply[offset..];
        }
        offset += line.len();
    }
    reply // no heading found — leave it exactly as-is
}

/// Drop a ```` ```markdown ```` fence the model may wrap its answer in.
fn strip_fence(s: &str) -> &str {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix("```") {
        let rest = rest.split_once('\n').map(|(_, b)| b).unwrap_or(rest);
        return rest.strip_suffix("```").unwrap_or(rest).trim();
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_front_matter_and_sources() {
        let raw = "---\nsources:\n  - \"core-api/a/**\"\n  - \"PoR/b/**\"\n---\n\n# Title\n\nBody.";
        let (fm, sources, body) = split_front_matter(raw);
        assert!(fm.starts_with("---\n") && fm.contains("core-api/a/**"));
        assert_eq!(sources, vec!["core-api/a/**", "PoR/b/**"]);
        assert_eq!(body.trim(), "# Title\n\nBody.");
    }

    #[test]
    fn no_front_matter_is_all_body() {
        let (fm, sources, body) = split_front_matter("# Just a doc");
        assert!(fm.is_empty() && sources.is_empty());
        assert_eq!(body, "# Just a doc");
    }

    #[test]
    fn glob_spans_directories() {
        assert!(glob_matches(
            "PoR/app/**/shift*/**",
            "PoR/app/models/shift_thing/x.rb"
        ));
        assert!(glob_matches(
            "core-api/*/src/**/*Shift*",
            "core-api/service-plans/src/main/Shift.kt"
        ));
        assert!(!glob_matches(
            "PoR/app/**/shift*/**",
            "PoR/app/models/worker/x.rb"
        ));
    }

    #[test]
    fn strips_code_fence() {
        assert_eq!(strip_fence("```markdown\n# Hi\n```"), "# Hi");
        assert_eq!(strip_fence("# Hi"), "# Hi");
    }

    #[test]
    fn strips_leading_commentary_above_the_heading() {
        let orig = "# Title\n\nBody.";
        // Model prepended an explanation of its edits — drop it.
        let reply = "The only change is the label.\n\nAlso a note.\n\n# Title\n\nNew body.";
        assert_eq!(strip_preamble(orig, reply), "# Title\n\nNew body.");
        // No preamble → untouched.
        assert_eq!(strip_preamble(orig, "# Title\n\nBody."), "# Title\n\nBody.");
        // Reply has no heading → never delete content.
        assert_eq!(strip_preamble(orig, "just prose"), "just prose");
        // Original didn't start with a heading → don't strip anything.
        assert_eq!(strip_preamble("intro\n# H", "chatter\n# H"), "chatter\n# H");
    }

    #[test]
    fn trailing_doublestar_is_rewritten_for_file_walk() {
        // glob(3): `dir/**` matches only directories on disk, so gather_sources
        // rewrites it to `dir/**/*`. Mirror that transform here.
        let rewrite = |w: &str| match w.strip_suffix("/**") {
            Some(base) => format!("{base}/**/*"),
            None if w == "**" => "**/*".to_string(),
            None => w.to_string(),
        };
        assert_eq!(
            rewrite("app/operations/shift/**"),
            "app/operations/shift/**/*"
        );
        assert_eq!(rewrite("**"), "**/*");
        assert_eq!(rewrite("app/models/shift.rb"), "app/models/shift.rb");
        assert_eq!(
            rewrite("app/models/*rate_card*.rb"),
            "app/models/*rate_card*.rb"
        );
        // Middle ** is left for glob to handle; only the trailing one is rewritten.
        assert_eq!(
            rewrite("app/**/integrations/harbor/order_sync/**"),
            "app/**/integrations/harbor/order_sync/**/*"
        );
    }

    #[test]
    fn extract_json_reads_first_object_amid_prose() {
        let s = "Sure! Here it is:\n```json\n{\"contradicted\":[{\"claim\":\"x\",\"source_quote\":\"y\"}],\"citations\":[]}\n```\nHope that helps.";
        let c: CheckReply = extract_json(s).expect("parse");
        assert_eq!(c.contradicted.len(), 1);
        assert_eq!(c.contradicted[0].claim, "x");
        assert_eq!(c.contradicted[0].source_quote, "y");
        assert!(c.citations.is_empty());
    }

    #[test]
    fn extract_json_none_when_absent() {
        assert!(extract_json::<CheckReply>("no json here").is_none());
    }

    #[test]
    fn citation_grep_ignores_whitespace_but_needs_real_text() {
        let source = normalize_ws("def sign_off!\n    raise if     paid?\n  end");
        // Verbatim modulo whitespace (>= MIN_QUOTE_LEN) → grounded.
        assert!(contains_norm(&source, "sign_off!\n    raise if paid?"));
        // Not in the source → not grounded (would be flagged as fabrication).
        assert!(!contains_norm(&source, "raise unless signed_off_at?"));
        // Real text but too short to prove anything → not grounded.
        assert!(!contains_norm(&source, "raise if"));
    }

    #[test]
    fn parses_args() {
        let a = CurateArgs::from_args(&[
            "--provider".into(),
            "bedrock".into(),
            "--config-dir".into(),
            "cfg".into(),
            "--sources".into(),
            "core-api=/s/core-api".into(),
            "--sources".into(),
            "PoR=/s/por".into(),
            "--changed".into(),
            "ch.txt".into(),
            "--out".into(),
            "rep.json".into(),
        ])
        .unwrap();
        assert_eq!(a.provider, "bedrock");
        assert_eq!(a.config_dir, PathBuf::from("cfg"));
        assert_eq!(a.sources.len(), 2);
        assert_eq!(a.sources["core-api"], PathBuf::from("/s/core-api"));
    }
}
