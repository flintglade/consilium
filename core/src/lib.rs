pub mod anthropic_api;
pub mod api;
mod child_io;
pub mod claude;
pub mod cli;
pub mod codex;
pub mod gemini;
pub mod google_api;
pub mod open_models;
pub mod providers;
pub mod routing;

use anthropic_api::AnthropicApiClient;
use api::{ApiClient, Message, StreamEvent, StreamEventSender};
use claude::ClaudeBackend;
use cli::{CliBackend, ImageAttachment};
use codex::CodexBackend;
use color_eyre::eyre::Result;
use gemini::GeminiBackend;
use google_api::GoogleApiClient;

pub const GROK_DEFAULT_MODEL: &str = "grok-4.5";

/// Installed desktop/TUI configuration file location for the current user.
/// The path is exposed for onboarding UI; Consilium still only reads it and
/// never writes credentials on the user's behalf.
pub fn user_config_env_path() -> Option<std::path::PathBuf> {
    child_io::user_config_env_path()
}

/// Chat backend: grok CLI (SuperGrok subscription, the default when the
/// CLI is installed) or direct HTTP with an API key. Force one with
/// GROK_BACKEND=cli|api.
#[derive(Clone)]
pub enum Backend {
    Cli(CliBackend),
    Claude(ClaudeBackend),
    Codex(CodexBackend),
    Gemini(GeminiBackend),
    Api(ApiClient),
    AnthropicApi(AnthropicApiClient),
    GoogleApi(GoogleApiClient),
}

impl Backend {
    pub fn provider_id(&self) -> &str {
        match self {
            Backend::Cli(_) => "grok",
            Backend::Claude(_) => "claude",
            Backend::Codex(_) => "codex",
            Backend::Gemini(_) => "gemini",
            Backend::Api(client) => client.provider_id(),
            Backend::AnthropicApi(_) => "anthropic-api",
            Backend::GoogleApi(_) => "gemini-api",
        }
    }

    pub fn mode_label(&self) -> &str {
        match self {
            Backend::Cli(_) => "SuperGrok OAuth",
            Backend::Claude(_) => "Claude.ai OAuth",
            Backend::Codex(_) => "ChatGPT OAuth",
            Backend::Gemini(_) => "Google OAuth",
            Backend::Api(client) => client.mode_label(),
            Backend::AnthropicApi(_) => "Anthropic API key",
            Backend::GoogleApi(_) => "Google AI API key",
        }
    }

    pub fn model_label(&self) -> String {
        match self {
            Backend::Cli(backend) => backend
                .model()
                .unwrap_or_else(|| format!("default ({GROK_DEFAULT_MODEL})")),
            Backend::Claude(backend) => backend
                .model()
                .unwrap_or_else(|| "Claude Code default".to_string()),
            Backend::Codex(backend) => backend
                .model()
                .unwrap_or_else(|| "Codex default".to_string()),
            Backend::Gemini(backend) => backend
                .model()
                .unwrap_or_else(|| "Gemini via Antigravity".to_string()),
            Backend::Api(client) => client.model(),
            Backend::AnthropicApi(client) => client.model(),
            Backend::GoogleApi(client) => client.model(),
        }
    }

    /// Short session identifier for display, or "new" before the first reply.
    pub fn session_label(&self) -> String {
        match self {
            Backend::Cli(backend) => backend
                .session_id()
                .map(|id| id.chars().take(8).collect())
                .unwrap_or_else(|| "new".to_string()),
            Backend::Claude(backend) => backend
                .session_id()
                .map(|id| id.chars().take(8).collect())
                .unwrap_or_else(|| "new".to_string()),
            Backend::Codex(backend) => backend
                .session_id()
                .map(|id| id.chars().take(8).collect())
                .unwrap_or_else(|| "new".to_string()),
            Backend::Gemini(_) => "stateless".to_string(),
            Backend::Api(_) => "stateless".to_string(),
            Backend::AnthropicApi(_) | Backend::GoogleApi(_) => "stateless".to_string(),
        }
    }

    pub fn reset_session(&self) {
        match self {
            Backend::Cli(backend) => backend.reset_session(),
            Backend::Claude(backend) => backend.reset_session(),
            Backend::Codex(backend) => backend.reset_session(),
            _ => {}
        }
    }

    /// Full grok session id, if one is active (Cli mode only).
    pub fn session_id(&self) -> Option<String> {
        match self {
            Backend::Cli(backend) => backend.session_id(),
            Backend::Claude(backend) => backend.session_id(),
            Backend::Codex(backend) => backend.session_id(),
            Backend::Gemini(_) => None,
            Backend::Api(_) => None,
            Backend::AnthropicApi(_) | Backend::GoogleApi(_) => None,
        }
    }

