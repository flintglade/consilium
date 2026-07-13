use serde::Serialize;

/// Curated open-weight families that can be served through Consilium's
/// OpenAI-compatible route. These are suggestions, not claims that a model is
/// installed: the configured server remains the source of truth for model IDs.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct OpenModelInfo {
    pub id: String,
    pub name: String,
    pub family: String,
    pub organization: String,
    pub context_window: u64,
    pub reasoning: bool,
    pub tools: bool,
    pub vision: bool,
    pub source_url: String,
    pub note: String,
}

pub fn open_model_catalog() -> Vec<OpenModelInfo> {
    vec![
        model(
            "meta-llama/Llama-4-Scout-17B-16E-Instruct",
            "Llama 4 Scout",
            "Llama 4",
            "Meta",
            10_000_000,
            true,
            true,
            true,
            "https://github.com/meta-llama/llama-models",
            "Open weights under Meta's Llama license; large server-class deployment.",
        ),
        model(
            "meta-llama/Llama-4-Maverick-17B-128E-Instruct",
            "Llama 4 Maverick",
            "Llama 4",
            "Meta",
            1_000_000,
            true,
            true,
            true,
            "https://github.com/meta-llama/llama-models",
            "Open weights under Meta's Llama license; model ID varies by server.",
        ),
        model(
            "Qwen/Qwen3-32B",
            "Qwen3 32B",
            "Qwen3",
            "Qwen",
            131_072,
            true,
            true,
            false,
            "https://huggingface.co/Qwen/Qwen3-32B",
            "Apache-2.0 hybrid-thinking weights; 32,768 tokens are native and 131,072 requires the model card's YaRN configuration.",
        ),
        model(
            "Qwen/Qwen3-235B-A22B",
            "Qwen3 235B-A22B",
            "Qwen3",
            "Qwen",
            131_072,
            true,
            true,
            false,
            "https://huggingface.co/Qwen/Qwen3-235B-A22B",
            "Apache-2.0 MoE hybrid-thinking weights; 32,768 tokens are native and 131,072 requires the model card's YaRN configuration.",
        ),
        model(
            "mistralai/Mistral-Small-3.2-24B-Instruct-2506",
            "Mistral Small 3.2",
            "Mistral Small",
            "Mistral AI",
            128_000,
            true,
            true,
            true,
            "https://huggingface.co/mistralai/Mistral-Small-3.2-24B-Instruct-2506",
            "Apache-2.0 24B multimodal instruction checkpoint.",
        ),
        model(
            "mistralai/Magistral-Small-2509",
            "Magistral Small 1.2",
            "Magistral",
            "Mistral AI",
            128_000,
            true,
            true,
            true,
            "https://huggingface.co/mistralai/Magistral-Small-2509",
            "Apache-2.0 24B reasoning checkpoint; its model card recommends shorter practical contexts.",
        ),
        model(
            "mistralai/Mistral-Small-4-119B-2603",
            "Mistral Small 4 119B A6B",
            "Mistral Small",
            "Mistral AI",
            262_144,
            true,
            true,
            true,
            "https://huggingface.co/mistralai/Mistral-Small-4-119B-2603",
            "Apache-2.0 hybrid instruct, reasoning, and coding MoE checkpoint.",
        ),
        model(
            "deepseek-ai/DeepSeek-R1",
            "DeepSeek R1",
            "DeepSeek R1",
            "DeepSeek",
            128_000,
            true,
            true,
            false,
            "https://github.com/deepseek-ai/DeepSeek-R1",
            "MIT-licensed reasoning weights; distilled variants are also available.",
        ),
        model(
            "deepseek-ai/DeepSeek-V3",
            "DeepSeek V3",
            "DeepSeek V3",
            "DeepSeek",
            128_000,
            true,
            true,
            false,
            "https://github.com/deepseek-ai/DeepSeek-V3",
            "Open-weight MoE family with commercial-use terms in its model license.",
        ),
        model(
            "google/gemma-3-27b-it",
            "Gemma 3 27B IT",
            "Gemma 3",
            "Google DeepMind",
            128_000,
            false,
            true,
            true,
            "https://ai.google.dev/gemma/docs/get_started",
            "Open-weight instruction model for desktops and servers.",
        ),
        model(
            "google/gemma-3n-E4B-it",
            "Gemma 3n E4B IT",
            "Gemma 3n",
            "Google DeepMind",
            32_000,
            false,
            false,
            true,
            "https://ai.google.dev/gemma/docs/gemma-3n",
            "Efficient open-weight multimodal model for local devices.",
        ),
        model(
            "microsoft/Phi-4-reasoning-plus",
            "Phi-4 Reasoning Plus",
            "Phi-4",
            "Microsoft",
            32_768,
            true,
            true,
            false,
            "https://huggingface.co/microsoft/Phi-4-reasoning-plus",
            "MIT-licensed 14B reasoning checkpoint with a documented 32K context window.",
        ),
        model(
            "microsoft/Phi-4-mini-instruct",
            "Phi-4 Mini Instruct",
            "Phi-4",
            "Microsoft",
            131_072,
            false,
            true,
            false,
            "https://huggingface.co/microsoft/Phi-4-mini-instruct",
            "MIT-licensed compact instruction model with a 128K context window and function-calling format.",
        ),
        model(
            "zai-org/GLM-4.7-Flash",
            "GLM-4.7 Flash",
            "GLM-4.7",
            "Z.ai",
            202_752,
            true,
            true,
            false,
            "https://huggingface.co/zai-org/GLM-4.7-Flash",
            "Lightweight 30B-A3B member of the open GLM agentic family.",
        ),
        model(
            "moonshotai/Kimi-K2-Instruct",
            "Kimi K2 Instruct",
            "Kimi K2",
            "Moonshot AI",
            128_000,
            true,
            true,
            false,
            "https://github.com/MoonshotAI/Kimi-K2",
            "Open-weight MoE agentic model; use the server's exact ID.",
        ),
        model(
            "moonshotai/Kimi-K2.5",
            "Kimi K2.5",
            "Kimi K2.5",
            "Moonshot AI",
            262_144,
            true,
            true,
            true,
            "https://github.com/MoonshotAI/Kimi-K2.5",
            "Open-weight multimodal agentic family with a 256K context window; use the server's exact ID.",
        ),
    ]
}

