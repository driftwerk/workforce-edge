//! Git-backed config loader (Phase 1 of the git-config epic).
//!
//! When `[git_config]` is set, agent definitions, playbooks, and training data
//! are the git repo's — loaded here on every boot and made authoritative, with
//! the admin UI read-only for them (appearance stays editable via a DB overlay).
//!
//! We fetch the repo as a **zipball via the GitHub API** and extract it in
//! memory: the edge's base image has `curl`/TLS but no `git` binary, and a
//! read-only config snapshot needs no history. The token therefore needs only
//! `contents: read` on the one repo.
//!
//! The loader is two separable halves. [`GitHub::fetch_files`] is the network
//! half: it fetches and unzips a repo into repo-relative files. [`parse_tree`]
//! and [`parse_tree_with`] are the pure half: they turn those files into agents,
//! playbooks and training. Git mode (`runtime.rs`) chains the two and mirrors
//! the result as authoritative; the one-shot admin import (`server.rs`) chains
//! them once, with placeholders expanded from the request rather than the
//! process, and writes the result into an install that stays editable.
//!
//! Layout (see `docs/edge-git-config-and-curator-design.md`):
//! ```text
//! playbooks/<id>.toml                 # SHARED pool, referenced by id
//! agents/<agent-id>/agent.toml        # per-agent definition (env placeholders ok)
//! agents/<agent-id>/training/<name>.md
//! ```

use std::collections::BTreeMap;
use std::io::{Cursor, Read};
use std::time::Duration;

use eyre::{eyre, Result, WrapErr};

use workforce_common::config_expand::expand_env_vars;

use crate::agent::EdgeAgent;
use crate::config::GitConfig;
use crate::outbound::{self, OutboundPolicy};

/// Refuse to buffer a pathologically large archive into memory — a config repo
/// is tiny; anything this big is a misconfiguration, not config.
const MAX_ZIP_BYTES: usize = 64 * 1024 * 1024;
/// Ceilings on what the archive unpacks to: one entry, and all entries
/// together. A zip's headers are the repo's to write, so neither the
/// declared sizes nor the archive's own size says what an entry inflates to;
/// the bound is enforced on the bytes read.
const MAX_ZIP_ENTRY_BYTES: usize = 16 * 1024 * 1024;
const MAX_UNZIPPED_BYTES: usize = 64 * 1024 * 1024;
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// Where config repos are fetched from, and how. The API base is GitHub's
/// fixed public host in production; the install's infra carries it so a test
/// can point the loader at a mock. The policy is the tenant's outbound policy:
/// the host is not tenant-supplied, but every call a tenant causes follows its
/// policy, redirects (the zipball's hop to `codeload`) included.
#[derive(Clone, Debug)]
pub struct GitHub {
    api_base: String,
    policy: OutboundPolicy,
}

impl GitHub {
    /// The GitHub REST API.
    pub const API: &'static str = "https://api.github.com";

    /// The loader for `policy` against `api_base` (see [`Self::API`]).
    pub fn at(api_base: &str, policy: OutboundPolicy) -> Self {
        Self {
            api_base: api_base.trim_end_matches('/').to_string(),
            policy,
        }
    }

    /// Fetch + parse the repo at `cfg.git_ref`, placeholders expanded from the
    /// process environment — the git-mode loader. The pure parse is
    /// [`parse_tree`], tested without a network.
    pub async fn fetch(&self, cfg: &GitConfig) -> Result<EdgeConfigTree> {
        parse_tree(self.fetch_files(cfg).await?)
    }

    /// Fetch the repo at `cfg.git_ref` as repo-relative `path -> bytes`: the
    /// network and unzip half of the loader, with no opinion on what the files
    /// mean.
    pub async fn fetch_files(&self, cfg: &GitConfig) -> Result<BTreeMap<String, Vec<u8>>> {
        let bytes = self.fetch_zipball(cfg).await?;
        // Unzip is blocking CPU work; keep it off the async worker.
        tokio::task::spawn_blocking(move || unzip(&bytes))
            .await
            .wrap_err("unzip task panicked")?
    }