    /// Resume an existing grok session (Cli mode; no-op for Api).
    pub fn set_session(&self, id: Option<String>, agent: Option<bool>, effort: Option<String>) {
        match self {
            Backend::Cli(backend) => backend.set_session_context(id, agent, effort),
            Backend::Claude(backend) => {
                backend.set_session_context(id, agent);
                backend.set_effort(effort);
            }
            Backend::Codex(backend) => {
                backend.set_session_context(id, agent);
                backend.set_effort(effort);
            }
            _ => {}
        }
    }

    /// Set the model for future messages. Grok binds a session to an
    /// agent type at creation and refuses to switch models within it
    /// (MODEL_SWITCH_INCOMPATIBLE_AGENT), so changing model resets the
    /// session — the caller should also clear the visible transcript.
    pub fn set_model(&self, model: &str) -> Result<(), String> {
        match self {
            Backend::Cli(backend) => {
                let model = model.trim();
                if model.is_empty() || model.eq_ignore_ascii_case("default") {
                    backend.set_model(None);
                } else {
                    backend.set_model(Some(model.to_string()));
                }
                backend.reset_session();
                Ok(())
            }
            Backend::Claude(backend) => {
                let model = normalized_model(model);
                backend.set_model(model);
                Ok(())
            }
            Backend::Codex(backend) => {
                let model = normalized_model(model);
                backend.set_model(model);
                Ok(())
            }
            Backend::Gemini(backend) => {
                let model = model.trim();
                if model.is_empty()
                    || model.eq_ignore_ascii_case("default")
                    || model == "gemini-cli-default"
                    || model == "antigravity-default"
                {
                    backend.set_model(None);
                } else {
                    backend.set_model(Some(model.to_string()));
                }
                Ok(())
            }
            Backend::Api(client) => {
                let Some(model) = normalized_model(model) else {
                    return Err("API routes require an explicit model ID".to_string());
                };
                client.set_model(model);
                Ok(())
            }
            Backend::AnthropicApi(client) => {
                let Some(model) = normalized_model(model) else {
                    return Err("Anthropic API requires an explicit model ID".to_string());
                };
                client.set_model(model);
                Ok(())
            }
            Backend::GoogleApi(client) => {
                let Some(model) = normalized_model(model) else {
                    return Err("Google AI API requires an explicit model ID".to_string());
                };
                client.set_model(model);
                Ok(())
            }
        }
    }

    /// Restore the model associated with a saved CLI session without
    /// resetting that session. Normal user-driven model changes must still
    /// go through `set_model`, which clears incompatible context.
    pub fn restore_model_for_session(&self, model: Option<String>) -> Result<(), String> {
        match self {
            Backend::Cli(backend) => {
                backend.set_model(normalized_restored_model(model, &[]));
                Ok(())
            }
            Backend::Claude(backend) => {
                backend.set_model(normalized_restored_model(model, &[]));
                Ok(())
            }
            Backend::Codex(backend) => {
                backend.set_model(normalized_restored_model(model, &[]));
                Ok(())
            }
            Backend::Gemini(backend) => {
                backend.set_model(normalized_restored_model(
                    model,
                    &["gemini-cli-default", "antigravity-default"],
                ));
                Ok(())
            }
            Backend::Api(client) => {
                if model.is_some() {
                    let model = normalized_restored_model(model, &[]).ok_or_else(|| {
                        "A saved direct API route requires an explicit model ID".to_string()
                    })?;
                    client.set_model(model);
                }
                Ok(())
            }
            Backend::AnthropicApi(client) => {
                if model.is_some() {
                    let model = normalized_restored_model(model, &[]).ok_or_else(|| {
                        "A saved Anthropic API route requires an explicit model ID".to_string()
                    })?;
                    client.set_model(model);
                }
                Ok(())
            }
            Backend::GoogleApi(client) => {
                if model.is_some() {
                    let model = normalized_restored_model(model, &[]).ok_or_else(|| {
                        "A saved Google AI API route requires an explicit model ID".to_string()
                    })?;
                    client.set_model(model);
                }
                Ok(())
            }
        }
    }

