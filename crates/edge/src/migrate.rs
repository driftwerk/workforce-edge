//! `workforce-edge import-database` — a self-hosted edge's database, loaded
//! into a deployment that serves many tenants as one of them.
//!
//! A self-hosted install serves one tenant and keeps its sessions, audit
//! trail and analytics in the `sessions` and `edge_analytics` schemas of its
//! own database, at whatever shape its binary migrated them to — before
//! tenancy, without `tenant_id`. The cutover (empire `docs/edge-cloud.md`,
//! hosted cutover, step 4) moves those rows under the tenant the
//! install becomes. This takes a plain-SQL `pg_dump` of that database and
//! does exactly that, without ever running the dump as SQL: each `INSERT`
//! (or `COPY` block) is parsed, its columns are matched BY NAME to the
//! target table's, `tenant_id` is added, and the rows are written through
//! [`TenantPool`], so row-level security holds for the import as it does for
//! every other write.
//!
//! Dump the source with:
//!
//! ```text
//! pg_dump "$SOURCE_URL" --data-only --column-inserts \
//!     --schema=sessions --schema=edge_analytics > workforce_edge.sql
//! ```
//!
//! `--column-inserts` names every column on every row, which is what lets the
//! import map by name across a schema change. A dump without it — pg_dump's
//! default `COPY` blocks — names its columns in the `COPY` header and is
//! accepted too; `--inserts` without `--column-inserts` names none and is
//! refused. The refinery history tables the two schemas carry are passed
//! over like any other table the edge does not own, so they need no
//! exclusion.
//!
//! What lands, per table:
//!
//! - `sessions.sessions`, `sessions.session_archive`, `sessions.edge_audit`
//!   and the analytics tables `chat_identities`, `usage_events`, `feedback`,
//!   `answer_grades`, `turn_tool_evidence`: imported.
//! - `edge_agents`, `edge_playbooks`, `edge_knowledge`, `edge_operators`,
//!   `edge_revisions`: configuration, which the tenant's git import owns;
//!   skipped unless `--include-config`.
//! - `turn_claims`, `turn_journal`, `instance_heartbeats`, `active_turns`:
//!   the old process's live state, meaningless once it is frozen; always
//!   skipped.
//!
//! Columns the target no longer has (or never accepts: a generated column)
//! are dropped and reported. A `usage_events` row from before `purpose`
//! existed is a turn's, and is stamped so. The source's `BIGSERIAL` ids are
//! kept when the target's sequence has not handed them out, or when it has
//! and every such id is already this tenant's own row (a re-run), and the
//! sequence is moved past them; otherwise the sequence assigns new ids and
//! the summary says so. Owner keys (`ext:{issuer}␟{sub}`) are copied
//! verbatim: the issuer does not change with the host.
//!
//! Every batch is `ON CONFLICT DO NOTHING`, so a re-run adds nothing a prior
//! run already wrote — except to a table whose ids the sequence reassigned,
//! which has no other key to converge on. The summary never shows a row:
//! only counts, column names and outcomes, and a batch that Postgres
//! refuses is reported by SQLSTATE, table, batch and column — never by the
//! server's message, which quotes the value it choked on.
//!
//! Memory: the dump is read as a stream, one statement at a time, so a
//! `--column-inserts` dump of any size loads in bounded memory (a batch of
//! [`BATCH_ROWS`] at most, plus every table's serial ids from the first
//! pass). A `COPY` block is one statement holding the whole table, so a
//! `COPY`-format dump is held in memory a table at a time; dump a large
//! source with `--column-inserts`.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use eyre::{eyre, Result, WrapErr};
use tokio_postgres::types::ToSql;
use workforce_common::postgres::{PostgresConfig, PostgresPool, SslMode};
use workforce_common::session_postgres::PostgresSessionStore;

use crate::config::EdgeConfig;
use crate::tenant::{TenantConn, TenantId, TenantPool, TenantStore};

/// The subcommand's name on the `workforce-edge` binary.
pub const COMMAND: &str = "import-database";

/// Rows per `INSERT`: a few thousand parameters per statement, well under
/// Postgres's limit even for the widest table.
pub const BATCH_ROWS: usize = 500;

/// `workforce-edge import-database --help`.
pub const USAGE: &str = "\
workforce-edge import-database — load a self-hosted edge's database into this
deployment as one tenant.

USAGE:
    workforce-edge import-database --config edge.toml --tenant <slug> --from <dump.sql|->
        [--include-config] [--dry-run] [--overwrite] [--allow-default]

OPTIONS:
    --config <edge.toml>   This deployment's edge config; only [postgres] is used.
    --tenant <slug>        The tenant the rows are imported under. It must exist.
    --from <path|->        The dump, or `-` for stdin.
    --include-config       Also import edge_agents, edge_playbooks, edge_knowledge,
                           edge_operators and edge_revisions. Off by default: the
                           tenant's git import owns them.
    --dry-run              Parse the dump, check the target and print the summary;
                           write nothing.
    --overwrite            Proceed although the tenant already has rows in a table
                           the dump would fill. Each batch is ON CONFLICT DO NOTHING,
                           so rows a previous run wrote are skipped, not duplicated —
                           except in a table whose ids the sequence reassigned.
    --allow-default        Permit `--tenant default` (a self-hosted install's own
                           tenant), which is otherwise refused.

DUMP THE SOURCE WITH:
    pg_dump \"$SOURCE_URL\" --data-only --column-inserts \\
        --schema=sessions --schema=edge_analytics > workforce_edge.sql

A plain `--data-only` dump (COPY blocks) is accepted too. `--inserts` without
`--column-inserts` is not: the import maps columns by name.

Imported: sessions, session_archive, edge_audit, chat_identities, usage_events,
feedback, answer_grades, turn_tool_evidence. Always skipped (live state of the
old process): turn_claims, turn_journal, instance_heartbeats, active_turns.
Tables the edge does not own (refinery history, anything else) are skipped.
";

/// `import-database`'s command line.
#[derive(Debug)]
pub struct ImportArgs {
    pub config: PathBuf,
    pub from: String,
    pub options: ImportOptions,
}

/// What the import does with the dump, apart from where it reads it.
#[derive(Clone, Debug)]
pub struct ImportOptions {
    pub tenant: TenantId,
    pub include_config: bool,
    pub dry_run: bool,
    pub overwrite: bool,
    pub allow_default: bool,
}

impl ImportArgs {
    pub fn from_args(args: &[String]) -> Result<Self> {
        let mut config = None;
        let mut tenant = None;
        let mut from = None;
        let mut include_config = false;
        let mut dry_run = false;
        let mut overwrite = false;
        let mut allow_default = false;
        let mut it = args.iter();
        while let Some(flag) = it.next() {
            let mut val = || {
                it.next()
                    .cloned()
                    .ok_or_else(|| eyre!("{flag} needs a value"))
            };
            match flag.as_str() {
                "--config" => config = Some(PathBuf::from(val()?)),
                "--tenant" => tenant = Some(TenantId::parse(&val()?)?),
                "--from" => from = Some(val()?),
                "--include-config" => include_config = true,
                "--dry-run" => dry_run = true,
                "--overwrite" => overwrite = true,
                "--allow-default" => allow_default = true,
                other => return Err(eyre!("unknown {COMMAND} flag {other:?}\n\n{USAGE}")),
            }
        }
        Ok(Self {
            config: config.ok_or_else(|| eyre!("--config is required\n\n{USAGE}"))?,
            from: from.ok_or_else(|| eyre!("--from is required\n\n{USAGE}"))?,
            options: ImportOptions {
                tenant: tenant.ok_or_else(|| eyre!("--tenant is required\n\n{USAGE}"))?,
                include_config,
                dry_run,
                overwrite,
                allow_default,
            },
        })
    }
}

/// `workforce-edge import-database …`: parse the flags, reach the
/// deployment's Postgres, import, print the summary.
pub async fn run_cli(args: &[String]) -> Result<()> {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{USAGE}");
        return Ok(());
    }
    let args = ImportArgs::from_args(args)?;
    let config = EdgeConfig::load(&args.config)
        .wrap_err_with(|| format!("loading edge config from {}", args.config.display()))?;
    let pool = build_pool(&config.postgres, "workforce-edge-import")?;
    pool.ping()
        .await
        .wrap_err("reaching the deployment's Postgres")?;
    let mut dump = open_source(&args.from)?;
    let summary = import(&pool, &args.options, &mut dump).await?;
    println!("{summary}");
    Ok(())
}

/// The deployment's pool as boot builds it (`boot::postgres_config`), under
/// `application_name`, for an operator command. No migration runs here: the
/// target is a deployment already serving its tenants, and the command
/// checks the shape it finds rather than changing it.
pub(crate) fn build_pool(
    cfg: &crate::config::PostgresConfig,
    application_name: &str,
) -> Result<PostgresPool> {
    let sslmode = match cfg.sslmode.to_ascii_lowercase().as_str() {
        "disable" => SslMode::Disable,
        _ => SslMode::Require,
    };
    let pg = PostgresConfig {
        enabled: true,
        url: Some(cfg.url.clone()),
        sslmode,
        connect_timeout_secs: cfg.connect_budget_secs,
        application_name: Some(application_name.to_string()),
        ..PostgresConfig::default()
    };
    PostgresSessionStore::build_pool(&pg)
        .wrap_err_with(|| format!("building the {application_name} pool"))
}

/// The dump as a seekable file: the import reads it twice. Stdin is spooled
/// to a temporary file first.
fn open_source(from: &str) -> Result<File> {
    if from == "-" {
        let mut spool = tempfile::tempfile().wrap_err("creating a spool file for stdin")?;
        io::copy(&mut io::stdin().lock(), &mut spool).wrap_err("reading the dump from stdin")?;
        spool
            .seek(SeekFrom::Start(0))
            .wrap_err("rewinding the spooled dump")?;
        Ok(spool)
    } else {
        File::open(Path::new(from)).wrap_err_with(|| format!("opening the dump {from}"))
    }
}

// ─── The tables the edge owns ───────────────────────────────────────────────

/// Why a table is, or is not, imported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Disposition {
    /// A tenant's data: imported.
    Data,
    /// A tenant's configuration, which the git import owns: imported only on
    /// request.
    Config,
    /// The old process's live state: never imported.
    Live,
}

/// How far a `BIGSERIAL` column's values can collide.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SerialScope {
    /// The column alone is the key, across every tenant, so a source value
    /// may already be taken by another tenant's row.
    Install,
    /// The key leads with the tenant, so the source values are free; the
    /// sequence still has to move past them.
    Tenant,
}

