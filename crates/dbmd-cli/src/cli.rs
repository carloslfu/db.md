//! The complete `dbmd` command tree (clap derive).
//!
//! This file **locks the command surface**: every subcommand, its flags, and
//! its nested subcommands are declared here so the subcommand-body agents can
//! fill `cmd/<name>.rs` in parallel without ever editing this file or
//! `main.rs`. The dispatch in `main.rs` matches exhaustively on
//! [`Command`]; adding/removing a variant is the one change that touches both.
//!
//! Conventions enforced here (agent-primary ergonomics, SPEC.md § Tooling):
//!   - A global `--json` flag on the top-level command, inherited by all.
//!   - A global `--color <auto|always|never>` (default `auto` ⇒ off; pipe-safe).
//!   - Rich per-subcommand help via `///` doc comments (clap renders them).
//!   - No interactive prompts anywhere — flags only.
//!
//! Argument *shapes* are locked to match SPEC.md / TOOLS.md and the plan
//! (Block 2 + Block 5). The parsed-arg structs are the contract each
//! `cmd/<name>.rs` body reads from.

use clap::{Args, Parser, Subcommand};

use crate::context::ColorChoice;

/// `dbmd` — the reference command-line tool for **db.md**, the open database in
/// plain files.
///
/// db.md is one directory: raw evidence in `sources/`, atomic typed data in
/// `records/`, curator-synthesized narrative in `wiki/`, and a single `DB.md`
/// at the root. `dbmd` reads, writes, validates, searches, and indexes that
/// store. It embeds ripgrep and has zero AI/LLM dependencies — the agent
/// driving `dbmd` is the semantic layer; `dbmd` is deterministic plumbing.
///
/// Every subcommand supports `--json` for machine-parseable output and
/// `--help`; none prompt interactively. See `dbmd spec` for the full standard.
#[derive(Debug, Parser)]
#[command(
    name = "dbmd",
    version,
    about = "The reference CLI for db.md — the open standard for databases in plain files.",
    long_about = None,
    propagate_version = true,
    // Show the most useful help when invoked bare, rather than a terse error.
    arg_required_else_help = true,
    // We manage color ourselves via --color so output is pipe-safe by default.
    disable_colored_help = true,
)]
pub struct Cli {
    /// Emit machine-parseable JSON instead of human-readable text. Honored by
    /// every subcommand; errors render as `{"error": {...}}` on stderr.
    #[arg(long, global = true)]
    pub json: bool,

    /// When to colorize human output: `auto` (default — off; pipe-safe),
    /// `always`, or `never`. JSON output is never colorized.
    #[arg(long, global = true, value_enum, default_value_t = ColorChoice::Auto, value_name = "WHEN")]
    pub color: ColorChoice,

    /// The subcommand to run.
    #[command(subcommand)]
    pub command: Command,
}

/// Every top-level `dbmd` subcommand. Grouped in declaration order by session
/// phase (open → warm up → read → write → validate → maintain → close), the
/// same grouping SPEC.md § Tooling and TOOLS.md use.
#[derive(Debug, Subcommand)]
pub enum Command {
    // ── Validate ────────────────────────────────────────────────────────────
    /// Validate a store: frontmatter conformance, link integrity, layer-typed
    /// rules, `DB.md` sections, and entity collisions.
    ///
    /// Default = the **working set** (files changed since the last `validate`
    /// log entry, or since `--since`). `--all` runs a full-store SWEEP that
    /// additionally checks `log.md` well-formedness, every index level's sync,
    /// and entity-dedup. Exits non-zero when errors are found.
    Validate(ValidateArgs),

    // ── Format ──────────────────────────────────────────────────────────────
    /// Re-emit a file's frontmatter + body canonically (key order, YAML style,
    /// whitespace). Writes back in place.
    Format(FormatArgs),

    // ── Read: structured query ───────────────────────────────────────────────
    /// Query files by frontmatter `type` and `--where` filters (Dataview-style).
    /// Resolves against the `index.jsonl` sidecar — never a whole-store parse.
    Query(QueryArgs),

    /// List every incoming wiki-link to a target file (its dependents).
    Links(LinksArgs),

    /// List the `##` sections of a single file.
    Sections(SectionsArgs),

    // ── Read: extraction ─────────────────────────────────────────────────────
    /// Extract plain text from a document (PDF / docx / xlsx / epub / html) to
    /// stdout, auto-detecting the format by extension.
    Extract(ExtractArgs),

