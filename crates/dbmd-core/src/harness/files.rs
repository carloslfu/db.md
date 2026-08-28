// SPDX-License-Identifier: Apache-2.0

//! Workspace file operations for the `build` mask — the app-player tier.
//!
//! The boundary is the **declared workspace closure**: a root directory the
//! user names (`--workspace`, `DBMD_WORKSPACE`, or `workspace = <path>` in
//! `.dbmd/config`, resolved relative to the store root). Every operation is
//! confined beneath that root:
//!
//! - requested paths must be relative, with no `..` components;
//! - existing path components must not be symlinks (no link-out escape);
//! - the store's own subtree is refused — store files are managed through
//!   the store tools, never raw file writes (the contract must not be
//!   bypassable from inside the harness);
//! - there is no shell: writing source is as far as the build mask goes, and
//!   the running dev server (or the BYO agent) does any executing.

use std::path::{Component, Path, PathBuf};

use super::{truncate_tool_result, ToolOutcome};

/// One workspace file operation, as planned by [`super::tools::plan`].
#[derive(Debug, Clone)]
pub enum FileOp {
    /// List files beneath a workspace-relative directory.
    List {
        /// Workspace-relative directory (`None` = the workspace root).
        dir: Option<String>,
    },
    /// Read one file.
    Read {
        /// Workspace-relative path.
        path: String,
    },
    /// Create or replace one file.
    Write {
        /// Workspace-relative path.
        path: String,
        /// Full new content.
        content: String,
    },
    /// Exact-string replacement (must match exactly once).
    Edit {
        /// Workspace-relative path.
        path: String,
        /// Text to replace.
        old_text: String,
        /// Replacement.
        new_text: String,
    },
}

impl FileOp {
    /// Human-readable one-liner for the event stream.
    pub fn display(&self) -> String {
        match self {
            FileOp::List { dir } => format!("list_files {}", dir.as_deref().unwrap_or(".")),
            FileOp::Read { path } => format!("read_file {path}"),
            FileOp::Write { path, content } => {
                format!("write_file {path} ({} bytes)", content.len())
            }
            FileOp::Edit { path, .. } => format!("edit_file {path}"),
        }
    }
}

/// Directories skipped by `list_files` (build outputs and dependency trees —
/// noise the model never needs to walk).
const SKIP_DIRS: [&str; 6] = [".git", "node_modules", "target", "dist", ".dbmd", ".pi"];

/// Max content a `write_file` accepts.
const MAX_WRITE_BYTES: usize = 512 * 1024;
/// Max file size `read_file` returns before head+tail truncation.
const MAX_LIST_ENTRIES: usize = 500;

fn refuse(message: impl Into<String>) -> ToolOutcome {
    ToolOutcome {
        content: message.into(),
        is_error: true,
    }
}

/// Resolve one workspace-relative path safely beneath `workspace`.
/// Refuses absolute paths, `..`, and any existing symlinked component.
fn confine(workspace: &Path, raw: &str) -> Result<PathBuf, String> {
    let requested = Path::new(raw);
    if requested.is_absolute() {
        return Err(format!(
            "`{raw}` is absolute — paths are workspace-relative"
        ));
    }
    let mut clean = PathBuf::new();
    for component in requested.components() {
        match component {
            Component::Normal(part) => clean.push(part),
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(format!("`{raw}` leaves the workspace (`..` refused)"));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(format!("`{raw}` is not workspace-relative"));
            }
        }
    }
    // Walk the existing prefix and refuse symlinked components, so a link
    // inside the workspace cannot smuggle operations outside it.
    let mut probe = workspace.to_path_buf();
    for component in clean.components() {
        probe.push(component);
        match std::fs::symlink_metadata(&probe) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(format!(
                    "`{raw}` crosses a symlink ({}) — refused",
                    probe.display()
                ));
            }
            _ => {}
        }
    }
    Ok(workspace.join(clean))
}

/// Whether `target` sits inside the store subtree (which file tools refuse).
fn inside_store(target: &Path, store_root: &Path) -> bool {
    target.starts_with(store_root)
}

