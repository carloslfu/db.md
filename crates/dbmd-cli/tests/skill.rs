//! Integration tests for `dbmd install-skill` / `dbmd uninstall-skill` — the
//! persistent "agent way" install point (the sibling of `dbmd spec`).
//!
//! Intent: `install-skill` drops a skill file into a local coding agent's
//! config dir (Claude Code → `~/.claude/skills/db-md/SKILL.md`, Codex →
//! `~/.codex/instructions/db-md.md`), `uninstall-skill` removes exactly that,
//! the target is explicit (`--target`) or autodetected from which agent dir
//! exists, and clap rejects an unknown target. Every test points `HOME` at a
//! fresh tempdir so nothing touches the developer's real `~/.claude` or
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
    assert!(
        body.starts_with("---\n"),
        "skill opens with YAML frontmatter"
    );
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
fn install_refuses_to_overwrite_unmanaged_codex_instructions() {
    let home = tempfile::TempDir::new().unwrap();
    let skill = home.path().join(".codex/instructions/db-md.md");
    std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
    std::fs::write(&skill, "custom user instructions\n").unwrap();

    dbmd_home(home.path())
        .args(["install-skill", "--target", "codex"])
        .assert()
        .failure()
        .code(5);

    assert_eq!(
        std::fs::read_to_string(&skill).unwrap(),
        "custom user instructions\n"
    );
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
    assert_eq!(
        stdout_json(&out)["target"],
        serde_json::json!("claude-code")
    );
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

    // `uninstall-skill` removes exactly what `install-skill` wrote.
    let out = dbmd_home(home.path())
        .args(["--json", "uninstall-skill", "--target", "claude-code"])
        .assert()
        .success();
    assert_eq!(
        stdout_json(&out)["action"],
        serde_json::json!("uninstalled")
    );
    assert!(!skill.exists());

    // A second uninstall is a clean no-op, not an error.
    let out = dbmd_home(home.path())
        .args(["--json", "uninstall-skill", "--target", "claude-code"])
        .assert()
        .success();
    assert_eq!(stdout_json(&out)["action"], serde_json::json!("noop"));
}

#[test]
fn uninstall_refuses_to_remove_unmanaged_codex_instructions() {
    let home = tempfile::TempDir::new().unwrap();
    let skill = home.path().join(".codex/instructions/db-md.md");
    std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
    std::fs::write(&skill, "custom user instructions\n").unwrap();

    dbmd_home(home.path())
        .args(["uninstall-skill", "--target", "codex"])
        .assert()
        .failure()
        .code(5);

    assert_eq!(
        std::fs::read_to_string(&skill).unwrap(),
        "custom user instructions\n"
    );
}

#[test]
fn uninstall_claude_code_removes_only_the_managed_skill_file() {
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
