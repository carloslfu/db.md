// SPDX-License-Identifier: Apache-2.0

//! Anthropic credentials via **Anthropic's own CLI** (`ant`).
//!
//! Anthropic publishes exactly one supported way for a third-party process to
//! spend a user's Anthropic OAuth session: `ant auth login` stores a profile,
//! `ant auth print-credentials --access-token` prints a short-lived token
//! (refreshing it first when needed), and the caller sends it as
//! `Authorization: Bearer <token>` together with the beta header
//! `anthropic-beta: oauth-2025-04-20`. This module is that handoff and
//! nothing more.
//!
//! Why a subprocess instead of a native PKCE flow like [`super::oauth`]: the
//! Codex flow uses OpenAI's *public* client id, published for third-party
//! clients. Anthropic publishes no such id — the only way to run the flow
//! in-process would be to borrow the one belonging to Anthropic's own client
//! and pose as it. Shelling out to the vendor's CLI reaches the same session
//! with none of that, and the token never touches a store.
//!
//! Resolution order matches every other credential in the harness: an
//! explicit environment key wins, and this bridge is consulted only when the
//! environment is silent.

use std::process::Command;

use super::HarnessError;

/// The beta header Anthropic requires alongside an OAuth bearer token.
/// `/v1/messages` rejects the token without it.
pub const OAUTH_BETA: &str = "oauth-2025-04-20";

/// The CLI binary name.
pub const BIN: &str = "ant";

/// Install instructions, shown when the CLI is missing.
pub const INSTALL_HINT: &str = "install Anthropic's CLI first: `brew install anthropics/tap/ant` \
     (macOS) or see https://github.com/anthropics/anthropic-cli/releases";

/// Whether the `ant` binary is on `PATH`.
pub fn installed() -> bool {
    Command::new(BIN)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Fetch a fresh access token from the active `ant` profile.
///
/// Returns `Ok(None)` when the CLI is absent or simply has no active profile
/// — both are "not logged in", not failures, so the caller can fall through
/// to `ANTHROPIC_API_KEY`.
pub fn access_token() -> Result<Option<String>, HarnessError> {
    if !installed() {
        return Ok(None);
    }
    // `--access-token` prints the bare token. Without the flag this command
    // prints the whole credentials JSON, which would land in an Authorization
    // header as garbage — a documented foot-gun, so it is never called bare.
    let output = Command::new(BIN)
        .args(["auth", "print-credentials", "--access-token"])
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| HarnessError::Provider(format!("cannot run `{BIN}`: {e}")))?;
    if !output.status.success() {
        return Ok(None);
    }
    let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if token.is_empty() || token.contains(char::is_whitespace) {
        // A JSON blob or an error banner is not a token. Refuse rather than
        // send a malformed header.
        return Ok(None);
    }
    Ok(Some(token))
}

/// A one-line description of the active profile, for `dbmd login --status`.
pub fn status_line() -> Option<String> {
    if !installed() {
        return None;
    }
    let output = Command::new(BIN)
        .args(["auth", "status"])
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())?
        .to_string();
    Some(line)
}

/// Run `ant auth login` interactively, inheriting this process's terminal.
///
/// `no_browser` maps to the CLI's own `--no-browser`, which prints the
/// authorize URL and reads the code back — the headless path.
pub fn login(no_browser: bool) -> Result<(), HarnessError> {
    if !installed() {
        return Err(HarnessError::Config(format!(
            "`{BIN}` is not installed — {INSTALL_HINT}"
        )));
    }
    let mut command = Command::new(BIN);
    command.args(["auth", "login"]);
    if no_browser {
        command.arg("--no-browser");
    }
    let status = command
        .status()
        .map_err(|e| HarnessError::Provider(format!("cannot run `{BIN} auth login`: {e}")))?;
    if !status.success() {
        return Err(HarnessError::Provider(
            "`ant auth login` did not complete".to_string(),
        ));
    }
    Ok(())
}

/// Run `ant auth logout`.
pub fn logout() -> Result<(), HarnessError> {
    if !installed() {
        return Ok(());
    }
    let _ = Command::new(BIN)
        .args(["auth", "logout"])
        .stdin(std::process::Stdio::null())
        .status();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_cli_is_not_an_error() {
        // Whatever this machine has, the contract holds: absence resolves to
        // "no credential", never a hard failure, so the caller can fall
        // through to an API key.
        let resolved = access_token().expect("absence must not error");
        if !installed() {
            assert!(resolved.is_none());
        }
    }

    #[test]
    fn beta_header_is_the_documented_one() {
        // Anthropic's raw-HTTP handoff is Bearer + this exact beta value;
        // /v1/messages rejects the token without it.
        assert_eq!(OAUTH_BETA, "oauth-2025-04-20");
    }
}
