//! `dbmd install-skill` / `dbmd uninstall-skill` — manage the coding-agent skill
//! that teaches a local agent how to operate a db.md store with `dbmd`.
//!
//! These are the persistent half of the "agent way", the sibling of `dbmd spec`:
//! where `dbmd spec` loads the full contract into a harness's system prompt for
//! one session, `install-skill` drops a skill into a local coding agent (Claude
//! Code or Codex) so the agent knows `dbmd` on every future session — and
//! `uninstall-skill` cleanly removes exactly what `install-skill` wrote.
//!
//! The skill body is a thin pointer at `dbmd spec` (the single source of truth)
//! plus a phase-grouped cheat sheet; the full SPEC is never inlined, so the
//! installed skill cannot drift from the standard the binary already carries.
//!
//! Layout mirrors the `computer.md` reference CLI's `install-skill` /
//! `uninstall-skill`, so the two standards present the same shape to an agent:
//!   - Claude Code → `~/.claude/skills/db-md/SKILL.md` (the `name`+`description`
//!     frontmatter Claude Code auto-discovers it by).
//!   - Codex       → `~/.codex/instructions/db-md.md` (the exact path db.md's
//!     bootstrap docs already use; consumed by Codex's instructions-on-startup).

use std::path::{Path, PathBuf};

use crate::cli::{SkillArgs, SkillTarget};
use crate::context::Context;
use crate::error::{CliError, CliResult, ExitCode};

const MANAGED_MARKER: &str = "dbmd-managed-skill:v1";

/// Run `dbmd install-skill`: create the skill directory (if needed) and write
/// the skill file for the resolved target.
pub fn install(ctx: &Context, args: &SkillArgs) -> CliResult {
    let home = home_dir()?;
    let target = resolve_target(args.target, &home);
    let (dir, file) = paths_for(target, &home);

    std::fs::create_dir_all(&dir).map_err(|e| io_err("creating skill directory", &dir, e))?;
    let body = match target {
        SkillTarget::ClaudeCode => CLAUDE_CODE_SKILL,
        SkillTarget::Codex => CODEX_SKILL,
    };
    ensure_installable(&file)?;
    std::fs::write(&file, body).map_err(|e| io_err("writing skill", &file, e))?;
    emit(ctx, target, &file, "installed");
    Ok(())
}

/// Run `dbmd uninstall-skill`: remove only what `install-skill` wrote. A missing
/// skill is a clean `noop`, not an error.
pub fn uninstall(ctx: &Context, args: &SkillArgs) -> CliResult {
    let home = home_dir()?;
    let target = resolve_target(args.target, &home);
    let (dir, file) = paths_for(target, &home);

    if !file.exists() {
        emit(ctx, target, &file, "noop");
        return Ok(());
    }
    ensure_managed(&file, "uninstall")?;

    // Remove only what this command owns: the managed skill file. For Claude
    // Code, also remove the `db-md/` skill dir only if it becomes empty; user
    // siblings in that directory are left untouched.
    let removed = match target {
        SkillTarget::ClaudeCode => {
            std::fs::remove_file(&file).and_then(|()| match std::fs::remove_dir(&dir) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::DirectoryNotEmpty => Ok(()),
                Err(e) => Err(e),
            })
        }
        SkillTarget::Codex => std::fs::remove_file(&file),
    };
    removed.map_err(|e| io_err("removing skill", &file, e))?;
    emit(ctx, target, &file, "uninstalled");
    Ok(())
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

/// Resolve the target: an explicit `--target` wins; otherwise autodetect by
/// which agent config dir exists (Claude Code preferred), defaulting to Claude
/// Code when neither is present — creating the dir is harmless and the user
/// sees the skill on their next session.
fn resolve_target(explicit: Option<SkillTarget>, home: &Path) -> SkillTarget {
    if let Some(t) = explicit {
        return t;
    }
    if home.join(".claude").is_dir() {
        return SkillTarget::ClaudeCode;
    }
    if home.join(".codex").is_dir() {
        return SkillTarget::Codex;
    }
    SkillTarget::ClaudeCode
}

