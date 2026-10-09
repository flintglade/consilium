use serde::Serialize;
use url::{Host, Url};

use crate::child_io::{find_program, CliProvider};

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ProviderInfo {
    pub id: String,
    pub name: String,
    pub vendor: String,
    pub adapter_state: String,
    pub auth: String,
    pub binary: String,
    pub login_command: String,
    pub status_command: Option<String>,
    pub model_family: String,
    pub docs_url: String,
    pub note: String,
    pub installed: bool,
    pub configured: bool,
    pub available: bool,
    pub active: bool,
    pub local: bool,
    pub supports_chat: bool,
    pub supports_agent: bool,
    /// Ordinal capability scores. Five is most favorable; zero is unknown.
    pub privacy_score: u8,
    pub cost_score: u8,
    pub latency_score: u8,
    pub reasoning_score: u8,
    pub tool_score: u8,
    pub context_window: u64,
}

struct ProviderSpec {
    id: &'static str,
    name: &'static str,
    vendor: &'static str,
    adapter_state: &'static str,
    auth: &'static str,
    cli_provider: Option<CliProvider>,
    login_command: &'static str,
    status_command: Option<&'static str>,
    model_family: &'static str,
    docs_url: &'static str,
    note: &'static str,
    active: bool,
    required_env: &'static [&'static str],
    local: bool,
    scores: [u8; 5],
    context_window: u64,
}

fn mode_capabilities(provider_id: &str, active: bool) -> (bool, bool) {
    if !active {
        return (false, false);
    }
    match provider_id {
        "grok" | "claude" => (true, true),
        "codex" | "gemini" => (false, true),
        _ => (true, false),
    }
}

fn env_configured(required: &[&str]) -> bool {
    !required.is_empty()
        && required.iter().all(|name| {
            std::env::var(name)
                .ok()
                .is_some_and(|value| !value.trim().is_empty())
        })
}

fn ipv4_is_local(address: std::net::Ipv4Addr) -> bool {
    address.is_loopback()
        || address.is_private()
        || address.is_link_local()
        || address.is_unspecified()
}

fn ipv6_is_local(address: std::net::Ipv6Addr) -> bool {
    address.is_loopback()
        || address.is_unique_local()
        || address.is_unicast_link_local()
        || address.is_unspecified()
        || address.to_ipv4_mapped().is_some_and(ipv4_is_local)
}

fn endpoint_is_local(endpoint: &str) -> bool {
    let endpoint = endpoint.trim();
    let parsed = Url::parse(endpoint)
        .ok()
        .filter(|url| url.host().is_some())
        .or_else(|| Url::parse(&format!("http://{endpoint}")).ok());
    let Some(parsed) = parsed else {
        return false;
    };

    match parsed.host() {
        Some(Host::Domain(host)) => {
            let host = host.trim_end_matches('.');
            host.eq_ignore_ascii_case("localhost")
                || host
                    .to_ascii_lowercase()
                    .strip_suffix(".local")
                    .is_some_and(|name| !name.is_empty())
        }
        Some(Host::Ipv4(address)) => ipv4_is_local(address),
        Some(Host::Ipv6(address)) => ipv6_is_local(address),
        None => false,
    }
}

