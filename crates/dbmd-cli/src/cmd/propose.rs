// SPDX-License-Identifier: Apache-2.0

//! `dbmd propose` — submit evidence to a published site's inbox.
//!
//! Write without trust: the submission lands in the owner's `sources/inbox/`
//! (never as truth) for their curator to accept or reject. The door is
//! unauthenticated by design — no credential is sent, hub-side rate limits
//! and per-brain caps guard it. Exactly one body source is required:
//! `--body <text>` inline, or `--body-file <path>` (e.g. a record file whose
//! full text travels as the evidence). The hub's per-submission inbox cap is
//! mirrored client-side (`MAX_PROPOSE_BYTES`): an over-cap `--body-file`
//! fails from metadata before it is even read, the same fail-before-upload
//! contract as the push caps.

use std::io::Read as _;
use std::path::Path;

use dbmd_core::linkmd::{self, LinkError, MAX_PROPOSE_BYTES};
use serde_json::Value;

use crate::cli::ProposeArgs;
use crate::context::Context;
use crate::error::{CliError, CliResult, ExitCode};
use crate::sanitize::sanitize_single_line;

/// Run `dbmd propose`.
pub fn run(ctx: &Context, args: &ProposeArgs) -> CliResult {
    let body = match (&args.body, &args.body_file) {
        (Some(text), None) => text.clone(),
        (None, Some(path)) => read_body_file(path)?,
        _ => {
            return Err(CliError::new(
                ExitCode::Runtime,
                "BAD_BODY",
                "exactly one body source is required",
            )
            .with_hint("pass --body <text> or --body-file <path>"));
        }
    };

    let site = args.site.trim().trim_start_matches('@');
    let cfg = linkmd::hub_config(args.hub.as_deref(), Path::new(&args.dir))?;
    let receipt = linkmd::propose(&cfg, site, &args.app, &body)?;

    if ctx.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&receipt).unwrap_or_default()
        );
        return Ok(());
    }

    println!(
        "proposed to @{}/{} — landed as {}",
        sanitize_single_line(site),
        sanitize_single_line(&args.app),
        // The receipt path is hub-authored → terminal-sanitized.
        sanitize_single_line(
            receipt
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or("(inbox)")
        ),
    );
    Ok(())
}

/// Read one proposal body from exactly the regular inode selected by the
/// no-follow open. Metadata and the bounded `MAX+1` read both use that retained
/// descriptor, so a concurrent pathname swap cannot redirect the upload and a
/// growing file cannot bypass the size cap between check and read.
fn read_body_file(path: &str) -> Result<String, CliError> {
    let io_error = |error: std::io::Error| {
        CliError::new(
            ExitCode::Runtime,
            "IO_ERROR",
            format!("reading --body-file {path}: {error}"),
        )
    };
    let mut file = dbmd_core::fsx::open_regular_nofollow(Path::new(path)).map_err(io_error)?;
    #[cfg(test)]
    AFTER_BODY_FILE_OPEN.with(|slot| {
        if let Some(hook) = slot.borrow_mut().take() {
            hook();
        }
    });
    let declared = file.metadata().map_err(io_error)?.len();
    if declared > MAX_PROPOSE_BYTES {
        return Err(LinkError::ProposeTooLarge { bytes: declared }.into());
    }

    let mut bytes = Vec::with_capacity(declared as usize);
    file.by_ref()
        .take(MAX_PROPOSE_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    if bytes.len() as u64 > MAX_PROPOSE_BYTES {
        return Err(LinkError::ProposeTooLarge {
            bytes: bytes.len() as u64,
        }
        .into());
    }
    String::from_utf8(bytes)
        .map_err(|error| io_error(std::io::Error::new(std::io::ErrorKind::InvalidData, error)))
}

#[cfg(test)]
thread_local! {
    static AFTER_BODY_FILE_OPEN: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
}

#[cfg(test)]
fn set_after_body_file_open(hook: impl FnOnce() + 'static) {
    AFTER_BODY_FILE_OPEN.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(hook));
    });
}

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;
    use std::sync::mpsc;

    use super::*;
    use crate::context::ColorChoice;

    #[test]
    fn body_path_swap_cannot_redirect_the_uploaded_proposal() {
        let sandbox = tempfile::tempdir().unwrap();
        let root = sandbox.path().join("body-root");
        std::fs::create_dir_all(&root).unwrap();
        let body_path = root.join("evidence.md");
        std::fs::write(&body_path, "trusted evidence").unwrap();
        let detached = sandbox.path().join("detached");

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (sent, received) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4096];
            loop {
                let count = stream.read(&mut buffer).unwrap();
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..count]);
                if let Some(header_end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..header_end]);
                    let content_length = headers
                        .lines()
                        .find_map(|line| {
                            line.split_once(':').and_then(|(name, value)| {
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().unwrap())
                            })
                        })
                        .unwrap();
                    if request.len() >= header_end + 4 + content_length {
                        break;
                    }
                }
            }
            sent.send(String::from_utf8(request).unwrap()).unwrap();
            stream
                .write_all(
                    b"HTTP/1.1 201 Created\r\nContent-Type: application/json\r\nContent-Length: 38\r\nConnection: close\r\n\r\n{\"id\":\"x\",\"path\":\"sources/inbox/x.md\"}",
                )
                .unwrap();
        });

        let root_for_hook = root.clone();
        let detached_for_hook = detached.clone();
        set_after_body_file_open(move || {
            std::fs::rename(&root_for_hook, &detached_for_hook).unwrap();
            std::fs::create_dir_all(&root_for_hook).unwrap();
            std::fs::write(root_for_hook.join("evidence.md"), "attacker replacement").unwrap();
        });

        let args = ProposeArgs {
            site: "site".into(),
            app: "intake".into(),
            body: None,
            body_file: Some(body_path.to_string_lossy().into_owned()),
            hub: Some(format!("http://{address}")),
            dir: sandbox.path().to_string_lossy().into_owned(),
        };
        run(
            &Context {
                json: true,
                color: ColorChoice::Never,
            },
            &args,
        )
        .unwrap();
        server.join().unwrap();

        let request = received.recv().unwrap();
        let body = request.split_once("\r\n\r\n").unwrap().1;
        let payload: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(payload["body"], "trusted evidence");
        assert!(!body.contains("attacker replacement"));
    }
}
