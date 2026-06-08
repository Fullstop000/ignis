//! The `/connect` provider-setup wizard — a small state machine: pick provider
//! → enter API key → pick default model, persisting to `~/.ignis/config.toml`
//! and `~/.ignis/state.json` on the final step.
//!
//! Self-contained: each step returns a `PickerRequest` for the runner to install
//! and, on completion, the notices to show + the `(provider, model)` for `App` to
//! adopt — it never reaches into `App`. That keeps the flow's state (one
//! `Option<ConnectDraft>`) and ~150 lines of logic off the `App` god-struct;
//! `App` keeps only thin coordinator wrappers that emit the notices and update
//! its own `provider`/`model`/`effort` fields.

use crate::console::picker::{PickerAnswer, PickerOption, PickerQuestion, PickerRequest};

/// `/connect` multi-step flow state. Created when the user types `/connect`,
/// cleared on completion or cancel. Each step's answer feeds the next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ConnectStep {
    PickProvider,
    EnterApiKey,
    PickModel,
}

#[derive(Clone)]
pub(crate) struct ConnectDraft {
    pub(crate) step: ConnectStep,
    /// Provider id (the `SPECS` key, e.g. "openai"). Set after step 1.
    pub(crate) provider_id: Option<String>,
    /// Provider display name (e.g. "OpenAI"). Used for the API-key prompt.
    pub(crate) provider_display: Option<String>,
    /// Raw API key as typed. Stays in memory until the persist step writes
    /// `[providers.<id>] api_key = "…"` to `config.toml`. None for Ollama
    /// and similar providers with `api_key_required = false`.
    pub(crate) api_key: Option<String>,
    /// Selected model name (e.g. "gpt-5.5"). Set after step 3.
    pub(crate) model: Option<String>,
}

// Manual `Debug` that redacts `api_key` — a derived impl would print the
// plaintext key the moment something `dbg!(&draft)`s or a tracing span captures
// `App` state. Keep the redaction; never derive Debug on this struct.
impl std::fmt::Debug for ConnectDraft {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectDraft")
            .field("step", &self.step)
            .field("provider_id", &self.provider_id)
            .field("provider_display", &self.provider_display)
            .field("api_key", &self.api_key.as_ref().map(|_| "***"))
            .field("model", &self.model)
            .finish()
    }
}

/// What `App::advance_connect` tells the caller (`keys.rs`) to do next. This is
/// the public, App-facing signal; the richer [`ConnectOutcome`] stays internal.
#[derive(Debug)]
pub(crate) enum ConnectAdvance {
    /// Send this picker over `picker_tx` — it's the next step in the flow.
    NextPicker(PickerRequest),
    /// Connect succeeded. The agent loop needs a fresh config from disk so its
    /// in-memory `agent_config` reflects the new `api_key` (the existing
    /// `SetModel` variant doesn't carry providers, hence the dedicated
    /// `ReloadConfig` request).
    Saved,
    /// Connect aborted (user picked Custom, persist failed, etc). A user-facing
    /// notice has already been added; the caller does nothing else.
    Failed,
}

/// Result of advancing the flow one step — what [`ConnectFlow`] hands back to
/// the `App` coordinator, which turns it into a [`ConnectAdvance`].
pub(crate) enum ConnectOutcome {
    /// The flow continues; install this picker.
    NextPicker(PickerRequest),
    /// The flow ended. Emit `notices` in order; if `applied` is `Some`, `App`
    /// should adopt the new `(provider, model)` and request a config reload.
    Done {
        notices: Vec<String>,
        applied: Option<(String, String)>,
    },
}

#[derive(Default)]
pub(crate) struct ConnectFlow {
    draft: Option<ConnectDraft>,
}

impl ConnectFlow {
    /// The in-flight draft, for test introspection of step/field transitions.
    /// Production routes by `is_active()` and the answer alone — it never needs
    /// to read the draft's contents.
    #[cfg(test)]
    pub(crate) fn draft(&self) -> Option<&ConnectDraft> {
        self.draft.as_ref()
    }

    /// Whether a `/connect` flow is currently in progress.
    pub(crate) fn is_active(&self) -> bool {
        self.draft.is_some()
    }

