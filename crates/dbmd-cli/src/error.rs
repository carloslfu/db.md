//! CLI error type + the documented **exit-code convention**.
//!
//! `dbmd` is an agent-primary tool: every failure is machine-parseable. A
//! command returns a [`CliError`]; [`crate::main`] maps it to a stable exit
//! code (see [`ExitCode`]) and, under `--json`, prints a structured
//! `{"error": {...}}` object to stderr so the calling agent can branch on
//! `code` without scraping prose.
//!
//! # Exit codes (stable contract)
//!
//! | Code | Meaning                       | Example                                   |
//! |------|-------------------------------|-------------------------------------------|
//! | `0`  | success                       | command ran, no problems                  |
//! | `1`  | runtime error                 | I/O failure, parse failure, file missing  |
//! | `2`  | usage error                   | bad flags / args (emitted by `clap`)      |
//! | `3`  | not a db.md store             | no `DB.md` at the resolved root           |
//! | `4`  | policy refusal                | write blocked by a `DB.md ## Policies` rule |
//! | `5`  | collision / conflict          | `dbmd write` onto an existing path        |
//! | `6`  | validation found issues       | `dbmd validate` reported errors           |
//! | `64` | not yet implemented           | reserved; no current body returns it      |
//!
//! These codes are part of the tool's interface; do not renumber them. New
//! failure classes get a new code, never a reuse. `clap` owns exit code `2`
//! for argument-parsing failures — handlers here never return it.

use std::fmt;

/// Stable process exit codes. The numeric values are a public contract; see
/// the module docs. `clap` emits `2` for arg-parse errors on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum ExitCode {
    /// Everything succeeded.
    Success = 0,
    /// A runtime error: I/O, parse, missing file, or any uncategorized failure.
    Runtime = 1,
    /// Bad invocation (flags / args). Reserved for `clap`; handlers don't use it.
    Usage = 2,
    /// The resolved path is not a db.md store (no `DB.md` at the root).
    NotAStore = 3,
    /// A write was refused by a `DB.md ## Policies` rule (e.g. a frozen page).
    Policy = 4,
    /// A path / entity collision (e.g. `dbmd write` onto an existing file).
    Collision = 5,
    /// `dbmd validate` completed but reported one or more errors.
    ValidationFailed = 6,
    /// A subcommand body not yet implemented. Reserved: every current body is
    /// implemented, so nothing returns this today, but the code stays allocated
    /// so a future not-yet-built subcommand has a stable, unambiguous exit code.
    NotImplemented = 64,
}

impl ExitCode {
    /// The raw integer this code maps to for `std::process::exit`.
    pub fn code(self) -> i32 {
        self as i32
    }
}

/// A short, stable machine code string used in `--json` error output and in
/// human messages. Kept distinct from [`ExitCode`] so several string codes can
/// share one exit code (e.g. several policy codes all exit `4`).
///
/// `dbmd-core` already defines the canonical write-path codes (`NOT_A_STORE`,
/// `POLICY_FROZEN_PAGE`, …); this mirrors them at the CLI boundary.
#[derive(Debug, Clone)]
pub struct CliError {
    /// The exit code this error maps to.
    pub exit: ExitCode,
    /// A stable machine-parseable code string, e.g. `"NOT_A_STORE"`,
    /// `"NOT_IMPLEMENTED"`, `"IO_ERROR"`. Surfaced verbatim in `--json`.
    pub code: &'static str,
    /// Human-readable, single-line explanation.
    pub message: String,
    /// Optional remediation hint (a command to run, a path to fix).
    pub hint: Option<String>,
}

impl CliError {
    /// Construct an error with an explicit exit code + machine code.
    pub fn new(exit: ExitCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            exit,
            code,
            message: message.into(),
            hint: None,
        }
    }

    /// Attach a remediation hint (chainable).
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    /// The canonical "this subcommand is not built yet" error. No current body
    /// returns it — every subcommand is implemented — but it is kept as the
    /// reserved constructor for the `64` contract code so a future not-yet-built
    /// subcommand (and tests) can signal an unimplemented path unambiguously.
    pub fn not_implemented(subcommand: &str) -> Self {
        Self::new(
            ExitCode::NotImplemented,
            "NOT_IMPLEMENTED",
            format!("`dbmd {subcommand}` is not implemented yet"),
        )
        .with_hint("this subcommand is recognized but its body is not implemented in this build")
    }

    /// A generic runtime error (exit `1`, code `RUNTIME_ERROR`).
    pub fn runtime(message: impl Into<String>) -> Self {
        Self::new(ExitCode::Runtime, "RUNTIME_ERROR", message)
    }

    /// Render this error as a structured JSON object for `--json` mode. Shape:
    /// `{"error": {"code": "...", "message": "...", "hint": "..."}}`.
    pub fn to_json(&self) -> serde_json::Value {
        let mut obj = serde_json::Map::new();
        obj.insert(
            "code".to_string(),
            serde_json::Value::String(self.code.to_string()),
        );
        obj.insert(
            "message".to_string(),
            serde_json::Value::String(self.message.clone()),
        );
        if let Some(hint) = &self.hint {
            obj.insert("hint".to_string(), serde_json::Value::String(hint.clone()));
        }
        serde_json::json!({ "error": serde_json::Value::Object(obj) })
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)?;
        if let Some(hint) = &self.hint {
            write!(f, "\n  hint: {hint}")?;
        }
        Ok(())
    }
}