    // ── Read: free-text + structured search ──────────────────────────────────
    /// Search the store with embedded ripgrep, narrowed by db.md-aware filters
    /// (`--type`, `--in`, `--where`, link filters, time windows). Structured
    /// filters resolve via the sidecar; the free-text query scans only the
    /// resulting candidate set. Output is `file:line: text`, `rg`-compatible.
    Search(SearchArgs),

    // ── Read: the relationship graph ─────────────────────────────────────────
    /// Inspect the wiki-link graph (backlinks, forward links, neighborhood,
    /// orphans). All on-demand; no maintained graph.
    Graph(GraphArgs),

    // ── Read / Write: frontmatter ────────────────────────────────────────────
    /// Read, write, query, or initialize file frontmatter.
    Fm(FmArgs),

    // ── Read: structural views ───────────────────────────────────────────────
    /// Pretty-print the store as a tree, optionally scoped by layer or type.
    Tree(TreeArgs),

    /// Print a store overview: file counts (overall / per-layer / per-type),
    /// total size, orphan + broken-link counts, top types. A SWEEP; never
    /// precomputed.
    Stats(StatsArgs),

    /// Print the section / sub-section outline of a single file.
    Outline(OutlineArgs),

    // ── Read / Maintain: the index catalog ───────────────────────────────────
    /// Maintain or read the write-through index catalog (`index.md` +
    /// `index.jsonl`): rebuild, show, or query.
    Index(IndexArgs),

    // ── Warm up / Close: the chronological log ───────────────────────────────
    /// Append to, or read from, the append-only store log. The append form is
    /// `dbmd log <kind> <object> [-m <note>]`; `tail` and `since` read it back.
    Log(LogArgs),

    // ── Write ────────────────────────────────────────────────────────────────
    /// Create a new file with canonical frontmatter. Auto-composes `summary`
    /// when `--summary` is absent; source-layer paths auto-shard by date and
    /// the resolved store-relative path is printed. Refuses on path collision.
    Write(WriteArgs),

    /// Append a wiki-link from one file to another (the common-case helper).
    Link(LinkArgs),

    /// Move a file and rewrite every incoming wiki-link across the store.
    /// Updates both affected type-folder indexes write-through.
    Rename(RenameArgs),

    // ── Agent bootstrap ──────────────────────────────────────────────────────
    /// Print the bundled canonical SPEC.md (compiled in at build time). The
    /// installation point: `dbmd spec` loads the standard into an agent's
    /// system prompt.
    Spec(SpecArgs),
}

// ─────────────────────────────────────────────────────────────────────────────
// validate
// ─────────────────────────────────────────────────────────────────────────────

/// `dbmd validate` — working-set by default, full SWEEP under `--all`.
#[derive(Debug, Args)]
pub struct ValidateArgs {
    /// Store root to validate. Defaults to the current directory (which must be
    /// a db.md store, i.e. contain `DB.md`).
    #[arg(value_name = "DIR", default_value = ".")]
    pub dir: String,

    /// Run a full-store SWEEP (every file, every index level, `log.md`
    /// well-formedness, entity-dedup) instead of the default working set.
    #[arg(long)]
    pub all: bool,

    /// Override the working-set cutoff: validate files changed at or after this
    /// RFC3339 timestamp. Ignored when `--all` is set. Date-only is accepted
    /// and treated as `T00:00:00Z`.
    #[arg(long, value_name = "RFC3339")]
    pub since: Option<String>,
}

// ─────────────────────────────────────────────────────────────────────────────
// format
// ─────────────────────────────────────────────────────────────────────────────

/// `dbmd format <file>` — canonical re-emit, writes back in place.
#[derive(Debug, Args)]
pub struct FormatArgs {
    /// The file to re-format canonically.
    #[arg(value_name = "FILE")]
    pub file: String,
}

// ─────────────────────────────────────────────────────────────────────────────
// query
// ─────────────────────────────────────────────────────────────────────────────

/// `dbmd query` — frontmatter filter over the sidecar.
#[derive(Debug, Args)]
pub struct QueryArgs {
    /// Filter to files whose frontmatter `type` equals this value.
    #[arg(long, value_name = "TYPE")]
    pub r#type: Option<String>,

    /// Scope to a single layer: `sources`, `records`, or `wiki`.
    #[arg(long, value_name = "LAYER")]
    pub r#in: Option<String>,

