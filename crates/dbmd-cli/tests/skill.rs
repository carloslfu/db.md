//! Integration tests for `dbmd install-skill` / `dbmd uninstall-skill` — the
//! persistent "agent way" install point (the sibling of `dbmd spec`).
//!
//! Intent: `install-skill` drops the one cross-agent Agent Skill into each
//! agent's skills directory in the open format — Claude Code →
//! `~/.claude/skills/db-md/SKILL.md`, Codex → `~/.codex/skills/db-md/SKILL.md`,
//! same file, frontmatter and all. With no `--target` it points every detected
//! agent in one command; `--target` narrows to one. `uninstall-skill` removes
//! exactly what it wrote, clap rejects an unknown target, and the managed-marker
//! check refuses to clobber a file dbmd did not write. Every test points `HOME`
//! at a fresh tempdir so nothing touches the developer's real `~/.claude` or
//! `~/.codex`.

mod common;

use common::dbmd;

/// A `dbmd` command with `HOME` pointed at a throwaway tempdir, so install /
/// uninstall never touch the real machine.
fn dbmd_home(home: &std::path::Path) -> assert_cmd::Command {
    let mut cmd = dbmd();
    cmd.env("HOME", home);
    cmd
}

fn stdout_json(out: &assert_cmd::assert::Assert) -> serde_json::Value {
    let bytes = out.get_output().stdout.clone();
    serde_json::from_str(&String::from_utf8(bytes).unwrap()).unwrap()
}

/// Both agents use the open Agent Skills layout: `<skills-root>/db-md/SKILL.md`.
fn claude_skill(home: &std::path::Path) -> std::path::PathBuf {
    home.join(".claude/skills/db-md/SKILL.md")
}
fn codex_skill(home: &std::path::Path) -> std::path::PathBuf {
    home.join(".codex/skills/db-md/SKILL.md")
}

#[test]
fn installs_claude_code_skill_with_frontmatter() {
    let home = tempfile::TempDir::new().unwrap();
    dbmd_home(home.path())
        .args(["install-skill", "--target", "claude-code"])
        .assert()
        .success();

    let body = std::fs::read_to_string(claude_skill(home.path())).expect("SKILL.md written");
    // The frontmatter Claude Code reads to discover + trigger the skill.
    assert!(
        body.starts_with("---\n"),
        "skill opens with YAML frontmatter"
    );
    assert!(body.contains("name: db-md"));
    assert!(body.contains("description:"));
    assert!(body.contains("version:"));
    // The body is a pointer at the single source of truth, not an inlined SPEC.
    assert!(body.contains("dbmd spec"));
}

#[test]
fn installs_codex_skill_in_agent_skills_format() {
    let home = tempfile::TempDir::new().unwrap();
    dbmd_home(home.path())
        .args(["install-skill", "--target", "codex"])
        .assert()
        .success();

    // Codex reads the same open Agent Skills layout as Claude Code — a
    // `db-md/SKILL.md` folder with frontmatter, NOT a plain instructions file.
    let body = std::fs::read_to_string(codex_skill(home.path())).expect("codex skill written");
    assert!(
        body.starts_with("---\n"),
        "codex skill carries frontmatter too"
    );
    assert!(body.contains("name: db-md"));
    assert!(body.contains("dbmd spec"));
}

#[test]
fn claude_and_codex_get_byte_identical_skills() {
    // Single source: the same embedded SKILL.md is written for every agent.
    let home = tempfile::TempDir::new().unwrap();
    dbmd_home(home.path())
        .args(["install-skill", "--target", "claude-code"])
        .assert()
        .success();
    dbmd_home(home.path())
        .args(["install-skill", "--target", "codex"])
        .assert()
        .success();
    let claude = std::fs::read_to_string(claude_skill(home.path())).unwrap();
    let codex = std::fs::read_to_string(codex_skill(home.path())).unwrap();
    assert_eq!(claude, codex, "every agent gets the identical skill body");
}

#[test]
fn default_points_every_detected_agent() {
    let home = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(home.path().join(".claude")).unwrap();
    std::fs::create_dir_all(home.path().join(".codex")).unwrap();

    let out = dbmd_home(home.path())
        .args(["--json", "install-skill"])
        .assert()
        .success();

    let arr = stdout_json(&out);
    let targets: Vec<&str> = arr
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["target"].as_str().unwrap())
        .collect();
    assert!(targets.contains(&"claude-code"));
    assert!(targets.contains(&"codex"));
    assert!(claude_skill(home.path()).exists());
    assert!(codex_skill(home.path()).exists());
}

