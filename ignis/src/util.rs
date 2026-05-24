use std::path::{Path, PathBuf};

use crate::types::Message;

/// Resolve a potentially relative path against a cwd.
pub fn resolve_path(cwd: &Path, path: &str) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    }
}

/// Sanitize a session ID to a filesystem-safe string.
pub fn sanitize_session_id(session_id: &str) -> String {
    session_id
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
        .collect()
}

/// Parse JSONL session content into Messages.
pub fn parse_jsonl_messages(content: &str) -> Vec<Message> {
    let mut messages = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(record): Result<serde_json::Value, _> = serde_json::from_str(line) else {
            continue;
        };
        let record_type = record.get("type").and_then(|t| t.as_str());
        if record_type != Some("message") && record_type != Some("tool_result") {
            continue;
        }
        let Some(payload) = record.get("payload") else {
            continue;
        };
        if let Ok(message) = serde_json::from_value::<Message>(payload.clone()) {
            messages.push(message);
        }
    }
    messages
}

/// Generate a unique temporary directory path for tests.
pub fn unique_temp_dir(prefix: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock should be after unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("{prefix}-{nanos}"))
}