    /// Additional frontmatter filter as `key=value`. Repeatable.
    #[arg(long = "where", value_name = "K=V")]
    pub r#where: Vec<String>,

    /// Cap the number of results.
    #[arg(long, value_name = "N")]
    pub limit: Option<usize>,

    /// Store root. Defaults to the current directory.
    #[arg(long, value_name = "DIR", default_value = ".")]
    pub dir: String,
}

// ─────────────────────────────────────────────────────────────────────────────
// links
// ─────────────────────────────────────────────────────────────────────────────

/// `dbmd links <target>` — incoming wiki-links to a file.
#[derive(Debug, Args)]
pub struct LinksArgs {
    /// The store-relative target path whose incoming links to list.
    #[arg(value_name = "TARGET")]
    pub target: String,

    /// Store root. Defaults to the current directory.
    #[arg(long, value_name = "DIR", default_value = ".")]
    pub dir: String,
}

// ─────────────────────────────────────────────────────────────────────────────
// sections
// ─────────────────────────────────────────────────────────────────────────────

/// `dbmd sections <file>` — list `##` sections in a file.
#[derive(Debug, Args)]
pub struct SectionsArgs {
    /// The file whose sections to list.
    #[arg(value_name = "FILE")]
    pub file: String,
}

// ─────────────────────────────────────────────────────────────────────────────
// extract
// ─────────────────────────────────────────────────────────────────────────────

/// `dbmd extract <file>` — document text extraction.
#[derive(Debug, Args)]
pub struct ExtractArgs {
    /// The document to extract text from (PDF / docx / xlsx / epub / html;
    /// format auto-detected by extension).
    #[arg(value_name = "FILE")]
    pub file: String,

    /// Write the extracted text to this path instead of stdout.
    #[arg(long, value_name = "PATH")]
    pub out: Option<String>,
}

// ─────────────────────────────────────────────────────────────────────────────
// search
// ─────────────────────────────────────────────────────────────────────────────

/// `dbmd search <query>` — ripgrep over a sidecar-resolved candidate set.
#[derive(Debug, Args)]
pub struct SearchArgs {
    /// The free-text query (a regex; alternation like `(revenue|sales|ARR)` is
    /// the agent's query-expansion path — no embeddings).
    #[arg(value_name = "QUERY")]
    pub query: String,

    /// Filter to files whose frontmatter `type` equals this value.
    #[arg(long, value_name = "TYPE")]
    pub r#type: Option<String>,

    /// Scope to a single layer: `sources`, `records`, or `wiki`.
    #[arg(long, value_name = "LAYER")]
    pub r#in: Option<String>,

    /// Additional frontmatter filter as `key=value`. Repeatable.
    #[arg(long = "where", value_name = "K=V")]
    pub r#where: Vec<String>,

    /// Restrict to files that the given file links TO (forward links).
    #[arg(long, value_name = "PATH")]
    pub linked_from: Option<String>,

    /// Restrict to files that link TO the given file (backlinks).
    #[arg(long, value_name = "PATH")]
    pub linked_to: Option<String>,

    /// Only files whose `updated` is at or after this RFC3339 timestamp.
    #[arg(long, value_name = "RFC3339")]
    pub updated_after: Option<String>,

    /// Only files whose `updated` is at or before this RFC3339 timestamp.
    #[arg(long, value_name = "RFC3339")]
    pub updated_before: Option<String>,

    /// Only files whose `created` is at or after this RFC3339 timestamp.
    #[arg(long, value_name = "RFC3339")]
    pub created_after: Option<String>,

    /// Only files whose `created` is at or before this RFC3339 timestamp.
    #[arg(long, value_name = "RFC3339")]
    pub created_before: Option<String>,

    /// Cap the number of matches.
    #[arg(long, value_name = "N")]
    pub limit: Option<usize>,

    /// Store root. Defaults to the current directory.
    #[arg(long, value_name = "DIR", default_value = ".")]
    pub dir: String,
}

// ─────────────────────────────────────────────────────────────────────────────
// graph (backlinks / forwardlinks / neighborhood / orphans)
// ─────────────────────────────────────────────────────────────────────────────

/// `dbmd graph <sub>` — the relationship-retrieval axis.
#[derive(Debug, Args)]
pub struct GraphArgs {
    /// Which graph view to compute.
    #[command(subcommand)]
    pub command: GraphCommand,
}