struct KnownTable {
    schema: &'static str,
    name: &'static str,
    disposition: Disposition,
    serial: Option<(&'static str, SerialScope)>,
    /// `(column, value)` written when the source does not carry the column.
    defaults: &'static [(&'static str, &'static str)],
}

impl KnownTable {
    fn qualified(&self) -> String {
        format!("{}.{}", self.schema, self.name)
    }
}

const KNOWN_TABLES: &[KnownTable] = &[
    KnownTable {
        schema: "sessions",
        name: "sessions",
        disposition: Disposition::Data,
        serial: None,
        defaults: &[],
    },
    KnownTable {
        schema: "sessions",
        name: "session_archive",
        disposition: Disposition::Data,
        serial: Some(("seq", SerialScope::Tenant)),
        defaults: &[],
    },
    KnownTable {
        schema: "sessions",
        name: "edge_audit",
        disposition: Disposition::Data,
        serial: Some(("id", SerialScope::Install)),
        defaults: &[],
    },
    KnownTable {
        schema: "sessions",
        name: "turn_claims",
        disposition: Disposition::Live,
        serial: None,
        defaults: &[],
    },
    KnownTable {
        schema: "sessions",
        name: "turn_journal",
        disposition: Disposition::Live,
        serial: None,
        defaults: &[],
    },
    KnownTable {
        schema: "sessions",
        name: "instance_heartbeats",
        disposition: Disposition::Live,
        serial: None,
        defaults: &[],
    },
    KnownTable {
        schema: "sessions",
        name: "edge_agents",
        disposition: Disposition::Config,
        serial: None,
        defaults: &[],
    },
    KnownTable {
        schema: "sessions",
        name: "edge_playbooks",
        disposition: Disposition::Config,
        serial: None,
        defaults: &[],
    },
    KnownTable {
        schema: "sessions",
        name: "edge_knowledge",
        disposition: Disposition::Config,
        serial: None,
        defaults: &[],
    },
    KnownTable {
        schema: "sessions",
        name: "edge_operators",
        disposition: Disposition::Config,
        serial: None,
        defaults: &[],
    },
    KnownTable {
        schema: "sessions",
        name: "edge_revisions",
        disposition: Disposition::Config,
        serial: None,
        defaults: &[],
    },
    KnownTable {
        schema: "edge_analytics",
        name: "chat_identities",
        disposition: Disposition::Data,
        serial: None,
        defaults: &[],
    },
    KnownTable {
        schema: "edge_analytics",
        name: "usage_events",
        disposition: Disposition::Data,
        serial: Some(("id", SerialScope::Install)),
        // Before `purpose` existed only the customer turn wrote a row.
        defaults: &[("purpose", "turn")],
    },
    KnownTable {
        schema: "edge_analytics",
        name: "feedback",
        disposition: Disposition::Data,
        serial: Some(("id", SerialScope::Install)),
        defaults: &[],
    },
    KnownTable {
        schema: "edge_analytics",
        name: "answer_grades",
        disposition: Disposition::Data,
        serial: Some(("id", SerialScope::Install)),
        defaults: &[],
    },
    KnownTable {
        schema: "edge_analytics",
        name: "turn_tool_evidence",
        disposition: Disposition::Data,
        serial: None,
        defaults: &[],
    },
    KnownTable {
        schema: "edge_analytics",
        name: "active_turns",
        disposition: Disposition::Live,
        serial: None,
        defaults: &[],
    },
];

fn known_table(schema: &str, name: &str) -> Option<&'static KnownTable> {
    KNOWN_TABLES
        .iter()
        .find(|t| t.schema == schema && t.name == name)
}

// ─── The dump's grammar ─────────────────────────────────────────────────────

/// A value as the dump spells it. Every form is handed to Postgres as text
/// and cast to the target column's type there, so the import needs nothing
/// of a column but its name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Literal {
    Null,
    Bool(bool),
    /// The digits as written: `12`, `-3.5`, `1e6`.
    Number(String),
    /// A string with its quoting and escapes resolved.
    Text(String),
}

impl Literal {
    fn into_param(self) -> Option<String> {
        match self {
            Self::Null => None,
            Self::Bool(b) => Some(if b { "true" } else { "false" }.to_string()),
            Self::Number(n) | Self::Text(n) => Some(n),
        }
    }

    fn as_i64(&self) -> Option<i64> {
        match self {
            Self::Number(n) | Self::Text(n) => n.parse().ok(),
            Self::Null | Self::Bool(_) => None,
        }
    }
}

/// One `INSERT` statement or `COPY` block: a table, the columns it names, and
/// its rows, each as long as the column list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Statement {
    pub schema: String,
    pub table: String,
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Literal>>,
}

/// Reads a plain-SQL `pg_dump` data section one data statement at a time.
/// Everything that is not an `INSERT` or a `COPY` block — the `SET` lines,
/// comments, `setval` calls, pg_dump's `\restrict` markers — is passed over.
pub struct DumpReader<R: BufRead> {
    input: R,
    line_no: usize,
}

impl<R: BufRead> DumpReader<R> {
    pub fn new(input: R) -> Self {
        Self { input, line_no: 0 }
    }

    /// The next data statement, or `None` at the end of the dump.
    pub fn next_statement(&mut self) -> Result<Option<Statement>> {
        loop {
            let Some(line) = self.read_line()? else {
                return Ok(None);
            };
            let trimmed = line.trim_start();
            if trimmed.is_empty() || trimmed.starts_with("--") {
                continue;
            }
            if starts_with_keyword(trimmed, "COPY") {
                return self.copy_block(trimmed).map(Some);
            }
            if starts_with_keyword(trimmed, "INSERT") {
                return self.insert_statement(line).map(Some);
            }
        }
    }

    fn read_line(&mut self) -> Result<Option<String>> {
        let mut line = String::new();
        let read = self
            .input
            .read_line(&mut line)
            .wrap_err_with(|| format!("reading the dump after line {}", self.line_no))?;
        if read == 0 {
            return Ok(None);
        }
        self.line_no += 1;
        Ok(Some(line))
    }

    /// An `INSERT` runs to the first `;` outside a string; a string may hold
    /// newlines, so the statement may span lines.
    fn insert_statement(&mut self, first: String) -> Result<Statement> {
        let started_at = self.line_no;
        let mut text = first;
        let mut scanner = StringScanner::default();
        scanner.feed(&text);
        while !scanner.ends_statement(&text) {
            let Some(line) = self.read_line()? else {
                return Err(eyre!(
                    "the INSERT at line {started_at} is not terminated before the end of the dump"
                ));
            };
            scanner.feed(&line);
            text.push_str(&line);
        }
        parse_insert(&text).wrap_err_with(|| format!("the INSERT at line {started_at}"))
    }

    /// `COPY schema.table (cols) FROM stdin;` then one row per line until the
    /// `\.` terminator.
    fn copy_block(&mut self, header: &str) -> Result<Statement> {
        let started_at = self.line_no;
        let (schema, table, columns) =
            parse_copy_header(header).wrap_err_with(|| format!("the COPY at line {started_at}"))?;
        let mut rows = Vec::new();
        loop {
            let Some(line) = self.read_line()? else {
                return Err(eyre!(
                    "the COPY block at line {started_at} is not terminated (no `\\.`) before the \
                     end of the dump"
                ));
            };
            let line = line.strip_suffix('\n').unwrap_or(&line);
            let line = line.strip_suffix('\r').unwrap_or(line);
            if line == "\\." {
                break;
            }
            let row: Vec<Literal> = line.split('\t').map(copy_field).collect();
            if row.len() != columns.len() {
                return Err(eyre!(
                    "COPY {schema}.{table}: line {} has {} fields for {} columns",
                    self.line_no,
                    row.len(),
                    columns.len()
                ));
            }
            rows.push(row);
        }
        Ok(Statement {
            schema,
            table,
            columns,
            rows,
        })
    }
}

fn starts_with_keyword(text: &str, keyword: &str) -> bool {
    text.len() > keyword.len()
        && text.as_bytes()[..keyword.len()].eq_ignore_ascii_case(keyword.as_bytes())
        && text.as_bytes()[keyword.len()].is_ascii_whitespace()
}

/// Tracks whether the text fed so far ends inside a string literal, so a
/// `;` can be told from one inside a value. `''` inside a string is a quote,
/// not an end; in an `E'…'` string a backslash escapes the next character.
#[derive(Default)]
struct StringScanner {
    state: ScanState,
    /// The two characters before the current one, newest first.
    prev: [Option<char>; 2],
}

#[derive(Default, Clone, Copy)]
enum ScanState {
    #[default]
    Outside,
    Inside {
        escapes: bool,
    },
    /// A backslash in an escaping string: the next character is literal.
    Escaped {
        escapes: bool,
    },
    /// A quote inside a string: another quote continues the string, anything
    /// else means it ended before this character.
    Closing {
        escapes: bool,
    },
}

impl StringScanner {
    fn feed(&mut self, text: &str) {
        for ch in text.chars() {
            self.state = match self.state {
                ScanState::Outside if ch == '\'' => ScanState::Inside {
                    escapes: matches!(self.prev[0], Some('E' | 'e'))
                        && !self.prev[1].is_some_and(|p| p.is_alphanumeric() || p == '_'),
                },
                ScanState::Outside => ScanState::Outside,
                ScanState::Inside { escapes } if ch == '\'' => ScanState::Closing { escapes },
                ScanState::Inside { escapes: true } if ch == '\\' => {
                    ScanState::Escaped { escapes: true }
                }
                ScanState::Inside { escapes } => ScanState::Inside { escapes },
                ScanState::Escaped { escapes } => ScanState::Inside { escapes },
                ScanState::Closing { escapes } if ch == '\'' => ScanState::Inside { escapes },
                ScanState::Closing { .. } => ScanState::Outside,
            };
            self.prev = [Some(ch), self.prev[0]];
        }
    }

    fn ends_statement(&self, text: &str) -> bool {
        matches!(self.state, ScanState::Outside | ScanState::Closing { .. })
            && text.trim_end().ends_with(';')
    }
}

