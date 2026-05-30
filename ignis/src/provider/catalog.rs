//! Built-in provider catalog: brand metadata (endpoints, auth, model lists)
//! compiled into the binary. `config.toml` only supplies the API key plus
//! optional overrides; everything else is baked here. Adding a provider is a new
//! [`ProviderSpec`] literal in the `SPECS` table below — there is no dispatch
//! code to edit.
//!
//! Model lists are a curated, coding-relevant subset per brand; extend or
//! override any of them via `[providers.<id>].models` in config.toml.

use serde::Deserialize;

/// Wire protocol — selects the concrete provider struct in
/// [`crate::provider::build`] and gates tool support (only `Ollama` lacks it).
/// Deserialized directly from a config `protocol = "..."` override.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    #[serde(alias = "openai-compatible")]
    OpenAi,
    Anthropic,
    Gemini,
    Ollama,
}

impl Protocol {
    pub fn label(self) -> &'static str {
        match self {
            Protocol::OpenAi => "openai",
            Protocol::Anthropic => "anthropic",
            Protocol::Gemini => "gemini",
            Protocol::Ollama => "ollama",
        }
    }
}

/// How the API key is attached. Decoupled from [`Protocol`]: MiniMax's Anthropic
/// endpoint uses `Bearer`, while real Anthropic uses `XApiKey`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Auth {
    /// `Authorization: Bearer <key>`
    Bearer,
    /// `x-api-key: <key>` (Anthropic)
    XApiKey,
    /// `?key=<key>` query parameter (Gemini)
    QueryKey,
    /// No credential (Ollama)
    None,
}

/// One protocol variant a provider exposes.
pub struct Endpoint {
    pub protocol: Protocol,
    /// API root; the protocol struct appends its own path.
    pub base_url: &'static str,
    pub auth: Auth,
}

/// A model offered by a provider, with the metadata the picker needs.
pub struct ModelSpec {
    pub name: &'static str,
    /// Context-window override; `None` falls back to models.dev.
    pub context: Option<u64>,
    /// Reasoning-effort levels, in display order (empty = no effort control).
    pub reasoning_effort: &'static [&'static str],
}

/// A selectable provider ("brand"). The user picks it by `id` and supplies an
/// API key; everything else here is baked in.
pub struct ProviderSpec {
    /// The `[providers.<id>]` table name and the `provider` half of
    /// `model = "provider/model"`.
    pub id: &'static str,
    pub display_name: &'static str,
    /// Ordered; `[0]` is the auto-selected default. Empty for `custom`, whose
    /// endpoint is synthesized from config.
    pub endpoints: &'static [Endpoint],
    pub api_key_required: bool,
    /// Whitelisted User-Agent baked in (e.g. Kimi); else `None`.
    pub user_agent: Option<&'static str>,
    /// Built-in model list. Empty for `custom` (declared in config).
    pub models: &'static [ModelSpec],
}

