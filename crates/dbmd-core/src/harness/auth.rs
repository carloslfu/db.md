// SPDX-License-Identifier: Apache-2.0

//! Credential storage for subscription logins — `auth.json` in the toolkit's
//! own state directory, never inside a store (a secret in the store tree is
//! one sync away from leaking, the same rule the hub client follows for its
//! bearer).
//!
//! Shape is pi's (`~/.pi/agent/auth.json`): a JSON object keyed by provider,
//! each `{"type":"oauth","access":…,"refresh":…,"expires":<ms>}`, written
//! 0600. An access token is refreshed in place once it has expired, and the
//! refreshed pair is persisted immediately so the next process reuses it.
//!
//! Location precedence matches the hub client's state dir: `DBMD_STATE_DIR` >
//! `XDG_STATE_HOME/dbmd` > macOS `~/Library/Application Support/dbmd/state` >
//! Linux `~/.local/state/dbmd` > Windows `%LOCALAPPDATA%\dbmd\state`.

use std::path::PathBuf;

use serde_json::{json, Value};

use super::oauth::{self, OAuthTokens};
use super::HarnessError;

/// Env var overriding the toolkit state directory (shared with the hub client).
pub const STATE_DIR_ENV: &str = "DBMD_STATE_DIR";

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

/// The toolkit state directory holding `auth.json`.
pub fn state_dir() -> Result<PathBuf, HarnessError> {
    if let Some(path) = env_nonempty(STATE_DIR_ENV) {
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            return Err(HarnessError::Config(format!(
                "{STATE_DIR_ENV} must be an absolute path"
            )));
        }
        return Ok(path);
    }
    // Each branch is the function's tail expression on its own platform, so
    // neither carries a `return` (clippy::needless_return is Windows-only
    // otherwise — it never compiles this block elsewhere).
    #[cfg(windows)]
    {
        let base = env_nonempty("LOCALAPPDATA").ok_or_else(|| {
            HarnessError::Config(format!(
                "cannot locate user state; set {STATE_DIR_ENV} or LOCALAPPDATA"
            ))
        })?;
        Ok(PathBuf::from(base).join("dbmd").join("state"))
    }
    #[cfg(not(windows))]
    {
        if let Some(base) = env_nonempty("XDG_STATE_HOME") {
            let base = PathBuf::from(base);
            if base.is_absolute() {
                return Ok(base.join("dbmd"));
            }
        }
        let home = PathBuf::from(env_nonempty("HOME").ok_or_else(|| {
            HarnessError::Config(format!("cannot locate user state; set {STATE_DIR_ENV}"))
        })?);
        #[cfg(target_os = "macos")]
        {
            Ok(home
                .join("Library")
                .join("Application Support")
                .join("dbmd")
                .join("state"))
        }
        #[cfg(not(target_os = "macos"))]
        {
            Ok(home.join(".local").join("state").join("dbmd"))
        }
    }
}

/// Absolute path of the credential file.
pub fn auth_path() -> Result<PathBuf, HarnessError> {
    Ok(state_dir()?.join("auth.json"))
}

fn read_all() -> Result<Value, HarnessError> {
    let path = auth_path()?;
    match std::fs::read_to_string(&path) {
        Ok(text) => Ok(serde_json::from_str(&text).unwrap_or_else(|_| json!({}))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(error) => Err(HarnessError::Config(format!(
            "cannot read {}: {error}",
            path.display()
        ))),
    }
}

fn write_all(value: &Value) -> Result<(), HarnessError> {
    let path = auth_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            HarnessError::Config(format!("cannot create {}: {error}", parent.display()))
        })?;
    }
    let text = serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".to_string());
    std::fs::write(&path, text).map_err(|error| {
        HarnessError::Config(format!("cannot write {}: {error}", path.display()))
    })?;
    restrict(&path)?;
    Ok(())
}

/// Owner-only permissions on the credential file (0600 on unix).
fn restrict(path: &std::path::Path) -> Result<(), HarnessError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(
            |error| HarnessError::Config(format!("cannot restrict {}: {error}", path.display())),
        )?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn tokens_from(record: &Value) -> Option<OAuthTokens> {
    if record.get("type").and_then(|t| t.as_str()) != Some("oauth") {
        return None;
    }
    Some(OAuthTokens {
        access: record.get("access")?.as_str()?.to_string(),
        refresh: record.get("refresh")?.as_str()?.to_string(),
        expires_ms: record
            .get("expires")
            .and_then(|e| e.as_u64())
            .map(u128::from)
            .unwrap_or(0),
    })
}

fn record_from(tokens: &OAuthTokens) -> Value {
    json!({
        "type": "oauth",
        "access": tokens.access,
        "refresh": tokens.refresh,
        "expires": u64::try_from(tokens.expires_ms).unwrap_or(u64::MAX),
    })
}

