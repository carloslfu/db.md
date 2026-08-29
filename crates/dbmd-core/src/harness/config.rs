// SPDX-License-Identifier: Apache-2.0

//! Provider resolution for the embedded harness — flag > env > store-local
//! `.dbmd/config` > local autodetect, mirroring the hub client's
//! `hub_config` discipline exactly, including its central security rule:
//!
//! **The credential is environment-only** (`DBMD_LLM_KEY`, or a preset's
//! conventional key variable), never a file inside the store — a secret in
//! the store tree is one sync away from leaking. And when the *endpoint*
//! came from store-local config (untrusted: it syncs with the store), an
//! ambient key is refused unless `DBMD_LLM_KEY_ORIGIN` explicitly binds it
//! to that exact origin — otherwise a cloned store could point the harness
//! at an attacker endpoint and harvest the user's key.
//!
//! There is no default vendor: presets are inert rows (base URL + protocol +
//! conventional key variable), and with nothing configured the resolver
//! probes the well-known LOCAL servers only (Ollama, LM Studio, llama.cpp) —
//! zero-config never means "silently pick a cloud".

use std::path::Path;
use std::time::Duration;

use super::{HarnessError, Protocol, Provider};

/// Env var: explicit provider preset name (or a delegation backend).
pub const PROVIDER_ENV: &str = "DBMD_LLM_PROVIDER";
/// Env var: endpoint base URL.
pub const BASE_URL_ENV: &str = "DBMD_LLM_BASE_URL";
/// Env var: wire protocol (`openai` | `anthropic`).
pub const PROTOCOL_ENV: &str = "DBMD_LLM_PROTOCOL";
/// Env var: model id.
pub const MODEL_ENV: &str = "DBMD_LLM_MODEL";
/// Env var: the credential. The ONLY file-independent key source; wins over
/// preset key variables.
pub const KEY_ENV: &str = "DBMD_LLM_KEY";
/// Env var: origin binding required before an ambient key may be sent to an
/// endpoint selected by store-local config (e.g. `https://llm.example.com`).
pub const KEY_ORIGIN_ENV: &str = "DBMD_LLM_KEY_ORIGIN";
/// Env var: allow plain http to a NON-loopback endpoint (LAN Ollama etc.).
pub const ALLOW_INSECURE_ENV: &str = "DBMD_LLM_ALLOW_INSECURE_HTTP";

/// `.dbmd/config` keys (non-secret knobs only — a key line in this file is
/// never read).
const CONFIG_KEYS: [&str; 4] = ["llm_provider", "llm_base_url", "llm_protocol", "llm_model"];

/// A store may also pin a value per provider (`llm_model_ollama`,
/// `llm_model_codex`), which survives an explicit `--provider` override.
fn is_config_key(key: &str) -> bool {
    CONFIG_KEYS.contains(&key)
        || CONFIG_KEYS
            .iter()
            .any(|base| key.starts_with(&format!("{base}_")))
}

/// Flag-level overrides from the CLI (all optional).
#[derive(Debug, Default, Clone)]
pub struct Overrides {
    /// `--provider` — preset name or delegation backend.
    pub provider: Option<String>,
    /// `--base-url`.
    pub base_url: Option<String>,
    /// `--protocol`.
    pub protocol: Option<String>,
    /// `--model`.
    pub model: Option<String>,
}

/// One inert preset row: a name, the protocol it speaks, its base URL, and
/// the conventional key variable users of that provider already export.
#[derive(Debug)]
struct Preset {
    name: &'static str,
    protocol: Protocol,
    base_url: &'static str,
    key_env: Option<&'static str>,
}

