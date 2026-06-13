//! `ask_user` — model-initiated interactive picker. The tool builds a
//! [`PickerRequest`] from the model's JSON args, sends it to the console over
//! the shared mpsc channel, and `await`s the user's response on a oneshot.
//!
//! Designed to be the *only* primitive in the agent loop that pauses on user
//! input mid-turn. Without an attached channel the tool is a no-op (returns an
//! error explaining the situation); this keeps subagents and one-shot CLI runs
//! safe to construct even when no TUI is wired up.
use crate::interaction::{
    PickerAnswer, PickerOption, PickerQuestion, PickerRequest, PickerResponse,
};
use crate::{AgentTool, ExecutionMode, ToolResult};
use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};

/// Hard limits on header chip length and free-text "Other" answers. These are
/// guard-rails against either the model or the user blowing up the picker.
pub(crate) const MAX_HEADER_LEN: usize = 12;
/// Cap on the free-text "Other" answer (bytes). 4 KiB is plenty for the
/// purpose and prevents paste-bombing the model.
pub const MAX_OTHER_LEN: usize = 4096;
/// The auto-appended free-text option's label.
pub const OTHER_LABEL: &str = "Other (type custom)…";

pub struct AskUserTool {
    /// `None` when no console is attached (e.g. one-shot CLI, subagent in a
    /// non-TUI context) — `call` returns is_error explaining that.
    picker_tx: Option<mpsc::Sender<PickerRequest>>,
    /// Permission state shared with the agent loop. When `afk` is on we
    /// auto-dismiss the question with a fixed reply so the model can finish
    /// end-to-end without waiting for a user who isn't there.
    permissions: Option<std::sync::Arc<crate::permissions::runtime::PermissionState>>,
}

impl AskUserTool {
    pub fn new(picker_tx: Option<mpsc::Sender<PickerRequest>>) -> Self {
        Self {
            picker_tx,
            permissions: None,
        }
    }

    /// Attach the shared permission state. Constructed at session-build time
    /// in `main.rs`; tests typically leave it `None`.
    pub fn with_permissions(
        mut self,
        permissions: std::sync::Arc<crate::permissions::runtime::PermissionState>,
    ) -> Self {
        self.permissions = Some(permissions);
        self
    }
}

#[async_trait]
impl AgentTool for AskUserTool {
    fn name(&self) -> &str {
        "ask_user"
    }

    fn description(&self) -> &str {
        "Ask the user one or more questions and wait for their pick. \
         Reserve this for decisions whose answer changes your next action \
         (disambiguating a request, choosing between concrete approaches, \
         confirming irreversible actions). Do NOT use it for things you can \
         verify yourself or for casual chit-chat. Each question gets 2–4 \
         options; an 'Other (type custom)' option is appended automatically \
         so the user can always free-text. To recommend an option, put it \
         first and append ' (Recommended)' to its label. \
         Use 'preview' on an option ONLY for design-style choices where the \
         user benefits from SEEING the alternative (UI layouts, code \
         snippets, ASCII diagrams, diffs). The presence of 'preview' on any \
         option flips the picker into a side-by-side layout — leave it off \
         for plain text-only choices."
    }