/// Persist one provider's tokens.
pub fn save(provider: &str, tokens: &OAuthTokens) -> Result<(), HarnessError> {
    let mut all = read_all()?;
    if !all.is_object() {
        all = json!({});
    }
    if let Some(map) = all.as_object_mut() {
        map.insert(provider.to_string(), record_from(tokens));
    }
    write_all(&all)
}

/// Forget one provider's tokens. Returns whether anything was removed.
pub fn clear(provider: &str) -> Result<bool, HarnessError> {
    let mut all = read_all()?;
    let removed = all
        .as_object_mut()
        .map(|map| map.remove(provider).is_some())
        .unwrap_or(false);
    if removed {
        write_all(&all)?;
    }
    Ok(removed)
}

/// The stored tokens for a provider, without refreshing.
pub fn peek(provider: &str) -> Result<Option<OAuthTokens>, HarnessError> {
    Ok(read_all()?.get(provider).and_then(tokens_from))
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis())
        .unwrap_or(0)
}

/// Refresh a minute before the deadline so a long turn cannot start on a
/// token that expires mid-stream.
const EXPIRY_SKEW_MS: u128 = 60_000;

/// The current access token for a provider, refreshing and persisting first
/// when the stored one has expired. `Ok(None)` means "not logged in".
pub fn access_token(provider: &str) -> Result<Option<String>, HarnessError> {
    let Some(tokens) = peek(provider)? else {
        return Ok(None);
    };
    if now_ms() + EXPIRY_SKEW_MS < tokens.expires_ms {
        return Ok(Some(tokens.access));
    }
    let refreshed = oauth::refresh_tokens(&tokens.refresh).map_err(|error| {
        HarnessError::Config(format!(
            "{error} — the stored {provider} login could not be refreshed; \
             run `dbmd login {provider}` again"
        ))
    })?;
    save(provider, &refreshed)?;
    Ok(Some(refreshed.access))
}

/// Every provider with stored credentials, for `dbmd login --status`.
pub fn logged_in_providers() -> Result<Vec<String>, HarnessError> {
    let all = read_all()?;
    let Some(map) = all.as_object() else {
        return Ok(Vec::new());
    };
    let mut names: Vec<String> = map
        .iter()
        .filter(|(_, record)| tokens_from(record).is_some())
        .map(|(name, _)| name.clone())
        .collect();
    names.sort();
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Point the state dir at a scratch directory for the duration of a test.
    /// Env is process-global, so these tests run under one mutex.
    fn with_scratch<T>(body: impl FnOnce(&std::path::Path) -> T) -> T {
        use std::sync::Mutex;
        static GUARD: Mutex<()> = Mutex::new(());
        let _lock = GUARD
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let scratch = tempfile::tempdir().expect("tempdir");
        let previous = std::env::var(STATE_DIR_ENV).ok();
        std::env::set_var(STATE_DIR_ENV, scratch.path());
        let result = body(scratch.path());
        match previous {
            Some(value) => std::env::set_var(STATE_DIR_ENV, value),
            None => std::env::remove_var(STATE_DIR_ENV),
        }
        result
    }

    fn tokens(expires_ms: u128) -> OAuthTokens {
        OAuthTokens {
            access: "access-token".into(),
            refresh: "refresh-token".into(),
            expires_ms,
        }
    }

    #[test]
    fn save_peek_clear_round_trip() {
        with_scratch(|_| {
            assert!(peek("codex").expect("peek").is_none());
            let stored = tokens(now_ms() + 3_600_000);
            save("codex", &stored).expect("save");
            assert_eq!(peek("codex").expect("peek").as_ref(), Some(&stored));
            assert_eq!(
                logged_in_providers().expect("list"),
                vec!["codex".to_string()]
            );
            assert!(clear("codex").expect("clear"));
            assert!(peek("codex").expect("peek").is_none());
            assert!(!clear("codex").expect("clear"));
        });
    }

    #[cfg(unix)]
    #[test]
    fn credentials_are_owner_only() {
        with_scratch(|_| {
            use std::os::unix::fs::PermissionsExt as _;
            save("codex", &tokens(now_ms() + 3_600_000)).expect("save");
            let mode = std::fs::metadata(auth_path().expect("path"))
                .expect("metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        });
    }

    #[test]
    fn a_live_token_is_returned_without_refreshing() {
        with_scratch(|_| {
            save("codex", &tokens(now_ms() + 3_600_000)).expect("save");
            // A refresh would need the network; reaching it would fail the test.
            assert_eq!(
                access_token("codex").expect("token"),
                Some("access-token".to_string())
            );
        });
    }

    #[test]
    fn a_missing_login_is_not_an_error() {
        with_scratch(|_| {
            assert_eq!(access_token("codex").expect("token"), None);
        });
    }

    #[test]
    fn unrelated_providers_survive_a_clear() {
        with_scratch(|_| {
            save("codex", &tokens(now_ms() + 3_600_000)).expect("save");
            save("other", &tokens(now_ms() + 3_600_000)).expect("save");
            clear("codex").expect("clear");
            assert!(peek("other").expect("peek").is_some());
        });
    }
}