/// The preset table. Data, not code: every row rides one of the two wire
/// adapters (or a delegation backend). No row is a default.
const PRESETS: [Preset; 13] = [
    Preset {
        name: "anthropic",
        protocol: Protocol::Anthropic,
        base_url: "https://api.anthropic.com",
        key_env: Some("ANTHROPIC_API_KEY"),
    },
    Preset {
        name: "openai",
        protocol: Protocol::OpenAi,
        base_url: "https://api.openai.com/v1",
        key_env: Some("OPENAI_API_KEY"),
    },
    Preset {
        name: "openrouter",
        protocol: Protocol::OpenAi,
        base_url: "https://openrouter.ai/api/v1",
        key_env: Some("OPENROUTER_API_KEY"),
    },
    Preset {
        name: "groq",
        protocol: Protocol::OpenAi,
        base_url: "https://api.groq.com/openai/v1",
        key_env: Some("GROQ_API_KEY"),
    },
    Preset {
        name: "together",
        protocol: Protocol::OpenAi,
        base_url: "https://api.together.xyz/v1",
        key_env: Some("TOGETHER_API_KEY"),
    },
    Preset {
        name: "deepseek",
        protocol: Protocol::OpenAi,
        base_url: "https://api.deepseek.com/v1",
        key_env: Some("DEEPSEEK_API_KEY"),
    },
    Preset {
        name: "mistral",
        protocol: Protocol::OpenAi,
        base_url: "https://api.mistral.ai/v1",
        key_env: Some("MISTRAL_API_KEY"),
    },
    Preset {
        name: "ollama",
        protocol: Protocol::OpenAi,
        base_url: "http://127.0.0.1:11434/v1",
        key_env: None,
    },
    Preset {
        name: "lmstudio",
        protocol: Protocol::OpenAi,
        base_url: "http://127.0.0.1:1234/v1",
        key_env: None,
    },
    Preset {
        name: "llamacpp",
        protocol: Protocol::OpenAi,
        base_url: "http://127.0.0.1:8080/v1",
        key_env: None,
    },
    Preset {
        name: "codex",
        protocol: Protocol::Codex,
        base_url: super::codex::DEFAULT_BASE_URL,
        key_env: None,
    },
    Preset {
        name: "claude-code",
        protocol: Protocol::ClaudeCli,
        base_url: "",
        key_env: None,
    },
    Preset {
        name: "codex-cli",
        protocol: Protocol::CodexCli,
        base_url: "",
        key_env: None,
    },
];

/// The default model for a ChatGPT-subscription session when the user names
/// none. ChatGPT accounts accept only the models their plan exposes through
/// Codex, and that set moves: a rejected name comes back as a plain HTTP 400
/// naming the model, and `--model` (or `llm_model_codex` in `.dbmd/config`)
/// picks another without a new release.
pub const CODEX_DEFAULT_MODEL: &str = "gpt-5.6-sol";

/// Where a resolved value came from — the origin-binding rule keys on this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Flag,
    Env,
    StoreConfig,
    Preset,
    Autodetect,
}

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

/// Parse the harness keys out of `.dbmd/config` (`key = value` lines, `#`
/// comments). Unknown keys are ignored; a `llm_key`-looking line is
/// deliberately NOT a key source.
fn store_config(store_root: &Path) -> Vec<(String, String)> {
    let path = store_root.join(".dbmd").join("config");
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim();
            let value = value.trim();
            if is_config_key(key) && !value.is_empty() {
                out.push((key.to_string(), value.to_string()));
            }
        }
    }
    out
}

fn config_value(config: &[(String, String)], key: &str) -> Option<String> {
    config
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.clone())
}

fn parse_protocol(raw: &str) -> Result<Protocol, HarnessError> {
    match raw {
        "openai" | "openai-completions" => Ok(Protocol::OpenAi),
        "anthropic" | "anthropic-messages" => Ok(Protocol::Anthropic),
        "codex" | "codex-responses" => Ok(Protocol::Codex),
        other => Err(HarnessError::Config(format!(
            "unknown protocol `{other}` — use `openai`, `anthropic`, or `codex`"
        ))),
    }
}

fn find_preset(name: &str) -> Result<&'static Preset, HarnessError> {
    PRESETS.iter().find(|p| p.name == name).ok_or_else(|| {
        let names: Vec<&str> = PRESETS.iter().map(|p| p.name).collect();
        HarnessError::Config(format!(
            "unknown provider `{name}` — known presets: {}",
            names.join(", ")
        ))
    })
}