    fn execution_mode(&self) -> ExecutionMode {
        // Only one picker can be open at a time; if a batch contained two
        // `ask_user` calls in parallel, the second would race into the
        // console's "already-open" guard and come back as a phantom Cancel.
        // Force serial scheduling so that never happens.
        ExecutionMode::Sequential
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "questions": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 4,
                    "description": "1-4 questions to ask in sequence.",
                    "items": {
                        "type": "object",
                        "required": ["question", "header", "options"],
                        "properties": {
                            "question":    { "type": "string", "description": "The complete question text." },
                            "header":      { "type": "string", "description": "Short chip label (≤12 chars)." },
                            "multiSelect": { "type": "boolean", "default": false, "description": "true for space-to-toggle multi-select." },
                            "options": {
                                "type": "array",
                                "minItems": 2,
                                "maxItems": 4,
                                "description": "2-4 distinct options (an 'Other' free-text row is added automatically).",
                                "items": {
                                    "type": "object",
                                    "required": ["label", "description"],
                                    "properties": {
                                        "label":       { "type": "string", "description": "1-5 word display text." },
                                        "description": { "type": "string", "description": "Context explaining the choice." },
                                        "preview":     { "type": "string", "description": "Optional multi-line code/ASCII/mockup. Triggers a side-by-side picker layout (option list left, bordered Preview pane right). Use ONLY for visual/design choices the user benefits from seeing; skip for plain text decisions." }
                                    }
                                }
                            }
                        }
                    }
                }
            },
            "required": ["questions"]
        })
    }

    async fn call(&self, args: Value) -> ToolResult {
        let questions = match parse_questions(&args) {
            Ok(q) => q,
            Err(e) => return ToolResult::error(format!("ask_user: {e}")),
        };

        // Fully-unattended mode auto-dismisses: no user is present to answer.
        // Reply with empty answers + a clear `dismissed`/`reason` so the model
        // can adapt. The lighter HandsFree mode leaves `ask_user` alone — at
        // the keyboard, model can still consult the user.
        if let Some(p) = &self.permissions {
            if p.mode().is_fully_unattended() {
                let body = serde_json::json!({
                    "answers": questions
                        .iter()
                        .map(|q| serde_json::json!({
                            "question": q.question,
                            "answer": null,
                            "dismissed": true,
                        }))
                        .collect::<Vec<_>>(),
                    "dismissed": true,
                    "reason": "Running fully unattended. No user is present. Make your best judgment and proceed.",
                });
                return ToolResult::ok(body.to_string());
            }
        }

        let Some(tx) = &self.picker_tx else {
            return ToolResult::error(
                "ask_user: no interactive console is attached (running headless?). \
                 Ask the user in prose instead."
                    .to_string(),
            );
        };

        let (reply_tx, reply_rx) = oneshot::channel();
        let request = PickerRequest {
            questions: questions.clone(),
            reply: reply_tx,
        };
        if tx.send(request).await.is_err() {
            return ToolResult::error(
                "ask_user: console closed before the question opened.".to_string(),
            );
        }
        let response = match reply_rx.await {
            Ok(r) => r,
            Err(_) => {
                return ToolResult::error(
                    "ask_user: session closed before the user answered.".to_string(),
                )
            }
        };

        match response {
            PickerResponse::Cancelled => {
                ToolResult::error("User cancelled the question.".to_string())
            }
            PickerResponse::Answered(answers) => format_answers(&questions, &answers),
        }
    }
}

/// Parse + validate the `questions` array. Header length is truncated
/// silently (it's cosmetic). "Other" is *not* added here — that's the
/// console's responsibility so changing the label doesn't fork.
pub(crate) fn parse_questions(args: &Value) -> Result<Vec<PickerQuestion>, String> {
    let arr = args
        .get("questions")
        .and_then(Value::as_array)
        .ok_or("'questions' must be an array")?;
    if !(1..=4).contains(&arr.len()) {
        return Err(format!(
            "'questions' must have 1-4 items, got {}",
            arr.len()
        ));
    }
    let mut out = Vec::with_capacity(arr.len());
    for (i, q) in arr.iter().enumerate() {
        let question = q
            .get("question")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("question[{i}].question must be a string"))?
            .to_string();
        if question.trim().is_empty() {
            return Err(format!("question[{i}].question must be non-empty"));
        }
        let raw_header = q
            .get("header")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("question[{i}].header must be a string"))?;
        // Truncate by char-count, not byte index, so multi-byte chars stay valid.
        let header: String = raw_header.chars().take(MAX_HEADER_LEN).collect();
        let multi_select = q
            .get("multiSelect")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let opts_arr = q
            .get("options")
            .and_then(Value::as_array)
            .ok_or_else(|| format!("question[{i}].options must be an array"))?;
        if !(2..=4).contains(&opts_arr.len()) {
            return Err(format!(
                "question[{i}].options must have 2-4 items, got {}",
                opts_arr.len()
            ));
        }
        let mut options = Vec::with_capacity(opts_arr.len());
        for (j, o) in opts_arr.iter().enumerate() {
            let label = o
                .get("label")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("question[{i}].options[{j}].label must be a string"))?
                .to_string();
            if label.trim().is_empty() {
                return Err(format!(
                    "question[{i}].options[{j}].label must be non-empty"
                ));
            }
            let description = o
                .get("description")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("question[{i}].options[{j}].description must be a string"))?
                .to_string();
            let preview = o.get("preview").and_then(Value::as_str).map(str::to_string);
            options.push(PickerOption {
                label,
                description,
                preview,
            });
        }
        out.push(PickerQuestion {
            question,
            kind: "ask_user".to_string(),
            header,
            multi_select,
            options,
            allow_other: true,
            text_input: false,
            mask: false,
        });
    }
    Ok(out)
}

