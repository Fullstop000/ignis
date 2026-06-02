use std::path::{Path, PathBuf};

use crate::Message;

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
        // Lift the record envelope's timestamp onto the Message so saved
        // sessions round-trip per-message capture time. `created_at_ms` is
        // `#[serde(skip)]`, so it never appears in the payload — we have to
        // populate it from the envelope explicitly.
        if let Ok(mut message) = serde_json::from_value::<Message>(payload.clone()) {
            message.created_at_ms = record.get("timestamp").and_then(|v| v.as_u64());
            messages.push(message);
        }
    }
    messages
}

/// Serializes tests that mutate the process-global `$HOME` (state.json lives
/// under it). `$HOME` is shared across the whole process, so such tests must
/// not run in parallel or they clobber each other's state file.
#[cfg(test)]
pub static ENV_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Generate a unique temporary directory path for tests.
///
/// Combines wall-clock nanos with a process-wide monotonic counter so
/// two tests calling this in the same nanosecond bucket (very possible
/// under cargo test's thread-pool) still get distinct paths.
pub fn unique_temp_dir(prefix: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock should be after unix epoch")
        .as_nanos();
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("{prefix}-{nanos}-{seq}"))
}