    /// The HEAD commit SHA of `cfg.git_ref` — a cheap check the sync poll uses
    /// to skip re-applying when nothing changed. `Accept: …sha` makes GitHub
    /// return the bare 40-char SHA as the body.
    pub async fn latest_commit_sha(&self, cfg: &GitConfig) -> Result<String> {
        let url = format!(
            "{}/repos/{}/commits/{}",
            self.api_base,
            cfg.repo.trim_matches('/'),
            cfg.git_ref
        );
        let resp = outbound::client(self.policy, FETCH_TIMEOUT)
            .get(&url)
            .bearer_auth(&cfg.token)
            .header(reqwest::header::USER_AGENT, "workforce-edge")
            .header(reqwest::header::ACCEPT, "application/vnd.github.sha")
            .send()
            .await
            .map_err(|e| eyre!("git sha check failed: {e}"))?;
        if !resp.status().is_success() {
            return Err(eyre!(
                "git sha check for {}@{} → {}",
                cfg.repo,
                cfg.git_ref,
                resp.status()
            ));
        }
        Ok(resp
            .text()
            .await
            .map_err(|e| eyre!("reading sha: {e}"))?
            .trim()
            .to_string())
    }

    async fn fetch_zipball(&self, cfg: &GitConfig) -> Result<Vec<u8>> {
        // GitHub 302s zipball → a pre-signed codeload URL on another host; reqwest
        // follows it and drops the Authorization header cross-host (correct — the
        // redirect URL is already signed).
        let url = format!(
            "{}/repos/{}/zipball/{}",
            self.api_base,
            cfg.repo.trim_matches('/'),
            cfg.git_ref
        );
        let resp = outbound::client(self.policy, FETCH_TIMEOUT)
            .get(&url)
            .bearer_auth(&cfg.token)
            .header(reqwest::header::USER_AGENT, "workforce-edge")
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .send()
            .await
            .map_err(|e| eyre!("git config fetch failed: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            let body: String = resp
                .text()
                .await
                .unwrap_or_default()
                .chars()
                .take(200)
                .collect();
            return Err(eyre!(
                "git config fetch for {}@{} rejected ({status}): {body}",
                cfg.repo,
                cfg.git_ref
            ));
        }
        if let Some(len) = resp.content_length() {
            if len as usize > MAX_ZIP_BYTES {
                return Err(eyre!("git config archive too large ({len} bytes)"));
            }
        }
        let bytes = outbound::read_capped(self.policy, resp, MAX_ZIP_BYTES)
            .await
            .map_err(|e| eyre!("reading git config archive: {e}"))?;
        if bytes.len() > MAX_ZIP_BYTES {
            return Err(eyre!(
                "git config archive too large ({} bytes)",
                bytes.len()
            ));
        }
        Ok(bytes)
    }
}

/// One training document for an agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrainingFile {
    pub agent_id: String,
    /// File name within `training/`, e.g. `returns.md`.
    pub name: String,
    pub content: String,
}

/// Everything the edge loads from the config repo for one boot.
#[derive(Debug, Default)]
pub struct EdgeConfigTree {
    /// Parsed, `${ENV}`-expanded agent definitions.
    pub agents: Vec<EdgeAgent>,
    /// `(id, toml)` playbook sources, ready for `playbook::parse_sources`.
    pub playbooks: Vec<(String, String)>,
    /// Training markdown, grouped later by `agent_id`.
    pub training: Vec<TrainingFile>,
}

/// Whether a repo-relative path is one the tree reads at all
/// ([`parse_tree_with`] takes `agents/…` and `playbooks/…`); anything else
/// is never extracted, so a large README or asset costs nothing.
fn is_config_path(rel: &str) -> bool {
    rel.starts_with("agents/") || rel.starts_with("playbooks/")
}

