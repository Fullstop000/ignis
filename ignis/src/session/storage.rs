use crate::{Message, Usage};
use async_trait::async_trait;
use serde::Serialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::sync::RwLock;

#[async_trait]
pub trait SessionStorage: Send + Sync + 'static {
    async fn load_session(&self, session_id: &str) -> Result<Vec<Message>, anyhow::Error>;
    async fn save_session(
        &self,
        session_id: &str,
        messages: &[Message],
        start_dir: Option<&str>,
    ) -> Result<(), anyhow::Error>;

    /// Persist the session's cumulative token usage. Default no-op so backends
    /// that don't track it (e.g. in-memory tests) need no changes.
    async fn save_usage(&self, _session_id: &str, _usage: &Usage) -> Result<(), anyhow::Error> {
        Ok(())
    }

    /// Load a session's cumulative token usage (default: none).
    async fn load_usage(&self, _session_id: &str) -> Result<Usage, anyhow::Error> {
        Ok(Usage::default())
    }
}

#[derive(Clone)]
pub struct InMemoryStorage {
    sessions: Arc<RwLock<HashMap<String, Vec<Message>>>>,
}

impl InMemoryStorage {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

impl Default for InMemoryStorage {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SessionStorage for InMemoryStorage {
    async fn load_session(&self, session_id: &str) -> Result<Vec<Message>, anyhow::Error> {
        let lock = self.sessions.read().await;
        if let Some(history) = lock.get(session_id) {
            Ok(history.clone())
        } else {
            Ok(Vec::new())
        }
    }

    async fn save_session(
        &self,
        session_id: &str,
        messages: &[Message],
        _start_dir: Option<&str>,
    ) -> Result<(), anyhow::Error> {
        let mut lock = self.sessions.write().await;
        lock.insert(session_id.to_string(), messages.to_vec());
        Ok(())
    }
}

pub struct FileStorage {
    base_dir: PathBuf,
    // lock to prevent race conditions during write from the same process
    write_lock: Arc<RwLock<()>>,
}

#[derive(Serialize)]
struct SessionRecord<'a> {
    #[serde(rename = "type")]
    record_type: &'a str,
    timestamp: u128,
    payload: serde_json::Value,
}

impl FileStorage {
    pub fn new(base_dir: PathBuf) -> Self {
        Self {
            base_dir,
            write_lock: Arc::new(RwLock::new(())),
        }
    }

    fn sanitize_session_id(&self, session_id: &str) -> Result<String, anyhow::Error> {
        let sanitized = crate::util::sanitize_session_id(session_id);
        if sanitized.is_empty() {
            return Err(anyhow::anyhow!(
                "Invalid session_id: contains no alphanumeric characters"
            ));
        }
        Ok(sanitized)
    }

    fn get_session_path(&self, session_id: &str) -> Result<PathBuf, anyhow::Error> {
        let clean_id = self.sanitize_session_id(session_id)?;
        Ok(self.base_dir.join(format!("{}.jsonl", clean_id)))
    }

    fn get_legacy_session_path(&self, session_id: &str) -> Result<PathBuf, anyhow::Error> {
        let clean_id = self.sanitize_session_id(session_id)?;
        Ok(self.base_dir.join(format!("{}.json", clean_id)))
    }

    fn timestamp_millis() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    }

    fn message_record_type(message: &Message) -> &'static str {
        if message.role == "tool" {
            "tool_result"
        } else {
            "message"
        }
    }

    fn serialize_jsonl_session(
        session_id: &str,
        messages: &[Message],
        start_dir: Option<&str>,
    ) -> Result<Vec<u8>, anyhow::Error> {
        let mut out = String::new();
        let timestamp = Self::timestamp_millis();
        let mut meta_payload = serde_json::json!({ "id": session_id });
        if let Some(dir) = start_dir {
            meta_payload["start_dir"] = serde_json::json!(dir);
        }
        let meta = SessionRecord {
            record_type: "session_meta",
            timestamp,
            payload: meta_payload,
        };
        out.push_str(&serde_json::to_string(&meta)?);
        out.push('\n');

        // Per-message timestamps: prefer the wall-clock capture time stamped at
        // the agent loop / session push site; fall back to the save-time for
        // unstamped messages (older sessions before this change, or test
        // fixtures). Without this, every record gets the same `timestamp`,
        // collapsing the `/sessions` waterfall to zero-duration ticks.
        for message in messages {
            let record = SessionRecord {
                record_type: Self::message_record_type(message),
                timestamp: message
                    .created_at_ms
                    .map(|ms| ms as u128)
                    .unwrap_or(timestamp),
                payload: serde_json::to_value(message)?,
            };
            out.push_str(&serde_json::to_string(&record)?);
            out.push('\n');
        }

        Ok(out.into_bytes())
    }
}

#[async_trait]
impl SessionStorage for FileStorage {
    async fn load_session(&self, session_id: &str) -> Result<Vec<Message>, anyhow::Error> {
        let path = self.get_session_path(session_id)?;
        if !path.exists() {
            let legacy_path = self.get_legacy_session_path(session_id)?;
            if legacy_path.exists() {
                let _lock = self.write_lock.read().await;
                let file_content = tokio::fs::read_to_string(&legacy_path).await?;
                let messages: Vec<Message> = serde_json::from_str(&file_content)?;
                return Ok(messages);
            }
            return Ok(Vec::new());
        }

        let _lock = self.write_lock.read().await;
        let file_content = tokio::fs::read_to_string(&path).await?;
        let messages = crate::util::parse_jsonl_messages(&file_content);
        Ok(messages)
    }