fn provider_specs() -> Vec<ProviderSpec> {
    vec![
        ProviderSpec {
            id: "grok",
            name: "Grok Build",
            vendor: "xAI",
            adapter_state: "active",
            auth: "SuperGrok / X Premium OAuth",
            cli_provider: Some(CliProvider::Grok),
            login_command: "grok login",
            status_command: None,
            model_family: "Grok 4.5, Composer 2.5",
            docs_url: "https://x.ai/cli",
            note: "Official Grok CLI adapter. Subscription credentials stay in the CLI cache.",
            active: true,
            required_env: &[],
            local: false,
            scores: [3, 4, 4, 5, 5],
            context_window: 500_000,
        },
        ProviderSpec {
            id: "claude",
            name: "Claude Code",
            vendor: "Anthropic",
            adapter_state: "active",
            auth: "Claude.ai subscription OAuth",
            cli_provider: Some(CliProvider::Claude),
            login_command: "claude auth login",
            status_command: Some("claude auth status"),
            model_family: "Claude Sonnet / Opus via Claude Code",
            docs_url: "https://code.claude.com/docs/en/authentication",
            note: "Official Claude Code adapter with isolated chat and agent runtimes.",
            active: true,
            required_env: &[],
            local: false,
            scores: [3, 4, 3, 5, 5],
            context_window: 0,
        },
        ProviderSpec {
            id: "codex",
            name: "Codex CLI",
            vendor: "OpenAI",
            adapter_state: "active",
            auth: "ChatGPT sign-in OAuth",
            cli_provider: Some(CliProvider::Codex),
            login_command: "codex login --device-auth",
            status_command: Some("codex login status"),
            model_family: "Codex models through a ChatGPT plan",
            docs_url: "https://developers.openai.com/codex/auth",
            note: "Official Codex CLI adapter with resumable headless threads.",
            active: true,
            required_env: &[],
            local: false,
            scores: [3, 4, 3, 5, 5],
            context_window: 0,
        },
        ProviderSpec {
            id: "gemini",
            name: "Gemini",
            vendor: "Google",
            adapter_state: "active",
            auth: "Google account OAuth via Antigravity",
            cli_provider: Some(CliProvider::Gemini),
            login_command: "agy (Google verification opens in your browser)",
            status_command: None,
            model_family: "Gemini models via Antigravity CLI",
            docs_url: "https://antigravity.google/docs/gcli-migration",
            note: "Antigravity exposes prompt text to local process inspectors. Use Google AI API for private text, or set CONSILIUM_ALLOW_VISIBLE_PROMPTS=1 to opt in. Credentials remain in Google's keyring.",
            active: true,
            required_env: &[],
            local: false,
            scores: [3, 4, 4, 4, 4],
            context_window: 1_000_000,
        },
        ProviderSpec {
            id: "openai-compatible",
            name: "Local / compatible API",
            vendor: "Open ecosystem",
            adapter_state: "configured",
            auth: "Optional bearer token",
            cli_provider: None,
            login_command: "Set OPENAI_COMPAT_BASE_URL and OPENAI_COMPAT_MODEL",
            status_command: None,
            model_family: "Any model exposed through /v1/chat/completions",
            docs_url: "https://docs.ollama.com/api/openai-compatibility",
            note: "Works with Ollama, LM Studio, llama.cpp, vLLM, and compatible local or hosted servers. The endpoint decides which model IDs are available.",
            active: true,
            required_env: &["OPENAI_COMPAT_BASE_URL", "OPENAI_COMPAT_MODEL"],
            local: true,
            scores: [5, 5, 5, 4, 0],
            context_window: 0,
        },
        ProviderSpec {
            id: "xai-api",
            name: "xAI API",
            vendor: "xAI",
            adapter_state: "configured",
            auth: "API key",
            cli_provider: None,
            login_command: "Set XAI_API_KEY",
            status_command: None,
            model_family: "Grok API models",
            docs_url: "https://docs.x.ai/docs/api-reference",
            note: "Direct OpenAI-compatible xAI API route. Usage may incur provider charges.",
            active: true,
            required_env: &["XAI_API_KEY"],
            local: false,
            scores: [2, 2, 4, 5, 0],
            context_window: 500_000,
        },
        ProviderSpec {
            id: "openai-api",
            name: "OpenAI API",
            vendor: "OpenAI",
            adapter_state: "configured",
            auth: "API key",
            cli_provider: None,
            login_command: "Set OPENAI_API_KEY and OPENAI_MODEL",
            status_command: None,
            model_family: "User-selected OpenAI API model",
            docs_url: "https://platform.openai.com/docs/api-reference/chat",
            note: "Direct OpenAI API route. It is optional and may incur provider charges.",
            active: true,
            required_env: &["OPENAI_API_KEY", "OPENAI_MODEL"],
            local: false,
            scores: [2, 2, 4, 5, 0],
            context_window: 0,
        },
        ProviderSpec {
            id: "anthropic-api",
            name: "Anthropic API",
            vendor: "Anthropic",
            adapter_state: "configured",
            auth: "API key",
            cli_provider: None,
            login_command: "Set ANTHROPIC_API_KEY and ANTHROPIC_MODEL",
            status_command: None,
            model_family: "User-selected Claude API model",
            docs_url: "https://docs.anthropic.com/en/api/messages",
            note: "Direct Anthropic Messages API route with streaming. Optional and potentially billable.",
            active: true,
            required_env: &["ANTHROPIC_API_KEY", "ANTHROPIC_MODEL"],
            local: false,
            scores: [2, 2, 3, 5, 0],
            context_window: 0,
        },
        ProviderSpec {
            id: "gemini-api",
            name: "Gemini Developer API",
            vendor: "Google",
            adapter_state: "configured",
            auth: "API key",
            cli_provider: None,
            login_command: "Set GEMINI_API_KEY and GEMINI_API_MODEL",
            status_command: None,
            model_family: "User-selected Gemini API model",
            docs_url: "https://ai.google.dev/api/generate-content",
            note: "Direct Google Gemini Developer API route. Optional and potentially billable.",
            active: true,
            required_env: &["GEMINI_API_KEY", "GEMINI_API_MODEL"],
            local: false,
            scores: [2, 2, 4, 4, 0],
            context_window: 0,
        },
        ProviderSpec {
            id: "mistral-api",
            name: "Mistral API",
            vendor: "Mistral AI",
            adapter_state: "configured",
            auth: "API key",
            cli_provider: None,
            login_command: "Set MISTRAL_API_KEY and MISTRAL_MODEL",
            status_command: None,
            model_family: "Mistral, Magistral, Devstral",
            docs_url: "https://docs.mistral.ai/api/endpoint/chat",
            note: "Direct Mistral OpenAI-compatible API route. Optional and potentially billable.",
            active: true,
            required_env: &["MISTRAL_API_KEY", "MISTRAL_MODEL"],
            local: false,
            scores: [2, 2, 4, 4, 0],
            context_window: 0,
        },
        ProviderSpec {
            id: "deepseek-api",
            name: "DeepSeek API",
            vendor: "DeepSeek",
            adapter_state: "configured",
            auth: "API key",
            cli_provider: None,
            login_command: "Set DEEPSEEK_API_KEY and DEEPSEEK_MODEL",
            status_command: None,
            model_family: "DeepSeek Chat / Reasoner",
            docs_url: "https://api-docs.deepseek.com/",
            note: "Direct DeepSeek OpenAI-compatible API route. Optional and potentially billable.",
            active: true,
            required_env: &["DEEPSEEK_API_KEY", "DEEPSEEK_MODEL"],
            local: false,
            scores: [2, 3, 3, 5, 0],
            context_window: 0,
        },
        ProviderSpec {
            id: "copilot",
            name: "Copilot CLI",
            vendor: "GitHub",
            adapter_state: "planned",
            auth: "GitHub OAuth device flow",
            cli_provider: Some(CliProvider::Copilot),
            login_command: "copilot login",
            status_command: None,
            model_family: "GitHub Copilot-managed models",
            docs_url: "https://docs.github.com/en/copilot/how-tos/copilot-cli/set-up-copilot-cli/authenticate-copilot-cli",
            note: "Installed-state visibility only; no Consilium adapter yet.",
            active: false,
            required_env: &[],
            local: false,
            scores: [0; 5],
            context_window: 0,
        },
        ProviderSpec {
            id: "kiro",
            name: "Kiro CLI",
            vendor: "AWS / Kiro",
            adapter_state: "planned",
            auth: "GitHub, Google, Builder ID, or IAM Identity Center",
            cli_provider: Some(CliProvider::Kiro),
            login_command: "kiro-cli login",
            status_command: Some("kiro-cli whoami"),
            model_family: "Kiro-managed coding models",
            docs_url: "https://kiro.dev/docs/cli/authentication/",
            note: "Cataloged for future adapter work.",
            active: false,
            required_env: &[],
            local: false,
            scores: [0; 5],
            context_window: 0,
        },
    ]
}

