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

    assert_eq!(out, PromptHookResult::Continue("HELLO_FROM_HOOK".to_string()));
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