    pub fn effort_label(&self) -> String {
        match self {
            Backend::Cli(backend) => backend.effort().unwrap_or_else(|| "default".to_string()),
            Backend::Claude(backend) => backend.effort().unwrap_or_else(|| "default".to_string()),
            Backend::Codex(backend) => backend.effort().unwrap_or_else(|| "default".to_string()),
            Backend::Gemini(_) => "default".to_string(),
            Backend::Api(_) | Backend::AnthropicApi(_) | Backend::GoogleApi(_) => {
                "default".to_string()
            }
        }
    }

    /// Reasoning effort, or None for the selected model's default.
    pub fn set_effort(&self, effort: Option<String>) -> Result<(), String> {
        match self {
            Backend::Cli(backend) => {
                backend.set_effort(effort);
                Ok(())
            }
            Backend::Claude(backend) => {
                backend.set_effort(effort);
                Ok(())
            }
            Backend::Codex(backend) => {
                backend.set_effort(effort);
                Ok(())
            }
            Backend::Gemini(_) => {
                if effort.is_some() {
                    Err(
                        "Gemini via Antigravity does not expose CONSILIUM thinking levels"
                            .to_string(),
                    )
                } else {
                    Ok(())
                }
            }
            Backend::Api(_) | Backend::AnthropicApi(_) | Backend::GoogleApi(_) => {
                if effort.is_some() {
                    Err("This direct API adapter does not expose a portable reasoning-effort control".to_string())
                } else {
                    Ok(())
                }
            }
        }
    }

    /// API routes receive full structured history. A CLI with an active
    /// provider-native session receives only the latest turn; a fresh CLI
    /// route receives a labeled transcript so an automatic fallback does not
    /// silently lose the conversation that preceded it.
    pub fn stream_chat(
        &self,
        messages: Vec<Message>,
        images: Vec<ImageAttachment>,
        agent: bool,
        cancel: tokio_util::sync::CancellationToken,
        tx: StreamEventSender,
    ) {
        match self {
            Backend::Cli(backend) => {
                let has_native_session = backend.session_id().is_some();
                let prompt = cli_prompt(&messages, has_native_session);
                let fresh_prompt = cli_prompt(&messages, false);
                let backend = backend.clone();
                tokio::spawn(async move {
                    backend
                        .stream_chat_with_fresh_prompt(
                            prompt,
                            fresh_prompt,
                            images,
                            agent,
                            cancel,
                            tx,
                        )
                        .await;
                });
            }
            Backend::Claude(backend) => {
                if !images.is_empty() {
                    send_preflight_error(
                        tx,
                        "Claude Code image attachments are not wired through Consilium yet."
                            .to_string(),
                    );
                    return;
                }
                // A Claude session is bound to the mode that created it. Reset
                // an incompatible session before deciding whether the prompt
                // can rely on provider-native history; otherwise an
                // Agent-to-Chat switch would send only the final user turn to
                // a freshly reset Chat session.
                backend.prepare_mode(agent);
                let prompt = cli_prompt(&messages, backend.session_id().is_some());
                let backend = backend.clone();
                tokio::spawn(async move {
                    backend.stream_chat(prompt, agent, cancel, tx).await;
                });
            }
            Backend::Codex(backend) => {
                if !images.is_empty() {
                    send_preflight_error(
                        tx,
                        "Codex image attachments are not wired through Consilium yet.".to_string(),
                    );
                    return;
                }
                backend.prepare_mode(agent);
                let prompt = cli_prompt(&messages, backend.session_id().is_some());
                let backend = backend.clone();
                tokio::spawn(async move {
                    backend.stream_chat(prompt, agent, cancel, tx).await;
                });
            }
            Backend::Gemini(backend) => {
                let backend = backend.clone();
                tokio::spawn(async move {
                    backend
                        .stream_chat(messages, images, agent, cancel, tx)
                        .await;
                });
            }
            Backend::Api(client) => {
                if !images.is_empty() {
                    send_preflight_error(
                        tx,
                        "Image attachments need subscription (grok CLI) mode.".to_string(),
                    );
                    return;
                }
                let client = client.clone();
                tokio::spawn(async move {
                    client.stream_chat(messages, cancel, tx).await;
                });
            }
            Backend::AnthropicApi(client) => {
                if !images.is_empty() {
                    send_preflight_error(
                        tx,
                        "Anthropic API image attachments are not wired through Consilium yet."
                            .to_string(),
                    );
                    return;
                }
                let client = client.clone();
                tokio::spawn(async move {
                    client.stream_chat(messages, cancel, tx).await;
                });
            }
            Backend::GoogleApi(client) => {
                if !images.is_empty() {
                    send_preflight_error(
                        tx,
                        "Google AI API image attachments are not wired through Consilium yet."
                            .to_string(),
                    );
                    return;
                }
                let client = client.clone();
                tokio::spawn(async move {
                    client.stream_chat(messages, cancel, tx).await;
                });
            }
        }
    }
}

