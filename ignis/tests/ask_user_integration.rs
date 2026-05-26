//! End-to-end integration test for the `ask_user` tool: drives the same path
//! the console takes (PickerRequest → InlinePickerState → simulated key events
//! → oneshot reply) and asserts the tool's JSON result shape.
//!
//! The two halves are unit-tested in their own modules; this checks the wire
//! between them — the only path with no other coverage.
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ignis::console::inline_picker::{InlinePickerState, KeyOutcome};
use ignis::picker::{PickerRequest, PickerResponse};
use ignis::tools::AskUserTool;
use ignis::AgentTool;
use serde_json::{json, Value};
use tokio::sync::mpsc;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::empty())
}

/// Drive the picker by feeding `keys` until it terminates; reply with the
/// resulting response. Returns the response sent back to the tool's caller.
fn drive(mut state: InlinePickerState, keys: &[KeyEvent]) -> PickerResponse {
    for k in keys {
        match state.on_key(*k) {
            KeyOutcome::Continue => {}
            KeyOutcome::Cancel => {
                let reply = state.reply.take().expect("reply present");
                let _ = reply.send(PickerResponse::Cancelled);
                return PickerResponse::Cancelled;
            }
            KeyOutcome::Done(answers) => {
                let resp = PickerResponse::Answered(answers);
                let reply = state.reply.take().expect("reply present");
                let _ = reply.send(resp.clone());
                return resp;
            }
        }
    }
    panic!("ran out of keys before the picker resolved");
}

#[tokio::test]
async fn end_to_end_single_select() {
    let (tx, mut rx) = mpsc::channel::<PickerRequest>(1);
    let tool = AskUserTool::new(Some(tx));
    let call = tokio::spawn(async move {
        tool.call(json!({
            "questions": [{
                "question": "Which library?",
                "header": "Library",
                "options": [
                    {"label": "serde_json", "description": "stable, std"},
                    {"label": "simd-json",  "description": "fast"}
                ]
            }]
        }))
        .await
    });
    let req = rx.recv().await.unwrap();
    let state = InlinePickerState::new(req);
    // ↓ then ↵ → pick "simd-json"
    drive(state, &[key(KeyCode::Down), key(KeyCode::Enter)]);
    let result = call.await.unwrap();
    assert!(!result.is_error, "got error: {}", result.content);
    let v: Value = serde_json::from_str(&result.content).unwrap();
    assert_eq!(v["answers"][0]["answer"], "simd-json");
    assert_eq!(v["answers"][0]["question"], "Which library?");
}

#[tokio::test]
async fn end_to_end_multi_select() {
    let (tx, mut rx) = mpsc::channel::<PickerRequest>(1);
    let tool = AskUserTool::new(Some(tx));
    let call = tokio::spawn(async move {
        tool.call(json!({
            "questions": [{
                "question": "Which features?",
                "header": "Features",
                "multiSelect": true,
                "options": [
                    {"label": "auth",    "description": "login"},
                    {"label": "logging", "description": "structured logs"},
                    {"label": "metrics", "description": "prometheus"}
                ]
            }]
        }))
        .await
    });
    let req = rx.recv().await.unwrap();
    let state = InlinePickerState::new(req);
    // space (toggle auth) · ↓ ↓ (move to metrics) · space (toggle metrics) · ↵
    drive(
        state,
        &[
            key(KeyCode::Char(' ')),
            key(KeyCode::Down),
            key(KeyCode::Down),
            key(KeyCode::Char(' ')),
            key(KeyCode::Enter),
        ],
    );
    let result = call.await.unwrap();
    assert!(!result.is_error);
    let v: Value = serde_json::from_str(&result.content).unwrap();
    assert_eq!(v["answers"][0]["answer"], json!(["auth", "metrics"]));
}

#[tokio::test]
async fn end_to_end_other_typed_text() {
    let (tx, mut rx) = mpsc::channel::<PickerRequest>(1);
    let tool = AskUserTool::new(Some(tx));
    let call = tokio::spawn(async move {
        tool.call(json!({
            "questions": [{
                "question": "Name?",
                "header": "Name",
                "options": [
                    {"label": "default", "description": "use default"},
                    {"label": "skip",    "description": "skip naming"}
                ]
            }]
        }))
        .await
    });
    let req = rx.recv().await.unwrap();
    let state = InlinePickerState::new(req);
    // ↓ ↓ (move to Other) · type "my-thing" · ↵
    let mut keys = vec![key(KeyCode::Down), key(KeyCode::Down)];
    for c in "my-thing".chars() {
        keys.push(key(KeyCode::Char(c)));
    }
    keys.push(key(KeyCode::Enter));
    drive(state, &keys);
    let result = call.await.unwrap();
    assert!(!result.is_error);
    let v: Value = serde_json::from_str(&result.content).unwrap();
    assert_eq!(v["answers"][0]["answer"], "my-thing");
}

#[tokio::test]
async fn end_to_end_escape_returns_is_error() {
    let (tx, mut rx) = mpsc::channel::<PickerRequest>(1);
    let tool = AskUserTool::new(Some(tx));
    let call = tokio::spawn(async move {
        tool.call(json!({
            "questions": [{
                "question": "Confirm?",
                "header": "Confirm",
                "options": [
                    {"label": "yes", "description": "do it"},
                    {"label": "no",  "description": "abort"}
                ]
            }]
        }))
        .await
    });
    let req = rx.recv().await.unwrap();
    let state = InlinePickerState::new(req);
    drive(state, &[key(KeyCode::Esc)]);
    let result = call.await.unwrap();
    assert!(result.is_error);
    assert!(
        result.content.to_lowercase().contains("cancel"),
        "expected 'cancel' in: {}",
        result.content
    );
}

#[tokio::test]
async fn end_to_end_two_question_flow() {
    let (tx, mut rx) = mpsc::channel::<PickerRequest>(1);
    let tool = AskUserTool::new(Some(tx));
    let call = tokio::spawn(async move {
        tool.call(json!({
            "questions": [
                {"question": "Lib?",  "header": "Lib", "options": [
                    {"label": "serde_json", "description": "x"},
                    {"label": "simd-json",  "description": "y"}
                ]},
                {"question": "Mode?", "header": "Mode", "options": [
                    {"label": "strict", "description": "x"},
                    {"label": "lax",    "description": "y"}
                ]}
            ]
        }))
        .await
    });
    let req = rx.recv().await.unwrap();
    let state = InlinePickerState::new(req);
    // Q1: ↵ pick serde_json (cursor at 0). Q2: ↓ then ↵ pick lax.
    drive(
        state,
        &[key(KeyCode::Enter), key(KeyCode::Down), key(KeyCode::Enter)],
    );
    let result = call.await.unwrap();
    assert!(!result.is_error);
    let v: Value = serde_json::from_str(&result.content).unwrap();
    let answers = v["answers"].as_array().unwrap();
    assert_eq!(answers.len(), 2);
    assert_eq!(answers[0]["answer"], "serde_json");
    assert_eq!(answers[1]["answer"], "lax");
}