    /// Begin the flow: stash a fresh draft and return the provider picker.
    /// `Err(notice)` (no draft created) if a picker is already open — the caller
    /// shows the notice instead of stomping the existing picker.
    pub(crate) fn start(
        &mut self,
        picker_open: bool,
        current_provider: Option<String>,
    ) -> Result<PickerRequest, String> {
        if picker_open {
            return Err("/connect: another picker is open; close it first.".to_string());
        }
        self.draft = Some(ConnectDraft {
            step: ConnectStep::PickProvider,
            provider_id: None,
            provider_display: None,
            api_key: None,
            model: None,
        });
        Ok(build_provider_picker(current_provider.as_deref()))
    }

    /// Drive the flow one step forward given the picker's answer. Mutates the
    /// draft, and on the final step writes `config.toml` + `state.json`.
    pub(crate) fn advance(&mut self, answers: Vec<PickerAnswer>) -> ConnectOutcome {
        // The draft must be set by `start` before this is called; a missing
        // draft is a programming error, not a user-facing situation.
        let Some(draft) = self.draft.as_mut() else {
            return ConnectOutcome::Done {
                notices: vec![],
                applied: None,
            };
        };
        let answer = match answers.into_iter().next() {
            Some(PickerAnswer::Single(s)) => s,
            // Connect pickers are all single-select; a Multi answer means the
            // picker shape got out of sync somewhere — treat as cancel.
            _ => {
                self.draft = None;
                return ConnectOutcome::Done {
                    notices: vec![],
                    applied: None,
                };
            }
        };
        match draft.step {
            ConnectStep::PickProvider => {
                let Some(spec) = crate::llm::providers::all()
                    .iter()
                    .find(|s| s.display_name == answer)
                else {
                    self.draft = None;
                    return ConnectOutcome::Done {
                        notices: vec![format!("Unknown provider: {answer}")],
                        applied: None,
                    };
                };
                // The `custom` brand requires `api_url` + `models` fields that
                // need a multi-field form; we don't build that wizard in v1.
                // Bail out with a pointer to the example config.
                if spec.id == "custom" {
                    self.draft = None;
                    return ConnectOutcome::Done {
                        notices: vec![
                            "For custom providers, edit ~/.ignis/config.toml — see config.example.toml."
                                .to_string(),
                        ],
                        applied: None,
                    };
                }
                draft.provider_id = Some(spec.id.to_string());
                draft.provider_display = Some(spec.display_name.to_string());
                // Ollama-class providers skip the key step entirely.
                if spec.api_key_required {
                    draft.step = ConnectStep::EnterApiKey;
                    ConnectOutcome::NextPicker(build_api_key_picker(spec.display_name))
                } else {
                    draft.step = ConnectStep::PickModel;
                    ConnectOutcome::NextPicker(build_model_picker(spec))
                }
            }
            ConnectStep::EnterApiKey => {
                draft.api_key = Some(answer);
                let provider_id = draft.provider_id.clone().unwrap_or_default();
                let Some(spec) = crate::llm::providers::lookup(&provider_id) else {
                    self.draft = None;
                    return ConnectOutcome::Done {
                        notices: vec![format!("Unknown provider id: {provider_id}")],
                        applied: None,
                    };
                };
                draft.step = ConnectStep::PickModel;
                ConnectOutcome::NextPicker(build_model_picker(spec))
            }
            ConnectStep::PickModel => {
                draft.model = Some(answer);
                let draft = self.draft.take().expect("draft set above");
                persist(draft)
            }
        }
    }

    /// Discard the in-flight draft; returns the cancel notice iff a draft was
    /// actually in flight (an Esc/Ctrl-C with no `/connect` open is silent).
    pub(crate) fn cancel(&mut self) -> Option<String> {
        self.draft.take().map(|_| "/connect cancelled.".to_string())
    }
}

/// Write the resolved draft to disk. Returns the notices to show and, on
/// success, the `(provider, model)` for `App` to adopt.
fn persist(draft: ConnectDraft) -> ConnectOutcome {
    let mut notices = Vec::new();
    let (provider_id, model) = match (draft.provider_id, draft.model) {
        (Some(p), Some(m)) => (p, m),
        _ => {
            return ConnectOutcome::Done {
                notices,
                applied: None,
            }
        }
    };
    // Key may be empty for providers that don't require one (Ollama). The config
    // writer is only called when there's actually a key to store — otherwise
    // we'd write `api_key = ""` which is meaningless.
    if let Some(api_key) = draft
        .api_key
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        if let Err(e) = crate::config::write_provider_key(&provider_id, api_key) {
            notices.push(format!(
                "Failed to write ~/.ignis/config.toml: {e}. Nothing saved."
            ));
            return ConnectOutcome::Done {
                notices,
                applied: None,
            };
        }
    }
    // Default-model write into state.json. Failure here is recoverable — the
    // api_key is the expensive thing the user typed; preserve it and tell the
    // user to /model manually.
    if let Err(e) = crate::state::persist_model_selection(&provider_id, &model, None) {
        notices.push(format!(
            "Provider saved but default model not set: {e}. Run /model to set it."
        ));
    }
    notices.push(format!(
        "✓ Connected to {provider_id}. Default model: {model}.\n  \
         Wrote ~/.ignis/config.toml and ~/.ignis/state.json."
    ));
    ConnectOutcome::Done {
        notices,
        applied: Some((provider_id, model)),
    }
}

