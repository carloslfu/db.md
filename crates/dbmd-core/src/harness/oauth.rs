// SPDX-License-Identifier: Apache-2.0

//! Subscription login by native OAuth — the ChatGPT (Codex) authorization-code
//! flow with PKCE, ported from the pi coding agent's implementation
//! (`@earendil-works/pi-ai`, MIT) so a ChatGPT Plus/Pro subscription can drive
//! `dbmd ask` with no vendor CLI installed.
//!
//! The flow, step for step (identical to pi's, which is in turn the documented
//! Codex CLI flow OpenAI endorses for third-party OSS clients):
//!
//! 1. Generate a PKCE verifier (32 random bytes, base64url) and its S256
//!    challenge, plus 16 random bytes of `state`.
//! 2. Open `https://auth.openai.com/oauth/authorize` in the user's browser
//!    with `client_id`, `redirect_uri=http://localhost:1455/auth/callback`,
//!    `scope=openid profile email offline_access`, the challenge, the state,
//!    `id_token_add_organizations=true`, `codex_cli_simplified_flow=true`,
//!    and `originator`.
//! 3. Serve `127.0.0.1:1455` until the browser redirects back with `code`
//!    (state-checked), or accept a pasted code/URL as the fallback.
//! 4. Exchange the code at `https://auth.openai.com/oauth/token` for an
//!    access + refresh token, and read `chatgpt_account_id` out of the access
//!    token's `https://api.openai.com/auth` JWT claim.
//!
//! **Honest client identity.** `originator` is `dbmd` on both the authorize
//! URL and every API request, and the User-Agent names dbmd — this toolkit
//! never presents itself as another vendor's first-party client. (pi's
//! Anthropic OAuth path is deliberately NOT ported: it only works by
//! injecting a "You are Claude Code" system block and `claude-code-*` beta
//! headers so a Claude subscription token is accepted, which is impersonation.
//! Claude subscriptions reach dbmd through [`super::delegate`] instead.)

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::time::Duration;

use serde_json::Value;

use super::HarnessError;

/// OpenAI's public Codex OAuth client id (the same public client the Codex
/// CLI and other OSS clients use; not a secret, and not a credential).
pub const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
const DEFAULT_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
/// Override for the token endpoint. Exists so the flow can be exercised
/// against a scripted local endpoint in tests; never set in normal use.
pub const TOKEN_URL_ENV: &str = "DBMD_OAUTH_TOKEN_URL";
const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const CALLBACK_PORT: u16 = 1455;
const SCOPE: &str = "openid profile email offline_access";
/// The JWT claim carrying ChatGPT auth details (account id, plan).
pub const JWT_CLAIM_PATH: &str = "https://api.openai.com/auth";
/// How this toolkit identifies itself to OpenAI. Never another client's name.
pub const ORIGINATOR: &str = "dbmd";

/// One set of OAuth credentials, as persisted by [`super::auth`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthTokens {
    /// Bearer token sent on API requests.
    pub access: String,
    /// Long-lived token used to mint a new access token.
    pub refresh: String,
    /// Absolute expiry, milliseconds since the Unix epoch.
    pub expires_ms: u128,
}