/// Every known provider, in picker/display order. One entry per brand; a brand
/// that ships several products (e.g. MiniMax Token Plan + a future Platform API)
/// groups them here under the same section.
static SPECS: &[ProviderSpec] = &[
    // ── OpenAI ──────────────────────────────────────────────────────────────
    ProviderSpec {
        id: "openai",
        display_name: "OpenAI",
        endpoints: &[Endpoint {
            protocol: Protocol::OpenAi,
            base_url: "https://api.openai.com/v1",
            auth: Auth::Bearer,
        }],
        api_key_required: true,
        user_agent: None,
        models: &[
            ModelSpec {
                name: "gpt-4o",
                context: None,
                reasoning_effort: &[],
            },
            ModelSpec {
                name: "o3",
                context: None,
                reasoning_effort: &["low", "medium", "high"],
            },
        ],
    },
    // ── Anthropic ───────────────────────────────────────────────────────────
    ProviderSpec {
        id: "anthropic",
        display_name: "Anthropic",
        endpoints: &[Endpoint {
            protocol: Protocol::Anthropic,
            // Root only; the Anthropic struct appends `/v1/messages`.
            base_url: "https://api.anthropic.com",
            auth: Auth::XApiKey,
        }],
        api_key_required: true,
        user_agent: None,
        models: &[ModelSpec {
            name: "claude-sonnet-4-20250514",
            context: None,
            reasoning_effort: &[],
        }],
    },
    // ── DeepSeek ────────────────────────────────────────────────────────────
    ProviderSpec {
        id: "deepseek",
        display_name: "DeepSeek",
        endpoints: &[Endpoint {
            protocol: Protocol::OpenAi,
            base_url: "https://api.deepseek.com/v1",
            auth: Auth::Bearer,
        }],
        api_key_required: true,
        user_agent: None,
        models: &[
            ModelSpec {
                name: "deepseek-v4-flash",
                context: None,
                reasoning_effort: &[],
            },
            ModelSpec {
                name: "deepseek-v4-pro",
                context: Some(128000),
                reasoning_effort: &["high", "max"],
            },
        ],
    },
    // ── Google Gemini ───────────────────────────────────────────────────────
    ProviderSpec {
        id: "gemini",
        display_name: "Google Gemini",
        endpoints: &[Endpoint {
            protocol: Protocol::Gemini,
            base_url: "https://generativelanguage.googleapis.com/v1beta",
            auth: Auth::QueryKey,
        }],
        api_key_required: true,
        user_agent: None,
        models: &[ModelSpec {
            name: "gemini-2.5-pro",
            context: None,
            reasoning_effort: &[],
        }],
    },
    // ── Kimi Coding Plan (whitelisted User-Agent baked in) ──────────────────
    ProviderSpec {
        id: "kimi-code",
        display_name: "Kimi Coding Plan",
        endpoints: &[Endpoint {
            protocol: Protocol::OpenAi,
            base_url: "https://api.kimi.com/coding/v1",
            auth: Auth::Bearer,
        }],
        api_key_required: true,
        user_agent: Some("KimiCLI/1.44.0"),
        models: &[ModelSpec {
            name: "kimi-for-coding",
            // models.dev doesn't know this alias; declare its 256K window.
            context: Some(262144),
            reasoning_effort: &[],
        }],
    },
    // ── Moonshot AI open platform (China) ───────────────────────────────────
    ProviderSpec {
        id: "moonshot-platform-cn",
        display_name: "Moonshot Platform CN",
        endpoints: &[Endpoint {
            protocol: Protocol::OpenAi,
            base_url: "https://api.moonshot.cn/v1",
            auth: Auth::Bearer,
        }],
        api_key_required: true,
        user_agent: None,
        models: &[
            ModelSpec {
                name: "kimi-latest",
                context: None,
                reasoning_effort: &[],
            },
            ModelSpec {
                name: "moonshot-v1-128k",
                context: None,
                reasoning_effort: &[],
            },
        ],
    },
    // ── MiniMax ─────────────────────────────────────────────────────────────
    // The subscription Token Plan ships now; a future pay-as-you-go Platform API
    // would be a second ProviderSpec in this section. Token Plan serves the same
    // models over two protocols — Anthropic is listed first (and auto-selected)
    // because MiniMax recommends it for prompt-cache advantages; override with
    // `protocol = "openai"`.
    ProviderSpec {
        id: "minimax-token-plan",
        display_name: "MiniMax Token Plan",
        endpoints: &[
            Endpoint {
                protocol: Protocol::Anthropic,
                base_url: "https://api.minimaxi.com/anthropic",
                auth: Auth::Bearer,
            },
            Endpoint {
                protocol: Protocol::OpenAi,
                base_url: "https://api.minimaxi.com/v1",
                auth: Auth::Bearer,
            },
        ],
        api_key_required: true,
        user_agent: None,
        models: &[
            ModelSpec {
                name: "MiniMax-M2.7",
                context: None,
                reasoning_effort: &[],
            },
            ModelSpec {
                name: "MiniMax-M2.7-highspeed",
                context: None,
                reasoning_effort: &[],
            },
        ],
    },
    // ── Ollama (local; no key, no tool support) ─────────────────────────────
    ProviderSpec {
        id: "ollama",
        display_name: "Ollama (local)",
        endpoints: &[Endpoint {
            protocol: Protocol::Ollama,
            base_url: "http://localhost:11434",
            auth: Auth::None,
        }],
        api_key_required: false,
        user_agent: None,
        models: &[ModelSpec {
            name: "llama3",
            context: None,
            reasoning_effort: &[],
        }],
    },
    // ── Custom: a generic OpenAI-compatible escape hatch ────────────────────
    // Nothing is baked: selecting `custom` requires the user to supply `api_url`
    // and `models` in `[providers.custom]`; the endpoint is synthesized as
    // OpenAI + Bearer during resolution.
    ProviderSpec {
        id: "custom",
        display_name: "Custom (OpenAI-compatible)",
        endpoints: &[],
        api_key_required: true,
        user_agent: None,
        models: &[],
    },
];

/// Every known provider, in picker/display order.
pub fn all() -> &'static [ProviderSpec] {
    SPECS
}

/// Look up a provider by its `id` (the `[providers.<id>]` table name).
pub fn lookup(id: &str) -> Option<&'static ProviderSpec> {
    SPECS.iter().find(|s| s.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_unique_and_specs_well_formed() {
        let mut ids = std::collections::HashSet::new();
        for spec in all() {
            assert!(ids.insert(spec.id), "duplicate provider id: {}", spec.id);
            if spec.id == "custom" {
                assert!(spec.endpoints.is_empty(), "custom must have no endpoints");
                assert!(spec.models.is_empty(), "custom must have no models");
            } else {
                assert!(!spec.endpoints.is_empty(), "{} has no endpoints", spec.id);
                assert!(!spec.models.is_empty(), "{} has no models", spec.id);
            }
        }
    }

    #[test]
    fn protocol_deserializes_from_config_string() {
        assert_eq!(
            serde_json::from_str::<Protocol>("\"openai\"").unwrap(),
            Protocol::OpenAi
        );
        assert_eq!(
            serde_json::from_str::<Protocol>("\"openai-compatible\"").unwrap(),
            Protocol::OpenAi
        );
        assert_eq!(
            serde_json::from_str::<Protocol>("\"anthropic\"").unwrap(),
            Protocol::Anthropic
        );
        assert!(serde_json::from_str::<Protocol>("\"nope\"").is_err());
    }

    #[test]
    fn lookup_works() {
        assert_eq!(
            lookup("minimax-token-plan").map(|s| s.id),
            Some("minimax-token-plan")
        );
        assert!(lookup("does-not-exist").is_none());
    }
}