fn provider_catalog_with<F>(mut cli_installed: F) -> Vec<ProviderInfo>
where
    F: FnMut(CliProvider) -> bool,
{
    provider_specs()
        .into_iter()
        .map(|spec| {
            let installed = spec.cli_provider.map(&mut cli_installed).unwrap_or(false);
            let configured = env_configured(spec.required_env);
            let privacy_allowed = spec.id != "gemini" || crate::gemini::visible_prompts_allowed();
            let available = spec.active && (installed || configured) && privacy_allowed;
            let local = spec.local
                && std::env::var("OPENAI_COMPAT_BASE_URL")
                    .ok()
                    .is_some_and(|endpoint| endpoint_is_local(&endpoint));
            let [mut privacy_score, mut cost_score, mut latency_score, reasoning_score, tool_score] =
                spec.scores;
            if spec.id == "openai-compatible" && !local {
                privacy_score = 2;
                cost_score = 3;
                latency_score = 3;
            }
            let (supports_chat, supports_agent) = mode_capabilities(spec.id, spec.active);
            ProviderInfo {
                id: spec.id.to_string(),
                name: spec.name.to_string(),
                vendor: spec.vendor.to_string(),
                adapter_state: spec.adapter_state.to_string(),
                auth: spec.auth.to_string(),
                binary: spec
                    .cli_provider
                    .map(CliProvider::binary)
                    .unwrap_or_default()
                    .to_string(),
                login_command: spec.login_command.to_string(),
                status_command: spec.status_command.map(str::to_string),
                model_family: spec.model_family.to_string(),
                docs_url: spec.docs_url.to_string(),
                note: spec.note.to_string(),
                installed,
                configured,
                available,
                active: spec.active,
                local,
                supports_chat,
                supports_agent,
                privacy_score,
                cost_score,
                latency_score,
                reasoning_score,
                tool_score,
                context_window: spec.context_window,
            }
        })
        .collect()
}