/// A character cursor over one statement.
struct Cursor<'a> {
    text: &'a str,
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(text: &'a str) -> Self {
        Self { text, pos: 0 }
    }

    fn rest(&self) -> &'a str {
        &self.text[self.pos..]
    }

    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn peek_at(&self, n: usize) -> Option<char> {
        self.rest().chars().nth(n)
    }

    fn bump(&mut self) -> Option<char> {
        let ch = self.peek()?;
        self.pos += ch.len_utf8();
        Some(ch)
    }

    fn skip_ws(&mut self) {
        while self.peek().is_some_and(char::is_whitespace) {
            self.bump();
        }
    }

    fn eat(&mut self, ch: char) -> bool {
        self.skip_ws();
        if self.peek() == Some(ch) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, ch: char) -> Result<()> {
        if self.eat(ch) {
            Ok(())
        } else {
            Err(eyre!("expected {ch:?} at offset {}", self.pos))
        }
    }

    /// Whether a keyword (case-insensitive, whole word) is next.
    fn at_keyword(&self, keyword: &str) -> bool {
        let rest = self.rest();
        rest.len() >= keyword.len()
            && rest.as_bytes()[..keyword.len()].eq_ignore_ascii_case(keyword.as_bytes())
            && !rest[keyword.len()..]
                .chars()
                .next()
                .is_some_and(|c| c.is_alphanumeric() || c == '_')
    }

    fn eat_keyword(&mut self, keyword: &str) -> bool {
        self.skip_ws();
        if self.at_keyword(keyword) {
            self.pos += keyword.len();
            true
        } else {
            false
        }
    }

    fn keyword(&mut self, keyword: &str) -> Result<()> {
        if self.eat_keyword(keyword) {
            Ok(())
        } else {
            Err(eyre!("expected {keyword} at offset {}", self.pos))
        }
    }

    /// A bare identifier (folded to lower case, as Postgres does) or a
    /// double-quoted one.
    fn identifier(&mut self) -> Result<String> {
        self.skip_ws();
        if self.eat('"') {
            let mut name = String::new();
            loop {
                match self.bump() {
                    Some('"') if self.peek() == Some('"') => {
                        self.bump();
                        name.push('"');
                    }
                    Some('"') => return Ok(name),
                    Some(ch) => name.push(ch),
                    None => return Err(eyre!("unterminated quoted identifier")),
                }
            }
        }
        let start = self.pos;
        while self
            .peek()
            .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '$')
        {
            self.bump();
        }
        if self.pos == start {
            return Err(eyre!("expected an identifier at offset {start}"));
        }
        Ok(self.text[start..self.pos].to_ascii_lowercase())
    }

    /// `( item, item, … )` with the opening parenthesis already consumed.
    fn list<T>(&mut self, mut item: impl FnMut(&mut Self) -> Result<T>) -> Result<Vec<T>> {
        let mut items = vec![item(self)?];
        loop {
            if self.eat(',') {
                items.push(item(self)?);
            } else {
                self.expect(')')?;
                return Ok(items);
            }
        }
    }

    fn literal(&mut self) -> Result<Literal> {
        self.skip_ws();
        let literal = match self.peek() {
            Some('\'') => Literal::Text(self.quoted_string(false)?),
            Some('E' | 'e') if self.peek_at(1) == Some('\'') => {
                self.bump();
                Literal::Text(self.quoted_string(true)?)
            }
            _ if self.at_keyword("NULL") => {
                self.pos += "NULL".len();
                Literal::Null
            }
            _ if self.at_keyword("TRUE") => {
                self.pos += "TRUE".len();
                Literal::Bool(true)
            }
            _ if self.at_keyword("FALSE") => {
                self.pos += "FALSE".len();
                Literal::Bool(false)
            }
            Some(c) if c == '-' || c == '+' || c.is_ascii_digit() => {
                let start = self.pos;
                self.bump();
                while self
                    .peek()
                    .is_some_and(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-'))
                {
                    self.bump();
                }
                Literal::Number(self.text[start..self.pos].to_string())
            }
            Some(other) => {
                return Err(eyre!(
                    "unsupported value starting with {other:?} at offset {}",
                    self.pos
                ))
            }
            None => return Err(eyre!("unexpected end of statement")),
        };
        // A `::type` cast after a value names the source's type, which the
        // target's column decides anyway.
        if self.rest().starts_with("::") {
            self.pos += 2;
            while self
                .peek()
                .is_some_and(|c| c.is_alphanumeric() || matches!(c, '_' | '.' | '[' | ']' | ' '))
            {
                self.bump();
            }
            if self.peek() == Some('(') {
                while !matches!(self.bump(), Some(')') | None) {}
            }
        }
        Ok(literal)
    }

    /// `'…'` with `''` for a quote; with `escapes`, an `E'…'` string whose
    /// backslash escapes are resolved.
    fn quoted_string(&mut self, escapes: bool) -> Result<String> {
        self.expect('\'')?;
        let mut out = String::new();
        loop {
            match self.bump() {
                Some('\'') if self.peek() == Some('\'') => {
                    self.bump();
                    out.push('\'');
                }
                Some('\'') => return Ok(out),
                Some('\\') if escapes => out.push(self.escape()?),
                Some(ch) => out.push(ch),
                None => return Err(eyre!("unterminated string")),
            }
        }
    }

    /// The character an `E'…'` backslash escape stands for, with the
    /// backslash already consumed.
    fn escape(&mut self) -> Result<char> {
        let ch = match self.bump() {
            Some('b') => '\u{8}',
            Some('f') => '\u{c}',
            Some('n') => '\n',
            Some('r') => '\r',
            Some('t') => '\t',
            Some(d @ '0'..='7') => {
                let mut code = d.to_digit(8).unwrap_or(0);
                for _ in 0..2 {
                    match self.peek().and_then(|c| c.to_digit(8)) {
                        Some(digit) => {
                            self.bump();
                            code = code * 8 + digit;
                        }
                        None => break,
                    }
                }
                char::from_u32(code)
                    .ok_or_else(|| eyre!("octal escape \\{code:o} is not a character"))?
            }
            Some('x') => self.hex_escape(2, true)?,
            Some('u') => self.hex_escape(4, false)?,
            Some('U') => self.hex_escape(8, false)?,
            Some(other) => other,
            None => return Err(eyre!("unterminated escape")),
        };
        Ok(ch)
    }

    fn hex_escape(&mut self, digits: usize, at_most: bool) -> Result<char> {
        let mut code = 0u32;
        let mut taken = 0;
        while taken < digits {
            match self.peek().and_then(|c| c.to_digit(16)) {
                Some(digit) => {
                    self.bump();
                    code = code * 16 + digit;
                    taken += 1;
                }
                None if at_most && taken > 0 => break,
                None => return Err(eyre!("malformed hex escape at offset {}", self.pos)),
            }
        }
        char::from_u32(code).ok_or_else(|| eyre!("hex escape {code:x} is not a character"))
    }

    fn at_end(&mut self) -> bool {
        self.skip_ws();
        self.pos == self.text.len()
    }
}

/// `INSERT INTO schema.table (cols) VALUES (row)[, (row)…] [ON CONFLICT DO NOTHING];`
fn parse_insert(text: &str) -> Result<Statement> {
    let mut c = Cursor::new(text);
    c.keyword("INSERT")?;
    c.keyword("INTO")?;
    let (schema, table) = qualified_table(&mut c)?;
    if !c.eat('(') {
        return Err(eyre!(
            "INSERT INTO {schema}.{table} names no columns; dump with --column-inserts"
        ));
    }
    let columns = c.list(Cursor::identifier)?;
    c.keyword("VALUES")?;
    let mut rows = Vec::new();
    loop {
        c.expect('(')?;
        let row = c.list(Cursor::literal)?;
        if row.len() != columns.len() {
            return Err(eyre!(
                "INSERT INTO {schema}.{table}: a row has {} values for {} columns",
                row.len(),
                columns.len()
            ));
        }
        rows.push(row);
        if !c.eat(',') {
            break;
        }
    }
    if c.eat_keyword("ON") {
        c.keyword("CONFLICT")?;
        c.keyword("DO")?;
        c.keyword("NOTHING")?;
    }
    c.expect(';')?;
    if !c.at_end() {
        return Err(eyre!("text after the INSERT's terminator"));
    }
    Ok(Statement {
        schema,
        table,
        columns,
        rows,
    })
}

/// `COPY schema.table (cols) FROM stdin;`
fn parse_copy_header(header: &str) -> Result<(String, String, Vec<String>)> {
    let mut c = Cursor::new(header);
    c.keyword("COPY")?;
    let (schema, table) = qualified_table(&mut c)?;
    if !c.eat('(') {
        return Err(eyre!("COPY {schema}.{table} names no columns"));
    }
    let columns = c.list(Cursor::identifier)?;
    c.keyword("FROM")?;
    c.keyword("stdin")?;
    c.expect(';')?;
    Ok((schema, table, columns))
}

/// `schema.table`. pg_dump qualifies every table; an unqualified one is
/// refused rather than guessed at.
fn qualified_table(c: &mut Cursor<'_>) -> Result<(String, String)> {
    let first = c.identifier()?;
    if !c.eat('.') {
        return Err(eyre!(
            "table {first} is not schema-qualified; dump with a current pg_dump"
        ));
    }
    let table = c.identifier()?;
    Ok((first, table))
}

/// One field of a `COPY … FROM stdin` text row: `\N` is null, a backslash
/// escapes the characters COPY cannot write raw.
fn copy_field(raw: &str) -> Literal {
    if raw == "\\N" {
        return Literal::Null;
    }
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('b') => out.push('\u{8}'),
            Some('f') => out.push('\u{c}'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('v') => out.push('\u{b}'),
            Some(d @ '0'..='7') => {
                let mut code = d.to_digit(8).unwrap_or(0);
                for _ in 0..2 {
                    match chars.peek().and_then(|c| c.to_digit(8)) {
                        Some(digit) => {
                            chars.next();
                            code = code * 8 + digit;
                        }
                        None => break,
                    }
                }
                out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
            }
            Some('x') => {
                let mut code = 0;
                let mut taken = 0;
                while taken < 2 {
                    match chars.peek().and_then(|c| c.to_digit(16)) {
                        Some(digit) => {
                            chars.next();
                            code = code * 16 + digit;
                            taken += 1;
                        }
                        None => break,
                    }
                }
                out.push(if taken == 0 {
                    'x'
                } else {
                    char::from_u32(code).unwrap_or('\u{fffd}')
                });
            }
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    Literal::Text(out)
}

// ─── Pass 1: what the dump holds ────────────────────────────────────────────

/// One table's rows as the dump has them.
struct ScannedTable {
    known: Option<&'static KnownTable>,
    parsed: u64,
    /// Every column any of its statements named.
    columns: BTreeSet<String>,
    /// Every distinct column list its statements carry — pg_dump writes one
    /// per table, but the plan maps each it finds, so the load meets no
    /// list the first pass did not check.
    column_lists: BTreeSet<Vec<String>>,
    /// Its serial column's values, when it has one and the dump carries it.
    /// Ids only — eight bytes a row — so the whole dump's fit.
    serial_ids: BTreeSet<i64>,
}

/// Parse the whole dump, keeping counts, column sets and serial ids per
/// table and no row. A dump that fails to parse fails here, before a write.
fn scan(input: impl BufRead) -> Result<BTreeMap<String, ScannedTable>> {
    let mut reader = DumpReader::new(input);
    let mut tables: BTreeMap<String, ScannedTable> = BTreeMap::new();
    while let Some(statement) = reader.next_statement()? {
        let key = format!("{}.{}", statement.schema, statement.table);
        let known = known_table(&statement.schema, &statement.table);
        let entry = tables.entry(key.clone()).or_insert_with(|| ScannedTable {
            known,
            parsed: 0,
            columns: BTreeSet::new(),
            column_lists: BTreeSet::new(),
            serial_ids: BTreeSet::new(),
        });
        entry.parsed += statement.rows.len() as u64;
        entry.columns.extend(statement.columns.iter().cloned());
        entry.column_lists.insert(statement.columns.clone());
        let serial_at = known
            .and_then(|t| t.serial)
            .and_then(|(column, _)| statement.columns.iter().position(|c| c == column));
        if let Some(at) = serial_at {
            for row in &statement.rows {
                let value = row[at]
                    .as_i64()
                    .ok_or_else(|| eyre!("{key}: a serial id is not an integer"))?;
                entry.serial_ids.insert(value);
            }
        }
    }
    Ok(tables)
}

// ─── The target's shape ─────────────────────────────────────────────────────

struct TargetColumn {
    name: String,
    /// The type as `format_type` spells it, for the cast from text.
    sql_type: String,
    /// A generated column takes no value.
    generated: bool,
}

/// What the source's ids do in the target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdOutcome {
    /// The source values are written and the sequence moved past them.
    Kept,
    /// The sequence may already have handed them out: it assigns new ones.
    Reassigned,
}

struct SerialPlan {
    column: &'static str,
    sequence: String,
    outcome: IdOutcome,
    source_max: i64,
}

/// How one table's rows land: its current columns, what its serial column
/// does, and where each column list the dump carries goes — every one
/// mapped and checked in the first pass, so a dry run reports what the
/// load would refuse.
struct TablePlan {
    known: &'static KnownTable,
    columns: Vec<TargetColumn>,
    serial: Option<SerialPlan>,
    mappings: HashMap<Vec<String>, Arc<Mapping>>,
}

impl TablePlan {
    async fn build(
        conn: &TenantConn,
        tenant: &TenantId,
        known: &'static KnownTable,
        scanned: &ScannedTable,
    ) -> Result<Self> {
        let table = known.qualified();
        let rows = conn
            .query(
                "SELECT a.attname::text, format_type(a.atttypid, a.atttypmod), a.attgenerated <> '' \
                 FROM pg_attribute a \
                 WHERE a.attrelid = ($1::text)::regclass AND a.attnum > 0 AND NOT a.attisdropped \
                 ORDER BY a.attnum",
                &[&table],
            )
            .await
            .wrap_err_with(|| format!("reading the columns of {table}; is the target migrated?"))?;
        let columns: Vec<TargetColumn> = rows
            .into_iter()
            .map(|row| TargetColumn {
                name: row.get(0),
                sql_type: row.get(1),
                generated: row.get(2),
            })
            .collect();
        if !columns.iter().any(|c| c.name == "tenant_id") {
            return Err(eyre!(
                "{table} has no tenant_id column: the target is not a multi-tenant edge, or its \
                 migrations have not run"
            ));
        }
        let serial = match known.serial {
            Some((column, scope)) if scanned.columns.contains(column) => {
                Some(Self::serial_plan(conn, tenant, known, column, scope, scanned).await?)
            }
            _ => None,
        };
        let mut plan = Self {
            known,
            columns,
            serial,
            mappings: HashMap::new(),
        };
        for list in &scanned.column_lists {
            let mapping = plan
                .mapping(list)
                .wrap_err_with(|| format!("{table}: mapping the dump's columns"))?;
            plan.mappings.insert(list.clone(), Arc::new(mapping));
        }
        Ok(plan)
    }