/// Execute one file operation beneath `workspace`. `store_root` is the
/// canonical store root, refused as a file-tool target.
pub fn run(workspace: &Path, store_root: &Path, op: &FileOp) -> ToolOutcome {
    match op {
        FileOp::List { dir } => {
            let root = match confine(workspace, dir.as_deref().unwrap_or(".")) {
                Ok(root) => root,
                Err(message) => return refuse(message),
            };
            let mut entries: Vec<String> = Vec::new();
            let mut stack = vec![root.clone()];
            while let Some(current) = stack.pop() {
                let Ok(read) = std::fs::read_dir(&current) else {
                    continue;
                };
                let mut children: Vec<PathBuf> =
                    read.filter_map(|e| e.ok().map(|e| e.path())).collect();
                children.sort();
                for child in children {
                    if entries.len() >= MAX_LIST_ENTRIES {
                        entries.push(format!("[... capped at {MAX_LIST_ENTRIES} entries ...]"));
                        stack.clear();
                        break;
                    }
                    let name = child
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let meta = std::fs::symlink_metadata(&child);
                    let is_symlink = meta
                        .as_ref()
                        .map(|m| m.file_type().is_symlink())
                        .unwrap_or(false);
                    let rel = child
                        .strip_prefix(workspace)
                        .unwrap_or(&child)
                        .to_string_lossy()
                        .replace('\\', "/");
                    if child.is_dir() && !is_symlink {
                        if SKIP_DIRS.contains(&name.as_str()) {
                            continue;
                        }
                        if inside_store(&child, store_root) {
                            continue; // the store shows through store tools
                        }
                        stack.push(child);
                    } else {
                        entries.push(rel);
                    }
                }
            }
            entries.sort();
            ToolOutcome {
                content: truncate_tool_result(&entries.join("\n")),
                is_error: false,
            }
        }
        FileOp::Read { path } => {
            let target = match confine(workspace, path) {
                Ok(target) => target,
                Err(message) => return refuse(message),
            };
            if inside_store(&target, store_root) {
                return refuse(format!(
                    "`{path}` is inside the store — read it with the store tools \
                     (show / query / search), not file tools"
                ));
            }
            match std::fs::read_to_string(&target) {
                Ok(content) => ToolOutcome {
                    content: truncate_tool_result(&content),
                    is_error: false,
                },
                Err(error) => refuse(format!("cannot read `{path}`: {error}")),
            }
        }
        FileOp::Write { path, content } => {
            if content.len() > MAX_WRITE_BYTES {
                return refuse(format!(
                    "content too large ({} bytes; cap {MAX_WRITE_BYTES}) — split the file",
                    content.len()
                ));
            }
            let target = match confine(workspace, path) {
                Ok(target) => target,
                Err(message) => return refuse(message),
            };
            if inside_store(&target, store_root) {
                return refuse(format!(
                    "`{path}` is inside the store — write it with the store tools \
                     (write / fm_set / body_set), not file tools"
                ));
            }
            if let Some(parent) = target.parent() {
                if let Err(error) = std::fs::create_dir_all(parent) {
                    return refuse(format!("cannot create parent of `{path}`: {error}"));
                }
            }
            match std::fs::write(&target, content) {
                Ok(()) => ToolOutcome {
                    content: format!("wrote {path} ({} bytes)", content.len()),
                    is_error: false,
                },
                Err(error) => refuse(format!("cannot write `{path}`: {error}")),
            }
        }
        FileOp::Edit {
            path,
            old_text,
            new_text,
        } => {
            let target = match confine(workspace, path) {
                Ok(target) => target,
                Err(message) => return refuse(message),
            };
            if inside_store(&target, store_root) {
                return refuse(format!(
                    "`{path}` is inside the store — edit it with the store tools, \
                     not file tools"
                ));
            }
            let content = match std::fs::read_to_string(&target) {
                Ok(content) => content,
                Err(error) => return refuse(format!("cannot read `{path}`: {error}")),
            };
            let matches = content.matches(old_text.as_str()).count();
            if matches == 0 {
                return refuse(format!(
                    "`old_text` not found in {path} — read the file and retry with \
                     the exact current text"
                ));
            }
            if matches > 1 {
                return refuse(format!(
                    "`old_text` occurs {matches} times in {path} — provide a longer, \
                     unique excerpt"
                ));
            }
            let updated = content.replacen(old_text.as_str(), new_text, 1);
            match std::fs::write(&target, &updated) {
                Ok(()) => ToolOutcome {
                    content: format!("edited {path} ({} bytes)", updated.len()),
                    is_error: false,
                },
                Err(error) => refuse(format!("cannot write `{path}`: {error}")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let workspace = dir.path().to_path_buf();
        let store = workspace.join("db");
        std::fs::create_dir_all(&store).expect("store dir");
        (dir, workspace, store)
    }

    #[test]
    fn absolute_and_dotdot_paths_are_refused() {
        let (_dir, workspace, store) = scratch();
        let escape = run(
            &workspace,
            &store,
            &FileOp::Read {
                path: "../outside.txt".into(),
            },
        );
        assert!(escape.is_error);
        let absolute = run(
            &workspace,
            &store,
            &FileOp::Read {
                path: "/etc/hosts".into(),
            },
        );
        assert!(absolute.is_error);
    }

    #[test]
    fn store_subtree_is_refused_for_file_tools() {
        let (_dir, workspace, store) = scratch();
        let outcome = run(
            &workspace,
            &store,
            &FileOp::Write {
                path: "db/records/x.md".into(),
                content: "raw".into(),
            },
        );
        assert!(outcome.is_error);
        assert!(outcome.content.contains("store tools"));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_components_are_refused() {
        let (_dir, workspace, store) = scratch();
        std::os::unix::fs::symlink("/", workspace.join("link")).expect("symlink");
        let outcome = run(
            &workspace,
            &store,
            &FileOp::Read {
                path: "link/etc/hosts".into(),
            },
        );
        assert!(outcome.is_error);
        assert!(outcome.content.contains("symlink"));
    }

    #[test]
    fn write_read_edit_round_trip() {
        let (_dir, workspace, store) = scratch();
        let write = run(
            &workspace,
            &store,
            &FileOp::Write {
                path: "src/app.ts".into(),
                content: "const a = 1;\n".into(),
            },
        );
        assert!(!write.is_error, "{}", write.content);
        let edit = run(
            &workspace,
            &store,
            &FileOp::Edit {
                path: "src/app.ts".into(),
                old_text: "a = 1".into(),
                new_text: "a = 2".into(),
            },
        );
        assert!(!edit.is_error, "{}", edit.content);
        let read = run(
            &workspace,
            &store,
            &FileOp::Read {
                path: "src/app.ts".into(),
            },
        );
        assert!(read.content.contains("a = 2"));
    }

    #[test]
    fn edit_requires_exactly_one_match() {
        let (_dir, workspace, store) = scratch();
        run(
            &workspace,
            &store,
            &FileOp::Write {
                path: "twice.txt".into(),
                content: "x x".into(),
            },
        );
        let outcome = run(
            &workspace,
            &store,
            &FileOp::Edit {
                path: "twice.txt".into(),
                old_text: "x".into(),
                new_text: "y".into(),
            },
        );
        assert!(outcome.is_error);
        assert!(outcome.content.contains("2 times"));
    }
}