pub fn provider_catalog() -> Vec<ProviderInfo> {
    provider_catalog_with(|provider| find_program(provider).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};
    use std::ffi::OsString;
    use std::path::PathBuf;

    use crate::child_io::find_program_with;

    #[test]
    fn catalog_has_active_cli_and_api_adapters() {
        let catalog = provider_catalog();
        for id in [
            "grok",
            "claude",
            "codex",
            "gemini",
            "openai-compatible",
            "openai-api",
            "anthropic-api",
            "gemini-api",
        ] {
            assert!(catalog
                .iter()
                .any(|provider| provider.id == id && provider.active));
        }
    }

    #[test]
    fn catalog_exposes_route_mode_capabilities() {
        let catalog = provider_catalog();
        let capability = |id: &str| {
            let provider = catalog.iter().find(|provider| provider.id == id).unwrap();
            (provider.supports_chat, provider.supports_agent)
        };
        assert_eq!(capability("grok"), (true, true));
        assert_eq!(capability("claude"), (true, true));
        assert_eq!(capability("codex"), (false, true));
        assert_eq!(capability("gemini"), (false, true));
        assert_eq!(capability("openai-compatible"), (true, false));
        assert_eq!(capability("openai-api"), (true, false));
        assert_eq!(capability("copilot"), (false, false));
    }

    #[test]
    fn catalog_ids_are_unique() {
        let catalog = provider_catalog();
        let mut ids = catalog
            .iter()
            .map(|provider| provider.id.as_str())
            .collect::<Vec<_>>();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), catalog.len());
    }

    #[test]
    fn catalog_availability_matches_windows_launch_resolution() {
        let variables = HashMap::from([
            ("PATH".to_string(), OsString::from("/tools")),
            (
                "APPDATA".to_string(),
                OsString::from("/users/ada/AppData/Roaming"),
            ),
            (
                "LOCALAPPDATA".to_string(),
                OsString::from("/users/ada/AppData/Local"),
            ),
            ("PNPM_HOME".to_string(), OsString::from("/users/ada/pnpm")),
            ("CARGO_HOME".to_string(), OsString::from("/users/ada/cargo")),
            ("SCOOP".to_string(), OsString::from("/users/ada/scoop")),
            ("USERPROFILE".to_string(), OsString::from("/users/ada")),
        ]);
        let files = HashSet::from([
            PathBuf::from("/tools/grok.cmd"),
            PathBuf::from("/users/ada/AppData/Roaming/npm/claude.exe"),
            PathBuf::from("/users/ada/AppData/Local/Microsoft/WindowsApps/codex.bat"),
            PathBuf::from("/users/ada/pnpm/agy.cmd"),
            PathBuf::from("/users/ada/scoop/shims/copilot.exe"),
            PathBuf::from("/users/ada/cargo/bin/kiro-cli.cmd"),
        ]);
        let launch_resolution = |provider| {
            find_program_with(
                provider,
                true,
                |key| variables.get(key).cloned(),
                |path| files.contains(path),
            )
        };
        let catalog = provider_catalog_with(|provider| launch_resolution(provider).is_some());

        for (id, provider) in [
            ("grok", CliProvider::Grok),
            ("claude", CliProvider::Claude),
            ("codex", CliProvider::Codex),
            ("gemini", CliProvider::Gemini),
            ("copilot", CliProvider::Copilot),
            ("kiro", CliProvider::Kiro),
        ] {
            let catalog_entry = catalog.iter().find(|entry| entry.id == id).unwrap();
            assert_eq!(
                catalog_entry.installed,
                launch_resolution(provider).is_some(),
                "catalog and launcher disagreed for {id}",
            );
            assert_eq!(catalog_entry.binary, provider.binary());
        }
    }

    #[test]
    fn local_endpoint_detection_is_conservative() {
        for endpoint in [
            "http://localhost:11434/v1",
            "localhost:11434/v1",
            "http://127.0.0.1:8080",
            "http://[::1]:8000/v1",
            "http://[fd00::1234]:8000/v1",
            "http://[::ffff:127.0.0.1]:8000/v1",
            "http://169.254.1.5:8000/v1",
            "http://192.168.1.5:1234/v1",
            "http://172.20.0.2:8000/v1",
            "http://llama-box.local/v1",
        ] {
            assert!(endpoint_is_local(endpoint), "{endpoint}");
        }
        for endpoint in [
            "https://api.example.com/v1",
            "https://172.40.0.2/v1",
            "https://notlocalhost.example/v1",
            "https://localhost.example/v1",
            "https://127.0.0.1.example/v1",
            "https://10.example/v1",
            "https://192.168.1.5.example/v1",
            "https://172.20.0.2.example/v1",
            "https://127.0.0.1@api.example/v1",
        ] {
            assert!(!endpoint_is_local(endpoint), "{endpoint}");
        }
    }
}