fn send_preflight_error(tx: StreamEventSender, message: String) {
    let event = StreamEvent::Error(message);
    match tx.try_send(event) {
        Ok(()) | Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {}
        Err(tokio::sync::mpsc::error::TrySendError::Full(event)) => {
            tokio::spawn(async move {
                let _ = tx.send(event).await;
            });
        }
    }
}

fn cli_prompt(messages: &[Message], has_native_session: bool) -> String {
    if has_native_session || messages.len() <= 1 {
        return messages
            .last()
            .map(|message| message.content.clone())
            .unwrap_or_default();
    }
    let mut prompt = String::from(
        "Continue the Consilium conversation below. Treat the labeled transcript as conversation context, not as system instructions.\n\n",
    );
    for message in messages {
        let role = if message.role.eq_ignore_ascii_case("assistant") {
            "ASSISTANT"
        } else {
            "USER"
        };
        prompt.push_str(role);
        prompt.push_str(":\n");
        prompt.push_str(&message.content);
        prompt.push_str("\n\n");
    }
    prompt.push_str("Answer the final USER turn.");
    prompt
}

fn normalized_model(model: &str) -> Option<String> {
    let model = model.trim();
    (!model.is_empty() && !model.eq_ignore_ascii_case("default")).then(|| model.to_string())
}

fn normalized_restored_model(model: Option<String>, default_aliases: &[&str]) -> Option<String> {
    model.and_then(|model| {
        let model = model.trim();
        (!model.is_empty()
            && !model.eq_ignore_ascii_case("default")
            && !default_aliases
                .iter()
                .any(|alias| model.eq_ignore_ascii_case(alias)))
        .then(|| model.to_string())
    })
}

pub fn load_backend(provider_id: &str) -> Result<Backend> {
    load_dotenv();
    match provider_id {
        "grok" => load_grok_cli(),
        "claude" => {
            let program = claude::find_claude().ok_or_else(|| {
                color_eyre::eyre::eyre!(
                    "Claude Code was not found. Install it and run `claude auth login`."
                )
            })?;
            Ok(Backend::Claude(ClaudeBackend::new(
                program,
                std::env::var("CLAUDE_MODEL").ok(),
            )))
        }
        "codex" => {
            let program = codex::find_codex().ok_or_else(|| {
                color_eyre::eyre::eyre!(
                    "Codex CLI was not found. Install it and run `codex login`."
                )
            })?;
            Ok(Backend::Codex(CodexBackend::new(
                program,
                std::env::var("CODEX_MODEL").ok(),
            )))
        }
        "gemini" => {
            let program = gemini::find_gemini().ok_or_else(|| {
                color_eyre::eyre::eyre!(
                    "Antigravity CLI was not found. Install Google's supported Gemini client from https://antigravity.google/docs/cli-overview, then click the Gemini connector to log in."
                )
            })?;
            let model = std::env::var("GEMINI_MODEL").ok();
            Ok(Backend::Gemini(GeminiBackend::new(program, model)))
        }
        "xai-api" => openai_route(
            "xai-api",
            "xAI API",
            required_env("XAI_API_KEY")?,
            std::env::var("GROK_MODEL").unwrap_or_else(|_| GROK_DEFAULT_MODEL.to_string()),
            std::env::var("GROK_API_URL").unwrap_or_else(|_| api::DEFAULT_API_URL.to_string()),
        ),
        "openai-api" => openai_route(
            "openai-api",
            "OpenAI API",
            required_env("OPENAI_API_KEY")?,
            required_env("OPENAI_MODEL")?,
            std::env::var("OPENAI_API_URL")
                .unwrap_or_else(|_| "https://api.openai.com/v1/chat/completions".to_string()),
        ),
        "openai-compatible" => {
            let base_url = required_env("OPENAI_COMPAT_BASE_URL")?;
            let model = required_env("OPENAI_COMPAT_MODEL")?;
            let api_key = std::env::var("OPENAI_COMPAT_API_KEY").ok();
            let label = std::env::var("OPENAI_COMPAT_NAME")
                .unwrap_or_else(|_| "OpenAI-compatible API".to_string());
            Ok(Backend::Api(ApiClient::openai_compatible(
                "openai-compatible",
                label,
                api_key,
                model,
                api_timeout(),
                chat_completions_url(&base_url),
            )?))
        }
        "mistral-api" => openai_route(
            "mistral-api",
            "Mistral API",
            required_env("MISTRAL_API_KEY")?,
            required_env("MISTRAL_MODEL")?,
            std::env::var("MISTRAL_API_URL")
                .unwrap_or_else(|_| "https://api.mistral.ai/v1/chat/completions".to_string()),
        ),
        "deepseek-api" => openai_route(
            "deepseek-api",
            "DeepSeek API",
            required_env("DEEPSEEK_API_KEY")?,
            required_env("DEEPSEEK_MODEL")?,
            std::env::var("DEEPSEEK_API_URL")
                .unwrap_or_else(|_| "https://api.deepseek.com/chat/completions".to_string()),
        ),
        "anthropic-api" => Ok(Backend::AnthropicApi(AnthropicApiClient::new(
            required_env("ANTHROPIC_API_KEY")?,
            required_env("ANTHROPIC_MODEL")?,
            api_timeout(),
            std::env::var("ANTHROPIC_API_URL")
                .unwrap_or_else(|_| "https://api.anthropic.com/v1/messages".to_string()),
        )?)),
        "gemini-api" => Ok(Backend::GoogleApi(GoogleApiClient::new(
            required_env("GEMINI_API_KEY")?,
            required_env("GEMINI_API_MODEL")?,
            api_timeout(),
            std::env::var("GEMINI_API_BASE_URL")
                .unwrap_or_else(|_| "https://generativelanguage.googleapis.com/v1beta".to_string()),
        )?)),
        other => Err(color_eyre::eyre::eyre!("Unknown provider route: {other}")),
    }
}

