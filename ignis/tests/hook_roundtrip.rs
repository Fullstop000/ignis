//! Integration test for the hook protocol: a real subprocess shell script
//! reads the JSON envelope from stdin, emits a rewrite on stdout, and the
//! `HookRegistry` API surfaces the rewrite back to the caller.
//!
//! Skipped automatically on non-Unix (no `chmod +x`, no `/bin/sh`); the
//! protocol is portable but this fixture isn't.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use ignis::hooks::{HookContext, HookRegistry, HookSpec, HooksConfig, PromptHookResult};
use tokio::sync::mpsc;

fn write_executable(dir: &std::path::Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
    path
}

#[tokio::test]
async fn user_prompt_submit_chain_rewrites_prompt() {
    let dir = tempfile::tempdir().unwrap();

    // Stub hook: reads JSON envelope from stdin (so the protocol stays
    // round-trippable), then echoes a rewrite. Deliberately ignores the
    // input — the test only cares that the rewrite propagates.
    let hook = write_executable(
        dir.path(),
        "translate.sh",
        r#"#!/bin/sh
# Drain stdin so the writer doesn't see EPIPE.
cat >/dev/null
printf '%s' '{"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","updatedInput":"HELLO_FROM_HOOK"}}'
"#,
    );

    let cfg = HooksConfig {
        user_prompt_submit: vec![HookSpec {
            program: hook,
            args: vec![],
            timeout_ms: 5_000,
        }],
        assistant_message_render: vec![],
    };
    let reg = HookRegistry::from_config(cfg);

    let (tx, mut rx) = mpsc::channel(8);
    let out = reg
        .run_user_prompt_submit(
            "原文",
            HookContext {
                session_id: "test-session",
                cwd: "/tmp",
            },
            &tx,
        )
        .await;
    drop(tx);

    assert_eq!(
        out,
        PromptHookResult::Continue("HELLO_FROM_HOOK".to_string())
    );
    // No warnings on the happy path.
    assert!(rx.recv().await.is_none());
}

#[tokio::test]
async fn assistant_message_render_chain_rewrites_output() {
    let dir = tempfile::tempdir().unwrap();
    let hook = write_executable(
        dir.path(),
        "translate-out.sh",
        r#"#!/bin/sh
cat >/dev/null
printf '%s' '{"hookSpecificOutput":{"hookEventName":"AssistantMessageRender","updatedOutput":"渲染后"}}'
"#,
    );
    let cfg = HooksConfig {
        user_prompt_submit: vec![],
        assistant_message_render: vec![HookSpec {
            program: hook,
            args: vec![],
            timeout_ms: 5_000,
        }],
    };
    let reg = HookRegistry::from_config(cfg);
    let (tx, mut rx) = mpsc::channel(8);
    let out = reg
        .run_assistant_message_render(
            "raw english",
            HookContext {
                session_id: "s",
                cwd: "/tmp",
            },
            &tx,
        )
        .await;
    drop(tx);
    assert_eq!(out, "渲染后");
    assert!(rx.recv().await.is_none());
}