/// Extract a zipball into `path -> bytes`, stripping GitHub's top-level
/// `<owner>-<repo>-<sha>/` wrapper directory so paths are repo-relative.
/// Only config paths are read, each bounded by [`MAX_ZIP_ENTRY_BYTES`] and
/// all together by [`MAX_UNZIPPED_BYTES`]; an archive past either is refused
/// whole, so nothing of it is applied.
fn unzip(bytes: &[u8]) -> Result<BTreeMap<String, Vec<u8>>> {
    let mut archive =
        zip::ZipArchive::new(Cursor::new(bytes)).wrap_err("opening git config archive")?;
    let mut out = BTreeMap::new();
    let mut total = 0usize;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).wrap_err("reading archive entry")?;
        if entry.is_dir() {
            continue;
        }
        // Use the safe sanitised name; skip anything with no path.
        let Some(name) = entry.enclosed_name() else {
            continue;
        };
        let name = name.to_string_lossy().replace('\\', "/");
        // Strip the single wrapper component GitHub adds.
        let Some((_, rel)) = name.split_once('/') else {
            continue;
        };
        if rel.is_empty() || !is_config_path(rel) {
            continue;
        }
        if entry.size() as usize > MAX_ZIP_ENTRY_BYTES {
            return Err(eyre!(
                "git config archive entry {rel} declares {} bytes, over the {MAX_ZIP_ENTRY_BYTES}-byte limit",
                entry.size()
            ));
        }
        // Clamp the pre-allocation to the declared (attacker/repo-controlled) size
        // so a lying zip header can't force a huge allocation; the read itself is
        // what bounds the real content.
        let hint = (entry.size() as usize).min(1 << 20);
        let mut buf = Vec::with_capacity(hint);
        (&mut entry)
            .take(MAX_ZIP_ENTRY_BYTES as u64 + 1)
            .read_to_end(&mut buf)
            .wrap_err("extracting archive entry")?;
        if buf.len() > MAX_ZIP_ENTRY_BYTES {
            return Err(eyre!(
                "git config archive entry {rel} inflates past the {MAX_ZIP_ENTRY_BYTES}-byte limit"
            ));
        }
        total += buf.len();
        if total > MAX_UNZIPPED_BYTES {
            return Err(eyre!(
                "git config archive inflates past the {MAX_UNZIPPED_BYTES}-byte limit"
            ));
        }
        out.insert(rel.to_string(), buf);
    }
    Ok(out)
}

/// Where `${NAME}` / `${NAME:-default}` placeholders in an `agent.toml` are
/// resolved from.
#[derive(Clone, Copy, Debug)]
pub enum Expansion<'a> {
    /// The process environment, as the main config file: an unset name with
    /// no default expands to the empty string. Git mode's binding — the
    /// install's own environment is the install's to read.
    ProcessEnv,
    /// Only the supplied map, never the process: the one-shot import runs
    /// on an operator's request, and a `${EDGE_MASTER_KEY}` in a repo must
    /// not read this process's secrets into an agent. A name absent from the
    /// map with no default expands to the empty string and is reported in
    /// [`ParsedTree::unresolved`], so the caller can refuse the whole import.
    Map(&'a BTreeMap<String, String>),
}

/// The outcome of [`parse_tree_with`]: the tree, and every placeholder name
/// the expansion could not resolve (sorted, de-duplicated; always empty under
/// [`Expansion::ProcessEnv`]).
#[derive(Debug, Default)]
pub struct ParsedTree {
    pub tree: EdgeConfigTree,
    pub unresolved: Vec<String>,
}

/// Pure parse of repo-relative `path -> bytes` into an [`EdgeConfigTree`],
/// placeholders expanded from the process environment. Unknown files
/// (READMEs, manifests, …) are ignored, not errors.
pub fn parse_tree(files: BTreeMap<String, Vec<u8>>) -> Result<EdgeConfigTree> {
    parse_tree_with(files, Expansion::ProcessEnv).map(|parsed| parsed.tree)
}