/// The scheme+authority prefix of a URL, for origin binding and loopback
/// checks. Not a general URL parser — enough for `http(s)://host[:port]`.
fn origin_of(url: &str) -> String {
    let after_scheme = url.find("://").map(|i| i + 3).unwrap_or(0);
    let end = url[after_scheme..]
        .find('/')
        .map(|i| after_scheme + i)
        .unwrap_or(url.len());
    url[..end].trim_end_matches('/').to_string()
}

fn is_loopback_url(url: &str) -> bool {
    let origin = origin_of(url);
    let host_port = origin.split("://").nth(1).unwrap_or(&origin);
    let host = host_port
        .rsplit_once(':')
        .map(|(h, _)| h)
        .unwrap_or(host_port);
    matches!(
        host,
        "127.0.0.1" | "localhost" | "[::1]" | "::1" | "0.0.0.0"
    )
}

/// Resolve the provider for a run. `store_root` supplies `.dbmd/config`.
pub fn resolve(store_root: &Path, overrides: &Overrides) -> Result<Provider, HarnessError> {
    let config = store_config(store_root);

    // ── provider preset ──────────────────────────────────────────────────
    // A provider named by flag or env OVERRIDES the store's configured setup.
    // The store's other `llm_*` values were written for whatever provider that
    // store expects (a local model name, say), so they must not silently ride
    // to a different one — a ChatGPT account rejecting `qwen3.8-27b-32k` is the
    // friendly version of that mistake; a wrong endpoint would be the ugly one.
    // A provider-scoped key (`llm_model_codex`) is honored either way.
    let overriding_provider = overrides
        .provider
        .clone()
        .or_else(|| env_nonempty(PROVIDER_ENV));
    let provider_is_overridden = overriding_provider.is_some();
    let preset_name = overriding_provider.or_else(|| config_value(&config, "llm_provider"));
    let preset: Option<&Preset> = match &preset_name {
        Some(name) => Some(find_preset(name)?),
        None => None,
    };
    // Store-local values that still apply after an explicit override: only the
    // ones scoped to the provider actually in use.
    let scoped = |config: &[(String, String)], key: &str| -> Option<String> {
        let name = preset_name.as_deref()?;
        config_value(config, &format!("{key}_{name}"))
    };
    let store_value = |config: &[(String, String)], key: &str| -> Option<String> {
        scoped(config, key).or_else(|| {
            if provider_is_overridden {
                None
            } else {
                config_value(config, key)
            }
        })
    };

    if let Some(preset) = preset {
        if preset.protocol.is_delegate() {
            return Ok(Provider {
                protocol: preset.protocol,
                base_url: String::new(),
                model: String::new(),
                key: None,
                source: format!("delegation to the `{}` CLI", preset.name),
            });
        }
        // The ChatGPT backend takes a subscription token from `dbmd login
        // codex`, never an API key: the credential comes from the toolkit
        // state dir (refreshed in place), not from the environment.
        if preset.protocol == Protocol::Codex {
            let access = super::auth::access_token("codex")?.ok_or_else(|| {
                HarnessError::Config(
                    "not signed in to ChatGPT — run `dbmd login codex` to use your \
                     subscription (or `--provider codex-cli` to delegate to an \
                     installed, logged-in codex CLI)"
                        .to_string(),
                )
            })?;
            let plan = super::oauth::plan_type(&access)
                .map(|plan| format!(" ({plan})"))
                .unwrap_or_default();
            return Ok(Provider {
                protocol: Protocol::Codex,
                base_url: overrides
                    .base_url
                    .clone()
                    .or_else(|| env_nonempty(BASE_URL_ENV))
                    .unwrap_or_else(|| preset.base_url.to_string()),
                model: overrides
                    .model
                    .clone()
                    .or_else(|| env_nonempty(MODEL_ENV))
                    .or_else(|| store_value(&config, "llm_model"))
                    .unwrap_or_else(|| CODEX_DEFAULT_MODEL.to_string()),
                key: Some(access),
                source: format!("ChatGPT subscription{plan}"),
            });
        }
    }

    // ── base URL (with provenance) ───────────────────────────────────────
    let (base_url, url_source) = if let Some(url) = overrides.base_url.clone() {
        (Some(url), Source::Flag)
    } else if let Some(url) = env_nonempty(BASE_URL_ENV) {
        (Some(url), Source::Env)
    } else if let Some(url) = store_value(&config, "llm_base_url") {
        (Some(url), Source::StoreConfig)
    } else if let Some(preset) = preset {
        (Some(preset.base_url.to_string()), Source::Preset)
    } else {
        (None, Source::Autodetect)
    };

    // ── protocol ─────────────────────────────────────────────────────────
    let protocol = if let Some(raw) = overrides
        .protocol
        .clone()
        .or_else(|| env_nonempty(PROTOCOL_ENV))
        .or_else(|| store_value(&config, "llm_protocol"))
    {
        Some(parse_protocol(&raw)?)
    } else {
        preset.map(|p| p.protocol)
    };

    // ── model ────────────────────────────────────────────────────────────
    let model = overrides
        .model
        .clone()
        .or_else(|| env_nonempty(MODEL_ENV))
        .or_else(|| store_value(&config, "llm_model"));

    // ── key: ENVIRONMENT ONLY ────────────────────────────────────────────
    let key =
        env_nonempty(KEY_ENV).or_else(|| preset.and_then(|p| p.key_env).and_then(env_nonempty));

    let (base_url, protocol, model, source) = match base_url {
        Some(url) => {
            let url = url.trim_end_matches('/').to_string();
            let protocol = protocol.ok_or_else(|| {
                HarnessError::Config(
                    "an endpoint is configured but its protocol is not — set \
                     `--protocol openai|anthropic` (or DBMD_LLM_PROTOCOL / \
                     `llm_protocol` in .dbmd/config)"
                        .to_string(),
                )
            })?;
            let model = model.ok_or_else(|| {
                HarnessError::Config(
                    "no model configured — set `--model` (or DBMD_LLM_MODEL / \
                     `llm_model` in .dbmd/config)"
                        .to_string(),
                )
            })?;
            let source = match url_source {
                Source::Flag => "endpoint from --base-url".to_string(),
                Source::Env => format!("endpoint from {BASE_URL_ENV}"),
                Source::StoreConfig => "endpoint from .dbmd/config".to_string(),
                Source::Preset => format!("preset {}", preset.map(|p| p.name).unwrap_or_default()),
                Source::Autodetect => unreachable!("Some(url) never carries Autodetect"),
            };
            (url, protocol, model, source)
        }
        None => {
            let found = autodetect(model.as_deref())?;
            (found.base_url, Protocol::OpenAi, found.model, found.source)
        }
    };

    // ── the origin-binding rule ──────────────────────────────────────────
    // An endpoint selected by store-local config is untrusted (it syncs with
    // the store); refusing to pair it with an ambient key unless the user
    // explicitly bound that key to this exact origin closes the cloned-store
    // key-exfiltration hole. Mirrors the hub client's UnboundCredential rule.
    if key.is_some() && url_source == Source::StoreConfig {
        let origin = origin_of(&base_url);
        let bound = env_nonempty(KEY_ORIGIN_ENV);
        if bound.as_deref().map(|b| b.trim_end_matches('/')) != Some(origin.as_str()) {
            return Err(HarnessError::Config(format!(
                "refusing to send a credential to `{origin}`: the endpoint came \
                 from store-local .dbmd/config (which syncs with the store), so \
                 the ambient key must be explicitly bound — set \
                 {KEY_ORIGIN_ENV}={origin} to allow it"
            )));
        }
    }

    // ── scheme discipline ────────────────────────────────────────────────
    if base_url.starts_with("http://")
        && !is_loopback_url(&base_url)
        && env_nonempty(ALLOW_INSECURE_ENV).as_deref() != Some("1")
    {
        return Err(HarnessError::Config(format!(
            "refusing plain http to a non-loopback endpoint ({}) — use https, \
             or set {ALLOW_INSECURE_ENV}=1 for a trusted LAN server",
            origin_of(&base_url)
        )));
    }
    if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
        return Err(HarnessError::Config(format!(
            "endpoint must be an http(s) URL, got `{base_url}`"
        )));
    }

    Ok(Provider {
        protocol,
        base_url,
        model,
        key,
        source,
    })
}