fn base64url(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn random_bytes(count: usize) -> Result<Vec<u8>, HarnessError> {
    use ring::rand::SecureRandom as _;
    let mut buffer = vec![0u8; count];
    ring::rand::SystemRandom::new()
        .fill(&mut buffer)
        .map_err(|_| HarnessError::Provider("cannot generate secure random bytes".to_string()))?;
    Ok(buffer)
}

/// A PKCE verifier and its S256 challenge.
#[derive(Debug, Clone)]
pub struct Pkce {
    /// The high-entropy verifier, replayed at token exchange.
    pub verifier: String,
    /// base64url(SHA-256(verifier)), sent on the authorize URL.
    pub challenge: String,
}

/// Generate a PKCE pair: 32 random bytes base64url-encoded as the verifier,
/// SHA-256 of those ASCII bytes as the challenge (pi's `generatePKCE`).
pub fn generate_pkce() -> Result<Pkce, HarnessError> {
    let verifier = base64url(&random_bytes(32)?);
    let digest = ring::digest::digest(&ring::digest::SHA256, verifier.as_bytes());
    Ok(Pkce {
        challenge: base64url(digest.as_ref()),
        verifier,
    })
}

/// 16 random bytes, hex-encoded — the CSRF `state` (pi's `createState`).
pub fn create_state() -> Result<String, HarnessError> {
    Ok(random_bytes(16)?
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// Build the browser authorization URL for one login attempt.
pub fn authorize_url(challenge: &str, state: &str) -> String {
    let query = [
        ("response_type", "code"),
        ("client_id", CODEX_CLIENT_ID),
        ("redirect_uri", REDIRECT_URI),
        ("scope", SCOPE),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
        ("state", state),
        ("id_token_add_organizations", "true"),
        ("codex_cli_simplified_flow", "true"),
        ("originator", ORIGINATOR),
    ]
    .iter()
    .map(|(key, value)| format!("{key}={}", urlencode(value)))
    .collect::<Vec<_>>()
    .join("&");
    format!("{AUTHORIZE_URL}?{query}")
}

/// Percent-encode a query value (RFC 3986 unreserved set kept verbatim).
fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Accept a pasted authorization code, a `code#state` pair, a query string, or
/// the whole redirect URL (pi's `parseAuthorizationInput`).
pub fn parse_authorization_input(input: &str) -> (Option<String>, Option<String>) {
    let value = input.trim();
    if value.is_empty() {
        return (None, None);
    }
    if let Some(query) = value
        .split_once("://")
        .and_then(|(_, rest)| rest.split_once('?'))
        .map(|(_, query)| query)
    {
        let (code, state) = parse_query_pair(query);
        if code.is_some() {
            return (code, state);
        }
    }
    if let Some((code, state)) = value.split_once('#') {
        return (Some(code.to_string()), Some(state.to_string()));
    }
    if value.contains("code=") {
        return parse_query_pair(value);
    }
    (Some(value.to_string()), None)
}

fn parse_query_pair(query: &str) -> (Option<String>, Option<String>) {
    let mut code = None;
    let mut state = None;
    for pair in query.split('&') {
        match pair.split_once('=') {
            Some(("code", value)) => code = Some(percent_decode(value)),
            Some(("state", value)) => state = Some(percent_decode(value)),
            _ => {}
        }
    }
    (code, state)
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(if bytes[index] == b'+' {
            b' '
        } else {
            bytes[index]
        });
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Decode a JWT payload without verifying its signature. The token is
/// presented to us over TLS by the issuer we just called; this only reads the
/// account id out of it, and the server re-validates on every request.
pub fn decode_jwt_payload(token: &str) -> Option<Value> {
    use base64::Engine as _;
    let payload = token.split('.').nth(1)?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    serde_json::from_slice(&decoded).ok()
}

/// The `chatgpt_account_id` claim required on every Codex API request.
pub fn account_id(access_token: &str) -> Option<String> {
    decode_jwt_payload(access_token)?
        .get(JWT_CLAIM_PATH)?
        .get("chatgpt_account_id")?
        .as_str()
        .filter(|id| !id.is_empty())
        .map(|id| id.to_string())
}

/// The subscription plan claim, when present (for a friendly login summary).
pub fn plan_type(access_token: &str) -> Option<String> {
    decode_jwt_payload(access_token)?
        .get(JWT_CLAIM_PATH)?
        .get("chatgpt_plan_type")?
        .as_str()
        .map(|plan| plan.to_string())
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis())
        .unwrap_or(0)
}

fn token_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .redirects(0)
        .timeout_connect(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .user_agent(&format!("dbmd/{}", env!("CARGO_PKG_VERSION")))
        .build()
}

fn read_tokens(body: &str) -> Result<OAuthTokens, HarnessError> {
    let json: Value = serde_json::from_str(body)
        .map_err(|error| HarnessError::Provider(format!("token response is not JSON: {error}")))?;
    let access = json.get("access_token").and_then(|v| v.as_str());
    let refresh = json.get("refresh_token").and_then(|v| v.as_str());
    let expires_in = json.get("expires_in").and_then(|v| v.as_u64());
    match (access, refresh, expires_in) {
        (Some(access), Some(refresh), Some(expires_in)) => Ok(OAuthTokens {
            access: access.to_string(),
            refresh: refresh.to_string(),
            expires_ms: now_ms() + u128::from(expires_in) * 1000,
        }),
        _ => Err(HarnessError::Provider(
            "token response is missing access_token, refresh_token, or expires_in".to_string(),
        )),
    }
}

/// The token endpoint (the documented OpenAI one unless overridden).
fn token_url() -> String {
    std::env::var(TOKEN_URL_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_TOKEN_URL.to_string())
}

fn post_form(pairs: &[(&str, &str)]) -> Result<String, HarnessError> {
    let body = pairs
        .iter()
        .map(|(key, value)| format!("{key}={}", urlencode(value)))
        .collect::<Vec<_>>()
        .join("&");
    let response = token_agent()
        .post(&token_url())
        .set("content-type", "application/x-www-form-urlencoded")
        .send_string(&body);
    match response {
        Ok(response) => response.into_string().map_err(|error| {
            HarnessError::Provider(format!("cannot read token response: {error}"))
        }),
        Err(ureq::Error::Status(status, response)) => {
            let mut detail = response.into_string().unwrap_or_default();
            detail.truncate(400);
            Err(HarnessError::Provider(format!(
                "OpenAI token endpoint returned HTTP {status}: {detail}"
            )))
        }
        Err(error) => Err(HarnessError::Provider(format!(
            "cannot reach the OpenAI token endpoint: {error}"
        ))),
    }
}

/// Exchange an authorization code for tokens (PKCE verifier replayed).
pub fn exchange_code(code: &str, verifier: &str) -> Result<OAuthTokens, HarnessError> {
    read_tokens(&post_form(&[
        ("grant_type", "authorization_code"),
        ("client_id", CODEX_CLIENT_ID),
        ("code", code),
        ("code_verifier", verifier),
        ("redirect_uri", REDIRECT_URI),
    ])?)
}

/// Mint a fresh access token from a refresh token.
pub fn refresh_tokens(refresh_token: &str) -> Result<OAuthTokens, HarnessError> {
    read_tokens(&post_form(&[
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", CODEX_CLIENT_ID),
    ])?)
}

/// The success/error pages the callback serves, kept deliberately plain.
fn callback_page(title: &str, message: &str) -> String {
    format!(
        "<!doctype html><meta charset=\"utf-8\"><title>{title}</title>\
         <body style=\"font-family:system-ui,sans-serif;padding:3rem;color:#1c2420\">\
         <h1 style=\"font-size:1.25rem\">{title}</h1><p>{message}</p></body>"
    )
}

/// Wait on `127.0.0.1:1455` for the browser redirect and return its `code`.
/// `state` must match or the request is refused. Returns `None` on timeout so
/// the caller can fall back to a pasted code.
pub fn wait_for_callback(state: &str, timeout: Duration) -> Result<Option<String>, HarnessError> {
    let listener = TcpListener::bind(("127.0.0.1", CALLBACK_PORT)).map_err(|error| {
        HarnessError::Provider(format!(
            "cannot listen on 127.0.0.1:{CALLBACK_PORT} for the OAuth callback \
             ({error}) — another login may be in progress"
        ))
    })?;
    listener
        .set_nonblocking(true)
        .map_err(|error| HarnessError::Provider(error.to_string()))?;
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        match listener.accept() {
            Ok((mut stream, _)) => {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let mut line = String::new();
                if BufReader::new(
                    stream
                        .try_clone()
                        .map_err(|error| HarnessError::Provider(error.to_string()))?,
                )
                .read_line(&mut line)
                .is_err()
                {
                    continue;
                }
                let target = line.split_whitespace().nth(1).unwrap_or("");
                let (path, query) = target.split_once('?').unwrap_or((target, ""));
                let (code, got_state) = parse_query_pair(query);
                let (status, page) = if path != "/auth/callback" {
                    (
                        "404 Not Found",
                        callback_page("Not found", "Unexpected callback route."),
                    )
                } else if got_state.as_deref() != Some(state) {
                    (
                        "400 Bad Request",
                        callback_page("Login failed", "State mismatch."),
                    )
                } else if code.is_none() {
                    (
                        "400 Bad Request",
                        callback_page("Login failed", "Missing authorization code."),
                    )
                } else {
                    (
                        "200 OK",
                        callback_page(
                            "Signed in to dbmd",
                            "OpenAI authentication completed. You can close this window.",
                        ),
                    )
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: text/html; charset=utf-8\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n{page}",
                    page.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
                if let Some(code) = code {
                    if got_state.as_deref() == Some(state) && path == "/auth/callback" {
                        return Ok(Some(code));
                    }
                }
            }
            Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(120));
            }
            Err(error) => return Err(HarnessError::Provider(error.to_string())),
        }
    }
    Ok(None)
}

/// Best-effort browser launch. A failure is not fatal: the URL is printed and
/// the pasted-code path still works (headless machines, SSH sessions).
pub fn open_browser(url: &str) -> bool {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(windows) {
        "explorer"
    } else {
        "xdg-open"
    };
    std::process::Command::new(opener)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_is_sha256_of_the_verifier() {
        let pkce = generate_pkce().expect("pkce");
        let expected = base64url(
            ring::digest::digest(&ring::digest::SHA256, pkce.verifier.as_bytes()).as_ref(),
        );
        assert_eq!(pkce.challenge, expected);
        // base64url, no padding, 32 bytes in and 32 bytes of digest out.
        assert_eq!(pkce.verifier.len(), 43);
        assert_eq!(pkce.challenge.len(), 43);
        assert!(!pkce.verifier.contains('=') && !pkce.verifier.contains('+'));
    }

    #[test]
    fn state_is_sixteen_random_hex_bytes() {
        let state = create_state().expect("state");
        assert_eq!(state.len(), 32);
        assert!(state.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(state, create_state().expect("state"));
    }

    #[test]
    fn authorize_url_carries_the_documented_parameters() {
        let url = authorize_url("CHALLENGE", "STATE");
        assert!(url.starts_with("https://auth.openai.com/oauth/authorize?"));
        for expected in [
            "response_type=code",
            "client_id=app_EMoamEEZ73f0CkXaXp7hrann",
            "code_challenge=CHALLENGE",
            "code_challenge_method=S256",
            "state=STATE",
            "id_token_add_organizations=true",
            "codex_cli_simplified_flow=true",
            "originator=dbmd",
            "redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback",
        ] {
            assert!(url.contains(expected), "missing {expected} in {url}");
        }
        // Never present as another vendor's client.
        assert!(!url.contains("originator=codex"));
    }

    #[test]
    fn authorization_input_accepts_every_paste_shape() {
        assert_eq!(
            parse_authorization_input("https://localhost:1455/auth/callback?code=abc&state=xyz"),
            (Some("abc".into()), Some("xyz".into()))
        );
        assert_eq!(
            parse_authorization_input("abc#xyz"),
            (Some("abc".into()), Some("xyz".into()))
        );
        assert_eq!(
            parse_authorization_input("code=abc&state=xyz"),
            (Some("abc".into()), Some("xyz".into()))
        );
        assert_eq!(
            parse_authorization_input("  bare-code  "),
            (Some("bare-code".into()), None)
        );
        assert_eq!(parse_authorization_input("   "), (None, None));
    }

    #[test]
    fn account_id_and_plan_come_out_of_the_jwt_claim() {
        let payload = serde_json::json!({
            JWT_CLAIM_PATH: { "chatgpt_account_id": "acct_123", "chatgpt_plan_type": "plus" }
        });
        let encoded = base64url(payload.to_string().as_bytes());
        let token = format!("header.{encoded}.signature");
        assert_eq!(account_id(&token).as_deref(), Some("acct_123"));
        assert_eq!(plan_type(&token).as_deref(), Some("plus"));
        assert!(account_id("not-a-jwt").is_none());
    }
}