    async fn save_session(
        &self,
        session_id: &str,
        messages: &[Message],
        start_dir: Option<&str>,
    ) -> Result<(), anyhow::Error> {
        let path = self.get_session_path(session_id)?;
        let parent = path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("No parent directory for session file"))?;

        tokio::fs::create_dir_all(parent).await?;

        let _lock = self.write_lock.write().await;

        // Atomic write procedure: write to a temp file, fsync, then rename.
        let temp_filename = format!("{}.json.tmp", uuid::Uuid::new_v4());
        let temp_path = parent.join(temp_filename);
        let serialized = Self::serialize_jsonl_session(session_id, messages, start_dir)?;
        {
            let mut file = tokio::fs::File::create(&temp_path).await?;
            file.write_all(&serialized).await?;
            file.flush().await?;
            file.sync_all().await?;
        }

        // Atomic rename
        tokio::fs::rename(&temp_path, &path).await?;

        // Directory sync (Linux/Unix durability)
        #[cfg(unix)]
        {
            if let Ok(dir) = tokio::fs::File::open(parent).await {
                let _ = dir.sync_all().await;
            }
        }

        Ok(())
    }

    async fn save_usage(&self, session_id: &str, usage: &Usage) -> Result<(), anyhow::Error> {
        let clean_id = self.sanitize_session_id(session_id)?;
        let path = self.base_dir.join(format!("{}.usage.json", clean_id));
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let _lock = self.write_lock.write().await;
        tokio::fs::write(&path, serde_json::to_string(usage)?).await?;
        Ok(())
    }

    async fn load_usage(&self, session_id: &str) -> Result<Usage, anyhow::Error> {
        let clean_id = self.sanitize_session_id(session_id)?;
        let path = self.base_dir.join(format!("{}.usage.json", clean_id));
        match tokio::fs::read_to_string(&path).await {
            Ok(s) => Ok(serde_json::from_str(&s).unwrap_or_default()),
            Err(_) => Ok(Usage::default()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    /// Per-message timestamps from `Message::stamp_now()` must survive the
    /// JSONL round-trip — otherwise the `/sessions` waterfall collapses to
    /// zero-duration ticks on real persisted sessions. This was caught in
    /// PR #84 review after dogfood missed it (the fixture used a single
    /// hand-crafted timestamp per event, hiding the bug).
    #[tokio::test]
    async fn per_message_timestamps_round_trip_through_save_and_load() {
        let tmp = crate::util::unique_temp_dir("ignis-storage-ts");
        std::fs::create_dir_all(&tmp).unwrap();
        let storage = FileStorage::new(tmp.clone());

        let m1 = Message {
            role: "user".to_string(),
            content: Some("hi".to_string()),
            reasoning_content: None,
            name: None,
            tool_call_id: None,
            tool_calls: None,
            created_at_ms: Some(1_000_000),
        };
        let m2 = Message {
            role: "assistant".to_string(),
            content: Some("hello".to_string()),
            reasoning_content: None,
            name: None,
            tool_call_id: None,
            tool_calls: None,
            created_at_ms: Some(1_002_500),
        };
        let m3 = Message {
            role: "tool".to_string(),
            content: Some("{\"is_error\":false}".to_string()),
            reasoning_content: None,
            name: Some("bash".to_string()),
            tool_call_id: Some("c1".to_string()),
            tool_calls: None,
            created_at_ms: Some(1_003_700),
        };

        storage
            .save_session("sess-ts", &[m1, m2, m3], Some("/proj"))
            .await
            .unwrap();

        // Round trip via load_session — Message-level created_at_ms preserved.
        let loaded = storage.load_session("sess-ts").await.unwrap();
        assert_eq!(loaded.len(), 3);
        assert_eq!(loaded[0].created_at_ms, Some(1_000_000));
        assert_eq!(loaded[1].created_at_ms, Some(1_002_500));
        assert_eq!(loaded[2].created_at_ms, Some(1_003_700));

        // Verify on-disk JSONL has per-record envelope timestamps (not all
        // equal to save-time). This is the bug the codex review caught.
        let raw = std::fs::read_to_string(tmp.join("sess-ts.jsonl")).unwrap();
        let records: Vec<u64> = raw
            .lines()
            .skip(1) // skip session_meta
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter_map(|v| v.get("timestamp").and_then(|t| t.as_u64()))
            .collect();
        assert_eq!(records, vec![1_000_000, 1_002_500, 1_003_700]);

        // Verify the `tool_result` record type lands so extract_turns can
        // join it back to its call.
        let kinds: Vec<String> = raw
            .lines()
            .skip(1)
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter_map(|v| v.get("type").and_then(|t| t.as_str()).map(str::to_string))
            .collect();
        assert_eq!(
            kinds,
            vec![
                "message".to_string(),
                "message".to_string(),
                "tool_result".to_string()
            ]
        );

        std::fs::remove_dir_all(&tmp).ok();
    }
}