struct Detected {
    base_url: String,
    model: String,
    source: String,
}

/// Probe the well-known local servers (Ollama, LM Studio, llama.cpp) with a
/// short connect timeout. `wanted_model` (from flag/env/config) is trusted
/// verbatim when set; otherwise a single available model is chosen and
/// multiple models are an error listing the choices.
fn autodetect(wanted_model: Option<&str>) -> Result<Detected, HarnessError> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_millis(400))
        .timeout_read(Duration::from_secs(3))
        .redirects(0)
        .build();

    let candidates: [(&str, &str, &str); 3] = [
        ("ollama", "http://127.0.0.1:11434", "/api/tags"),
        ("lmstudio", "http://127.0.0.1:1234", "/v1/models"),
        ("llama.cpp", "http://127.0.0.1:8080", "/v1/models"),
    ];
    for (name, origin, probe) in candidates {
        let Ok(response) = agent.get(&format!("{origin}{probe}")).call() else {
            continue;
        };
        let Ok(body) = response.into_string() else {
            continue;
        };
        let Ok(json) = serde_json::from_str::<serde_json::Value>(&body) else {
            continue;
        };
        let mut models: Vec<String> = Vec::new();
        if let Some(list) = json.get("models").and_then(|m| m.as_array()) {
            for entry in list {
                if let Some(model) = entry.get("name").and_then(|n| n.as_str()) {
                    models.push(model.to_string());
                }
            }
        }
        if let Some(list) = json.get("data").and_then(|d| d.as_array()) {
            for entry in list {
                if let Some(model) = entry.get("id").and_then(|i| i.as_str()) {
                    models.push(model.to_string());
                }
            }
        }
        let base_url = format!("{origin}/v1");
        let model = match wanted_model {
            Some(model) => model.to_string(),
            None if models.len() == 1 => models.remove(0),
            None if models.is_empty() => {
                return Err(HarnessError::Config(format!(
                    "found a local {name} server at {origin} but it has no \
                     models — pull one, or set `--model`"
                )));
            }
            None => {
                models.truncate(20);
                return Err(HarnessError::Config(format!(
                    "found a local {name} server at {origin} with several \
                     models — pick one with `--model` (or DBMD_LLM_MODEL): {}",
                    models.join(", ")
                )));
            }
        };
        return Ok(Detected {
            base_url,
            model,
            source: format!("autodetected local {name} at {origin}"),
        });
    }
    Err(HarnessError::Config(
        "no model endpoint configured and no local server found — set \
         `--provider <preset>` with its key, `--base-url` + `--protocol` + \
         `--model`, or start a local server (Ollama, LM Studio, llama.cpp)"
            .to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_extraction() {
        assert_eq!(
            origin_of("https://api.example.com/v1/messages"),
            "https://api.example.com"
        );
        assert_eq!(origin_of("http://127.0.0.1:8080"), "http://127.0.0.1:8080");
    }

    #[test]
    fn loopback_detection() {
        assert!(is_loopback_url("http://127.0.0.1:11434/v1"));
        assert!(is_loopback_url("http://localhost:1234/v1"));
        assert!(!is_loopback_url("http://192.168.1.20:11434/v1"));
    }

    #[test]
    fn unknown_preset_lists_choices() {
        let error = find_preset("nope").expect_err("unknown");
        assert!(error.to_string().contains("ollama"));
    }
}