#[tokio::test]
async fn config_file_round_trip_drives_chain() {
    // Asserts the user-facing path end-to-end:
    //   write hooks.json on disk → HookRegistry::from_config_dir(home)
    //   → run_user_prompt_submit returns rewrite.
    let home = tempfile::tempdir().unwrap();
    let ignis_dir = home.path().join(".ignis");
    std::fs::create_dir_all(&ignis_dir).unwrap();
    let hook = write_executable(
        &ignis_dir,
        "rewriter.sh",
        r#"#!/bin/sh
cat >/dev/null
printf '%s' '{"hookSpecificOutput":{"updatedInput":"FROM_DISK"}}'
"#,
    );
    let hook_str = hook.to_string_lossy();
    let raw = format!(r#"{{"hooks": {{"UserPromptSubmit": [{{"command": "{hook_str}"}}]}}}}"#);
    std::fs::write(ignis_dir.join("hooks.json"), raw).unwrap();

    let reg = HookRegistry::from_config_dir(home.path()).unwrap();
    let (tx, _rx) = mpsc::channel(8);
    let out = reg
        .run_user_prompt_submit(
            "ignored",
            HookContext {
                session_id: "s",
                cwd: "/tmp",
            },
            &tx,
        )
        .await;
    assert_eq!(out, PromptHookResult::Continue("FROM_DISK".to_string()));
}

/// Inject (Ctrl+S steer) text must run through the UserPromptSubmit
/// chain — otherwise a bilingual hook only translates the initial
/// prompt and every steer message reaches the model untranslated.
/// Drives Session::prompt with one stub hook that uppercases the text
/// + an inject preloaded on the channel, then inspects storage.
#[tokio::test]
async fn inject_runs_through_user_prompt_submit_hook_chain() {
    use async_trait::async_trait;
    use futures_util::stream::{self, BoxStream, StreamExt};
    use ignis::llm::{LlmProvider, LlmResponseDelta};
    use ignis::storage::{InMemoryStorage, SessionStorage};
    use ignis::{AgentEvent, Message, Session};

    // Minimal mock provider — returns one empty response per call so
    // both turn rounds complete.
    struct EmptyProvider;
    #[async_trait]
    impl LlmProvider for EmptyProvider {
        fn model_id(&self) -> &str {
            "mock"
        }
        fn provider_name(&self) -> &str {
            "mock"
        }
        async fn chat_stream(
            &self,
            _system_prompt: &str,
            _messages: &[Message],
            _tools: &[serde_json::Value],
        ) -> Result<BoxStream<'static, Result<LlmResponseDelta, anyhow::Error>>, anyhow::Error>
        {
            // One token so the round produces a message and exits cleanly.
            Ok(stream::iter(vec![Ok(LlmResponseDelta::Text("ok".to_string()))]).boxed())
        }
    }

    let dir = tempfile::tempdir().unwrap();
    // Upper-case translator: reads the prompt out of the stdin envelope
    // via `jq`-free awk-ish slicing, ignores it for simplicity, and emits
    // a fixed rewrite. The inject test only needs to prove the chain
    // runs — the post-hook text differs from the pre-hook text.
    let upper = write_executable(
        dir.path(),
        "upper.sh",
        r#"#!/bin/sh
# Drain stdin so the writer doesn't see EPIPE.
RAW=$(cat)
# Crude: pull the inject body and uppercase it. Avoids a jq dependency
# in the integration test; the protocol round-trip is covered elsewhere.
PROMPT=$(printf '%s' "$RAW" | sed -E 's/.*"prompt":"([^"]*)".*/\1/' )
UPPER=$(printf '%s' "$PROMPT" | tr '[:lower:]' '[:upper:]')
printf '{"hookSpecificOutput":{"updatedInput":"%s"}}' "$UPPER"
"#,
    );

    let storage = InMemoryStorage::new();
    let mut session = Session::open(
        "inj-hook".to_string(),
        "system".to_string(),
        Box::new(EmptyProvider),
        Box::new(storage.clone()),
        "/tmp".to_string(),
    )
    .await
    .unwrap();

    let cfg = HooksConfig {
        user_prompt_submit: vec![HookSpec {
            program: upper,
            args: vec![],
            timeout_ms: 5_000,
        }],
        assistant_message_render: vec![],
    };
    session.set_hook_registry(HookRegistry::from_config(cfg));

    // Preload one steer message before calling prompt — it lands on the
    // inject channel and drain_injected picks it up between rounds.
    let (inj_tx, inj_rx) = tokio::sync::mpsc::channel::<String>(8);
    inj_tx.try_send("steer me".to_string()).unwrap();
    session.set_inject_source(inj_rx);

    let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(64);
    session.prompt("hi there", tx).await.unwrap();
    // Drain events so the channel closes.
    while rx.recv().await.is_some() {}

    let hist = storage.load_session("inj-hook").await.unwrap();
    let user_msgs: Vec<&str> = hist
        .iter()
        .filter(|m| m.role == "user")
        .filter_map(|m| m.content.as_deref())
        .collect();
    // Both messages — the original prompt AND the inject — were
    // upper-cased by the hook before reaching history.
    assert_eq!(user_msgs, vec!["HI THERE", "STEER ME"]);
}