    /// Source columns the target does not take, across every column list
    /// the dump carries for the table.
    fn dropped(&self) -> BTreeSet<String> {
        self.mappings
            .values()
            .flat_map(|mapping| mapping.dropped.iter().cloned())
            .collect()
    }

    /// Whether the source's values of a serial column can be kept. The
    /// column's primary key is install-wide and the policies hide other
    /// tenants' rows, so the sequence — which every row's id came from — is
    /// what is asked: a value beyond what it has handed out is free, and one
    /// within is kept only when this tenant's own row already holds it (a
    /// re-run, which `ON CONFLICT` then skips). Any other taken id may be
    /// another tenant's row, which an insert would silently lose to.
    async fn serial_plan(
        conn: &TenantConn,
        tenant: &TenantId,
        known: &KnownTable,
        column: &'static str,
        scope: SerialScope,
        scanned: &ScannedTable,
    ) -> Result<SerialPlan> {
        let table = known.qualified();
        let source_max = *scanned
            .serial_ids
            .iter()
            .next_back()
            .ok_or_else(|| eyre!("{table}: the dump carries {column} but no value for it"))?;
        let sequence: Option<String> = conn
            .query_one(
                "SELECT pg_get_serial_sequence($1::text, $2::text)",
                &[&table, &column],
            )
            .await
            .wrap_err_with(|| format!("finding the sequence of {table}.{column}"))?
            .get(0);
        let sequence =
            sequence.ok_or_else(|| eyre!("{table}.{column} is not backed by a sequence"))?;
        let outcome = match scope {
            SerialScope::Tenant => IdOutcome::Kept,
            SerialScope::Install => {
                let row = conn
                    .query_one(
                        &format!("SELECT last_value, is_called FROM {sequence}"),
                        &[],
                    )
                    .await
                    .wrap_err_with(|| format!("reading the sequence of {table}.{column}"))?;
                let last_value: i64 = row.get(0);
                let is_called: bool = row.get(1);
                let handed_out: Vec<i64> = if is_called {
                    scanned.serial_ids.range(..=last_value).copied().collect()
                } else {
                    Vec::new()
                };
                if handed_out.is_empty() {
                    IdOutcome::Kept
                } else {
                    let own: i64 = conn
                        .query_one(
                            &format!(
                                "SELECT count(*) FROM {table} \
                                 WHERE tenant_id = $1 AND {column} = ANY($2)"
                            ),
                            &[&tenant.as_str(), &handed_out],
                        )
                        .await
                        .wrap_err_with(|| format!("checking {table}'s ids against the tenant's"))?
                        .get(0);
                    if own == handed_out.len() as i64 {
                        IdOutcome::Kept
                    } else {
                        IdOutcome::Reassigned
                    }
                }
            }
        };
        Ok(SerialPlan {
            column,
            sequence,
            outcome,
            source_max,
        })
    }

    /// The target column each source column lands in, by name; the rest are
    /// dropped. The serial column is dropped too when the sequence assigns,
    /// and so is a `tenant_id` the source carries: the import stamps the
    /// target tenant's, so a dump taken from a multi-tenant edge lands
    /// under the tenant asked for, not the one it came from. A list that
    /// would land two values in one column is refused.
    fn mapping(&self, source: &[String]) -> Result<Mapping> {
        let reassigned = self
            .serial
            .as_ref()
            .filter(|s| s.outcome == IdOutcome::Reassigned)
            .map(|s| s.column);
        let mut picks: Vec<(usize, String, String)> = Vec::new();
        let mut dropped = Vec::new();
        for (at, name) in source.iter().enumerate() {
            let target = self
                .columns
                .iter()
                .find(|c| &c.name == name && !c.generated);
            match target {
                Some(_) if reassigned == Some(name.as_str()) => {}
                Some(_) if name == TENANT_COLUMN => dropped.push(name.clone()),
                Some(column) => picks.push((at, column.name.clone(), column.sql_type.clone())),
                None => dropped.push(name.clone()),
            }
        }
        let defaults: Vec<(&'static str, &'static str)> = self
            .known
            .defaults
            .iter()
            .filter(|(column, _)| !source.iter().any(|c| c == column))
            .copied()
            .collect();
        let mut landing = BTreeSet::new();
        for column in picks
            .iter()
            .map(|(_, name, _)| name.as_str())
            .chain(std::iter::once(TENANT_COLUMN))
            .chain(defaults.iter().map(|(column, _)| *column))
        {
            if !landing.insert(column) {
                return Err(eyre!("column {column} is named more than once"));
            }
        }
        Ok(Mapping {
            picks,
            dropped,
            defaults,
        })
    }
}

/// The column every target table carries and the import writes itself.
const TENANT_COLUMN: &str = "tenant_id";

/// Where one statement's columns go.
struct Mapping {
    /// `(source index, target column, target type)`.
    picks: Vec<(usize, String, String)>,
    dropped: Vec<String>,
    defaults: Vec<(&'static str, &'static str)>,
}

impl Mapping {
    /// The batch's `INSERT`, every value bound as text and cast to its
    /// column's type by Postgres, `tenant_id` and the defaults appended.
    fn insert_sql(&self, table: &str, rows: usize) -> String {
        let mut columns: Vec<&str> = self
            .picks
            .iter()
            .map(|(_, name, _)| name.as_str())
            .collect();
        columns.push(TENANT_COLUMN);
        columns.extend(self.defaults.iter().map(|(column, _)| *column));
        let width = columns.len();
        let mut sql = format!("INSERT INTO {table} ({}) VALUES ", columns.join(", "));
        for row in 0..rows {
            if row > 0 {
                sql.push_str(", ");
            }
            sql.push('(');
            for (at, (_, _, sql_type)) in self.picks.iter().enumerate() {
                if at > 0 {
                    sql.push_str(", ");
                }
                sql.push_str(&format!("${}::text::{sql_type}", row * width + at + 1));
            }
            let tenant_at = row * width + self.picks.len() + 1;
            if !self.picks.is_empty() {
                sql.push_str(", ");
            }
            sql.push_str(&format!("${tenant_at}::text"));
            for extra in 0..self.defaults.len() {
                sql.push_str(&format!(", ${}::text", tenant_at + extra + 1));
            }
            sql.push(')');
        }
        sql.push_str(" ON CONFLICT DO NOTHING");
        sql
    }

    /// One row's parameters, in the order [`Self::insert_sql`] binds them.
    fn params(&self, row: Vec<Literal>, tenant: &TenantId) -> Vec<Option<String>> {
        let mut row: Vec<Option<Literal>> = row.into_iter().map(Some).collect();
        let mut params: Vec<Option<String>> = self
            .picks
            .iter()
            .map(|(at, _, _)| row[*at].take().and_then(Literal::into_param))
            .collect();
        params.push(Some(tenant.as_str().to_string()));
        params.extend(
            self.defaults
                .iter()
                .map(|(_, value)| Some(value.to_string())),
        );
        params
    }
}

// ─── Pass 2: the load ───────────────────────────────────────────────────────

/// A table's open transaction and the batch being filled for it.
struct OpenTable {
    key: String,
    conn: TenantConn,
    plan: Arc<TablePlan>,
    batch_mapping: Option<Arc<Mapping>>,
    batch: Vec<Vec<Option<String>>>,
    /// Batches written so far, which a refused batch is reported by.
    batches: usize,
    inserted: u64,
    dropped: BTreeSet<String>,
}

impl OpenTable {
    async fn push(&mut self, tenant: &TenantId, statement: Statement) -> Result<()> {
        let mapping = self
            .plan
            .mappings
            .get(&statement.columns)
            .map(Arc::clone)
            .ok_or_else(|| {
                eyre!(
                    "{}: the load met a column list the first pass did not",
                    self.key
                )
            })?;
        self.dropped.extend(mapping.dropped.iter().cloned());
        let same = self
            .batch_mapping
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &mapping));
        if !same {
            self.flush().await?;
            self.batch_mapping = Some(Arc::clone(&mapping));
        }
        for row in statement.rows {
            self.batch.push(mapping.params(row, tenant));
            if self.batch.len() >= BATCH_ROWS {
                self.flush().await?;
            }
        }
        Ok(())
    }

    async fn flush(&mut self) -> Result<()> {
        let Some(mapping) = self.batch_mapping.as_ref() else {
            return Ok(());
        };
        if self.batch.is_empty() {
            return Ok(());
        }
        let sql = mapping.insert_sql(&self.key, self.batch.len());
        let binds: Vec<&(dyn ToSql + Sync)> = self
            .batch
            .iter()
            .flatten()
            .map(|value| value as &(dyn ToSql + Sync))
            .collect();
        self.batches += 1;
        let inserted = self
            .conn
            .execute(&sql, &binds)
            .await
            .map_err(|e| batch_error(&self.key, self.batches, &e))?;
        self.inserted += inserted;
        self.batch.clear();
        Ok(())
    }

    /// Write what is left and commit.
    async fn finish(mut self) -> Result<(u64, BTreeSet<String>)> {
        self.flush().await?;
        let (inserted, dropped) = (self.inserted, std::mem::take(&mut self.dropped));
        self.conn.commit().await?;
        Ok((inserted, dropped))
    }
}

/// A refused batch, by what Postgres says about it that cannot quote a row:
/// the SQLSTATE, the table, which batch, and the column or constraint when
/// the error names one. The server's message itself — `invalid input syntax
/// for type integer: "…"` — carries the value and is left out. A failure
/// below the protocol (the connection dropping) has no row to quote and is
/// passed through.
fn batch_error(table: &str, batch: usize, error: &tokio_postgres::Error) -> eyre::Report {
    match error.as_db_error() {
        Some(db) => {
            let mut named = String::new();
            if let Some(column) = db.column() {
                named.push_str(&format!(", column {column}"));
            }
            if let Some(constraint) = db.constraint() {
                named.push_str(&format!(", constraint {constraint}"));
            }
            eyre!(
                "inserting batch {batch} into {table} failed with SQLSTATE {}{named}",
                db.code().code()
            )
        }
        None => eyre!("inserting batch {batch} into {table}: {error}"),
    }
}

/// Move every kept sequence past the source's ids, before a row is loaded:
/// a live insert during the load then takes an id above the source's rather
/// than one the load is about to write. Done once on `conn`, committed by
/// the caller before the load begins.
async fn advance_sequences(
    conn: &TenantConn,
    plans: &BTreeMap<String, Arc<TablePlan>>,
) -> Result<()> {
    for (key, plan) in plans {
        let Some(serial) = plan
            .serial
            .as_ref()
            .filter(|s| s.outcome == IdOutcome::Kept)
        else {
            continue;
        };
        conn.execute(
            &format!(
                "SELECT setval('{seq}', GREATEST(last_value, $1::text::bigint), true) FROM {seq}",
                seq = serial.sequence
            ),
            &[&serial.source_max.to_string()],
        )
        .await
        .wrap_err_with(|| format!("advancing the sequence of {key}"))?;
    }
    Ok(())
}

/// What the load wrote per table.
#[derive(Default)]
struct Loaded {
    inserted: u64,
    dropped: BTreeSet<String>,
}