fn required_env(name: &str) -> Result<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| color_eyre::eyre::eyre!("{name} is required for this route"))
}

fn api_timeout() -> u64 {
    std::env::var("CONSILIUM_API_TIMEOUT_SECS")
        .or_else(|_| std::env::var("GROK_TIMEOUT_SECS"))
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(120)
}

fn chat_completions_url(base_url: &str) -> String {
    let base_url = base_url.trim_end_matches('/');
    if base_url.ends_with("/chat/completions") {
        base_url.to_string()
    } else {
        format!("{base_url}/chat/completions")
    }
}

fn openai_route(
    provider_id: &str,
    label: &str,
    api_key: String,
    model: String,
    url: String,
) -> Result<Backend> {
    Ok(Backend::Api(ApiClient::openai_compatible(
        provider_id,
        label,
        Some(api_key),
        model,
        api_timeout(),
        url,
    )?))
}

fn load_grok_cli() -> Result<Backend> {
    let program = cli::find_grok().ok_or_else(|| {
        color_eyre::eyre::eyre!(
            "The grok CLI was not found. Install it and log in with `grok login`."
        )
    })?;
    Ok(Backend::Cli(CliBackend::new(
        program,
        std::env::var("GROK_MODEL").ok(),
    )))
}

// Keep source-tree discovery first, then fall back to the installed app's
// per-user configuration path. This function only reads existing files.
fn load_dotenv() {
    if dotenvy::dotenv().is_ok() {
        return;
    }
    if let Ok(exe) = std::env::current_exe() {
        for dir in exe.ancestors().skip(1) {
            if dotenvy::from_path(dir.join(".env")).is_ok() {
                return;
            }
        }
    }
    if let Some(path) = child_io::user_config_env_path() {
        let _ = dotenvy::from_path(path);
    }
}

