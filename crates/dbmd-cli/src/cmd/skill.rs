//! `dbmd install-skill` / `dbmd uninstall-skill` — manage the cross-agent
//! Agent Skill that teaches a local coding agent to operate a db.md store with
//! `dbmd`.
//!
//! **One source, every agent.** The skill is authored once at
//! `skills/db-md/SKILL.md` in the repo, embedded into the binary, and dropped
//! into each agent's skills directory in the open [Agent Skills](https://agentskills.io)
//! format (`<skills-dir>/db-md/SKILL.md` with `name`/`description` frontmatter,
//! model-invoked via progressive disclosure). The same file works for every
//! agent — only the install directory differs:
//!   - Claude Code → `~/.claude/skills/db-md/SKILL.md`
//!   - Codex       → `~/.codex/skills/db-md/SKILL.md`
//!
//! The skill body is a thin pointer at `dbmd spec` (the single source of truth)
//! plus a phase-grouped cheat sheet; the full SPEC is never inlined, so the
//! installed skill cannot drift from the standard the binary already carries.
//!
//! This is the persistent half of the "agent way", the sibling of `dbmd spec`:
//! where `dbmd spec` loads the contract into a harness's system prompt for one
//! session, `install-skill` drops a skill the agent discovers on every future
//! session — and `uninstall-skill` removes exactly what `install-skill` wrote.
//! With no `--target`, both verbs act on every agent detected on the machine
//! (one command points every agent); `--target` narrows to one.

use std::path::{Path, PathBuf};

use crate::cli::{SkillArgs, SkillTarget};
use crate::context::Context;
use crate::error::{CliError, CliResult, ExitCode};

const MANAGED_MARKER: &str = "dbmd-managed-skill:v1";

/// The canonical Agent Skill, embedded so the binary is the single source.
/// Same content for every agent — the open format is portable; only the install
/// directory differs. The source of truth is the repo-root `skills/db-md/SKILL.md`;
/// `crates/dbmd-cli/skills/db-md/SKILL.md` is its in-crate mirror (so the path
/// stays inside the crate for `cargo package`), kept identical by `make sync`
/// and guarded by the `bundled_assets_match_repo_root` test.
const SKILL_BODY: &str = include_str!("../../skills/db-md/SKILL.md");

/// Run `dbmd install-skill`: write the embedded skill into each target agent's
/// skills directory. With no `--target`, every detected agent is pointed in one
/// command. The managed-marker check is a pre-flight across all targets, so an
/// unmanaged file at any path refuses the whole command before anything writes.
pub fn install(ctx: &Context, args: &SkillArgs) -> CliResult {
    let home = home_dir()?;
    let planned = plan(args.target, &home);

    for (_, _, file) in &planned {
        ensure_installable(file)?;
    }

    let mut outcomes = Vec::with_capacity(planned.len());
    for (target, dir, file) in planned {
        std::fs::create_dir_all(&dir).map_err(|e| io_err("creating skill directory", &dir, e))?;
        std::fs::write(&file, SKILL_BODY).map_err(|e| io_err("writing skill", &file, e))?;
        outcomes.push((target, file, "installed"));
    }
    emit(ctx, &outcomes);
    Ok(())
}

/// Run `dbmd uninstall-skill`: remove only what `install-skill` wrote, for each
/// target. A missing skill is a clean `noop`. The managed-marker check is a
/// pre-flight, so an unmanaged file at any path refuses the whole command.
pub fn uninstall(ctx: &Context, args: &SkillArgs) -> CliResult {
    let home = home_dir()?;
    let planned = plan(args.target, &home);

    for (_, _, file) in &planned {
        if file.exists() {
            ensure_managed(file, "uninstall")?;
        }
    }

    let mut outcomes = Vec::with_capacity(planned.len());
    for (target, dir, file) in planned {
        if !file.exists() {
            outcomes.push((target, file, "noop"));
            continue;
        }
        remove_skill(&dir, &file).map_err(|e| io_err("removing skill", &file, e))?;
        outcomes.push((target, file, "uninstalled"));
    }
    emit(ctx, &outcomes);
    Ok(())
}

/// The `(target, skill_dir, skill_file)` list a verb will act on.
fn plan(explicit: Option<SkillTarget>, home: &Path) -> Vec<(SkillTarget, PathBuf, PathBuf)> {
    resolve_targets(explicit, home)
        .into_iter()
        .map(|t| {
            let (dir, file) = paths_for(t, home);
            (t, dir, file)
        })
        .collect()
}