/// Stream the dump into the target: one transaction per table (per run of
/// its statements, which pg_dump keeps together), batches of
/// [`BATCH_ROWS`]. Tables without a plan are passed over.
async fn load(
    input: impl BufRead,
    tenants: &TenantPool,
    tenant: &TenantId,
    plans: &BTreeMap<String, Arc<TablePlan>>,
) -> Result<BTreeMap<String, Loaded>> {
    let mut reader = DumpReader::new(input);
    let mut loaded: BTreeMap<String, Loaded> = BTreeMap::new();
    let mut open: Option<OpenTable> = None;
    while let Some(statement) = reader.next_statement()? {
        let key = format!("{}.{}", statement.schema, statement.table);
        let Some(plan) = plans.get(&key) else {
            continue;
        };
        if open.as_ref().is_some_and(|table| table.key != key) {
            let table = open.take().expect("an open table");
            close(table, &mut loaded).await?;
        }
        if open.is_none() {
            open = Some(OpenTable {
                key: key.clone(),
                conn: tenants.conn(tenant).await?,
                plan: Arc::clone(plan),
                batch_mapping: None,
                batch: Vec::new(),
                batches: 0,
                inserted: 0,
                dropped: BTreeSet::new(),
            });
        }
        let table = open.as_mut().expect("an open table");
        table.push(tenant, statement).await?;
    }
    if let Some(table) = open.take() {
        close(table, &mut loaded).await?;
    }
    Ok(loaded)
}

async fn close(table: OpenTable, loaded: &mut BTreeMap<String, Loaded>) -> Result<()> {
    let key = table.key.clone();
    let (inserted, dropped) = table.finish().await?;
    let entry = loaded.entry(key).or_default();
    entry.inserted += inserted;
    entry.dropped.extend(dropped);
    Ok(())
}

// ─── The summary ────────────────────────────────────────────────────────────

/// What became of one table in the dump.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Imported,
    /// Configuration, left to the git import (`--include-config` imports it).
    SkippedConfig,
    /// The old process's live state.
    SkippedLive,
    /// Not a table the edge owns.
    SkippedUnknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableSummary {
    /// `schema.table`.
    pub table: String,
    pub outcome: Outcome,
    pub parsed: u64,
    pub inserted: u64,
    /// Rows a unique key already held: a re-run's, or a duplicate's.
    pub conflicts: u64,
    /// Source columns the target does not take.
    pub dropped_columns: Vec<String>,
    /// `column = 'value'` written because the source lacked the column.
    pub defaulted: Vec<String>,
    pub ids: Option<IdOutcome>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Summary {
    pub tenant: TenantId,
    pub dry_run: bool,
    pub tables: Vec<TableSummary>,
}

impl Summary {
    pub fn table(&self, name: &str) -> Option<&TableSummary> {
        self.tables.iter().find(|t| t.table == name)
    }
}

impl fmt::Display for Summary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mode = if self.dry_run {
            " (dry run: nothing written)"
        } else {
            ""
        };
        writeln!(f, "Import into tenant {}{mode}", self.tenant)?;
        let width = self.tables.iter().map(|t| t.table.len()).max().unwrap_or(0);
        for t in &self.tables {
            match t.outcome {
                Outcome::Imported => {
                    write!(
                        f,
                        "  {:<width$}  imported  parsed {:>7}  inserted {:>7}  conflicts {:>7}",
                        t.table, t.parsed, t.inserted, t.conflicts
                    )?;
                    match t.ids {
                        Some(IdOutcome::Kept) => write!(f, "  ids kept")?,
                        Some(IdOutcome::Reassigned) => write!(
                            f,
                            "  ids reassigned by the sequence (a re-run duplicates these rows)"
                        )?,
                        None => {}
                    }
                    writeln!(f)?;
                    if !t.dropped_columns.is_empty() {
                        writeln!(
                            f,
                            "  {:<width$}    dropped columns: {}",
                            "",
                            t.dropped_columns.join(", ")
                        )?;
                    }
                    if !t.defaulted.is_empty() {
                        writeln!(
                            f,
                            "  {:<width$}    defaulted: {}",
                            "",
                            t.defaulted.join(", ")
                        )?;
                    }
                }
                Outcome::SkippedConfig => writeln!(
                    f,
                    "  {:<width$}  skipped   parsed {:>7}  (configuration; --include-config \
                     imports it)",
                    t.table, t.parsed
                )?,
                Outcome::SkippedLive => writeln!(
                    f,
                    "  {:<width$}  skipped   parsed {:>7}  (live state of the old process)",
                    t.table, t.parsed
                )?,
                Outcome::SkippedUnknown => writeln!(
                    f,
                    "  {:<width$}  skipped   parsed {:>7}  (not an edge table)",
                    t.table, t.parsed
                )?,
            }
        }
        Ok(())
    }
}

// ─── The import ─────────────────────────────────────────────────────────────

/// Plan every table the dump holds that `imports` says to take: its target
/// shape, its serial outcome and a mapping for each of its column lists.
/// Refuses, unless `overwrite`, when the tenant already has rows in one.
async fn plan(
    tenants: &TenantPool,
    tenant: &TenantId,
    scanned: &BTreeMap<String, ScannedTable>,
    overwrite: bool,
    imports: impl Fn(&KnownTable) -> bool,
) -> Result<BTreeMap<String, Arc<TablePlan>>> {
    let mut plans: BTreeMap<String, Arc<TablePlan>> = BTreeMap::new();
    let mut occupied = Vec::new();
    let conn = tenants.conn(tenant).await?;
    for (key, table) in scanned {
        let Some(known) = table.known.filter(|known| imports(known)) else {
            continue;
        };
        if table.parsed == 0 {
            continue;
        }
        let plan = TablePlan::build(&conn, tenant, known, table).await?;
        let has_rows: bool = conn
            .query_one(
                &format!("SELECT EXISTS (SELECT 1 FROM {key} WHERE tenant_id = $1)"),
                &[&tenant.as_str()],
            )
            .await
            .wrap_err_with(|| format!("checking {key} for tenant {tenant}'s rows"))?
            .get(0);
        if has_rows {
            occupied.push(key.clone());
        }
        plans.insert(key.clone(), Arc::new(plan));
    }
    conn.commit().await?;
    if !occupied.is_empty() && !overwrite {
        return Err(eyre!(
            "tenant {tenant} already has rows in {}; pass --overwrite to import over them \
             (rows already there are skipped, not replaced)",
            occupied.join(", ")
        ));
    }
    Ok(plans)
}