/// The `dbmd graph` subcommands.
#[derive(Debug, Subcommand)]
pub enum GraphCommand {
    /// Incoming wiki-links to a file (blast radius / dependents).
    Backlinks(GraphTargetArgs),

    /// Outgoing wiki-links from a file (follow the chain).
    Forwardlinks(GraphTargetArgs),

    /// Bounded BFS from a seed: each reached node, its `summary`, and how it
    /// connects — context hydration in one call.
    Neighborhood(NeighborhoodArgs),

    /// Content files with no incoming or outgoing links (the curation
    /// worklist).
    Orphans(OrphansArgs),
}

/// Shared args for `graph backlinks` / `graph forwardlinks`.
#[derive(Debug, Args)]
pub struct GraphTargetArgs {
    /// The store-relative file path to inspect.
    #[arg(value_name = "PATH")]
    pub path: String,

    /// Restrict to linking/linked files of this frontmatter `type`. For
    /// `backlinks` this scopes which type-folder `index.jsonl` sidecars are read
    /// (an I/O scope, not just a filter); for `forwardlinks` it filters the
    /// returned targets by their type.
    #[arg(long, value_name = "TYPE")]
    pub r#type: Option<String>,

    /// Restrict to a single layer: `sources`, `records`, or `wiki`. For
    /// `backlinks` this scopes the sidecar walk to that layer; for
    /// `forwardlinks` it filters the returned targets by layer.
    #[arg(long, value_name = "LAYER")]
    pub r#in: Option<String>,

    /// Cap the number of results.
    #[arg(long, value_name = "N")]
    pub limit: Option<usize>,

    /// Store root. Defaults to the current directory.
    #[arg(long, value_name = "DIR", default_value = ".")]
    pub dir: String,
}

/// `dbmd graph neighborhood <seed>`.
#[derive(Debug, Args)]
pub struct NeighborhoodArgs {
    /// The store-relative seed path to expand from.
    #[arg(value_name = "SEED")]
    pub seed: String,

    /// How many hops out from the seed to traverse.
    #[arg(long, value_name = "N", default_value_t = 1)]
    pub hops: usize,

    /// Restrict reached nodes to this frontmatter `type`.
    #[arg(long, value_name = "TYPE")]
    pub r#type: Option<String>,

    /// Restrict reached nodes to this layer.
    #[arg(long, value_name = "LAYER")]
    pub r#in: Option<String>,

    /// Cap the number of reached nodes. Also bounds the BFS traversal work (the
    /// per-node full-store backlinks scans), not just the printed result, and
    /// defaults to 200 when unset so the command is never unbounded on a
    /// densely-linked hub.
    #[arg(long, value_name = "N")]
    pub limit: Option<usize>,

    /// Store root. Defaults to the current directory.
    #[arg(long, value_name = "DIR", default_value = ".")]
    pub dir: String,
}

/// `dbmd graph orphans`.
#[derive(Debug, Args)]
pub struct OrphansArgs {
    /// Restrict to a single layer: `sources`, `records`, or `wiki`.
    #[arg(long, value_name = "LAYER")]
    pub r#in: Option<String>,

    /// Cap the number of results.
    #[arg(long, value_name = "N")]
    pub limit: Option<usize>,

    /// Store root. Defaults to the current directory.
    #[arg(long, value_name = "DIR", default_value = ".")]
    pub dir: String,
}

// ─────────────────────────────────────────────────────────────────────────────
// fm (get / set / query / init)
// ─────────────────────────────────────────────────────────────────────────────

/// `dbmd fm <sub>` — frontmatter read/write/query/init.
#[derive(Debug, Args)]
pub struct FmArgs {
    /// Which frontmatter operation to run.
    #[command(subcommand)]
    pub command: FmCommand,
}

/// The `dbmd fm` subcommands.
#[derive(Debug, Subcommand)]
pub enum FmCommand {
    /// Read a single frontmatter value: `dbmd fm get <file> <key>`.
    Get(FmGetArgs),

    /// Set (insert/update) a frontmatter value: `dbmd fm set <file> <key>=<value>`.
    /// Atomic; re-sorts the type-folder index entry if recency changed.
    Set(FmSetArgs),

    /// Query the store by frontmatter via the sidecar (the pre-write dedup
    /// primitive): `dbmd fm query <key>=<value> [--type --in]`.
    Query(FmQueryArgs),