/// Remove the managed skill file, then its `db-md/` dir if it became empty —
/// leaving any user-created siblings in that directory untouched.
fn remove_skill(dir: &Path, file: &Path) -> std::io::Result<()> {
    std::fs::remove_file(file)?;
    match std::fs::remove_dir(dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::DirectoryNotEmpty => Ok(()),
        Err(e) => Err(e),
    }
}

fn ensure_installable(file: &Path) -> CliResult {
    if !file.exists() {
        return Ok(());
    }
    ensure_managed(file, "install")
}

fn ensure_managed(file: &Path, action: &str) -> CliResult {
    let body = std::fs::read_to_string(file).map_err(|e| io_err("reading skill", file, e))?;
    if body.contains(MANAGED_MARKER) {
        return Ok(());
    }
    Err(CliError::new(
        ExitCode::Collision,
        "SKILL_NOT_MANAGED",
        format!(
            "refusing to {action} unmanaged db.md skill file at {}",
            file.display()
        ),
    )
    .with_hint("move the file aside, or remove it yourself if it is not managed by dbmd"))
}

/// Resolve which agents to act on: an explicit `--target` wins; otherwise act on
/// every agent whose config dir exists. When none is present, default to Claude
/// Code — creating the dir is harmless and the skill waits for the next session.
fn resolve_targets(explicit: Option<SkillTarget>, home: &Path) -> Vec<SkillTarget> {
    if let Some(t) = explicit {
        return vec![t];
    }
    let mut targets = Vec::new();
    if home.join(".claude").is_dir() {
        targets.push(SkillTarget::ClaudeCode);
    }
    if home.join(".codex").is_dir() {
        targets.push(SkillTarget::Codex);
    }
    if targets.is_empty() {
        targets.push(SkillTarget::ClaudeCode);
    }
    targets
}

/// The `(skill_dir, skill_file)` for a target. Every agent uses the open Agent
/// Skills layout — `<skills-root>/db-md/SKILL.md` — so only the per-agent skills
/// root differs.
fn paths_for(target: SkillTarget, home: &Path) -> (PathBuf, PathBuf) {
    let skills_root = match target {
        SkillTarget::ClaudeCode => home.join(".claude").join("skills"),
        SkillTarget::Codex => home.join(".codex").join("skills"),
    };
    let dir = skills_root.join("db-md");
    let file = dir.join("SKILL.md");
    (dir, file)
}

/// Resolve `$HOME`, the anchor for every agent config dir. A missing/empty
/// `$HOME` is a runtime error rather than a silent no-op so the calling agent
/// gets a clear, machine-parseable failure.
fn home_dir() -> Result<PathBuf, CliError> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| {
            CliError::new(
                ExitCode::Runtime,
                "NO_HOME",
                "cannot resolve the home directory ($HOME is unset)",
            )
            .with_hint("set $HOME so the skill can be written under ~/.claude or ~/.codex")
        })
}

/// One IO-error constructor so every filesystem failure carries the same
/// `IO_ERROR` code and a path-bearing message.
fn io_err(action: &str, path: &Path, e: std::io::Error) -> CliError {
    CliError::new(
        ExitCode::Runtime,
        "IO_ERROR",
        format!("{action} {}: {e}", path.display()),
    )
}

/// Emit the outcomes: under `--json`, an array of `{target, path, action}` (one
/// per agent acted on, always an array so a caller parses one shape); otherwise
/// a friendly line per agent.
fn emit(ctx: &Context, outcomes: &[(SkillTarget, PathBuf, &str)]) {
    if ctx.json {
        let arr: Vec<serde_json::Value> = outcomes
            .iter()
            .map(|(target, file, action)| {
                serde_json::json!({
                    "target": target.as_str(),
                    "path": file.display().to_string(),
                    "action": *action,
                })
            })
            .collect();
        println!("{}", serde_json::Value::Array(arr));
        return;
    }
    for (target, file, action) in outcomes {
        let target = target.as_str();
        let path = file.display();
        match *action {
            "installed" => println!(
                "Installed the db.md skill for {target} at {path}.\n  \
                 Start a new {target} session and it will know how to operate a db.md store with `dbmd`."
            ),
            "uninstalled" => println!("Removed the db.md skill for {target} ({path})."),
            "noop" => println!("No db.md skill installed for {target} ({path}); nothing to remove."),
            other => println!("{other}: {target} ({path})"),
        }
    }
}