/// [`parse_tree`] with the placeholder source chosen by the caller.
pub fn parse_tree_with(
    files: BTreeMap<String, Vec<u8>>,
    expansion: Expansion<'_>,
) -> Result<ParsedTree> {
    let mut agent_sources: Vec<(String, String)> = Vec::new();
    let mut tree = EdgeConfigTree::default();
    let mut unresolved = Vec::new();
    for (path, bytes) in files {
        let parts: Vec<&str> = path.split('/').collect();
        match parts.as_slice() {
            ["agents", id, "agent.toml"] => {
                agent_sources.push(((*id).to_string(), decode_utf8(&path, bytes)?));
            }
            ["agents", id, "training", name] if name.ends_with(".md") => {
                tree.training.push(TrainingFile {
                    agent_id: (*id).to_string(),
                    name: (*name).to_string(),
                    content: decode_utf8(&path, bytes)?,
                });
            }
            ["playbooks", name] if name.ends_with(".toml") => {
                let id = name.trim_end_matches(".toml").to_string();
                tree.playbooks.push((id, decode_utf8(&path, bytes)?));
            }
            _ => {}
        }
    }
    // Parse each agent.toml with the same `${ENV}` expansion as the main config,
    // so env bindings (`keycloak_issuer = "${EDGE_KC_ISSUER}"`) resolve per env.
    for (dir_id, raw) in agent_sources {
        let expanded = match expansion {
            Expansion::ProcessEnv => expand_env_vars(&raw),
            Expansion::Map(env) => {
                let (expanded, missing) = expand_from_map(&raw, env);
                unresolved.extend(missing);
                expanded
            }
        };
        let agent: EdgeAgent = toml::from_str(&expanded)
            .wrap_err_with(|| format!("parsing agents/{dir_id}/agent.toml"))?;
        if agent.id.is_empty() {
            return Err(eyre!("agents/{dir_id}/agent.toml has no `id`"));
        }
        if agent.id != dir_id {
            tracing::warn!(
                dir = %dir_id,
                id = %agent.id,
                "agent.toml `id` differs from its folder name; using the id field"
            );
        }
        tree.agents.push(agent);
    }
    // Determinism: stable order regardless of zip iteration.
    tree.agents.sort_by(|a, b| a.id.cmp(&b.id));
    tree.playbooks.sort_by(|a, b| a.0.cmp(&b.0));
    tree.training
        .sort_by(|a, b| (&a.agent_id, &a.name).cmp(&(&b.agent_id, &b.name)));
    unresolved.sort();
    unresolved.dedup();
    Ok(ParsedTree { tree, unresolved })
}

fn decode_utf8(path: &str, bytes: Vec<u8>) -> Result<String> {
    String::from_utf8(bytes).wrap_err_with(|| format!("{path} is not valid UTF-8"))
}

/// Expand `${NAME}` / `${NAME:-default}` from `env` alone — the same grammar
/// as `workforce_common::config_expand::expand_env_vars`, which reads the
/// process. Returns the expanded text and the names that were neither in
/// `env` nor given a default (each expands to the empty string, as the
/// process variant does for an unset variable). Anything that is not a
/// well-formed placeholder is left as written.
fn expand_from_map(input: &str, env: &BTreeMap<String, String>) -> (String, Vec<String>) {
    let mut out = String::with_capacity(input.len());
    let mut unresolved = Vec::new();
    let mut rest = input;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match placeholder(after) {
            Some((name, default, len)) => {
                match env.get(name) {
                    Some(value) => out.push_str(value),
                    None => match default {
                        Some(default) => out.push_str(default),
                        None => unresolved.push(name.to_string()),
                    },
                }
                rest = &after[len..];
            }
            None => {
                out.push_str("${");
                rest = after;
            }
        }
    }
    out.push_str(rest);
    (out, unresolved)
}

