//! `dbmd-core` — the reference library for **db.md**, the open database in
//! plain files.
//!
//! db.md is one directory: raw evidence in `sources/`, atomic typed data plus
//! curator-synthesized conclusions (`meta-type: conclusion`) in `records/`, and
//! a single `DB.md` config file at the root. Records are markdown files with
//! YAML frontmatter;
//! relationships are wiki-links; the index is the derived, write-through
//! `index.md` / `index.jsonl` catalog plus embedded ripgrep.
//!
//! This crate owns **all** toolkit logic. The `dbmd` binary (`dbmd-cli`) is a
//! thin wrapper that parses args, calls into here, and formats output. Any
//! Rust tool wanting to be db.md-aware can `cargo add dbmd-core` and get the
//! full library — the same shape as ripgrep, where the `grep`/`ignore` libs do
//! the work and `rg` is a thin CLI.
//!
//! # Hard invariants this crate is built to uphold
//!
//! - **Zero AI/LLM dependencies.** No provider SDKs, no API keys, no model
//!   calls, no embeddings, no vectors, no ANN — anywhere, ever. The agent
//!   driving `dbmd` is the semantic layer; `dbmd` is a deterministic tool.
//! - **The interactive loop is O(changed), never O(store).** Loop ops
//!   ([`graph::backlinks`], [`validate::validate_working_set`],
//!   [`index::Index::on_write`], …) never call [`store::Store::walk`] on a
//!   non-empty changed set. The one documented exception is
//!   [`validate::validate_working_set`], which falls back to a full sweep only
//!   when handed an empty changed set (the vacuous-pass guard). Whole-store
//!   walks otherwise belong only to SWEEP ops ([`validate::validate_all`],
//!   [`index::Index::rebuild_all`], [`stats`]).
//! - **Wiki-links are full store-relative paths.** A short-form wiki-link is a
//!   validation error ([`validate`] code `WIKI_LINK_SHORT_FORM`).
//! - **Embedded ripgrep.** Free-text body search uses the `grep` + `ignore`
//!   crates in-process; the toolkit never bundles or shells out to `rg`.
//!   Structured loop reads ([`graph::backlinks`], [`query::Query`]) ride the
//!   `index.jsonl` sidecars instead, never a frontmatter tree scan.

pub mod assets;
pub mod extract;
pub mod fsx;
pub mod graph;
pub mod index;
pub mod log;
pub mod parser;
pub mod query;
pub mod render;
pub mod stats;
pub mod store;
pub mod summary;
pub mod time;
pub mod validate;

// ── Shared public types, re-exported at the crate root ──────────────────────
//
// These are the locked interface every other crate and module builds against.

pub use assets::{AssetRecord, Declaration, ScanReport, StatusReport, VerifyReport};
pub use extract::{ExtractError, Extracted, Format, MetaValue};
pub use fsx::{write_atomic, write_atomic_new};
pub use graph::ContextSlice;
pub use index::{Index, IndexLevel, IndexRecord};
pub use log::{Log, LogEntry, LogKind};
pub use parser::{
    Config, FieldSpec, Frontmatter, MarkdownLink, ParseError, Schema, Section, Shape, WikiLink,
};
pub use query::Query;
pub use render::{Outline, Tree};
pub use store::{infer_type_from_path, layer_for_type, Layer, NotAStore, Store, StoreError};
pub use time::now;
pub use validate::{Issue, Severity};

/// Crate-wide result alias over [`Error`].
pub type Result<T> = std::result::Result<T, Error>;

/// Top-level error for `dbmd-core` operations.
///
/// Module-specific errors ([`ParseError`], [`StoreError`], [`NotAStore`])
/// convert into this so a CLI command can bubble a single error type while
/// preserving the structured variant for `--json` rendering.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The path is not a db.md store (no `DB.md` at the root). Surfaced as the
    /// machine-parseable code `NOT_A_STORE` with a non-zero exit.
    #[error(transparent)]
    NotAStore(#[from] NotAStore),

    /// A store-level operation failed (walk, locate, shard, sidecar read).
    #[error(transparent)]
    Store(#[from] StoreError),

    /// A markdown / frontmatter / `DB.md` parse failed.
    #[error(transparent)]
    Parse(#[from] ParseError),

    /// A write was refused by a `DB.md ## Policies` rule (e.g. a frozen page).
    /// Carries the structured validation code so the CLI can emit it verbatim.
    #[error("write refused by policy ({code}): {message}")]
    Policy {
        /// The structured issue code, e.g. `"POLICY_FROZEN_PAGE"`.
        code: &'static str,
        /// Human-readable explanation.
        message: String,
    },

    /// An underlying I/O failure.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
