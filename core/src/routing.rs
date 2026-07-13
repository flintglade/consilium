use serde::Serialize;

use crate::providers::ProviderInfo;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RoutingProfile {
    pub id: String,
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RouteCandidate {
    pub provider_id: String,
    pub provider_name: String,
    pub score: i32,
    pub explanation: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RouteDecision {
    pub profile_id: String,
    pub profile_name: String,
    pub selected_provider: Option<String>,
    pub candidates: Vec<RouteCandidate>,
    pub explanation: String,
}

#[derive(Clone, Copy)]
struct Weights {
    privacy: i32,
    cost: i32,
    latency: i32,
    reasoning: i32,
    tools: i32,
    context: i32,
    local_bonus: i32,
    require_tools: bool,
}

pub fn routing_profiles() -> Vec<RoutingProfile> {
    vec![
        profile("balanced", "Balanced", "Balances privacy, cost, latency, reasoning, tools, and context."),
        profile("local-first", "Local first", "Strongly prefers configured on-device or self-hosted endpoints; cloud routes remain ordered fallbacks."),
        profile("privacy", "Maximum privacy", "Prioritizes local processing and minimizes third-party data exposure."),
        profile("lowest-cost", "Lowest cost", "Prefers local and subscription routes before metered APIs."),
        profile("fastest", "Lowest latency", "Prioritizes responsive routes while preserving usable fallbacks."),
        profile("deep-reasoning", "Deep reasoning", "Prioritizes reasoning strength and useful context windows."),
        profile("agent-tools", "Agent and tools", "Enables Agent mode and uses only adapters that can perform real tool-driven work."),
        profile("long-context", "Long context", "Prioritizes routes with the largest documented context windows."),
    ]
}

fn profile(id: &str, name: &str, description: &str) -> RoutingProfile {
    RoutingProfile {
        id: id.to_string(),
        name: name.to_string(),
        description: description.to_string(),
    }
}

fn weights(profile: &str) -> Weights {
    match profile {
        "local-first" => Weights {
            privacy: 5,
            cost: 4,
            latency: 3,
            reasoning: 1,
            tools: 1,
            context: 1,
            local_bonus: 30,
            require_tools: false,
        },
        "privacy" => Weights {
            privacy: 7,
            cost: 2,
            latency: 1,
            reasoning: 1,
            tools: 1,
            context: 0,
            local_bonus: 40,
            require_tools: false,
        },
        "lowest-cost" => Weights {
            privacy: 2,
            cost: 7,
            latency: 2,
            reasoning: 1,
            tools: 0,
            context: 0,
            local_bonus: 20,
            require_tools: false,
        },
        "fastest" => Weights {
            privacy: 1,
            cost: 2,
            latency: 7,
            reasoning: 1,
            tools: 0,
            context: 0,
            local_bonus: 8,
            require_tools: false,
        },
        "deep-reasoning" => Weights {
            privacy: 1,
            cost: 1,
            latency: 1,
            reasoning: 7,
            tools: 2,
            context: 3,
            local_bonus: 0,
            require_tools: false,
        },
        "agent-tools" => Weights {
            privacy: 1,
            cost: 1,
            latency: 1,
            reasoning: 4,
            tools: 8,
            context: 1,
            local_bonus: 0,
            require_tools: true,
        },
        "long-context" => Weights {
            privacy: 1,
            cost: 1,
            latency: 1,
            reasoning: 2,
            tools: 1,
            context: 8,
            local_bonus: 0,
            require_tools: false,
        },
        _ => Weights {
            privacy: 3,
            cost: 3,
            latency: 3,
            reasoning: 3,
            tools: 2,
            context: 2,
            local_bonus: 5,
            require_tools: false,
        },
    }
}

fn context_score(tokens: u64) -> i32 {
    match tokens {
        1_000_000.. => 5,
        400_000.. => 4,
        200_000.. => 3,
        128_000.. => 2,
        1.. => 1,
        _ => 0,
    }
}

pub fn decide(profile_id: &str, providers: &[ProviderInfo], agent: bool) -> RouteDecision {
    let known_profile = routing_profiles()
        .into_iter()
        .find(|profile| profile.id == profile_id)
        .unwrap_or_else(|| profile("balanced", "Balanced", "Balanced fallback profile."));
    let weights = weights(&known_profile.id);
    let require_tools = weights.require_tools || agent;
    let mut candidates = providers
        .iter()
        .filter(|provider| provider.active && provider.available)
        .filter(|provider| {
            if agent {
                provider.supports_agent
            } else {
                provider.supports_chat
            }
        })
        .filter(|provider| !require_tools || provider.tool_score >= 4)
        .map(|provider| {
            let context = context_score(provider.context_window);
            let score = provider.privacy_score as i32 * weights.privacy
                + provider.cost_score as i32 * weights.cost
                + provider.latency_score as i32 * weights.latency
                + provider.reasoning_score as i32 * weights.reasoning
                + provider.tool_score as i32 * weights.tools
                + context * weights.context
                + if provider.local { weights.local_bonus } else { 0 };
            RouteCandidate {
                provider_id: provider.id.clone(),
                provider_name: provider.name.clone(),
                score,
                explanation: format!(
                    "privacy {}/5 · cost {}/5 · latency {}/5 · reasoning {}/5 · tools {}/5 · context {}{}",
                    provider.privacy_score,
                    provider.cost_score,
                    provider.latency_score,
                    provider.reasoning_score,
                    provider.tool_score,
                    if provider.context_window == 0 { "endpoint-defined".to_string() } else { format!("{}K", provider.context_window / 1000) },
                    if provider.local { " · local" } else { "" }
                ),
            }
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| left.provider_id.cmp(&right.provider_id))
    });
    let selected_provider = candidates
        .first()
        .map(|candidate| candidate.provider_id.clone());
    let explanation = if candidates.is_empty() {
        if require_tools {
            "No configured tool-capable route is available. Install and sign in to Grok, Claude Code, Codex CLI, or Gemini via Antigravity.".to_string()
        } else {
            "No configured route is available. Configure a local compatible endpoint, sign in to a CLI, or add an API configuration.".to_string()
        }
    } else {
        format!(
            "{} selected {} from {} available route(s). Chat fallbacks follow the displayed score order and are attempted only before answer text. Agent runs never switch providers automatically.",
            known_profile.name,
            candidates[0].provider_name,
            candidates.len()
        )
    };
    RouteDecision {
        profile_id: known_profile.id,
        profile_name: known_profile.name,
        selected_provider,
        candidates,
        explanation,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(id: &str, local: bool, scores: [u8; 5], context: u64) -> ProviderInfo {
        ProviderInfo {
            id: id.into(),
            name: id.into(),
            vendor: "test".into(),
            adapter_state: "active".into(),
            auth: "test".into(),
            binary: String::new(),
            login_command: String::new(),
            status_command: None,
            model_family: "test".into(),
            docs_url: String::new(),
            note: String::new(),
            installed: true,
            configured: true,
            available: true,
            active: true,
            local,
            supports_chat: true,
            supports_agent: scores[4] >= 4,
            privacy_score: scores[0],
            cost_score: scores[1],
            latency_score: scores[2],
            reasoning_score: scores[3],
            tool_score: scores[4],
            context_window: context,
        }
    }

    #[test]
    fn local_first_prefers_local_and_keeps_cloud_fallback() {
        let providers = vec![
            provider("cloud", false, [2, 2, 5, 5, 5], 1_000_000),
            provider("local", true, [5, 5, 3, 3, 0], 32_000),
        ];
        let decision = decide("local-first", &providers, false);
        assert_eq!(decision.selected_provider.as_deref(), Some("local"));
        assert_eq!(decision.candidates.len(), 2);
    }

    #[test]
    fn agent_profile_excludes_routes_without_real_tool_runtime() {
        let providers = vec![
            provider("api", false, [2, 2, 5, 5, 0], 1_000_000),
            provider("cli", false, [3, 4, 3, 4, 5], 200_000),
        ];
        let decision = decide("balanced", &providers, true);
        assert_eq!(decision.candidates.len(), 1);
        assert_eq!(decision.selected_provider.as_deref(), Some("cli"));
    }

    #[test]
    fn chat_routes_exclude_agent_only_adapters() {
        let mut agent_only = provider("agent-only", false, [3, 4, 4, 5, 5], 200_000);
        agent_only.supports_chat = false;
        agent_only.supports_agent = true;
        let chat = provider("chat", false, [2, 2, 3, 4, 0], 128_000);

        let decision = decide("balanced", &[agent_only, chat], false);
        assert_eq!(decision.candidates.len(), 1);
        assert_eq!(decision.selected_provider.as_deref(), Some("chat"));
    }
}
