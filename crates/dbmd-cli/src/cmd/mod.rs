//! Subcommand bodies — one module per top-level `dbmd` subcommand.
//!
//! Each module exposes a single entry point:
//!
//! ```ignore
//! pub fn run(ctx: &Context, args: &SomeArgs) -> CliResult
//! ```
//!
//! where `ctx` carries the global flags (`--json`, `--color`) and `args` is the
//! subcommand's parsed clap struct from [`crate::cli`]. The dispatch in
//! `main.rs` calls exactly these `run` functions; **adding logic means editing
//! only the relevant `cmd/<name>.rs`, never `main.rs` or `cli.rs`.** That is
//! the seam that lets the subcommand-body agents work in parallel.
//!
//! Each body is a thin wrapper: it parses `args`, calls into `dbmd-core`, and
//! formats output (text by default, JSON under `ctx.json`). All real logic
//! lives in `dbmd-core` — these modules only translate between the parsed clap
//! struct and the library, then render. (The `64` / `not_implemented` path is
//! retained in [`crate::error`] as a reserved contract code, but no body
//! returns it.)

pub mod extract;
pub mod fm;
pub mod format;
pub mod graph;
pub mod index;
pub mod link;
pub mod links;
pub mod log;
pub mod outline;
pub mod query;
pub mod rename;
pub mod search;
pub mod sections;
pub mod spec;
pub mod stats;
pub mod tree;
pub mod validate;
pub mod write;
