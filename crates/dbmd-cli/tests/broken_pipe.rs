//! Regression: a reader that closes `dbmd`'s stdout early (`dbmd spec | head`)
//! must exit cleanly, never with the broken-pipe panic (exit 101) that the
//! v0.3.3 release smoke test caught.

use std::io::Read;
use std::process::{Command, Stdio};

/// `dbmd spec` writes more than a pipe buffer's worth (the bundled SPEC is tens
/// of KB), so a reader that takes one sip and leaves forces a write to a closed
/// pipe. That must end in a clean exit, not a panic.
#[test]
fn spec_into_a_closed_pipe_exits_clean() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_dbmd"))
        .arg("spec")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn `dbmd spec`");

    // Read a little, then drop the read end: the consumer has "left", the way
    // `head` exits after its first lines.
    {
        let mut out = child.stdout.take().expect("child stdout");
        let mut buf = [0u8; 64];
        let _ = out.read(&mut buf);
    }

    let status = child.wait().expect("wait on `dbmd spec`");
    assert!(
        status.success(),
        "`dbmd spec` into a closed pipe must exit 0; got {status:?}"
    );
}
