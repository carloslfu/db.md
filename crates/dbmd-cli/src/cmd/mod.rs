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

pub mod api;
pub mod ask;
pub mod assets;
pub mod body;
pub mod emit;
pub mod extract;
mod file_target;
pub mod fm;
pub mod format;
pub mod grant;
pub mod graph;
mod httpd;
pub mod index;
pub mod install_verified;
pub mod key;
pub mod link;
pub mod log;
pub mod mirror;
pub mod outline;
mod projection;
pub mod proposal;
pub mod propose;
pub mod query;
pub mod rename;
pub mod resolve;
pub mod rm;
pub mod schema;
pub mod search;
pub mod section;
pub mod sections;
pub mod serve;
pub mod show;
pub mod spec;
pub mod stats;
pub mod subscribe;
pub mod sync;
pub mod tree;
pub mod validate;
pub mod watch;
pub mod write;
