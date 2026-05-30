//! The global execution context — flags that apply to every subcommand.
//!
//! Parsed once from the top-level [`crate::cli::Cli`] and threaded into every
//! `cmd::<name>::run(ctx, args)` call. Subcommand bodies read `ctx.json` to
//! decide between human text and structured JSON, and `ctx.color` to decide
//! whether to colorize (default: never, for pipe-safe output).
//!
//! This is the seam the subcommand-body agents build against: they get the
//! global flags here and their own parsed args struct, and never touch
//! `main.rs` or the dispatch.

/// When to emit ANSI color. Default is [`ColorChoice::Auto`], but `dbmd`
/// treats `Auto` as **never** unless the agent explicitly opts in with
/// `--color=always`: agent-primary output is pipe-clean by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum ColorChoice {
    /// Color only when explicitly requested. `dbmd` never auto-detects a TTY —
    /// the default is effectively "off" so piped/captured output stays clean.
    #[default]
    Auto,
    /// Always emit ANSI color.
    Always,
    /// Never emit ANSI color.
    Never,
}

impl ColorChoice {
    /// Resolve whether color should actually be emitted. `Auto` resolves to
    /// `false` (no TTY sniffing — pipe-safe by default for agents); only an
    /// explicit `--color=always` turns color on.
    pub fn enabled(self) -> bool {
        matches!(self, ColorChoice::Always)
    }
}

/// Global, subcommand-agnostic execution context.
///
/// Cheap to clone (two flags). Passed by reference into every subcommand body.
#[derive(Debug, Clone)]
pub struct Context {
    /// `--json`: emit machine-parseable JSON instead of human text. Every
    /// subcommand honors this; errors are rendered as `{"error": {...}}` to
    /// stderr (see [`crate::error::CliError::to_json`]).
    pub json: bool,
    /// `--color`: when to colorize human output. Never colorizes JSON.
    pub color: ColorChoice,
}

impl Context {
    /// True when ANSI color should be emitted for human (non-JSON) output.
    /// Always false in `--json` mode regardless of `--color`.
    pub fn use_color(&self) -> bool {
        !self.json && self.color.enabled()
    }
}