// ───────────────────────────────────────────────────────────────────────────
// `/connect` picker-request builders. Each returns a fully-formed
// `PickerRequest` for the runner's `picker_tx` mpsc; the reply oneshot is
// fire-and-forget because the flow's state lives in the `ConnectDraft`, not in
// awaiting tasks. The picker-completion path in `keys.rs` reads the draft to
// know which step's answer it just received.
// ───────────────────────────────────────────────────────────────────────────

/// Step 1: pick a provider from the baked-in `SPECS` catalog. The currently-
/// active provider (if any) is mentioned in the question text so users who
/// re-run `/connect` to rotate a key know what they're about to overwrite.
fn build_provider_picker(current_provider: Option<&str>) -> PickerRequest {
    let options: Vec<PickerOption> = crate::llm::providers::all()
        .iter()
        .map(|spec| PickerOption {
            label: spec.display_name.to_string(),
            description: provider_description(spec),
            preview: None,
        })
        .collect();
    let question = match current_provider {
        Some(id) => format!("Connect a provider (current: {id})"),
        None => "Connect a provider — pick one to configure".to_string(),
    };
    let (tx, _rx) = tokio::sync::oneshot::channel();
    PickerRequest {
        questions: vec![PickerQuestion {
            question,
            kind: "connect".to_string(),
            header: "Provider".to_string(),
            multi_select: false,
            allow_other: false,
            text_input: false,
            mask: false,
            options,
        }],
        reply: tx,
    }
}

/// Step 2: API-key entry. Text-input mode, masked (no shoulder-surfing).
fn build_api_key_picker(provider_display: &str) -> PickerRequest {
    let (tx, _rx) = tokio::sync::oneshot::channel();
    PickerRequest {
        questions: vec![PickerQuestion {
            question: format!("Paste your API key for {provider_display}"),
            kind: "connect".to_string(),
            header: "API Key".to_string(),
            multi_select: false,
            allow_other: false,
            text_input: true,
            mask: true,
            options: vec![],
        }],
        reply: tx,
    }
}

/// Step 3: default-model picker, scoped to the chosen provider's `&[ModelSpec]`.
fn build_model_picker(spec: &crate::llm::providers::ProviderSpec) -> PickerRequest {
    let options: Vec<PickerOption> = spec
        .models
        .iter()
        .map(|m| PickerOption {
            label: m.name.to_string(),
            description: model_description(m),
            preview: None,
        })
        .collect();
    let (tx, _rx) = tokio::sync::oneshot::channel();
    PickerRequest {
        questions: vec![PickerQuestion {
            question: format!("Pick a default model for {}", spec.display_name),
            kind: "connect".to_string(),
            header: "Model".to_string(),
            multi_select: false,
            allow_other: false,
            text_input: false,
            mask: false,
            options,
        }],
        reply: tx,
    }
}

/// One-line endpoint hint for the provider row, synthesized from the first
/// endpoint's `base_url`. Strips the protocol so the URL doesn't dominate
/// the line.
fn provider_description(spec: &crate::llm::providers::ProviderSpec) -> String {
    if spec.id == "custom" {
        return "Edit ~/.ignis/config.toml after selecting (api_url + models required)."
            .to_string();
    }
    let host = spec
        .endpoints
        .first()
        .map(|e| {
            e.base_url
                .trim_start_matches("https://")
                .trim_start_matches("http://")
                .to_string()
        })
        .unwrap_or_default();
    if spec.api_key_required {
        host
    } else {
        // Local-only providers don't take a key — call that out so users
        // don't expect to be prompted for one.
        format!("{host}  (no key required)")
    }
}

/// One-line model-row hint: context window if known, else empty. Keeps the
/// row short — full effort/reasoning details live in `/model`.
fn model_description(m: &crate::llm::providers::ModelSpec) -> String {
    match m.context {
        Some(ctx) => format!("context {}", super::format::format_context(ctx)),
        None => String::new(),
    }
}