/// Parse the body of a placeholder from just after its `${`: the name
/// (`[A-Za-z_][A-Za-z0-9_]*`), an optional `:-default`, and how many bytes
/// the body plus its closing `}` spans. `None` when the text is not a
/// placeholder.
fn placeholder(body: &str) -> Option<(&str, Option<&str>, usize)> {
    let bytes = body.as_bytes();
    if !bytes
        .first()
        .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_')
    {
        return None;
    }
    let name_len = bytes
        .iter()
        .take_while(|b| b.is_ascii_alphanumeric() || **b == b'_')
        .count();
    let name = &body[..name_len];
    let tail = &body[name_len..];
    if tail.starts_with('}') {
        return Some((name, None, name_len + 1));
    }
    let default_body = tail.strip_prefix(":-")?;
    let close = default_body.find('}')?;
    Some((name, Some(&default_body[..close]), name_len + 2 + close + 1))
}

/// A training file rendered as a knowledge doc: `(id, title, body)`. The id is
/// the file stem; YAML front-matter (the curator's `sources:` block) is stripped
/// so it never reaches the embeddings; the title is the first `# H1`, falling
/// back to the humanised stem.
pub fn training_doc(file: &TrainingFile) -> (String, String, String) {
    let id = file
        .name
        .strip_suffix(".md")
        .unwrap_or(&file.name)
        .to_string();
    let body = strip_front_matter(&file.content);
    let title = first_h1(body).unwrap_or_else(|| id.replace(['-', '_'], " "));
    (id, title, body.trim().to_string())
}

/// Drop a leading `---` YAML front-matter block, if present. CRLF-tolerant, so a
/// doc authored on Windows or via the GitHub web editor still strips cleanly
/// (otherwise the curator's `sources:` YAML would leak into the embeddings).
fn strip_front_matter(content: &str) -> &str {
    let trimmed = content.trim_start_matches('\u{feff}');
    let after_open = trimmed
        .strip_prefix("---\n")
        .or_else(|| trimmed.strip_prefix("---\r\n"));
    let Some(rest) = after_open else {
        return content;
    };
    // The closing fence at the start of a line, LF or CRLF.
    for (pat, skip) in [("\n---\n", 5), ("\n---\r\n", 6), ("\r\n---\r\n", 7)] {
        if let Some(end) = rest.find(pat) {
            return &rest[end + skip..];
        }
    }
    // Closes at EOF with no trailing newline.
    if let Some(end) = rest.find("\n---") {
        return rest[end..].trim_start_matches(['\n', '\r', '-']);
    }
    content
}