/// The `(skill_dir, skill_file)` for a target. Claude Code owns a whole
/// `db-md/` skill directory; Codex drops a single file into the shared
/// `instructions/` directory.
fn paths_for(target: SkillTarget, home: &Path) -> (PathBuf, PathBuf) {
    match target {
        SkillTarget::ClaudeCode => {
            let dir = home.join(".claude").join("skills").join("db-md");
            let file = dir.join("SKILL.md");
            (dir, file)
        }
        SkillTarget::Codex => {
            let dir = home.join(".codex").join("instructions");
            let file = dir.join("db-md.md");
            (dir, file)
        }
    }
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

/// Emit the outcome: a `{target, path, action}` object under `--json`, or a
/// friendly line otherwise.
fn emit(ctx: &Context, target: SkillTarget, file: &Path, action: &str) {
    let target = target.as_str();
    let path = file.display();
    if ctx.json {
        let out = serde_json::json!({
            "target": target,
            "path": path.to_string(),
            "action": action,
        });
        println!("{out}");
        return;
    }
    match action {
        "installed" => println!(
            "Installed the db.md skill for {target} at {path}.\n\
             Start a new {target} session and it will know how to operate a db.md store with `dbmd`."
        ),
        "uninstalled" => println!("Removed the db.md skill for {target} ({path})."),
        "noop" => println!("No db.md skill installed for {target} ({path}); nothing to remove."),
        other => println!("{other}: {target} ({path})"),
    }
}

/// The Claude Code skill (`~/.claude/skills/db-md/SKILL.md`). The `name` +
/// `description` frontmatter is what Claude Code reads to auto-discover and
/// trigger the skill; the body points at `dbmd spec` for the full contract.
const CLAUDE_CODE_SKILL: &str = r##"---
name: db-md
description: Operate a db.md store — the open database in plain files — with the `dbmd` CLI. Use when reading, writing, searching, validating, or curating a folder that has a DB.md at its root. Run `dbmd spec` for the full contract.
---

<!-- dbmd-managed-skill:v1 -->

# db.md (the `dbmd` CLI)

You have the `dbmd` binary on PATH. It operates a **db.md store**: a database
that is a plain directory — raw evidence in `sources/`, atomic typed data in
`records/`, curator-synthesized narrative in `wiki/`, all governed by a single
`DB.md` at the root. `dbmd` is deterministic file/data plumbing; **you are the
curator** — the reasoning, synthesis, and judgment are yours.

**Before anything else: load the contract once per session.**

```
dbmd spec            # prints the canonical SPEC — the curator contract
```

Then read the store's own `DB.md` for its identity, policies, and schemas;
`DB.md` overrides defaults, so read it before you write.

## Cheat sheet (grouped by session phase)

```
# Open — load the standard, then this store's rules
dbmd spec                                          # the contract (once per session)
dbmd fm get DB.md scope                             # this store's identity / policies / schemas

# Warm up — orient
dbmd tree                                           # the directory at a glance
dbmd stats                                          # counts, sizes, orphans, top types
dbmd index show                                     # the curated root catalog

# Read — find and hydrate context (every command takes --json)
dbmd search "(renewal|contract|ARR)" --in records   # ripgrep; the regex IS your query expansion (no embeddings)
dbmd query --type contact --where company=Acme       # structured frontmatter query via the sidecar
dbmd graph neighborhood records/contacts/sarah-chen --hops 2   # context in one call
dbmd links records/contacts/sarah-chen               # who points here (blast radius)

# Write — create and connect (frontmatter is composed for you)
dbmd write records/meetings/standup.md --type meeting --summary "weekly sync"
dbmd fm set <file> <key>=<value>                     # update one field, atomically
dbmd link <from> <to>                                # append a wiki-link

# Validate — before you close
dbmd validate                                        # the working set (changed files)
dbmd validate --all                                  # full-store sweep

# Maintain / close — record what you did
dbmd index rebuild                                   # repair the catalog if needed
dbmd log <kind> <object> -m "<note>"                 # append to the store timeline
```

## Output contract (memorize)

```
--json on every command   # machine-parseable; errors print {"error":{code,message,hint}} on stderr
exit: 0 ok · 1 runtime · 2 usage · 3 not-a-store · 4 policy refusal · 5 collision · 6 validation-failed
```

The full, authoritative reference is always `dbmd spec`. This skill is a pointer,
not a copy — when in doubt, run `dbmd spec` and read the store's `DB.md`.
"##;

/// The Codex skill (`~/.codex/instructions/db-md.md`) — same content, no
/// frontmatter (Codex consumes plain instructions files at startup). The path
/// matches db.md's existing `dbmd spec >> ~/.codex/instructions/db-md.md`
/// bootstrap convention.
const CODEX_SKILL: &str = r##"<!-- dbmd-managed-skill:v1 -->

# db.md (the `dbmd` CLI)

You have the `dbmd` binary on PATH. It operates a **db.md store**: a database
that is a plain directory — `sources/` (raw evidence), `records/` (atomic typed
data), `wiki/` (curator synthesis), governed by a single `DB.md` at the root.
`dbmd` is deterministic file/data plumbing; you are the curator.

Load the contract once per session, then read the store's `DB.md`:

```
dbmd spec            # the canonical SPEC — the curator contract
```

Most-used commands (every command takes `--json`):

```
# Orient
dbmd tree · dbmd stats · dbmd index show

# Read
dbmd search "(renewal|contract)" --in records
dbmd query --type contact --where company=Acme
dbmd graph neighborhood <seed> --hops 2
dbmd links <file>

# Write (frontmatter composed for you)
dbmd write <path> --type <t> --summary "<s>"
dbmd fm set <file> <key>=<value>
dbmd link <from> <to>

# Validate / record
dbmd validate           # working set
dbmd validate --all     # full sweep
dbmd log <kind> <object> -m "<note>"
```

Exit codes: 0 ok · 1 runtime · 2 usage · 3 not-a-store · 4 policy · 5 collision ·
6 validation-failed. Errors print `{"error":{...}}` on stderr under `--json`.
The full reference is always `dbmd spec`.
"##;