    /// Initialize canonical frontmatter on a file: auto-detect type by path,
    /// seed timestamps, compose a default `summary`, and fold the file into its
    /// `index`. `dbmd fm init <file> [--summary <str>]`.
    Init(FmInitArgs),
}

/// `dbmd fm get <file> <key>`.
#[derive(Debug, Args)]
pub struct FmGetArgs {
    /// The file to read frontmatter from (e.g. `DB.md` for store identity).
    #[arg(value_name = "FILE")]
    pub file: String,

    /// The frontmatter key to read.
    #[arg(value_name = "KEY")]
    pub key: String,
}

/// `dbmd fm set <file> <key>=<value>`.
#[derive(Debug, Args)]
pub struct FmSetArgs {
    /// The file to update.
    #[arg(value_name = "FILE")]
    pub file: String,

    /// The assignment, `key=value`. The value may be a wiki-link, scalar, or
    /// quoted string.
    #[arg(value_name = "K=V")]
    pub assignment: String,
}

/// `dbmd fm query <key>=<value>`.
#[derive(Debug, Args)]
pub struct FmQueryArgs {
    /// The frontmatter filter, `key=value`.
    #[arg(value_name = "K=V")]
    pub assignment: String,

    /// Restrict to files of this `type` (scopes to that type-folder's sidecar).
    #[arg(long, value_name = "TYPE")]
    pub r#type: Option<String>,

    /// Restrict to a single layer: `sources`, `records`, or `wiki`.
    #[arg(long, value_name = "LAYER")]
    pub r#in: Option<String>,

    /// Cap the number of results.
    #[arg(long, value_name = "N")]
    pub limit: Option<usize>,

    /// Store root. Defaults to the current directory.
    #[arg(long, value_name = "DIR", default_value = ".")]
    pub dir: String,
}

/// `dbmd fm init <file>`.
#[derive(Debug, Args)]
pub struct FmInitArgs {
    /// The file to initialize frontmatter on (type auto-detected by path).
    #[arg(value_name = "FILE")]
    pub file: String,

    /// Override the composed default `summary` with this string.
    #[arg(long, value_name = "STR")]
    pub summary: Option<String>,
}

// ─────────────────────────────────────────────────────────────────────────────
// tree
// ─────────────────────────────────────────────────────────────────────────────

/// `dbmd tree` — pretty-print the store.
#[derive(Debug, Args)]
pub struct TreeArgs {
    /// Restrict to a single layer: `sources`, `records`, or `wiki`.
    #[arg(long, value_name = "LAYER")]
    pub layer: Option<String>,

    /// Restrict to a single frontmatter `type`.
    #[arg(long, value_name = "TYPE")]
    pub r#type: Option<String>,

    /// Store root. Defaults to the current directory.
    #[arg(long, value_name = "DIR", default_value = ".")]
    pub dir: String,
}

// ─────────────────────────────────────────────────────────────────────────────
// stats
// ─────────────────────────────────────────────────────────────────────────────

/// `dbmd stats` — on-demand store overview (a SWEEP).
#[derive(Debug, Args)]
pub struct StatsArgs {
    /// Store root. Defaults to the current directory.
    #[arg(value_name = "DIR", default_value = ".")]
    pub dir: String,
}

// ─────────────────────────────────────────────────────────────────────────────
// outline
// ─────────────────────────────────────────────────────────────────────────────

/// `dbmd outline <file>` — section + sub-section outline of one file.
#[derive(Debug, Args)]
pub struct OutlineArgs {
    /// The file to outline.
    #[arg(value_name = "FILE")]
    pub file: String,

    /// The store directory (defaults to the current directory). Consistent with
    /// the other read commands so `outline` can target a store from elsewhere.
    #[arg(long, value_name = "DIR", default_value = ".")]
    pub dir: String,
}

// ─────────────────────────────────────────────────────────────────────────────
// index (rebuild / show / query)
// ─────────────────────────────────────────────────────────────────────────────

/// `dbmd index <sub>` — the write-through catalog.
#[derive(Debug, Args)]
pub struct IndexArgs {
    /// Which index operation to run.
    #[command(subcommand)]
    pub command: IndexCommand,
}

/// The `dbmd index` subcommands.
#[derive(Debug, Subcommand)]
pub enum IndexCommand {
    /// From-scratch repair of the catalog (not a loop step — writes maintain it
    /// write-through). Rebuilds the full hierarchy by default; scope with
    /// `--layer` / `--folder`; preview with `--dry-run`.
    Rebuild(IndexRebuildArgs),