/// The first Markdown H1 (`# Title`), trimmed.
fn first_h1(body: &str) -> Option<String> {
    body.lines()
        .find_map(|l| l.strip_prefix("# ").map(|h| h.trim().to_string()))
        .filter(|h| !h.is_empty())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn tree_from(pairs: &[(&str, &str)]) -> Result<EdgeConfigTree> {
        parse_tree(files_from(pairs))
    }

    fn files_from(pairs: &[(&str, &str)]) -> BTreeMap<String, Vec<u8>> {
        pairs
            .iter()
            .map(|(p, c)| (p.to_string(), c.as_bytes().to_vec()))
            .collect()
    }

    /// A GitHub-shaped zipball of `pairs`: every path under the single
    /// `<owner>-<repo>-<sha>/` wrapper directory the API adds.
    pub(crate) fn zipball(pairs: &[(&str, &str)]) -> Vec<u8> {
        use std::io::Write;
        let mut buf = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(Cursor::new(&mut buf));
            for (path, content) in pairs {
                zip.start_file::<_, ()>(
                    format!("acme-edge-config-0123abc/{path}"),
                    zip::write::FileOptions::default(),
                )
                .unwrap();
                zip.write_all(content.as_bytes()).unwrap();
            }
            zip.finish().unwrap();
        }
        buf
    }

    /// A mock GitHub serving `pairs` as the zipball of `owner/repo` at
    /// `git_ref`, to any bearer token. Returns the server (kept alive by the
    /// caller).
    pub(crate) async fn mock_github(
        repo: &str,
        git_ref: &str,
        pairs: &[(&str, &str)],
    ) -> wiremock::MockServer {
        let server = wiremock::MockServer::start().await;
        mount_zipball(&server, repo, git_ref, pairs).await;
        server
    }

    /// Serve `pairs` on `server` as the zipball of `repo` at `git_ref`.
    async fn mount_zipball(
        server: &wiremock::MockServer,
        repo: &str,
        git_ref: &str,
        pairs: &[(&str, &str)],
    ) {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};
        Mock::given(method("GET"))
            .and(path(format!("/repos/{repo}/zipball/{git_ref}")))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/zip")
                    .set_body_bytes(zipball(pairs)),
            )
            .mount(server)
            .await;
    }

    /// Serve `pairs` on `server` as commit `sha` of `repo` at `git_ref`: its
    /// HEAD SHA and its zipball.
    pub(crate) async fn serve_commit(
        server: &wiremock::MockServer,
        repo: &str,
        git_ref: &str,
        sha: &str,
        pairs: &[(&str, &str)],
    ) {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};
        Mock::given(method("GET"))
            .and(path(format!("/repos/{repo}/commits/{git_ref}")))
            .respond_with(ResponseTemplate::new(200).set_body_string(sha))
            .mount(server)
            .await;
        mount_zipball(server, repo, git_ref, pairs).await;
    }

    /// An archive whose config entry inflates past the per-entry cap is
    /// refused whole, whatever its header declares; one whose only large
    /// entry lies outside the config paths is never read and so passes.
    #[test]
    fn an_archive_entry_past_the_cap_is_refused_and_a_non_config_one_is_skipped() {
        use std::io::Write;
        let oversized = vec![b'#'; MAX_ZIP_ENTRY_BYTES + 1];
        let zip_with = |path: &str, big: &[u8]| {
            let mut buf = Vec::new();
            {
                let mut zip = zip::ZipWriter::new(Cursor::new(&mut buf));
                let options = zip::write::FileOptions::<()>::default()
                    .compression_method(zip::CompressionMethod::Deflated);
                zip.start_file("acme-edge-config-0123abc/agents/a/agent.toml", options)
                    .unwrap();
                zip.write_all(b"id = \"a\"\ndisplay_name = \"A\"\n")
                    .unwrap();
                zip.start_file(format!("acme-edge-config-0123abc/{path}"), options)
                    .unwrap();
                zip.write_all(big).unwrap();
                zip.finish().unwrap();
            }
            buf
        };

        let bomb = zip_with("agents/b/training/notes.md", &oversized);
        assert!(
            bomb.len() < 1024 * 1024,
            "the archive is small; its entry is what inflates"
        );
        let err = unzip(&bomb).unwrap_err();
        assert!(
            format!("{err:#}").contains("limit"),
            "refused for size: {err:#}"
        );

        let bulky_readme = zip_with("README.bin", &oversized);
        let files = unzip(&bulky_readme).unwrap();
        assert_eq!(
            files.keys().collect::<Vec<_>>(),
            ["agents/a/agent.toml"],
            "a non-config entry is skipped without being read"
        );
        assert!(!files.contains_key("README.bin"));
    }

    #[test]
    fn map_expansion_resolves_defaults_and_reports_the_rest() {
        let env = BTreeMap::from([("ISSUER".to_string(), "https://kc/realms/r".to_string())]);
        let (out, missing) = expand_from_map(
            "a=${ISSUER} b=${MISSING} c=${ALSO_MISSING:-dflt} d=${MISSING} e=${not valid} f=$",
            &env,
        );
        assert_eq!(out, "a=https://kc/realms/r b= c=dflt d= e=${not valid} f=$");
        assert_eq!(missing, vec!["MISSING".to_string(), "MISSING".to_string()]);
        assert_eq!(
            expand_from_map("${ISSUER", &env).0,
            "${ISSUER",
            "an unclosed placeholder is left as written"
        );
        assert_eq!(expand_from_map("${1BAD}", &env).0, "${1BAD}");
    }

    /// The import's expansion reads the map and only the map: a name set in
    /// the process environment is still unresolved, and the tree reports
    /// every missing name once, sorted.
    #[test]
    fn parse_tree_with_a_map_never_reads_the_process_environment() {
        std::env::set_var("EDGE_TEST_PROCESS_ONLY", "https://leaked.example");
        let files = files_from(&[
            (
                "agents/a/agent.toml",
                "id = \"a\"\ndisplay_name = \"A\"\n[embed]\nkeycloak_issuer = \"${EDGE_TEST_PROCESS_ONLY}\"\nallowed_origins = [\"${EDGE_ORIGIN}\"]",
            ),
            (
                "agents/b/agent.toml",
                "id = \"b\"\ndisplay_name = \"B\"\n[embed]\nkeycloak_issuer = \"${EDGE_ORIGIN}\"",
            ),
        ]);
        let parsed = parse_tree_with(files.clone(), Expansion::Map(&BTreeMap::new())).unwrap();
        assert_eq!(
            parsed.unresolved,
            vec![
                "EDGE_ORIGIN".to_string(),
                "EDGE_TEST_PROCESS_ONLY".to_string()
            ]
        );
        assert_eq!(parsed.tree.agents[0].embed.keycloak_issuer, "");

        let env = BTreeMap::from([
            (
                "EDGE_TEST_PROCESS_ONLY".to_string(),
                "https://kc/realms/r".to_string(),
            ),
            ("EDGE_ORIGIN".to_string(), "https://app.example".to_string()),
        ]);
        let parsed = parse_tree_with(files.clone(), Expansion::Map(&env)).unwrap();
        assert!(parsed.unresolved.is_empty());
        assert_eq!(
            parsed.tree.agents[0].embed.keycloak_issuer,
            "https://kc/realms/r"
        );
        assert_eq!(
            parsed.tree.agents[0].embed.allowed_origins,
            ["https://app.example"]
        );
        assert_eq!(
            parsed.tree.agents[1].embed.keycloak_issuer,
            "https://app.example"
        );

        let parsed = parse_tree_with(files, Expansion::ProcessEnv).unwrap();
        assert!(
            parsed.unresolved.is_empty(),
            "the process variant never reports"
        );
        assert_eq!(
            parsed.tree.agents[0].embed.keycloak_issuer,
            "https://leaked.example"
        );
        std::env::remove_var("EDGE_TEST_PROCESS_ONLY");
    }

    /// The network half: a zipball from the API base, unwrapped to
    /// repo-relative paths, with the token sent as a bearer.
    #[tokio::test]
    async fn fetch_files_unwraps_a_zipball_from_the_api_base() {
        let github = mock_github(
            "acme/edge-config",
            "main",
            &[
                ("agents/assist/agent.toml", "id = \"assist\""),
                ("playbooks/escalate.toml", "id = \"escalate\""),
            ],
        )
        .await;
        let cfg = GitConfig {
            repo: "acme/edge-config".into(),
            git_ref: "main".into(),
            token: "ghp_test".into(),
            poll_secs: 0,
        };
        let files = GitHub::at(&github.uri(), OutboundPolicy::Private)
            .fetch_files(&cfg)
            .await
            .unwrap();
        assert_eq!(
            files.keys().cloned().collect::<Vec<_>>(),
            ["agents/assist/agent.toml", "playbooks/escalate.toml"]
        );
        assert_eq!(files["playbooks/escalate.toml"], b"id = \"escalate\"");
        let sent = github.received_requests().await.unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(
            sent[0]
                .headers
                .get("authorization")
                .map(|v| v.to_str().unwrap()),
            Some("Bearer ghp_test")
        );
    }

    #[test]
    fn routes_agents_playbooks_and_training() {
        let tree = tree_from(&[
            (
                "agents/assist/agent.toml",
                "id = \"assist\"\ndisplay_name = \"Assist\"\n[embed]\nkeycloak_issuer = \"https://kc/realms/r\"",
            ),
            ("agents/assist/training/shifts.md", "# Shifts\ncontent"),
            ("agents/assist/training/rates.md", "# Rates"),
            ("playbooks/escalate.toml", "id = \"escalate\""),
            ("README.md", "ignore me"),
            ("agents/assist/notes.txt", "ignored non-training file"),
        ])
        .unwrap();
        assert_eq!(tree.agents.len(), 1);
        assert_eq!(tree.agents[0].id, "assist");
        assert_eq!(
            tree.playbooks,
            vec![("escalate".to_string(), "id = \"escalate\"".to_string())]
        );
        assert_eq!(tree.training.len(), 2);
        // sorted by name
        assert_eq!(tree.training[0].name, "rates.md");
        assert_eq!(tree.training[1].name, "shifts.md");
        assert_eq!(tree.training[0].agent_id, "assist");
    }

    #[test]
    fn expands_env_placeholders_in_agent_toml() {
        std::env::set_var("EDGE_TEST_ISSUER", "https://kc.example/realms/r");
        let tree = tree_from(&[(
            "agents/a/agent.toml",
            "id = \"a\"\ndisplay_name = \"A\"\n[embed]\nkeycloak_issuer = \"${EDGE_TEST_ISSUER}\"",
        )])
        .unwrap();
        assert_eq!(
            tree.agents[0].embed.keycloak_issuer,
            "https://kc.example/realms/r"
        );
        std::env::remove_var("EDGE_TEST_ISSUER");
    }

    #[test]
    fn training_doc_strips_front_matter_and_titles() {
        let f = TrainingFile {
            agent_id: "a".into(),
            name: "shifts.md".into(),
            content: "---\nsources: [\"core-api/**\"]\n---\n# Shifts\n\nBody text.".into(),
        };
        let (id, title, body) = training_doc(&f);
        assert_eq!(id, "shifts");
        assert_eq!(title, "Shifts");
        assert_eq!(body, "# Shifts\n\nBody text.");
        assert!(!body.contains("sources:"));
    }

    #[test]
    fn strips_crlf_front_matter() {
        let f = TrainingFile {
            agent_id: "a".into(),
            name: "x.md".into(),
            content: "---\r\nsources: [\"k/**\"]\r\n---\r\n# Title\r\n\r\nBody".into(),
        };
        let (_, title, body) = training_doc(&f);
        assert_eq!(title, "Title");
        assert!(!body.contains("sources:"), "front-matter leaked: {body:?}");
    }

    #[test]
    fn keeps_body_with_horizontal_rule() {
        // A doc with NO front-matter but a mid-body `---` thematic break must be
        // left intact.
        let f = TrainingFile {
            agent_id: "a".into(),
            name: "x.md".into(),
            content: "# Title\n\nPara one.\n\n---\n\nPara two.".into(),
        };
        let (_, _, body) = training_doc(&f);
        assert!(body.contains("Para one.") && body.contains("Para two."));
    }

    #[test]
    fn training_doc_title_falls_back_to_stem() {
        let f = TrainingFile {
            agent_id: "a".into(),
            name: "rate-cards.md".into(),
            content: "No heading here.".into(),
        };
        let (_, title, _) = training_doc(&f);
        assert_eq!(title, "rate cards");
    }

    #[test]
    fn empty_tree_is_ok() {
        let tree = tree_from(&[("README.md", "hi")]).unwrap();
        assert!(tree.agents.is_empty() && tree.playbooks.is_empty() && tree.training.is_empty());
    }

    #[test]
    fn malformed_agent_toml_errors() {
        let err = tree_from(&[("agents/x/agent.toml", "this = = broken")]).unwrap_err();
        assert!(err.to_string().contains("agents/x/agent.toml"));
    }
}
