use crate::Message;
use async_trait::async_trait;
use serde::Serialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

#[async_trait]
pub trait SessionStorage: Send + Sync + 'static {
    async fn load_session(
        &self,
        session_id: &str,
    ) -> Result<Vec<Message>, anyhow::Error>;
    async fn save_session(
        &self,
        session_id: &str,
        messages: &[Message],
        start_dir: Option<&str>,
    ) -> Result<(), anyhow::Error>;
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
    async fn load_session(
        &self,
        session_id: &str,
    ) -> Result<Vec<Message>, anyhow::Error> {
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

    fn sanitize_session_id(
        &self,
        session_id: &str,
    ) -> Result<String, anyhow::Error> {
        let sanitized = crate::util::sanitize_session_id(session_id);
        if sanitized.is_empty() {
            return Err(anyhow::anyhow!("Invalid session_id: contains no alphanumeric characters"));
        }
        Ok(sanitized)
    }

    fn get_session_path(
        &self,
        session_id: &str,
    ) -> Result<PathBuf, anyhow::Error> {
        let clean_id = self.sanitize_session_id(session_id)?;
        Ok(self.base_dir.join(format!("{}.jsonl", clean_id)))
    }

    fn get_legacy_session_path(
        &self,
        session_id: &str,
    ) -> Result<PathBuf, anyhow::Error> {
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

        for message in messages {
            let record = SessionRecord {
                record_type: Self::message_record_type(message),
                timestamp,
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
    async fn load_session(
        &self,
        session_id: &str,
    ) -> Result<Vec<Message>, anyhow::Error> {
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

        // Atomic write procedure
        let temp_filename = format!("{}.json.tmp", uuid::Uuid::new_v4());
        let temp_path = parent.join(temp_filename);

        // Scope to ensure file is closed and synced before rename
        {
            use std::io::Write;
            let serialized = Self::serialize_jsonl_session(session_id, messages, start_dir)?;

            // Standard synchronous file operations inside tokio::task::block_in_place or spawn_blocking
            // to ensure OS flush and sync_all are completed safely.
            let temp_path_clone = temp_path.clone();
            tokio::task::spawn_blocking(move || -> std::io::Result<()> {
                let mut file = std::fs::File::create(&temp_path_clone)?;
                file.write_all(&serialized)?;
                file.flush()?;
                file.sync_all()?;
                Ok(())
            })
            .await??;
        }

        // Atomic rename
        std::fs::rename(&temp_path, &path)?;

        // Directory sync (Linux/Unix durability)
        #[cfg(unix)]
        {
            let parent_clone = parent.to_path_buf();
            tokio::task::spawn_blocking(move || {
                if let Ok(dir) = std::fs::File::open(parent_clone) {
                    let _ = dir.sync_all();
                }
            })
            .await?;
        }

        Ok(())
    }
}