    /// Print an `index.md` to stdout. Default = root; pass a layer or
    /// type-folder path for a scoped index.
    Show(IndexShowArgs),

    /// Complete structured read/filter over the `index.jsonl` sidecar(s).
    /// Returns full records (path + summary + tags + links + fields).
    Query(IndexQueryArgs),
}

/// `dbmd index rebuild`.
#[derive(Debug, Args)]
pub struct IndexRebuildArgs {
    /// Scope the rebuild to a single layer: `sources`, `records`, or `wiki`.
    #[arg(long, value_name = "LAYER")]
    pub layer: Option<String>,

    /// Scope the rebuild to a single folder (store-relative).
    #[arg(long, value_name = "PATH")]
    pub folder: Option<String>,

    /// Print what would be written (with `--- <path> ---` separators) without
    /// writing anything.
    #[arg(long)]
    pub dry_run: bool,

    /// Store root. Defaults to the current directory.
    #[arg(long, value_name = "DIR", default_value = ".")]
    pub dir: String,
}

/// `dbmd index show [<path>]`.
#[derive(Debug, Args)]
pub struct IndexShowArgs {
    /// The layer or type-folder whose `index.md` to print (e.g.
    /// `wiki/people`). Omit for the root `index.md`.
    #[arg(value_name = "PATH")]
    pub path: Option<String>,

    /// Store root. Defaults to the current directory.
    #[arg(long, value_name = "DIR", default_value = ".")]
    pub dir: String,
}

/// `dbmd index query`.
#[derive(Debug, Args)]
pub struct IndexQueryArgs {
    /// Filter to records of this frontmatter `type`.
    #[arg(long, value_name = "TYPE")]
    pub r#type: Option<String>,

    /// Restrict to a single layer: `sources`, `records`, or `wiki`.
    #[arg(long, value_name = "LAYER")]
    pub r#in: Option<String>,

    /// Additional frontmatter filter as `key=value`. Repeatable.
    #[arg(long = "where", value_name = "K=V")]
    pub r#where: Vec<String>,

    /// Only records whose `updated` is at or after this RFC3339 timestamp.
    #[arg(long, value_name = "RFC3339")]
    pub updated_after: Option<String>,

    /// Only records whose `updated` is at or before this RFC3339 timestamp.
    #[arg(long, value_name = "RFC3339")]
    pub updated_before: Option<String>,

    /// Only records whose `created` is at or after this RFC3339 timestamp.
    #[arg(long, value_name = "RFC3339")]
    pub created_after: Option<String>,

    /// Only records whose `created` is at or before this RFC3339 timestamp.
    #[arg(long, value_name = "RFC3339")]
    pub created_before: Option<String>,

    /// Cap the number of results.
    #[arg(long, value_name = "N")]
    pub limit: Option<usize>,

    /// Store root. Defaults to the current directory.
    #[arg(long, value_name = "DIR", default_value = ".")]
    pub dir: String,
}

// ─────────────────────────────────────────────────────────────────────────────
// log (append form + tail + since)
// ─────────────────────────────────────────────────────────────────────────────

/// `dbmd log` — the store timeline.
///
/// Two shapes share this command. The **append** form takes a `<kind>` and an
/// `<object>` positionally with an optional `-m <note>`:
/// `dbmd log create records/meetings/standup.md -m "weekly sync"`. The **read**
/// forms are the explicit `tail` and `since` subcommands. clap routes any
/// first token that is not `tail`/`since`/`help` into the append form via an
/// external subcommand; the body parses `<kind> <object> [-m <note>]` out of
/// the captured tokens.
#[derive(Debug, Args)]
pub struct LogArgs {
    /// `tail`, `since`, or the append form (`<kind> <object> [-m <note>]`).
    #[command(subcommand)]
    pub command: LogCommand,
}

/// The `dbmd log` subcommands.
#[derive(Debug, Subcommand)]
pub enum LogCommand {
    /// Read the last N entries (default 20), oldest→newest (chronological): the
    /// last printed line is the most recent.
    Tail(LogTailArgs),

    /// Read entries newer than an RFC3339 timestamp (date-only is treated as
    /// `T00:00:00Z`).
    Since(LogSinceArgs),

