use crate::Message;
use async_trait::async_trait;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

#[async_trait]
pub trait SessionStorage: Send + Sync + 'static {
    async fn load_session(&self, session_id: &str) -> Result<Vec<Message>, Box<dyn std::error::Error + Send + Sync>>;
    async fn save_session(&self, session_id: &str, messages: &[Message]) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
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

#[async_trait]
impl SessionStorage for InMemoryStorage {
    async fn load_session(&self, session_id: &str) -> Result<Vec<Message>, Box<dyn std::error::Error + Send + Sync>> {
        let lock = self.sessions.read().await;
        if let Some(history) = lock.get(session_id) {
            Ok(history.clone())
        } else {
            Ok(Vec::new())
        }
    }

    async fn save_session(&self, session_id: &str, messages: &[Message]) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
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

impl FileStorage {
    pub fn new(base_dir: PathBuf) -> Self {
        Self {
            base_dir,
            write_lock: Arc::new(RwLock::new(())),
        }
    }

    fn sanitize_session_id(&self, session_id: &str) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let sanitized: String = session_id
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        if sanitized.is_empty() {
            return Err("Invalid session_id: contains no alphanumeric characters".into());
        }
        Ok(sanitized)
    }

    fn get_session_path(&self, session_id: &str) -> Result<PathBuf, Box<dyn std::error::Error + Send + Sync>> {
        let clean_id = self.sanitize_session_id(session_id)?;
        Ok(self.base_dir.join(format!("{}.json", clean_id)))
    }
}

#[async_trait]
impl SessionStorage for FileStorage {
    async fn load_session(&self, session_id: &str) -> Result<Vec<Message>, Box<dyn std::error::Error + Send + Sync>> {
        let path = self.get_session_path(session_id)?;
        if !path.exists() {
            return Ok(Vec::new());
        }

        let _lock = self.write_lock.read().await;
        let file_content = tokio::fs::read_to_string(&path).await?;
        let messages: Vec<Message> = serde_json::from_str(&file_content)?;
        Ok(messages)
    }

    async fn save_session(&self, session_id: &str, messages: &[Message]) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let path = self.get_session_path(session_id)?;
        let parent = path.parent().ok_or_else(|| "No parent directory for session file")?;
        
        tokio::fs::create_dir_all(parent).await?;

        let _lock = self.write_lock.write().await;

        // Atomic write procedure
        let temp_filename = format!("{}.json.tmp", uuid::Uuid::new_v4());
        let temp_path = parent.join(temp_filename);

        // Scope to ensure file is closed and synced before rename
        {
            use std::io::Write;
            let serialized = serde_json::to_vec(messages)?;
            
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