/// Build the JSON result the model sees: `{"answers": [{"question": ..., "answer": ...}, ...]}`.
/// Single-select → string, multi-select → array. Lengths must already match.
fn format_answers(questions: &[PickerQuestion], answers: &[PickerAnswer]) -> ToolResult {
    // Picker advances one question at a time, so the lengths are invariant —
    // any mismatch is a programmer error.
    debug_assert_eq!(questions.len(), answers.len());
    let mut out = Vec::with_capacity(questions.len());
    for (q, a) in questions.iter().zip(answers) {
        let answer_val = match a {
            PickerAnswer::Single(s) => Value::String(s.clone()),
            PickerAnswer::Multi(v) => Value::Array(v.iter().cloned().map(Value::String).collect()),
        };
        out.push(json!({
            "question": q.question,
            "answer": answer_val,
        }));
    }
    ToolResult::ok(json!({ "answers": out }).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(question: &str, header: &str, multi: bool, opts: &[(&str, &str)]) -> Value {
        json!({
            "question": question,
            "header": header,
            "multiSelect": multi,
            "options": opts.iter().map(|(l, d)| json!({"label": l, "description": d})).collect::<Vec<_>>(),
        })
    }

    #[test]
    fn parse_rejects_zero_or_five_questions() {
        assert!(parse_questions(&json!({"questions": []})).is_err());
        let five = (0..5)
            .map(|_| q("q", "h", false, &[("a", "A"), ("b", "B")]))
            .collect::<Vec<_>>();
        assert!(parse_questions(&json!({ "questions": five })).is_err());
    }

    #[test]
    fn parse_rejects_one_or_five_options() {
        let one = q("Q?", "h", false, &[("a", "A")]);
        assert!(parse_questions(&json!({"questions": [one]})).is_err());
        let five = q(
            "Q?",
            "h",
            false,
            &[("a", "A"), ("b", "B"), ("c", "C"), ("d", "D"), ("e", "E")],
        );
        assert!(parse_questions(&json!({"questions": [five]})).is_err());
    }

    #[test]
    fn parse_truncates_long_header() {
        let one = q(
            "Q?",
            "this-header-is-way-too-long",
            false,
            &[("a", "A"), ("b", "B")],
        );
        let qs = parse_questions(&json!({"questions": [one]})).unwrap();
        assert_eq!(qs[0].header.chars().count(), MAX_HEADER_LEN);
    }

    #[test]
    fn parse_accepts_minimal_valid_payload_and_preview() {
        let payload = json!({
            "questions": [{
                "question": "Pick one?",
                "header": "Choice",
                "options": [
                    {"label": "alpha", "description": "first", "preview": "// alpha\nfoo"},
                    {"label": "beta",  "description": "second"}
                ]
            }]
        });
        let qs = parse_questions(&payload).unwrap();
        assert_eq!(qs.len(), 1);
        assert!(!qs[0].multi_select);
        assert_eq!(qs[0].options[0].preview.as_deref(), Some("// alpha\nfoo"));
        assert_eq!(qs[0].options[1].preview, None);
    }

    #[test]
    fn parse_rejects_empty_question_or_label() {
        let bad_q = q("   ", "h", false, &[("a", "A"), ("b", "B")]);
        assert!(parse_questions(&json!({"questions": [bad_q]})).is_err());
        let bad_l = json!({
            "question": "Q?", "header": "h", "options": [
                {"label": "", "description": "x"}, {"label": "b", "description": "y"}
            ]
        });
        assert!(parse_questions(&json!({"questions": [bad_l]})).is_err());
    }

    #[test]
    fn format_single_answer_is_string() {
        let qs = parse_questions(
            &json!({"questions": [q("Q?", "h", false, &[("a", "A"), ("b", "B")])]}),
        )
        .unwrap();
        let res = format_answers(&qs, &[PickerAnswer::Single("a".to_string())]);
        let v: Value = serde_json::from_str(&res.content).unwrap();
        assert_eq!(v["answers"][0]["answer"], Value::String("a".to_string()));
        assert!(!res.is_error);
    }

    #[test]
    fn format_multi_answer_is_array() {
        let qs =
            parse_questions(&json!({"questions": [q("Q?", "h", true, &[("a", "A"), ("b", "B")])]}))
                .unwrap();
        let res = format_answers(
            &qs,
            &[PickerAnswer::Multi(vec!["a".to_string(), "b".to_string()])],
        );
        let v: Value = serde_json::from_str(&res.content).unwrap();
        assert_eq!(v["answers"][0]["answer"], json!(["a", "b"]));
    }

    #[tokio::test]
    async fn no_picker_channel_returns_helpful_error() {
        let tool = AskUserTool::new(None);
        let res = tool
            .call(json!({"questions": [q("Q?", "h", false, &[("a", "A"), ("b", "B")])]}))
            .await;
        assert!(res.is_error);
        assert!(res.content.contains("no interactive console"));
    }

    #[tokio::test]
    async fn fully_unattended_auto_dismisses_without_touching_picker_channel() {
        use crate::permissions::runtime::PermissionState;
        use crate::permissions::Mode;
        let (tx, mut rx) = mpsc::channel::<PickerRequest>(1);
        let state = PermissionState::new(Mode::FullyUnattended);
        let tool = AskUserTool::new(Some(tx)).with_permissions(state);
        let res = tool
            .call(json!({"questions": [q("What now?", "h", false, &[("a", "A"), ("b", "B")])]}))
            .await;
        // Tool should NOT have sent a request to the picker — channel stays empty.
        assert!(
            rx.try_recv().is_err(),
            "fully-unattended should not touch the picker channel"
        );
        // The reply must be a successful dismissal (NOT is_error), so the
        // model sees structured "make your best judgment" guidance.
        assert!(
            !res.is_error,
            "fully-unattended dismiss should be success, got error: {}",
            res.content
        );
        let body: serde_json::Value = serde_json::from_str(&res.content).unwrap();
        assert_eq!(body["dismissed"], serde_json::Value::Bool(true));
        assert!(
            body["reason"]
                .as_str()
                .unwrap_or("")
                .contains("fully unattended"),
            "expected 'fully unattended' in reason, got: {body}"
        );
    }

    #[tokio::test]
    async fn invalid_args_return_is_error_before_sending() {
        let (tx, mut rx) = mpsc::channel(1);
        let tool = AskUserTool::new(Some(tx));
        let res = tool.call(json!({"questions": []})).await;
        assert!(res.is_error);
        assert!(rx.try_recv().is_err(), "no request should have been sent");
    }

    #[tokio::test]
    async fn round_trip_single_select() {
        let (tx, mut rx) = mpsc::channel(1);
        let tool = AskUserTool::new(Some(tx));
        let call = tokio::spawn(async move {
            tool.call(json!({"questions": [q("Pick?", "h", false, &[("yes", "y"), ("no", "n")])]}))
                .await
        });
        let req = rx.recv().await.unwrap();
        req.reply
            .send(PickerResponse::Answered(vec![PickerAnswer::Single(
                "yes".to_string(),
            )]))
            .unwrap();
        let res = call.await.unwrap();
        assert!(!res.is_error);
        let v: Value = serde_json::from_str(&res.content).unwrap();
        assert_eq!(v["answers"][0]["answer"], "yes");
    }

    #[tokio::test]
    async fn cancel_returns_is_error() {
        let (tx, mut rx) = mpsc::channel(1);
        let tool = AskUserTool::new(Some(tx));
        let call = tokio::spawn(async move {
            tool.call(json!({"questions": [q("Pick?", "h", false, &[("a", "A"), ("b", "B")])]}))
                .await
        });
        let req = rx.recv().await.unwrap();
        req.reply.send(PickerResponse::Cancelled).unwrap();
        let res = call.await.unwrap();
        assert!(res.is_error);
        assert!(res.content.contains("cancelled"));
    }

    #[tokio::test]
    async fn dropped_reply_is_session_closed_error() {
        let (tx, mut rx) = mpsc::channel(1);
        let tool = AskUserTool::new(Some(tx));
        let call = tokio::spawn(async move {
            tool.call(json!({"questions": [q("Pick?", "h", false, &[("a", "A"), ("b", "B")])]}))
                .await
        });
        let req = rx.recv().await.unwrap();
        drop(req.reply);
        let res = call.await.unwrap();
        assert!(res.is_error);
        assert!(res.content.contains("session closed"));
    }
}

/// End-to-end tests that drive the same path the console takes
/// (PickerRequest → InlinePickerState → simulated keystrokes → oneshot
/// reply). Kept in-crate so the picker types stay `pub(crate)`.
#[cfg(test)]
mod end_to_end {
    use super::*;
    use crate::console::inline_picker::{InlinePickerState, KeyOutcome};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    /// Drive the picker by feeding `keys` until it terminates; reply on the
    /// oneshot the same way the console does. Returns the response.
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
    async fn single_select_round_trip() {
        let (tx, mut rx) = mpsc::channel::<PickerRequest>(1);
        let tool = AskUserTool::new(Some(tx));
        let call = tokio::spawn(async move {
            tool.call(json!({"questions": [{
                "question": "Which library?",
                "header": "Library",
                "options": [
                    {"label": "serde_json", "description": "stable, std"},
                    {"label": "simd-json",  "description": "fast"}
                ]
            }]}))
            .await
        });
        let state = InlinePickerState::new(rx.recv().await.unwrap());
        drive(state, &[key(KeyCode::Down), key(KeyCode::Enter)]);
        let result = call.await.unwrap();
        assert!(!result.is_error, "{}", result.content);
        let v: Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(v["answers"][0]["answer"], "simd-json");
    }

    #[tokio::test]
    async fn multi_select_round_trip() {
        let (tx, mut rx) = mpsc::channel::<PickerRequest>(1);
        let tool = AskUserTool::new(Some(tx));
        let call = tokio::spawn(async move {
            tool.call(json!({"questions": [{
                "question": "Which features?",
                "header": "Features",
                "multiSelect": true,
                "options": [
                    {"label": "auth",    "description": "login"},
                    {"label": "logging", "description": "structured logs"},
                    {"label": "metrics", "description": "prometheus"}
                ]
            }]}))
            .await
        });
        let state = InlinePickerState::new(rx.recv().await.unwrap());
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
        let v: Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(v["answers"][0]["answer"], json!(["auth", "metrics"]));
    }

    #[tokio::test]
    async fn other_typed_text_round_trip() {
        let (tx, mut rx) = mpsc::channel::<PickerRequest>(1);
        let tool = AskUserTool::new(Some(tx));
        let call = tokio::spawn(async move {
            tool.call(json!({"questions": [{
                "question": "Name?",
                "header": "Name",
                "options": [
                    {"label": "default", "description": "use default"},
                    {"label": "skip",    "description": "skip naming"}
                ]
            }]}))
            .await
        });
        let state = InlinePickerState::new(rx.recv().await.unwrap());
        let mut keys = vec![key(KeyCode::Down), key(KeyCode::Down)];
        for c in "my custom thing".chars() {
            keys.push(key(KeyCode::Char(c)));
        }
        keys.push(key(KeyCode::Enter));
        drive(state, &keys);
        let result = call.await.unwrap();
        let v: Value = serde_json::from_str(&result.content).unwrap();
        // Spaces must survive — regression guard for the multi-select bug.
        assert_eq!(v["answers"][0]["answer"], "my custom thing");
    }

    #[tokio::test]
    async fn escape_round_trip_is_error() {
        let (tx, mut rx) = mpsc::channel::<PickerRequest>(1);
        let tool = AskUserTool::new(Some(tx));
        let call = tokio::spawn(async move {
            tool.call(json!({"questions": [{
                "question": "Confirm?",
                "header": "Confirm",
                "options": [
                    {"label": "yes", "description": "do it"},
                    {"label": "no",  "description": "abort"}
                ]
            }]}))
            .await
        });
        let state = InlinePickerState::new(rx.recv().await.unwrap());
        drive(state, &[key(KeyCode::Esc)]);
        let result = call.await.unwrap();
        assert!(result.is_error);
        assert!(result.content.to_lowercase().contains("cancel"));
    }

    #[tokio::test]
    async fn two_question_flow_round_trip() {
        let (tx, mut rx) = mpsc::channel::<PickerRequest>(1);
        let tool = AskUserTool::new(Some(tx));
        let call = tokio::spawn(async move {
            tool.call(json!({"questions": [
                {"question": "Lib?",  "header": "Lib", "options": [
                    {"label": "serde_json", "description": "x"},
                    {"label": "simd-json",  "description": "y"}
                ]},
                {"question": "Mode?", "header": "Mode", "options": [
                    {"label": "strict", "description": "x"},
                    {"label": "lax",    "description": "y"}
                ]}
            ]}))
            .await
        });
        let state = InlinePickerState::new(rx.recv().await.unwrap());
        // Q1 Enter → serde_json; Q2 Down+Enter → lax → review; final Enter
        // submits the batch from the review-and-submit screen (multi-question
        // batches stop at review before returning).
        drive(
            state,
            &[
                key(KeyCode::Enter),
                key(KeyCode::Down),
                key(KeyCode::Enter),
                key(KeyCode::Enter),
            ],
        );
        let result = call.await.unwrap();
        let v: Value = serde_json::from_str(&result.content).unwrap();
        let answers = v["answers"].as_array().unwrap();
        assert_eq!(answers[0]["answer"], "serde_json");
        assert_eq!(answers[1]["answer"], "lax");
    }

    #[tokio::test]
    async fn execution_mode_is_sequential() {
        // Guard the P2 fix: parallel ask_user calls would race into the
        // console's "already-open" guard and surface phantom cancellations.
        let tool = AskUserTool::new(None);
        assert!(matches!(tool.execution_mode(), ExecutionMode::Sequential));
    }
}