impl std::error::Error for CliError {}

/// Map a `dbmd_core::Error` onto a [`CliError`] with the right exit code +
/// machine code. This is the single conversion point so every subcommand that
/// bubbles a core error gets consistent exit semantics.
impl From<dbmd_core::Error> for CliError {
    fn from(err: dbmd_core::Error) -> Self {
        match err {
            dbmd_core::Error::NotAStore(_) => {
                CliError::new(ExitCode::NotAStore, "NOT_A_STORE", err.to_string())
                    .with_hint("run `dbmd` from inside a db.md store, or pass the store path")
            }
            dbmd_core::Error::Policy { code, message } => {
                CliError::new(ExitCode::Policy, code, message)
            }
            dbmd_core::Error::Store(_) => {
                CliError::new(ExitCode::Runtime, "STORE_ERROR", err.to_string())
            }
            dbmd_core::Error::Parse(_) => {
                CliError::new(ExitCode::Runtime, "PARSE_ERROR", err.to_string())
            }
            dbmd_core::Error::Io(_) => {
                CliError::new(ExitCode::Runtime, "IO_ERROR", err.to_string())
            }
        }
    }
}

impl From<std::io::Error> for CliError {
    fn from(err: std::io::Error) -> Self {
        CliError::new(ExitCode::Runtime, "IO_ERROR", err.to_string())
    }
}

/// Map a link.md client error onto a [`CliError`]: every wire-or-config
/// failure is a `Runtime` (exit `1`) with a stable machine code — an agent
/// branches on the string code, not new exit numbers (the numeric table is a
/// locked contract and the link verbs add no new class to it).
impl From<dbmd_core::linkmd::LinkError> for CliError {
    fn from(err: dbmd_core::linkmd::LinkError) -> Self {
        use dbmd_core::linkmd::LinkError as L;
        let message = err.to_string();
        match err {
            L::NoHub => CliError::new(ExitCode::Runtime, "NO_HUB", message),
            L::NoCredential => CliError::new(ExitCode::Runtime, "NO_CREDENTIAL", message),
            L::BadKey => CliError::new(ExitCode::Runtime, "BAD_CREDENTIAL", message),
            L::UnsafeHub { .. } => CliError::new(ExitCode::Runtime, "HUB_NOT_HTTPS", message),
            L::Transport { .. } => CliError::new(ExitCode::Runtime, "HUB_UNREACHABLE", message),
            L::Http { code, .. } => {
                let e = CliError::new(ExitCode::Runtime, "HUB_ERROR", message);
                match code {
                    Some(c) => e.with_hint(format!("hub error code: {c}")),
                    None => e,
                }
            }
            L::NotJson { .. } => CliError::new(ExitCode::Runtime, "HUB_NOT_JSON", message),
            L::ResponseTooLarge => CliError::new(ExitCode::Runtime, "RESPONSE_TOO_LARGE", message),
            L::BadAddress { .. } => CliError::new(ExitCode::Runtime, "BAD_ADDRESS", message)
                .with_hint(
                    "addresses are `@brain`, `@brain/<record-id>`, or `@brain/<store-path>.md`",
                ),
            L::BadGrantId { .. } => CliError::new(ExitCode::Runtime, "BAD_GRANT_ID", message)
                .with_hint("copy the id from `dbmd grant list <brain>`"),
            L::UnsafePath { .. } => CliError::new(ExitCode::Runtime, "UNSAFE_PATH", message),
            L::PushTooLarge { .. } => CliError::new(ExitCode::Runtime, "PUSH_TOO_LARGE", message),
            L::ProposeTooLarge { .. } => {
                CliError::new(ExitCode::Runtime, "PROPOSE_TOO_LARGE", message)
            }
            L::NotUtf8 { .. } => CliError::new(ExitCode::Runtime, "NOT_UTF8", message),
            L::InvalidPack { .. } => CliError::new(ExitCode::Runtime, "INVALID_PACK", message),
            L::InvalidFeed { .. } => CliError::new(ExitCode::Runtime, "INVALID_FEED", message),
            L::Io(_) => CliError::new(ExitCode::Runtime, "IO_ERROR", message),
            L::Store(_) => CliError::new(ExitCode::Runtime, "STORE_ERROR", message),
        }
    }
}

/// Convenience result alias for subcommand bodies.
pub type CliResult = std::result::Result<(), CliError>;