#[test]
fn default_installs_only_the_detected_agent() {
    let home = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(home.path().join(".codex")).unwrap();

    let out = dbmd_home(home.path())
        .args(["--json", "install-skill"])
        .assert()
        .success();

    let arr = stdout_json(&out);
    let entries = arr.as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["target"], serde_json::json!("codex"));
    assert!(codex_skill(home.path()).exists());
    assert!(!claude_skill(home.path()).exists());
}

#[test]
fn default_falls_back_to_claude_when_no_agent_present() {
    let home = tempfile::TempDir::new().unwrap();
    let out = dbmd_home(home.path())
        .args(["--json", "install-skill"])
        .assert()
        .success();
    let arr = stdout_json(&out);
    let entries = arr.as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["target"], serde_json::json!("claude-code"));
}

#[test]
fn json_output_is_an_array_of_target_path_action() {
    let home = tempfile::TempDir::new().unwrap();
    let out = dbmd_home(home.path())
        .args(["--json", "install-skill", "--target", "claude-code"])
        .assert()
        .success();
    let arr = stdout_json(&out);
    let entries = arr.as_array().expect("install-skill --json emits an array");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["target"], serde_json::json!("claude-code"));
    assert_eq!(entries[0]["action"], serde_json::json!("installed"));
    assert!(entries[0]["path"].as_str().unwrap().ends_with("SKILL.md"));
}

#[test]
fn install_refuses_to_overwrite_an_unmanaged_skill() {
    let home = tempfile::TempDir::new().unwrap();
    let skill = codex_skill(home.path());
    std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
    std::fs::write(&skill, "my own hand-written skill\n").unwrap();

    dbmd_home(home.path())
        .args(["install-skill", "--target", "codex"])
        .assert()
        .failure()
        .code(5);

    assert_eq!(
        std::fs::read_to_string(&skill).unwrap(),
        "my own hand-written skill\n"
    );
}

#[test]
fn uninstall_removes_then_noops() {
    let home = tempfile::TempDir::new().unwrap();
    dbmd_home(home.path())
        .args(["install-skill", "--target", "claude-code"])
        .assert()
        .success();
    assert!(claude_skill(home.path()).exists());

    // `uninstall-skill` removes exactly what `install-skill` wrote.
    let out = dbmd_home(home.path())
        .args(["--json", "uninstall-skill", "--target", "claude-code"])
        .assert()
        .success();
    assert_eq!(
        stdout_json(&out).as_array().unwrap()[0]["action"],
        serde_json::json!("uninstalled")
    );
    assert!(!claude_skill(home.path()).exists());

    // A second uninstall is a clean no-op, not an error.
    let out = dbmd_home(home.path())
        .args(["--json", "uninstall-skill", "--target", "claude-code"])
        .assert()
        .success();
    assert_eq!(
        stdout_json(&out).as_array().unwrap()[0]["action"],
        serde_json::json!("noop")
    );
}

#[test]
fn uninstall_refuses_to_remove_an_unmanaged_skill() {
    let home = tempfile::TempDir::new().unwrap();
    let skill = codex_skill(home.path());
    std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
    std::fs::write(&skill, "my own hand-written skill\n").unwrap();

    dbmd_home(home.path())
        .args(["uninstall-skill", "--target", "codex"])
        .assert()
        .failure()
        .code(5);

    assert_eq!(
        std::fs::read_to_string(&skill).unwrap(),
        "my own hand-written skill\n"
    );
}

#[test]
fn uninstall_removes_only_the_managed_file_and_keeps_siblings() {
    let home = tempfile::TempDir::new().unwrap();
    dbmd_home(home.path())
        .args(["install-skill", "--target", "claude-code"])
        .assert()
        .success();

    let dir = home.path().join(".claude/skills/db-md");
    let skill = dir.join("SKILL.md");
    let sibling = dir.join("notes.md");
    std::fs::write(&sibling, "keep me\n").unwrap();

    dbmd_home(home.path())
        .args(["uninstall-skill", "--target", "claude-code"])
        .assert()
        .success();

    assert!(!skill.exists());
    assert_eq!(std::fs::read_to_string(&sibling).unwrap(), "keep me\n");
}

#[test]
fn rejects_unknown_target_with_usage_exit() {
    let home = tempfile::TempDir::new().unwrap();
    // clap validates the value_enum and exits 2 (usage); the body never runs —
    // on both the install and the uninstall verb.
    dbmd_home(home.path())
        .args(["install-skill", "--target", "emacs"])
        .assert()
        .failure()
        .code(2);
    dbmd_home(home.path())
        .args(["uninstall-skill", "--target", "emacs"])
        .assert()
        .failure()
        .code(2);
}