    /// The append form: `dbmd log <kind> <object> [-m <note>]`. Captured
    /// verbatim; the body splits out the kind, object, and optional `-m` note.
    /// (`<object>` is the file path the action was on, or `-` for store-wide.)
    #[command(external_subcommand)]
    Append(Vec<String>),
}

/// `dbmd log tail [N]`.
#[derive(Debug, Args)]
pub struct LogTailArgs {
    /// How many entries to read. The returned window is the last N entries,
    /// printed oldest→newest (chronological); the last line is the most recent.
    #[arg(value_name = "N", default_value_t = 20)]
    pub n: usize,

    /// Store root. Defaults to the current directory.
    #[arg(long, value_name = "DIR", default_value = ".")]
    pub dir: String,
}

/// `dbmd log since <timestamp>`.
#[derive(Debug, Args)]
pub struct LogSinceArgs {
    /// The RFC3339 timestamp; entries strictly newer are returned. Date-only
    /// (`2026-05-27`) is accepted and treated as `T00:00:00Z`.
    #[arg(value_name = "RFC3339")]
    pub timestamp: String,

    /// Store root. Defaults to the current directory.
    #[arg(long, value_name = "DIR", default_value = ".")]
    pub dir: String,
}

// ─────────────────────────────────────────────────────────────────────────────
// write
// ─────────────────────────────────────────────────────────────────────────────

/// `dbmd write <path> --type <t>` — create a new file with frontmatter.
#[derive(Debug, Args)]
pub struct WriteArgs {
    /// The store-relative path to create. Source-layer paths auto-shard by date
    /// (`sources/<type>/<YYYY>/<MM>/`); the resolved path is printed.
    #[arg(value_name = "PATH")]
    pub path: String,

    /// The frontmatter `type` for the new file (required).
    #[arg(long, value_name = "TYPE")]
    pub r#type: String,

    /// The canonical `summary`. If absent, a deterministic default is composed;
    /// a content file with no usable summary is refused.
    #[arg(long, value_name = "STR")]
    pub summary: Option<String>,

    /// Additional frontmatter as `key=value`. Repeatable.
    #[arg(long, value_name = "K=V")]
    pub fm: Vec<String>,

    /// Read the markdown body from this file (otherwise the body is empty).
    #[arg(long, value_name = "PATH")]
    pub body_file: Option<String>,

    /// Store root. Defaults to the current directory.
    #[arg(long, value_name = "DIR", default_value = ".")]
    pub dir: String,
}

// ─────────────────────────────────────────────────────────────────────────────
// link
// ─────────────────────────────────────────────────────────────────────────────

/// `dbmd link <from> <to>` — append a wiki-link.
#[derive(Debug, Args)]
pub struct LinkArgs {
    /// The file to add the wiki-link to.
    #[arg(value_name = "FROM")]
    pub from: String,

    /// The store-relative target the wiki-link points at.
    #[arg(value_name = "TO")]
    pub to: String,

    /// Store root. Defaults to the current directory.
    #[arg(long, value_name = "DIR", default_value = ".")]
    pub dir: String,
}

// ─────────────────────────────────────────────────────────────────────────────
// rename
// ─────────────────────────────────────────────────────────────────────────────

/// `dbmd rename <old> <new>` — move a file + rewrite incoming wiki-links.
#[derive(Debug, Args)]
pub struct RenameArgs {
    /// The current store-relative path.
    #[arg(value_name = "OLD")]
    pub old: String,

    /// The new store-relative path.
    #[arg(value_name = "NEW")]
    pub new: String,

    /// Store root. Defaults to the current directory.
    #[arg(long, value_name = "DIR", default_value = ".")]
    pub dir: String,
}

// ─────────────────────────────────────────────────────────────────────────────
// spec
// ─────────────────────────────────────────────────────────────────────────────

/// `dbmd spec` — print the bundled canonical SPEC.md.
#[derive(Debug, Args)]
pub struct SpecArgs {
    /// Print a specific SPEC instead of the compiled-in one (overrides the
    /// `DBMD_SPEC` env var).
    #[arg(long, value_name = "PATH")]
    pub spec: Option<String>,
}

// (install-skill / uninstall-skill removed: the installer is text — `dbmd spec`
// + the repo-root `llms.txt` + the distributable `skills/db-md/SKILL.md`. Agents
// and harness skill-installers place the skill; dbmd ships no per-harness code.)