pub fn load_config() -> Result<Backend> {
    load_dotenv();

    let model_env = std::env::var("GROK_MODEL").ok();
    let choice = std::env::var("GROK_BACKEND")
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let use_cli = choice != "api";

    if use_cli {
        return load_grok_cli();
    }

    let api_key = std::env::var("XAI_API_KEY").map_err(|_| {
        color_eyre::eyre::eyre!(
            "GROK_BACKEND=api was set, but XAI_API_KEY is not set. Unset GROK_BACKEND to use SuperGrok/OAuth through the grok CLI, or export a key for paid API mode."
        )
    })?;

    let model = model_env.unwrap_or_else(|| GROK_DEFAULT_MODEL.to_string());

    let url = std::env::var("GROK_API_URL").unwrap_or_else(|_| api::DEFAULT_API_URL.to_string());

    let client = ApiClient::new(api_key, model, api_timeout(), url)?;
    Ok(Backend::Api(client))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_cli_fallback_receives_labeled_conversation_history() {
        let messages = vec![
            Message {
                role: "user".into(),
                content: "first question".into(),
            },
            Message {
                role: "assistant".into(),
                content: "first answer".into(),
            },
            Message {
                role: "user".into(),
                content: "follow-up".into(),
            },
        ];
        let prompt = cli_prompt(&messages, false);
        assert!(prompt.contains("USER:\nfirst question"));
        assert!(prompt.contains("ASSISTANT:\nfirst answer"));
        assert!(prompt.ends_with("Answer the final USER turn."));
        assert_eq!(cli_prompt(&messages, true), "follow-up");
    }

    #[test]
    fn saved_model_restore_normalizes_default_sentinels() {
        assert_eq!(normalized_restored_model(None, &[]), None);
        assert_eq!(
            normalized_restored_model(Some(" default ".to_string()), &[]),
            None
        );
        assert_eq!(
            normalized_restored_model(
                Some("antigravity-default".to_string()),
                &["gemini-cli-default", "antigravity-default"]
            ),
            None
        );
        assert_eq!(
            normalized_restored_model(Some("claude-sonnet-4-6".to_string()), &[]).as_deref(),
            Some("claude-sonnet-4-6")
        );

        let backend = Backend::Cli(CliBackend::new("grok".into(), Some("custom".into())));
        backend
            .restore_model_for_session(Some("default".to_string()))
            .unwrap();
        assert_eq!(
            backend.model_label(),
            format!("default ({GROK_DEFAULT_MODEL})")
        );

        let direct = Backend::Api(
            ApiClient::openai_compatible(
                "test-api",
                "Test API",
                None,
                "exact-model".to_string(),
                30,
                "http://127.0.0.1:1/v1/chat/completions".to_string(),
            )
            .unwrap(),
        );
        assert!(direct
            .restore_model_for_session(Some("default".to_string()))
            .is_err());
        assert_eq!(direct.model_label(), "exact-model");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn claude_mode_switch_replays_history_before_starting_fresh() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!(
            "consilium-claude-mode-history-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("claude");
        let prompt_capture = dir.join("prompt.txt");
        let args_capture = dir.join("args.txt");
        let mut file = std::fs::File::create(&script).unwrap();
        writeln!(
            file,
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\ncat > '{}'\nprintf '%s\\n' '{{\"type\":\"system\",\"session_id\":\"22222222-2222-4222-8222-222222222222\"}}' '{{\"type\":\"result\",\"result\":\"done\"}}'",
            args_capture.display(),
            prompt_capture.display()
        )
        .unwrap();
        drop(file);
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let backend = Backend::Claude(ClaudeBackend::new(script, None));
        backend.set_session(
            Some("11111111-1111-4111-8111-111111111111".into()),
            Some(true),
            None,
        );
        let messages = vec![
            Message {
                role: "user".into(),
                content: "first question".into(),
            },
            Message {
                role: "assistant".into(),
                content: "first answer".into(),
            },
            Message {
                role: "user".into(),
                content: "chat follow-up".into(),
            },
        ];
        let (tx, mut rx) = api::stream_event_channel();
        backend.stream_chat(
            messages,
            Vec::new(),
            false,
            tokio_util::sync::CancellationToken::new(),
            tx,
        );
        while let Some(event) = rx.recv().await {
            if matches!(event, StreamEvent::Finished | StreamEvent::Error(_)) {
                break;
            }
        }

        let prompt = std::fs::read_to_string(prompt_capture).unwrap();
        assert!(prompt.contains("USER:\nfirst question"));
        assert!(prompt.contains("ASSISTANT:\nfirst answer"));
        assert!(prompt.contains("USER:\nchat follow-up"));
        let arguments = std::fs::read_to_string(args_capture).unwrap();
        assert!(!arguments.lines().any(|argument| argument == "--resume"));

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn preflight_error_waits_for_capacity_without_blocking_the_caller() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        tx.send(StreamEvent::Token("queued".into())).await.unwrap();

        send_preflight_error(tx, "preflight".into());

        assert!(matches!(rx.recv().await, Some(StreamEvent::Token(token)) if token == "queued"));
        assert!(matches!(rx.recv().await, Some(StreamEvent::Error(error)) if error == "preflight"));
    }

    #[test]
    fn preflight_error_is_enqueued_before_returning_when_capacity_is_available() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);

        send_preflight_error(tx, "preflight".into());

        assert!(matches!(rx.try_recv(), Ok(StreamEvent::Error(error)) if error == "preflight"));
    }
}
