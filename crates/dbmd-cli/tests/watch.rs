// SPDX-License-Identifier: Apache-2.0

//! `dbmd watch` — the local change feed.
//!
//! These tests drive the real long-running process: spawn `dbmd --json watch`
//! against a scratch store, mutate files, and assert the NDJSON events. A
//! reader thread feeds lines through a channel so every wait carries a
//! timeout; the child is killed on drop so a failing assert never leaks a
//! watcher. Timing is deliberately generous (1s poll, 30s per-event budget)
//! — the assertions are about WHICH events arrive, never about latency.

mod common;

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use common::write_file;

const EVENT_BUDGET: Duration = Duration::from_secs(30);

struct Watcher {
    child: Child,
    rx: mpsc::Receiver<String>,
}

impl Watcher {
    fn spawn(store: &std::path::Path, extra: &[&str]) -> Self {
        let mut child = Command::new(assert_cmd::cargo::cargo_bin("dbmd"))
            .args(["--json", "watch", "--interval", "1", "--dir"])
            .arg(store)
            .args(extra)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn dbmd watch");
        let stdout = child.stdout.take().expect("piped stdout");
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                match line {
                    Ok(l) => {
                        if tx.send(l).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        Self { child, rx }
    }

    /// The next event line, parsed.
    fn next(&self) -> serde_json::Value {
        let line = self
            .rx
            .recv_timeout(EVENT_BUDGET)
            .expect("watch event within budget");
        serde_json::from_str(&line).expect("valid NDJSON event line")
    }

    /// Drain events until one matches `event`; returns it. Tolerates
    /// interleaved events (e.g. a partial-write `modified` racing a poll).
    fn wait_for(&self, event: &str) -> serde_json::Value {
        loop {
            let v = self.next();
            if v["event"] == serde_json::json!(event) {
                return v;
            }
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn scratch_store() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::TempDir::new().unwrap();
    let store = dir.path().to_path_buf();
    write_file(
        &store,
        "DB.md",
        "---\ntype: db-md\nscope: test\nowner: T\n---\n# T\n",
    );
    write_file(
        &store,
        "records/widgets/a.md",
        "---\ntype: widget\nsummary: A\n---\n",
    );
    (dir, store)
}

/// The full lifecycle stream: a baseline first, then created / modified /
/// removed events as the store's files change out-of-band.
#[test]
fn watch_streams_create_modify_remove() {
    let (_tmp, store) = scratch_store();
    let watcher = Watcher::spawn(&store, &[]);

    let baseline = watcher.next();
    assert_eq!(baseline["event"], serde_json::json!("baseline"));
    assert_eq!(baseline["files"], serde_json::json!(2)); // DB.md + a.md

    write_file(
        &store,
        "records/widgets/b.md",
        "---\ntype: widget\nsummary: B\n---\n",
    );
    let created = watcher.wait_for("created");
    assert_eq!(created["path"], serde_json::json!("records/widgets/b.md"));
    assert!(created["at"].is_string());

    write_file(
        &store,
        "records/widgets/b.md",
        "---\ntype: widget\nsummary: B\n---\nnow with a body\n",
    );
    let modified = watcher.wait_for("modified");
    assert_eq!(modified["path"], serde_json::json!("records/widgets/b.md"));

    std::fs::remove_file(store.join("records/widgets/b.md")).unwrap();
    let removed = watcher.wait_for("removed");
    assert_eq!(removed["path"], serde_json::json!("records/widgets/b.md"));
}

/// `--path` narrows both the baseline membership and the event stream.
#[test]
fn watch_path_scopes_events() {
    let (_tmp, store) = scratch_store();
    write_file(
        &store,
        "records/notes/n.md",
        "---\ntype: note\nsummary: N\n---\n",
    );
    let watcher = Watcher::spawn(&store, &["--path", "records/widgets"]);

    let baseline = watcher.next();
    assert_eq!(baseline["files"], serde_json::json!(1)); // a.md only
    assert_eq!(baseline["path"], serde_json::json!("records/widgets"));

    // An out-of-scope change first, then an in-scope one: the first created
    // event must be the in-scope file — the notes write never surfaces.
    write_file(
        &store,
        "records/notes/n2.md",
        "---\ntype: note\nsummary: N2\n---\n",
    );
    write_file(
        &store,
        "records/widgets/w2.md",
        "---\ntype: widget\nsummary: W2\n---\n",
    );
    let created = watcher.wait_for("created");
    assert_eq!(created["path"], serde_json::json!("records/widgets/w2.md"));
}

/// `--interval 0` is refused before any I/O, the `subscribe` contract.
#[test]
fn watch_zero_interval_refused() {
    let (_tmp, store) = scratch_store();
    common::dbmd()
        .args(["watch", "--interval", "0", "--dir"])
        .arg(&store)
        .assert()
        .failure()
        .code(1); // ExitCode::Runtime
}