#[allow(clippy::too_many_arguments)]
fn model(
    id: &str,
    name: &str,
    family: &str,
    organization: &str,
    context_window: u64,
    reasoning: bool,
    tools: bool,
    vision: bool,
    source_url: &str,
    note: &str,
) -> OpenModelInfo {
    OpenModelInfo {
        id: id.to_string(),
        name: name.to_string(),
        family: family.to_string(),
        organization: organization.to_string(),
        context_window,
        reasoning,
        tools,
        vision,
        source_url: source_url.to_string(),
        note: note.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_covers_requested_open_weight_families_without_duplicate_ids() {
        let catalog = open_model_catalog();
        for family in [
            "Llama 4",
            "Qwen3",
            "Mistral Small",
            "DeepSeek R1",
            "Gemma 3",
            "Phi-4",
            "GLM-4.7",
            "Kimi K2",
        ] {
            assert!(catalog.iter().any(|model| model.family == family));
        }
        let mut ids = catalog.iter().map(|model| &model.id).collect::<Vec<_>>();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), catalog.len());
    }

    #[test]
    fn catalog_preserves_documented_context_and_checkpoint_sources() {
        let catalog = open_model_catalog();
        let get = |id: &str| {
            catalog
                .iter()
                .find(|model| model.id == id)
                .unwrap_or_else(|| panic!("missing catalog model {id}"))
        };

        assert_eq!(get("Qwen/Qwen3-32B").context_window, 131_072);
        assert!(get("Qwen/Qwen3-32B").note.contains("YaRN"));
        assert_eq!(get("microsoft/Phi-4-reasoning-plus").context_window, 32_768);
        assert_eq!(get("microsoft/Phi-4-mini-instruct").context_window, 131_072);
        assert_eq!(get("zai-org/GLM-4.7-Flash").context_window, 202_752);
        assert_eq!(get("moonshotai/Kimi-K2.5").context_window, 262_144);
        for id in [
            "Qwen/Qwen3-32B",
            "Qwen/Qwen3-235B-A22B",
            "microsoft/Phi-4-reasoning-plus",
            "microsoft/Phi-4-mini-instruct",
        ] {
            assert!(get(id).source_url.ends_with(id));
        }
    }
}
