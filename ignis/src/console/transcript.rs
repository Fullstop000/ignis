//! Shared reduction of a recorded message stream into display blocks.
//!
//! Both the native renderer (`App::render_session_history`) and the engine
//! transcript replay (`runner::transcript_blocks`) walk a `Vec<Message>` the
//! same way: user text, reasoning-before-reply, a tool call whose result is
//! filled by the most recent matching `tool` message, and an orphan result that
//! becomes a standalone block. This is that one walk; each caller maps the
//! canonical [`TranscriptItem`]s onto its own block type (`UIBlock` /
//! `TranscriptBlock`), so the fiddly reduction lives in exactly one place rather
//! than two copies that can drift.

/// One reduced transcript entry, before mapping to a renderer-specific block.
pub(crate) enum TranscriptItem {
    User(String),
    Reasoning(String),
    Assistant(String),
    /// A tool call and its result once matched. `result` is `None` while the
    /// call is an unfilled placeholder (the matching `tool` message hasn't been
    /// seen — e.g. an interrupted call) and `Some((text, is_error))` once
    /// filled. `id` is empty for an orphan result that never matched a call.
    Tool {
        id: String,
        name: String,
        args: String,
        result: Option<(String, bool)>,
    },
}

/// Reduce a recorded message stream into canonical transcript items, in order.
/// A `tool` message attaches its parsed result to the most recent call sharing
/// its `tool_call_id`; one with no match becomes a standalone (orphan) item.
pub(crate) fn reduce_transcript(messages: Vec<crate::Message>) -> Vec<TranscriptItem> {
    let mut items: Vec<TranscriptItem> = Vec::new();
    // tool_call_id -> index in `items` of its call, for back-filling results.
    let mut tool_idx: Vec<(String, usize)> = Vec::new();
    for message in messages {
        match message.role.as_str() {
            "user" => {
                if let Some(content) = message.content.filter(|c| !c.is_empty()) {
                    items.push(TranscriptItem::User(content));
                }
            }
            "assistant" => {
                // Reasoning before the reply, matching the streaming order;
                // either may be missing or empty.
                if let Some(reasoning) = message.reasoning_content.filter(|r| !r.is_empty()) {
                    items.push(TranscriptItem::Reasoning(reasoning));
                }
                if let Some(content) = message.content.filter(|c| !c.is_empty()) {
                    items.push(TranscriptItem::Assistant(content));
                }
                if let Some(tool_calls) = message.tool_calls {
                    for tc in tool_calls {
                        tool_idx.push((tc.id.clone(), items.len()));
                        items.push(TranscriptItem::Tool {
                            id: tc.id,
                            name: tc.function.name,
                            args: tc.function.arguments,
                            result: None,
                        });
                    }
                }
            }
            "tool" => {
                // Persisted tool content is {"result": <str>, "is_error": <bool>}.
                let (content, is_error) = crate::console::app::parse_tool_result(
                    message.content.as_deref().unwrap_or(""),
                );
                let idx = message.tool_call_id.as_deref().and_then(|id| {
                    tool_idx
                        .iter()
                        .rev()
                        .find(|(tid, _)| tid == id)
                        .map(|(_, i)| *i)
                });
                match idx {
                    Some(i) => {
                        if let TranscriptItem::Tool { result, .. } = &mut items[i] {
                            *result = Some((content, is_error));
                        }
                    }
                    None => items.push(TranscriptItem::Tool {
                        id: String::new(),
                        name: message.name.unwrap_or_else(|| "tool".to_string()),
                        args: String::new(),
                        result: Some((content, is_error)),
                    }),
                }
            }
            _ => {}
        }
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Message, ToolCall, ToolCallFunction};

    fn msg(role: &str, content: Option<&str>) -> Message {
        Message {
            role: role.to_string(),
            content: content.map(str::to_string),
            reasoning_content: None,
            name: None,
            tool_call_id: None,
            tool_calls: None,
            created_at_ms: None,
        }
    }

    #[test]
    fn reduces_reasoning_reply_and_attaches_then_orphans_tool_results() {
        let mut assistant = msg("assistant", Some("on it"));
        assistant.reasoning_content = Some("thinking".to_string());
        assistant.tool_calls = Some(vec![ToolCall {
            id: "call-1".to_string(),
            r#type: "function".to_string(),
            function: ToolCallFunction {
                name: "bash".to_string(),
                arguments: "ls".to_string(),
            },
        }]);
        let mut result = msg("tool", Some(r#"{"result":"file.txt","is_error":false}"#));
        result.tool_call_id = Some("call-1".to_string());
        // An orphan result whose id matches no call → standalone item.
        let mut orphan = msg("tool", Some(r#"{"result":"boom","is_error":true}"#));
        orphan.tool_call_id = Some("missing".to_string());
        orphan.name = Some("grep".to_string());

        let items = reduce_transcript(vec![msg("user", Some("do it")), assistant, result, orphan]);

        assert!(matches!(&items[0], TranscriptItem::User(t) if t == "do it"));
        assert!(matches!(&items[1], TranscriptItem::Reasoning(t) if t == "thinking"));
        assert!(matches!(&items[2], TranscriptItem::Assistant(t) if t == "on it"));
        match &items[3] {
            TranscriptItem::Tool {
                id,
                name,
                args,
                result,
            } => {
                assert_eq!(id, "call-1");
                assert_eq!(name, "bash");
                assert_eq!(args, "ls");
                assert_eq!(result.as_ref().unwrap(), &("file.txt".to_string(), false));
            }
            _ => panic!("expected the matched tool call"),
        }
        match &items[4] {
            TranscriptItem::Tool {
                id, name, result, ..
            } => {
                assert!(id.is_empty(), "orphan carries no call id");
                assert_eq!(name, "grep", "orphan uses the tool message name");
                assert_eq!(result.as_ref().unwrap(), &("boom".to_string(), true));
            }
            _ => panic!("expected the orphan result as a standalone item"),
        }
        assert_eq!(items.len(), 5);
    }

    #[test]
    fn empty_and_whitespace_user_content_is_dropped_but_unmatched_call_stays_placeholder() {
        // An assistant turn with a tool call but no following result keeps the
        // call as an unfilled placeholder (result: None).
        let mut assistant = msg("assistant", None);
        assistant.tool_calls = Some(vec![ToolCall {
            id: "c".to_string(),
            r#type: "function".to_string(),
            function: ToolCallFunction {
                name: "read".to_string(),
                arguments: "f".to_string(),
            },
        }]);
        let items = reduce_transcript(vec![msg("user", Some("")), assistant]);
        // Empty user content produces no item; only the placeholder tool call.
        assert_eq!(items.len(), 1);
        assert!(matches!(
            &items[0],
            TranscriptItem::Tool { result: None, .. }
        ));
    }
}
