//! Built-in provider declarations: brand metadata (endpoints, auth, model lists)
//! compiled into the binary. `config.toml` only supplies the API key plus
//! optional overrides; everything else is baked here. Adding a provider is a new
//! [`ProviderSpec`] literal in the `SPECS` table below — there is no dispatch
//! code to edit.
//!
//! Model lists are a curated, coding-relevant subset per brand; extend or
//! override any of them via `[providers.<id>].models` in config.toml.

use crate::llm::protocols::{Auth, Protocol};

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
    /// Extra HTTP headers baked in for a provider plan (e.g. Kimi's whitelisted
    /// User-Agent). Auth headers are owned by [`Endpoint::auth`], not this list.
    pub request_headers: &'static [(&'static str, &'static str)],
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
        request_headers: &[],
        models: &[
            ModelSpec {
                name: "gpt-5.5",
                context: Some(1_000_000),
                reasoning_effort: &["none", "low", "medium", "high", "xhigh"],
            },
            ModelSpec {
                name: "gpt-5.4-mini",
                context: Some(400_000),
                reasoning_effort: &["none", "low", "medium", "high", "xhigh"],
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
        request_headers: &[],
        models: &[
            ModelSpec {
                name: "claude-sonnet-4-6",
                context: Some(1_000_000),
                reasoning_effort: &[],
            },
            ModelSpec {
                name: "claude-opus-4-8",
                context: Some(1_000_000),
                reasoning_effort: &[],
            },
        ],
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
        request_headers: &[],
        models: &[
            ModelSpec {
                name: "deepseek-v4-flash",
                context: Some(1_000_000),
                reasoning_effort: &["high", "max"],
            },
            ModelSpec {
                name: "deepseek-v4-pro",
                context: Some(1_000_000),
                reasoning_effort: &["high", "max"],
            },
        ],
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
        request_headers: &[("User-Agent", "KimiCLI/1.44.0")],
        models: &[ModelSpec {
            name: "kimi-for-coding",
            // models.dev doesn't know this alias; declare its 256K window.
            context: Some(262144),
            reasoning_effort: &[],
        }],
    },
    // ── Volcengine Ark Coding Plan (flat-fee, aggregates third-party models) ─
    // The `/api/coding/v3` path consumes Coding Plan quota; the sibling
    // `/api/v3` is the pay-as-you-go inference endpoint and bypasses the plan.
    // Ark routes to multiple vendors under a single subscription — these names
    // are the literal `model` strings the Ark Coding Plan accepts, not the
    // upstream vendor IDs (e.g. `minimax-m3` here vs `MiniMax-M3` under
    // `minimax-token-plan`). Context windows left None to defer to models.dev;
    // reasoning_effort left empty pending Ark-side knob confirmation.
    ProviderSpec {
        id: "ark-coding",
        display_name: "Ark Coding Plan",
        endpoints: &[Endpoint {
            protocol: Protocol::OpenAi,
            base_url: "https://ark.cn-beijing.volces.com/api/coding/v3",
            auth: Auth::Bearer,
        }],
        api_key_required: true,
        request_headers: &[],
        models: &[
            ModelSpec {
                name: "doubao-seed-2.0-code",
                context: None,
                reasoning_effort: &[],
            },
            ModelSpec {
                name: "doubao-seed-2.0-pro",
                context: None,
                reasoning_effort: &[],
            },
            ModelSpec {
                name: "doubao-seed-2.0-lite",
                context: None,
                reasoning_effort: &[],
            },
            ModelSpec {
                name: "doubao-seed-code",
                context: None,
                reasoning_effort: &[],
            },
            ModelSpec {
                name: "minimax-m2.7",
                context: None,
                reasoning_effort: &[],
            },
            ModelSpec {
                name: "minimax-m3",
                context: None,
                reasoning_effort: &[],
            },
            ModelSpec {
                name: "glm-5.1",
                context: None,
                reasoning_effort: &[],
            },
            ModelSpec {
                name: "deepseek-v4-flash",
                context: None,
                reasoning_effort: &[],
            },
            ModelSpec {
                name: "deepseek-v4-pro",
                context: None,
                reasoning_effort: &[],
            },
            ModelSpec {
                name: "kimi-k2.6",
                context: None,
                reasoning_effort: &[],
            },
        ],
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
        request_headers: &[],
        models: &[
            ModelSpec {
                name: "kimi-k2.6",
                context: None,
                reasoning_effort: &[],
            },
            ModelSpec {
                name: "kimi-k2.5",
                context: Some(262_144),
                reasoning_effort: &[],
            },
            ModelSpec {
                name: "kimi-latest",
                context: None,
                reasoning_effort: &[],
            },
            ModelSpec {
                name: "moonshot-v1-128k",
                context: Some(131_072),
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
        request_headers: &[],
        models: &[
            ModelSpec {
                name: "MiniMax-M3",
                context: Some(1_000_000),
                reasoning_effort: &[],
            },
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
    // ── Zhipu GLM (BigModel open platform, China) ───────────────────────────
    ProviderSpec {
        id: "zhipu",
        display_name: "Zhipu GLM (BigModel)",
        endpoints: &[Endpoint {
            protocol: Protocol::OpenAi,
            base_url: "https://open.bigmodel.cn/api/paas/v4",
            auth: Auth::Bearer,
        }],
        api_key_required: true,
        request_headers: &[],
        models: &[ModelSpec {
            // GLM controls reasoning via a `thinking` body param, not OpenAI's
            // reasoning_effort, so leave the effort list empty.
            name: "glm-5.1",
            context: Some(200_000),
            reasoning_effort: &[],
        }],
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
        request_headers: &[],
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
        request_headers: &[],
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
        assert!(serde_json::from_str::<Protocol>("\"gemini\"").is_err());
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

    #[test]
    fn baked_models_track_current_default_ids() {
        let openai = lookup("openai").unwrap();
        assert_eq!(openai.models[0].name, "gpt-5.5");
        assert_eq!(openai.models[1].name, "gpt-5.4-mini");

        let anthropic = lookup("anthropic").unwrap();
        assert_eq!(anthropic.models[0].name, "claude-sonnet-4-6");
        assert!(anthropic.models.iter().any(|m| m.name == "claude-opus-4-8"));

        let moonshot = lookup("moonshot-platform-cn").unwrap();
        assert_eq!(moonshot.models[0].name, "kimi-k2.6");
        assert!(moonshot.models.iter().any(|m| m.name == "kimi-k2.5"));

        let zhipu = lookup("zhipu").unwrap();
        assert_eq!(zhipu.models[0].name, "glm-5.1");

        let ark = lookup("ark-coding").unwrap();
        assert_eq!(
            ark.endpoints[0].base_url,
            "https://ark.cn-beijing.volces.com/api/coding/v3"
        );
        // Multi-vendor aggregation under one subscription — sample one from each
        // vendor cluster to catch accidental removals from the curated list.
        for name in &[
            "glm-5.1",
            "minimax-m3",
            "deepseek-v4-flash",
            "kimi-k2.6",
            "doubao-seed-2.0-code",
        ] {
            assert!(
                ark.models.iter().any(|m| &m.name == name),
                "ark-coding missing model {name}"
            );
        }
    }

    #[test]
    fn gemini_is_not_baked_in() {
        assert!(lookup("gemini").is_none());
        assert!(!all().iter().any(|s| s.id == "gemini"));
    }

    #[test]
    fn deepseek_metadata_matches_current_v4_docs() {
        let deepseek = lookup("deepseek").unwrap();
        for model in deepseek.models {
            assert_eq!(model.context, Some(1_000_000));
            assert_eq!(model.reasoning_effort, &["high", "max"]);
        }
    }
}