/// Import `dump` under `options.tenant`, which must exist in the target's
/// registry. The dump is read twice: once to parse and count it whole, so a
/// malformed dump is refused before anything is written, and once to load
/// it. A dry run does everything but the second pass, so what it prints is
/// what a real run would do, including a refusal.
pub async fn import<D: Read + Seek>(
    pool: &PostgresPool,
    options: &ImportOptions,
    dump: &mut D,
) -> Result<Summary> {
    let tenant = &options.tenant;
    if tenant.is_default() && !options.allow_default {
        return Err(eyre!(
            "refusing to import into the default tenant without --allow-default: it is the \
             tenant a self-hosted install itself is"
        ));
    }
    let registry = TenantStore::new(pool.clone());
    registry
        .get(tenant)
        .await?
        .ok_or_else(|| eyre!("tenant {tenant} does not exist; create it first"))?;
    let tenants = TenantPool::new(pool.clone());

    let scanned = scan(BufReader::new(&mut *dump))?;

    let imports = |known: &KnownTable| match known.disposition {
        Disposition::Data => true,
        Disposition::Config => options.include_config,
        Disposition::Live => false,
    };
    let plans = plan(&tenants, tenant, &scanned, options.overwrite, imports).await?;

    let loaded = if options.dry_run {
        BTreeMap::new()
    } else {
        let conn = tenants.conn(tenant).await?;
        advance_sequences(&conn, &plans).await?;
        conn.commit().await?;
        dump.seek(SeekFrom::Start(0))
            .wrap_err("rewinding the dump for the load")?;
        load(BufReader::new(&mut *dump), &tenants, tenant, &plans).await?
    };

    let tables = scanned
        .iter()
        .map(|(key, table)| {
            let outcome = match table.known {
                Some(known) if imports(known) => Outcome::Imported,
                Some(known) => match known.disposition {
                    Disposition::Config => Outcome::SkippedConfig,
                    Disposition::Live => Outcome::SkippedLive,
                    Disposition::Data => unreachable!("data tables are always imported"),
                },
                None => Outcome::SkippedUnknown,
            };
            let plan = plans.get(key);
            let written = loaded.get(key);
            let dropped_columns: Vec<String> = match (plan, written) {
                (_, Some(written)) => written.dropped.iter().cloned().collect(),
                // A dry run reports what the plan mapped, which is what the
                // load would drop.
                (Some(plan), None) => plan.dropped().into_iter().collect(),
                (None, None) => Vec::new(),
            };
            let defaulted = plan
                .map(|plan| {
                    plan.known
                        .defaults
                        .iter()
                        .filter(|(column, _)| !table.columns.contains(*column))
                        .map(|(column, value)| format!("{column} = '{value}'"))
                        .collect()
                })
                .unwrap_or_default();
            let inserted = written.map(|w| w.inserted).unwrap_or(0);
            TableSummary {
                table: key.clone(),
                outcome,
                parsed: table.parsed,
                inserted,
                conflicts: if written.is_some() {
                    table.parsed - inserted
                } else {
                    0
                },
                dropped_columns,
                defaulted,
                ids: plan.and_then(|plan| plan.serial.as_ref().map(|s| s.outcome)),
            }
        })
        .collect();
    Ok(Summary {
        tenant: tenant.clone(),
        dry_run: options.dry_run,
        tables,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use workforce_common::session_postgres::{run_session_migrations, PostgresSessionStore};
    use workforce_postgres::test_support::with_test_pool;

    use super::*;
    use crate::analytics::run_edge_analytics_migrations;
    use crate::tenant::run_edge_migrations;

    fn read_all(dump: &str) -> Vec<Statement> {
        let mut reader = DumpReader::new(io::Cursor::new(dump));
        let mut out = Vec::new();
        while let Some(statement) = reader.next_statement().unwrap() {
            out.push(statement);
        }
        out
    }

    fn text(s: &str) -> Literal {
        Literal::Text(s.to_string())
    }

    fn number(s: &str) -> Literal {
        Literal::Number(s.to_string())
    }

    // ─── The parser ─────────────────────────────────────────────────────────

    #[test]
    fn column_inserts_parse_every_literal_form() {
        let dump = "\
--
-- PostgreSQL database dump
--

SET statement_timeout = 0;
SELECT pg_catalog.set_config('search_path', '', false);
\\restrict abc123

--
-- Data for Name: feedback; Type: TABLE DATA; Schema: edge_analytics; Owner: edge
--

INSERT INTO edge_analytics.feedback (id, ts, owner, rating, comment, org, ok, cost, meta) VALUES (7, '2026-08-19 10:11:12.123456+00', 'ext:https://auth.example.com/realms/x\u{1f}sub-1', -1, 'it''s fine', NULL, true, 0.000125, '{\"a\": [1, \"two\"], \"b\": null}');
INSERT INTO edge_analytics.feedback (id, ts, owner, rating, comment, org, ok, cost, meta) VALUES (8, '2026-08-19 10:11:13+00', E'back\\\\slash \\'quoted\\' tab\\there', 1, 'multi
line
comment;', 'acme'::text, FALSE, 1e-6, '{}'::jsonb);


SELECT pg_catalog.setval('edge_analytics.feedback_id_seq', 8, true);
\\unrestrict abc123
";
        let statements = read_all(dump);
        assert_eq!(statements.len(), 2, "two INSERTs, nothing else");
        let first = &statements[0];
        assert_eq!(
            (first.schema.as_str(), first.table.as_str()),
            ("edge_analytics", "feedback")
        );
        assert_eq!(
            first.columns,
            ["id", "ts", "owner", "rating", "comment", "org", "ok", "cost", "meta"]
        );
        assert_eq!(
            first.rows[0],
            vec![
                number("7"),
                text("2026-08-19 10:11:12.123456+00"),
                text("ext:https://auth.example.com/realms/x\u{1f}sub-1"),
                number("-1"),
                text("it's fine"),
                Literal::Null,
                Literal::Bool(true),
                number("0.000125"),
                text("{\"a\": [1, \"two\"], \"b\": null}"),
            ]
        );
        assert_eq!(
            statements[1].rows[0],
            vec![
                number("8"),
                text("2026-08-19 10:11:13+00"),
                text("back\\slash 'quoted' tab\there"),
                number("1"),
                text("multi\nline\ncomment;"),
                text("acme"),
                Literal::Bool(false),
                number("1e-6"),
                text("{}"),
            ],
            "an E-string's escapes, a newline inside a value, a cast suffix"
        );
    }

    #[test]
    fn multi_row_inserts_and_an_on_conflict_clause_parse() {
        let dump = "INSERT INTO sessions.edge_audit (id, operator) VALUES (1, 'a'), (2, 'b'),\n(3, 'c') ON CONFLICT DO NOTHING;\n";
        let statements = read_all(dump);
        assert_eq!(statements.len(), 1);
        assert_eq!(statements[0].rows.len(), 3);
        assert_eq!(statements[0].rows[2], vec![number("3"), text("c")]);
    }

    #[test]
    fn quoted_identifiers_fold_and_bare_ones_lower_case() {
        let dump =
            "INSERT INTO \"sessions\".\"Edge_Audit\" (ID, \"Weird\"\"Name\") VALUES (1, 'x');\n";
        let statements = read_all(dump);
        assert_eq!(
            statements[0].table, "Edge_Audit",
            "quoted identifiers keep their case"
        );
        assert_eq!(statements[0].columns, ["id", "Weird\"Name"]);
    }

    #[test]
    fn inserts_without_a_column_list_are_refused() {
        let mut reader = DumpReader::new(io::Cursor::new(
            "INSERT INTO sessions.edge_audit VALUES (1, 'a');\n",
        ));
        let err = format!("{:#}", reader.next_statement().unwrap_err());
        assert!(err.contains("--column-inserts"), "{err}");
    }

    #[test]
    fn an_unterminated_insert_and_a_wrong_arity_are_errors() {
        let mut reader = DumpReader::new(io::Cursor::new(
            "INSERT INTO sessions.edge_audit (id, operator) VALUES (1, 'never closed\n",
        ));
        let err = reader.next_statement().unwrap_err().to_string();
        assert!(err.contains("not terminated"), "{err}");

        let mut reader = DumpReader::new(io::Cursor::new(
            "INSERT INTO sessions.edge_audit (id, operator) VALUES (1);\n",
        ));
        let err = format!("{:#}", reader.next_statement().unwrap_err());
        assert!(err.contains("1 values for 2 columns"), "{err}");
    }

    #[test]
    fn copy_blocks_parse_nulls_and_escapes() {
        let dump = "\
SET client_encoding = 'UTF8';

COPY edge_analytics.chat_identities (owner, display_name, email, flag) FROM stdin;
ext:iss\u{1f}sub-1\tAda\\tLovelace\t\\N\tt
ext:iss\u{1f}sub-2\tline\\nbreak \\\\ back\t\tf
\\.

COPY sessions.turn_claims (agent_id, session_id) FROM stdin;
\\.
";
        let statements = read_all(dump);
        assert_eq!(statements.len(), 2);
        let identities = &statements[0];
        assert_eq!(
            identities.columns,
            ["owner", "display_name", "email", "flag"]
        );
        assert_eq!(
            identities.rows,
            vec![
                vec![
                    text("ext:iss\u{1f}sub-1"),
                    text("Ada\tLovelace"),
                    Literal::Null,
                    text("t"),
                ],
                vec![
                    text("ext:iss\u{1f}sub-2"),
                    text("line\nbreak \\ back"),
                    text(""),
                    text("f"),
                ],
            ]
        );
        assert!(
            statements[1].rows.is_empty(),
            "an empty COPY block is a statement with no rows"
        );
    }

    #[test]
    fn copy_rows_with_the_wrong_arity_are_an_error() {
        let mut reader = DumpReader::new(io::Cursor::new(
            "COPY sessions.edge_audit (id, operator) FROM stdin;\n1\ta\textra\n\\.\n",
        ));
        let err = reader.next_statement().unwrap_err().to_string();
        assert!(err.contains("3 fields for 2 columns"), "{err}");
    }

    #[test]
    fn the_args_parse_and_the_usage_names_the_pg_dump_command() {
        let args: Vec<String> = [
            "--config",
            "edge.toml",
            "--tenant",
            "acme",
            "--from",
            "-",
            "--dry-run",
            "--include-config",
            "--overwrite",
            "--allow-default",
        ]
        .iter()
        .map(ToString::to_string)
        .collect();
        let parsed = ImportArgs::from_args(&args).unwrap();
        assert_eq!(parsed.config, PathBuf::from("edge.toml"));
        assert_eq!(parsed.from, "-");
        assert_eq!(parsed.options.tenant.as_str(), "acme");
        assert!(parsed.options.dry_run && parsed.options.include_config);
        assert!(parsed.options.overwrite && parsed.options.allow_default);

        let missing: Vec<String> = ["--config", "edge.toml", "--from", "dump.sql"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let err = ImportArgs::from_args(&missing).unwrap_err().to_string();
        assert!(err.contains("--tenant is required"), "{err}");

        let bad: Vec<String> = ["--tenant", "Not A Slug"]
            .iter()
            .map(ToString::to_string)
            .collect();
        assert!(ImportArgs::from_args(&bad).is_err());

        assert!(USAGE.contains("pg_dump \"$SOURCE_URL\" --data-only --column-inserts"));
        assert!(USAGE.contains("--schema=sessions --schema=edge_analytics"));
    }

    // ─── The import (embedded Postgres) ─────────────────────────────────────

    const ROLE: &str = "edge_import_rls";

    /// A migrated database with two registered tenants, `acme` and `b`, and
    /// a second pool whose connections take a role the policies bind — the
    /// import runs on that one, as the edge does. Mirrors `tenant::tests`'
    /// harness, which is private to that module.
    async fn with_import_pools<F, Fut>(test: F)
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
                         EXCEPTION WHEN duplicate_object OR unique_violation THEN NULL; \
                         END $$; \
                         GRANT USAGE ON SCHEMA sessions, edge_analytics TO {ROLE}; \
                         GRANT ALL ON ALL TABLES IN SCHEMA sessions, edge_analytics TO {ROLE}; \
                         GRANT ALL ON ALL SEQUENCES IN SCHEMA sessions, edge_analytics TO {ROLE}"
                    ))
                    .await
                    .unwrap();
                let registry = TenantStore::new(superuser.clone());
                registry.create(&tenant("acme"), "Acme").await.unwrap();
                registry.create(&tenant("b"), "B").await.unwrap();
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

    fn tenant(slug: &str) -> TenantId {
        TenantId::parse(slug).unwrap()
    }

    fn options(slug: &str) -> ImportOptions {
        ImportOptions {
            tenant: tenant(slug),
            include_config: false,
            dry_run: false,
            overwrite: false,
            allow_default: false,
        }
    }

    const OWNER_1: &str = "ext:https://auth.example.com/realms/x\u{1f}sub-1";
    const OWNER_2: &str = "ext:https://auth.example.com/realms/x\u{1f}sub-2";
    const SESSION_1: &str = "11111111-1111-4111-8111-111111111111";
    const SESSION_2: &str = "22222222-2222-4222-8222-222222222222";

    /// A dump in the shape a pre-tenancy install writes: no `tenant_id`
    /// anywhere, `usage_events` without `purpose`, a `sessions` column that
    /// no longer exists (`legacy_note`), the archive's generated
    /// `content_tsv` written out, a live-state table, a config table and a
    /// refinery history table.
    fn fixture_dump() -> String {
        format!(
            "\
--
-- PostgreSQL database dump
--

SET statement_timeout = 0;
SET standard_conforming_strings = on;
SELECT pg_catalog.set_config('search_path', '', false);

--
-- Data for Name: sessions; Type: TABLE DATA; Schema: sessions; Owner: edge
--

INSERT INTO sessions.sessions (id, agent_id, channel, owner, label, kind, context_type, created_at, updated_at, total_tokens, message_count, model_override, compaction_history, metadata, messages, sealed, seq, prev_session_id, revision, turn_instance, turn_started_at, turn_message_id, turn_token, legacy_note) VALUES ('{SESSION_1}', 'contoso-assistant', 'edge', '{OWNER_1}', NULL, 'conversation', NULL, '2026-08-19 10:00:00+00', '2026-08-19 10:05:00+00', 1200, 4, NULL, '[]', '{{\"conversation\": \"c1\"}}', '[{{\"role\": \"user\", \"content\": \"it''s a question\"}}]', false, 0, NULL, 3, NULL, NULL, NULL, NULL, 'old note');
INSERT INTO sessions.sessions (id, agent_id, channel, owner, label, kind, context_type, created_at, updated_at, total_tokens, message_count, model_override, compaction_history, metadata, messages, sealed, seq, prev_session_id, revision, turn_instance, turn_started_at, turn_message_id, turn_token, legacy_note) VALUES ('{SESSION_2}', 'contoso-assistant', 'edge', '{OWNER_2}', 'Rates', 'conversation', NULL, '2026-08-20 09:00:00+00', '2026-08-20 09:01:00+00', 300, 2, NULL, '[]', 'null', '[]', true, 1, NULL, 1, NULL, NULL, NULL, NULL, NULL);

--
-- Data for Name: session_archive; Type: TABLE DATA; Schema: sessions; Owner: edge
--

INSERT INTO sessions.session_archive (session_id, seq, message_id, role, content, tool_data, ts, tokens, content_tsv) VALUES ('{SESSION_1}', 1, 'm1', 'user', 'How do I upload a rate card?', NULL, '2026-08-19 10:00:00+00', 12, '''card'':6 ''rate'':5 ''upload'':4');
INSERT INTO sessions.session_archive (session_id, seq, message_id, role, content, tool_data, ts, tokens, content_tsv) VALUES ('{SESSION_1}', 2, 'm2', 'assistant', 'Open Settings, then
Rate cards.', '{{\"calls\": 1}}', '2026-08-19 10:00:05+00', 30, '''card'':6');
INSERT INTO sessions.session_archive (session_id, seq, message_id, role, content, tool_data, ts, tokens, content_tsv) VALUES ('{SESSION_2}', 3, 'm3', 'user', 'Hi', NULL, '2026-08-20 09:00:00+00', 1, NULL);

--
-- Data for Name: turn_claims; Type: TABLE DATA; Schema: sessions; Owner: edge
--

INSERT INTO sessions.turn_claims (agent_id, session_id, owner_instance, claim_id, expires_at) VALUES ('contoso-assistant', '{SESSION_1}', 'edge-0', 'claim-1', '2026-08-19 10:06:00+00');

--
-- Data for Name: edge_audit; Type: TABLE DATA; Schema: sessions; Owner: edge
--

INSERT INTO sessions.edge_audit (id, operator, action, target, detail, at) VALUES (1, 'ops@contoso.example', 'agent.update', 'contoso-assistant', 'tools +1', '2026-08-18 08:00:00+00');
INSERT INTO sessions.edge_audit (id, operator, action, target, detail, at) VALUES (2, 'ops@contoso.example', 'playbook.update', 'refunds', '', '2026-08-18 08:05:00+00');

--
-- Data for Name: edge_agents; Type: TABLE DATA; Schema: sessions; Owner: edge
--

INSERT INTO sessions.edge_agents (id, config, updated_at) VALUES ('contoso-assistant', '{{\"id\": \"contoso-assistant\"}}', '2026-08-18 08:00:00+00');

--
-- Data for Name: refinery_schema_history; Type: TABLE DATA; Schema: sessions; Owner: edge
--

INSERT INTO sessions.refinery_schema_history (version, name, applied_on, checksum) VALUES (10, 'turn_principal', '2026-08-01T00:00:00Z', '1');

--
-- Data for Name: chat_identities; Type: TABLE DATA; Schema: edge_analytics; Owner: edge
--

INSERT INTO edge_analytics.chat_identities (owner, display_name, email, org, issuer, first_seen, last_seen) VALUES ('{OWNER_1}', 'Ada', 'ada@example.com', NULL, 'https://auth.example.com/realms/x', '2026-08-19 10:00:00+00', '2026-08-19 10:05:00+00');

--
-- Data for Name: usage_events; Type: TABLE DATA; Schema: edge_analytics; Owner: edge
--

INSERT INTO edge_analytics.usage_events (id, ts, agent_id, owner, conversation, message_id, model, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, rounds, tool_count, tool_failures, duration_ms, cost_usd, outcome, org) VALUES (1, '2026-08-19 10:00:05+00', 'contoso-assistant', '{OWNER_1}', 'c1', 'm2', 'claude-sonnet-5', 1000, 200, 0, 0, 2, 1, 0, 4321, 0.004500, 'ok', NULL);
INSERT INTO edge_analytics.usage_events (id, ts, agent_id, owner, conversation, message_id, model, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, rounds, tool_count, tool_failures, duration_ms, cost_usd, outcome, org) VALUES (2, '2026-08-19 10:03:00+00', 'contoso-assistant', '{OWNER_1}', 'c1', 'm4', 'claude-sonnet-5', 1500, 100, 800, 0, 1, 0, 0, 2100, 0.002250, 'ok', NULL);
INSERT INTO edge_analytics.usage_events (id, ts, agent_id, owner, conversation, message_id, model, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, rounds, tool_count, tool_failures, duration_ms, cost_usd, outcome, org) VALUES (3, '2026-08-20 09:00:30+00', 'contoso-assistant', '{OWNER_2}', 'c2', NULL, 'claude-sonnet-5', 50, 0, 0, 0, 1, 0, 1, 900, 0.000100, 'error', NULL);

--
-- Data for Name: feedback; Type: TABLE DATA; Schema: edge_analytics; Owner: edge
--

INSERT INTO edge_analytics.feedback (id, ts, agent_id, owner, conversation, message_id, rating, comment, org) VALUES (1, '2026-08-19 10:06:00+00', 'contoso-assistant', '{OWNER_1}', 'c1', 'm2', 1, 'it''s right', NULL);

--
-- Data for Name: answer_grades; Type: TABLE DATA; Schema: edge_analytics; Owner: edge
--

INSERT INTO edge_analytics.answer_grades (id, ts, agent_id, owner, conversation, message_id, quality, gap_topic, remedy, reason, source, model, org, question) VALUES (1, '2026-08-19 10:00:06+00', 'contoso-assistant', '{OWNER_1}', 'c1', 'm2', 'good', NULL, 'none', 'grounded', 'llm', NULL, NULL, 'How do I upload a rate card?');

--
-- Data for Name: turn_tool_evidence; Type: TABLE DATA; Schema: edge_analytics; Owner: edge
--

INSERT INTO edge_analytics.turn_tool_evidence (message_id, conversation, owner, agent_id, org, evidence, ts) VALUES ('m2', 'c1', '{OWNER_1}', 'contoso-assistant', NULL, 'tool: rate_cards.list -> 2 cards', '2026-08-19 10:00:05+00');

--
-- Data for Name: active_turns; Type: TABLE DATA; Schema: edge_analytics; Owner: edge
--

INSERT INTO edge_analytics.active_turns (turn_key, conversation, owner, agent_id, replica, started_at, cancel_requested_at) VALUES ('k1', 'c1', '{OWNER_1}', 'contoso-assistant', 'edge-0', '2026-08-19 10:05:00+00', NULL);

SELECT pg_catalog.setval('edge_analytics.usage_events_id_seq', 3, true);
"
        )
    }

    async fn count(pool: &PostgresPool, table: &str, slug: &str) -> i64 {
        pool.get()
            .await
            .unwrap()
            .query_one(
                &format!("SELECT count(*) FROM {table} WHERE tenant_id = $1"),
                &[&slug],
            )
            .await
            .unwrap()
            .get(0)
    }

    const IMPORTED: &[(&str, i64)] = &[
        ("sessions.sessions", 2),
        ("sessions.session_archive", 3),
        ("sessions.edge_audit", 2),
        ("edge_analytics.chat_identities", 1),
        ("edge_analytics.usage_events", 3),
        ("edge_analytics.feedback", 1),
        ("edge_analytics.answer_grades", 1),
        ("edge_analytics.turn_tool_evidence", 1),
    ];

    #[tokio::test]
    async fn imports_a_pre_tenancy_dump_under_one_tenant_and_a_rerun_skips_what_is_there() {
        with_import_pools(|superuser, bound| async move {
            let mut dump = io::Cursor::new(fixture_dump().into_bytes());
            let summary = import(&bound, &options("acme"), &mut dump).await.unwrap();

            for (table, expected) in IMPORTED {
                let t = summary
                    .table(table)
                    .unwrap_or_else(|| panic!("{table} in summary"));
                assert_eq!(t.outcome, Outcome::Imported, "{table}");
                assert_eq!(
                    (t.parsed, t.inserted, t.conflicts),
                    (*expected as u64, *expected as u64, 0),
                    "{table}"
                );
                assert_eq!(
                    count(&superuser, table, "acme").await,
                    *expected,
                    "{table} stamped acme"
                );
            }
            let sessions = summary.table("sessions.sessions").unwrap();
            assert_eq!(
                sessions.dropped_columns,
                ["legacy_note"],
                "a column the target lost is dropped"
            );
            let archive = summary.table("sessions.session_archive").unwrap();
            assert_eq!(
                archive.dropped_columns,
                ["content_tsv"],
                "a generated column takes no value"
            );
            assert_eq!(archive.ids, Some(IdOutcome::Kept));
            let usage = summary.table("edge_analytics.usage_events").unwrap();
            assert_eq!(usage.defaulted, ["purpose = 'turn'"]);
            assert_eq!(
                usage.ids,
                Some(IdOutcome::Kept),
                "a fresh sequence has handed out nothing"
            );
            for (table, reason) in [
                ("sessions.turn_claims", Outcome::SkippedLive),
                ("edge_analytics.active_turns", Outcome::SkippedLive),
                ("sessions.edge_agents", Outcome::SkippedConfig),
                ("sessions.refinery_schema_history", Outcome::SkippedUnknown),
            ] {
                let t = summary.table(table).unwrap();
                assert_eq!(t.outcome, reason, "{table}");
                assert_eq!((t.parsed, t.inserted), (1, 0), "{table}");
            }
            assert_eq!(count(&superuser, "sessions.edge_agents", "acme").await, 0);
            assert_eq!(count(&superuser, "sessions.turn_claims", "acme").await, 0);

            let conn = superuser.get().await.unwrap();
            let purposes: Vec<String> = conn
                .query(
                    "SELECT DISTINCT purpose FROM edge_analytics.usage_events",
                    &[],
                )
                .await
                .unwrap()
                .into_iter()
                .map(|r| r.get(0))
                .collect();
            assert_eq!(purposes, ["turn"], "rows from before `purpose` are turns");
            let ids: Vec<i64> = conn
                .query(
                    "SELECT id FROM edge_analytics.usage_events ORDER BY id",
                    &[],
                )
                .await
                .unwrap()
                .into_iter()
                .map(|r| r.get(0))
                .collect();
            assert_eq!(ids, [1, 2, 3], "the source's ids are kept");
            let next: i64 = conn
                .query_one("SELECT nextval('edge_analytics.usage_events_id_seq')", &[])
                .await
                .unwrap()
                .get(0);
            assert_eq!(next, 4, "the sequence moved past the imported ids");
            let next_seq: i64 = conn
                .query_one("SELECT nextval('sessions.session_archive_seq_seq')", &[])
                .await
                .unwrap()
                .get(0);
            assert_eq!(next_seq, 4, "the archive sequence too");
            let row = conn
                .query_one(
                    "SELECT s.owner, a.content FROM sessions.sessions s \
                     JOIN sessions.session_archive a ON a.session_id = s.id \
                     WHERE a.message_id = 'm2'",
                    &[],
                )
                .await
                .unwrap();
            let (owner, content): (String, String) = (row.get(0), row.get(1));
            assert_eq!(
                owner, OWNER_1,
                "owner keys are verbatim, separator included"
            );
            assert_eq!(
                content, "Open Settings, then\nRate cards.",
                "a value spanning lines"
            );
            let messages: serde_json::Value = conn
                .query_one(
                    "SELECT messages FROM sessions.sessions WHERE id = $1::text::uuid",
                    &[&SESSION_1],
                )
                .await
                .unwrap()
                .get(0);
            assert_eq!(
                messages[0]["content"], "it's a question",
                "JSONB lands as JSON"
            );
            drop(conn);

            // Tenant `b`, through the pool the edge uses, sees none of it.
            let tenants = TenantPool::new(bound.clone());
            let b = tenants.conn(&tenant("b")).await.unwrap();
            for (table, _) in IMPORTED {
                let seen: i64 = b
                    .query_one(&format!("SELECT count(*) FROM {table}"), &[])
                    .await
                    .unwrap()
                    .get(0);
                assert_eq!(seen, 0, "{table}: tenant b sees nothing");
            }
            b.commit().await.unwrap();

            // A re-run is refused without --overwrite, and with it adds nothing.
            let mut dump = io::Cursor::new(fixture_dump().into_bytes());
            let err = import(&bound, &options("acme"), &mut dump)
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains("--overwrite"), "{err}");
            assert!(err.contains("sessions.sessions"), "{err}");

            let mut dump = io::Cursor::new(fixture_dump().into_bytes());
            let again = import(
                &bound,
                &ImportOptions {
                    overwrite: true,
                    ..options("acme")
                },
                &mut dump,
            )
            .await
            .unwrap();
            for (table, expected) in IMPORTED {
                let t = again.table(table).unwrap();
                assert_eq!(
                    (t.inserted, t.conflicts),
                    (0, *expected as u64),
                    "{table} re-run"
                );
                assert_eq!(
                    count(&superuser, table, "acme").await,
                    *expected,
                    "{table} unchanged"
                );
            }
            let rendered = again.to_string();
            assert!(
                rendered.contains(&format!("conflicts {:>7}", 3)),
                "{rendered}"
            );
            assert!(
                !rendered.contains(OWNER_1) && !rendered.contains("rate card"),
                "no row content: {rendered}"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn a_dry_run_checks_everything_and_writes_nothing() {
        with_import_pools(|superuser, bound| async move {
            let mut dump = io::Cursor::new(fixture_dump().into_bytes());
            let summary = import(
                &bound,
                &ImportOptions {
                    dry_run: true,
                    ..options("acme")
                },
                &mut dump,
            )
            .await
            .unwrap();
            assert!(summary.dry_run);
            for (table, expected) in IMPORTED {
                let t = summary.table(table).unwrap();
                assert_eq!(
                    (t.parsed, t.inserted, t.conflicts),
                    (*expected as u64, 0, 0),
                    "{table}"
                );
                assert_eq!(
                    count(&superuser, table, "acme").await,
                    0,
                    "{table} untouched"
                );
            }
            assert_eq!(
                summary.table("sessions.sessions").unwrap().dropped_columns,
                ["legacy_note"],
                "the dry run reports what the load would drop"
            );
            assert_eq!(
                summary
                    .table("edge_analytics.usage_events")
                    .unwrap()
                    .defaulted,
                ["purpose = 'turn'"]
            );
            assert!(summary.to_string().contains("dry run"));
        })
        .await;
    }

    #[tokio::test]
    async fn include_config_imports_the_configuration_tables_too() {
        with_import_pools(|superuser, bound| async move {
            let mut dump = io::Cursor::new(fixture_dump().into_bytes());
            let summary = import(
                &bound,
                &ImportOptions {
                    include_config: true,
                    ..options("acme")
                },
                &mut dump,
            )
            .await
            .unwrap();
            let agents = summary.table("sessions.edge_agents").unwrap();
            assert_eq!(agents.outcome, Outcome::Imported);
            assert_eq!((agents.parsed, agents.inserted), (1, 1));
            assert_eq!(count(&superuser, "sessions.edge_agents", "acme").await, 1);
            assert_eq!(count(&superuser, "sessions.edge_agents", "b").await, 0);
        })
        .await;
    }

    #[tokio::test]
    async fn ids_the_sequence_already_handed_out_are_reassigned() {
        with_import_pools(|superuser, bound| async move {
            // Another tenant's row takes id 1 before the import.
            let tenants = TenantPool::new(bound.clone());
            let b = tenants.conn(&tenant("b")).await.unwrap();
            b.execute(
                "INSERT INTO edge_analytics.usage_events (tenant_id, agent_id, owner, conversation, \
                 model, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, rounds, \
                 tool_count, tool_failures, duration_ms, cost_usd, outcome) \
                 VALUES ('b', 'x', 'o', 'c', 'm', 1, 1, 0, 0, 1, 0, 0, 1, 0, 'ok')",
                &[],
            )
            .await
            .unwrap();
            b.commit().await.unwrap();

            let mut dump = io::Cursor::new(fixture_dump().into_bytes());
            let summary = import(&bound, &options("acme"), &mut dump).await.unwrap();
            let usage = summary.table("edge_analytics.usage_events").unwrap();
            assert_eq!(usage.ids, Some(IdOutcome::Reassigned));
            assert_eq!((usage.inserted, usage.conflicts), (3, 0), "no row is lost to a collision");
            assert_eq!(
                summary.table("edge_analytics.feedback").unwrap().ids,
                Some(IdOutcome::Kept),
                "an untouched sequence keeps its ids"
            );
            let ids: Vec<i64> = superuser
                .get()
                .await
                .unwrap()
                .query(
                    "SELECT id FROM edge_analytics.usage_events WHERE tenant_id = 'acme' ORDER BY id",
                    &[],
                )
                .await
                .unwrap()
                .into_iter()
                .map(|r| r.get(0))
                .collect();
            assert_eq!(ids, [2, 3, 4], "the sequence assigned fresh ids after b's");
            assert!(summary.to_string().contains("ids reassigned by the sequence"));
        })
        .await;
    }

    #[tokio::test]
    async fn the_tenant_must_exist_and_default_needs_allow_default() {
        with_import_pools(|_superuser, bound| async move {
            let mut dump = io::Cursor::new(fixture_dump().into_bytes());
            let err = import(&bound, &options("nobody"), &mut dump)
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains("tenant nobody does not exist"), "{err}");

            let mut dump = io::Cursor::new(fixture_dump().into_bytes());
            let err = import(&bound, &options("default"), &mut dump)
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains("--allow-default"), "{err}");

            let mut dump = io::Cursor::new(fixture_dump().into_bytes());
            let summary = import(
                &bound,
                &ImportOptions {
                    allow_default: true,
                    ..options("default")
                },
                &mut dump,
            )
            .await
            .unwrap();
            assert_eq!(summary.table("sessions.sessions").unwrap().inserted, 2);
        })
        .await;
    }

    #[tokio::test]
    async fn a_malformed_dump_is_refused_before_anything_is_written() {
        with_import_pools(|superuser, bound| async move {
            let mut text = fixture_dump();
            text.push_str("INSERT INTO sessions.edge_audit (id) VALUES (1, 'two');\n");
            let mut dump = io::Cursor::new(text.into_bytes());
            let err = format!(
                "{:#}",
                import(&bound, &options("acme"), &mut dump)
                    .await
                    .unwrap_err()
            );
            assert!(err.contains("2 values for 1 columns"), "{err}");
            for (table, _) in IMPORTED {
                assert_eq!(
                    count(&superuser, table, "acme").await,
                    0,
                    "{table} untouched"
                );
            }
        })
        .await;
    }

    #[tokio::test]
    async fn a_copy_format_dump_imports_the_same_way() {
        with_import_pools(|superuser, bound| async move {
            let dump = format!(
                "\
COPY sessions.edge_audit (id, operator, action, target, detail, at) FROM stdin;
1\tops@contoso.example\tagent.update\tcontoso-assistant\ttools +1\\nand a line\t2026-08-18 08:00:00+00
2\tops@contoso.example\tplaybook.update\trefunds\t\t2026-08-18 08:05:00+00
\\.

COPY edge_analytics.usage_events (id, ts, agent_id, owner, conversation, message_id, model, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, rounds, tool_count, tool_failures, duration_ms, cost_usd, outcome, org) FROM stdin;
1\t2026-08-19 10:00:05+00\tcontoso-assistant\t{OWNER_1}\tc1\tm2\tclaude-sonnet-5\t1000\t200\t0\t0\t2\t1\t0\t4321\t0.004500\tok\t\\N
\\.
"
            );
            let mut dump = io::Cursor::new(dump.into_bytes());
            let summary = import(&bound, &options("acme"), &mut dump).await.unwrap();
            assert_eq!(summary.table("sessions.edge_audit").unwrap().inserted, 2);
            assert_eq!(summary.table("edge_analytics.usage_events").unwrap().inserted, 1);
            assert_eq!(count(&superuser, "sessions.edge_audit", "acme").await, 2);
            let detail: String = superuser
                .get()
                .await
                .unwrap()
                .query_one("SELECT detail FROM sessions.edge_audit WHERE id = 1", &[])
                .await
                .unwrap()
                .get(0);
            assert_eq!(detail, "tools +1\nand a line");
            assert_eq!(count(&superuser, "edge_analytics.usage_events", "acme").await, 1);
        })
        .await;
    }

    /// A dump taken from a multi-tenant edge carries `tenant_id` (and
    /// `purpose`, which the target appends only when absent): the import
    /// stamps its own tenant, reports the source's column as dropped — in
    /// the dry run too — and lands every row under the tenant asked for.
    #[tokio::test]
    async fn a_dump_carrying_tenant_id_lands_under_the_target_tenant() {
        with_import_pools(|superuser, bound| async move {
            let dump = format!(
                "\
INSERT INTO sessions.edge_audit (tenant_id, id, operator, action, target, detail, at) VALUES ('old', 1, 'ops@old.example', 'agent.update', 'a', '', '2026-08-18 08:00:00+00');
INSERT INTO sessions.edge_audit (tenant_id, id, operator, action, target, detail, at) VALUES ('old', 2, 'ops@old.example', 'agent.update', 'b', '', '2026-08-18 08:01:00+00');
INSERT INTO edge_analytics.usage_events (tenant_id, id, ts, agent_id, owner, conversation, message_id, model, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, rounds, tool_count, tool_failures, duration_ms, cost_usd, outcome, org, purpose) VALUES ('old', 1, '2026-08-19 10:00:05+00', 'a', '{OWNER_1}', 'c1', 'm2', 'm', 1, 1, 0, 0, 1, 0, 0, 1, 0, 'ok', NULL, 'grader');
"
            );
            let mut dry = io::Cursor::new(dump.clone().into_bytes());
            let summary = import(
                &bound,
                &ImportOptions {
                    dry_run: true,
                    ..options("acme")
                },
                &mut dry,
            )
            .await
            .unwrap();
            assert_eq!(
                summary.table("sessions.edge_audit").unwrap().dropped_columns,
                ["tenant_id"],
                "the dry run reports the source's tenant as dropped"
            );
            let usage = summary.table("edge_analytics.usage_events").unwrap();
            assert_eq!(usage.dropped_columns, ["tenant_id"]);
            assert!(
                usage.defaulted.is_empty(),
                "a purpose the dump carries is not defaulted"
            );

            let mut dump = io::Cursor::new(dump.into_bytes());
            let summary = import(&bound, &options("acme"), &mut dump).await.unwrap();
            let audit = summary.table("sessions.edge_audit").unwrap();
            assert_eq!((audit.inserted, audit.conflicts), (2, 0), "{summary}");
            assert_eq!(audit.dropped_columns, ["tenant_id"]);
            assert_eq!(count(&superuser, "sessions.edge_audit", "acme").await, 2);
            assert_eq!(count(&superuser, "sessions.edge_audit", "old").await, 0);
            let purpose: String = superuser
                .get()
                .await
                .unwrap()
                .query_one(
                    "SELECT purpose FROM edge_analytics.usage_events WHERE tenant_id = 'acme'",
                    &[],
                )
                .await
                .unwrap()
                .get(0);
            assert_eq!(purpose, "grader", "the dump's purpose is kept");
        })
        .await;
    }

    /// A batch Postgres refuses is reported by SQLSTATE, table and batch,
    /// never by the server's message, which quotes the value it choked on.
    #[tokio::test]
    async fn a_refused_batch_is_reported_without_the_value() {
        with_import_pools(|superuser, bound| async move {
            let dump = format!(
                "\
INSERT INTO edge_analytics.feedback (id, ts, agent_id, owner, conversation, message_id, rating, comment, org) VALUES (1, '2026-08-19 10:06:00+00', 'a', '{OWNER_1}', 'c1', 'm2', 'SECRET-PAYLOAD-7', 'c', NULL);
"
            );
            let mut dump = io::Cursor::new(dump.into_bytes());
            let err = import(&bound, &options("acme"), &mut dump)
                .await
                .unwrap_err();
            let rendered = format!("{err:#}");
            assert!(
                rendered.contains("SQLSTATE 22P02")
                    && rendered.contains("edge_analytics.feedback")
                    && rendered.contains("batch 1"),
                "{rendered}"
            );
            assert!(
                !rendered.contains("SECRET-PAYLOAD") && !rendered.contains(OWNER_1),
                "no payload fragment: {rendered}"
            );
            assert_eq!(count(&superuser, "edge_analytics.feedback", "acme").await, 0);
        })
        .await;
    }

    /// A kept sequence is moved past the source's ids before the load, so
    /// a live insert while the load runs takes an id above them instead of
    /// one the load is about to write: the source row lands without a
    /// conflict, the live row keeps its own.
    #[tokio::test]
    async fn sequences_advance_before_the_load_so_live_inserts_cannot_collide() {
        with_import_pools(|superuser, bound| async move {
            let tenants = TenantPool::new(bound.clone());
            let acme = tenant("acme");
            let dump = fixture_dump();
            let scanned = scan(io::Cursor::new(dump.as_bytes())).unwrap();
            let plans = plan(&tenants, &acme, &scanned, false, |known| {
                known.disposition == Disposition::Data
            })
            .await
            .unwrap();
            assert_eq!(
                plans["edge_analytics.feedback"].serial.as_ref().unwrap().outcome,
                IdOutcome::Kept
            );
            let conn = tenants.conn(&acme).await.unwrap();
            advance_sequences(&conn, &plans).await.unwrap();
            conn.commit().await.unwrap();

            // A customer's feedback arriving between the plan and the load.
            let live = tenants.conn(&acme).await.unwrap();
            let live_id: i64 = live
                .query_one(
                    "INSERT INTO edge_analytics.feedback (tenant_id, agent_id, owner, conversation, \
                     message_id, rating) VALUES ('acme', 'a', 'o', 'c', 'm9', 1) RETURNING id",
                    &[],
                )
                .await
                .unwrap()
                .get(0);
            live.commit().await.unwrap();
            assert_eq!(live_id, 2, "above the source's id 1, not on it");

            let loaded = load(io::Cursor::new(dump.as_bytes()), &tenants, &acme, &plans)
                .await
                .unwrap();
            assert_eq!(
                loaded["edge_analytics.feedback"].inserted, 1,
                "the source row lands, no conflict"
            );
            assert_eq!(count(&superuser, "edge_analytics.feedback", "acme").await, 2);
            let next: i64 = superuser
                .get()
                .await
                .unwrap()
                .query_one("SELECT nextval('edge_analytics.feedback_id_seq')", &[])
                .await
                .unwrap()
                .get(0);
            assert_eq!(next, 3);
        })
        .await;
    }
}
