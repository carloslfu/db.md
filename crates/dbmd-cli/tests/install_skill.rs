//! Integration tests for `dbmd install-skill` — the persistent "agent way"
//! install point (the sibling of `dbmd spec`).
//!
//! Intent: `install-skill` drops a skill file into a local coding agent's
//! config dir (Claude Code → `~/.claude/skills/db-md/SKILL.md`, Codex →
//! `~/.codex/instructions/db-md.md`), `--uninstall` removes it, the target is
//! explicit (`--target`) or autodetected from which agent dir exists, and clap
//! rejects an unknown target. Every test points `HOME` at a fresh tempdir so
//! nothing touches the developer's real `~/.claude` or `~/.codex`.

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

#[test]
fn installs_claude_code_skill_with_frontmatter() {
    let home = tempfile::TempDir::new().unwrap();
    dbmd_home(home.path())
        .args(["install-skill", "--target", "claude-code"])
        .assert()
        .success();

    let skill = home.path().join(".claude/skills/db-md/SKILL.md");
    let body = std::fs::read_to_string(&skill).expect("SKILL.md written");
    // The frontmatter Claude Code reads to discover + trigger the skill.
    assert!(body.starts_with("---\n"), "skill opens with YAML frontmatter");
    assert!(body.contains("name: db-md"));
    assert!(body.contains("description:"));
    // The body is a pointer at the single source of truth, not an inlined SPEC.
    assert!(body.contains("dbmd spec"));
}

#[test]
fn installs_codex_skill_at_the_documented_bootstrap_path() {
    let home = tempfile::TempDir::new().unwrap();
    dbmd_home(home.path())
        .args(["install-skill", "--target", "codex"])
        .assert()
        .success();

    // Matches db.md's documented `dbmd spec >> ~/.codex/instructions/db-md.md`.
    let skill = home.path().join(".codex/instructions/db-md.md");
    let body = std::fs::read_to_string(&skill).expect("codex skill written");
    assert!(body.contains("dbmd spec"));
}

#[test]
fn json_output_reports_target_path_and_action() {
    let home = tempfile::TempDir::new().unwrap();
    let out = dbmd_home(home.path())
        .args(["--json", "install-skill", "--target", "claude-code"])
        .assert()
        .success();
    let v = stdout_json(&out);
    assert_eq!(v["target"], serde_json::json!("claude-code"));
    assert_eq!(v["action"], serde_json::json!("installed"));
    assert!(v["path"].as_str().unwrap().ends_with("SKILL.md"));
}

#[test]
fn autodetect_prefers_claude_code_when_present() {
    let home = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(home.path().join(".claude")).unwrap();
    let out = dbmd_home(home.path())
        .args(["--json", "install-skill"])
        .assert()
        .success();
    assert_eq!(stdout_json(&out)["target"], serde_json::json!("claude-code"));
}

#[test]
fn autodetect_uses_codex_when_only_codex_present() {
    let home = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(home.path().join(".codex")).unwrap();
    let out = dbmd_home(home.path())
        .args(["--json", "install-skill"])
        .assert()
        .success();
    assert_eq!(stdout_json(&out)["target"], serde_json::json!("codex"));
}

#[test]
fn uninstall_removes_then_noops() {
    let home = tempfile::TempDir::new().unwrap();
    dbmd_home(home.path())
        .args(["install-skill", "--target", "claude-code"])
        .assert()
        .success();
    let skill = home.path().join(".claude/skills/db-md/SKILL.md");
    assert!(skill.exists());

    let out = dbmd_home(home.path())
        .args([
            "--json",
            "install-skill",
            "--target",
            "claude-code",
            "--uninstall",
        ])
        .assert()
        .success();
    assert_eq!(stdout_json(&out)["action"], serde_json::json!("uninstalled"));
    assert!(!skill.exists());

    // A second uninstall is a clean no-op, not an error.
    let out = dbmd_home(home.path())
        .args([
            "--json",
            "install-skill",
            "--target",
            "claude-code",
            "--uninstall",
        ])
        .assert()
        .success();
    assert_eq!(stdout_json(&out)["action"], serde_json::json!("noop"));
}

#[test]
fn rejects_unknown_target_with_usage_exit() {
    let home = tempfile::TempDir::new().unwrap();
    // clap validates the value_enum and exits 2 (usage); the body never runs.
    dbmd_home(home.path())
        .args(["install-skill", "--target", "emacs"])
        .assert()
        .failure()
        .code(2);
}
