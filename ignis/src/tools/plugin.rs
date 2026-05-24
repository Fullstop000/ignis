use crate::{AgentTool, ExecutionMode, ToolResult};
use async_trait::async_trait;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Deserialize)]
struct PluginManifest {
    name: String,
    description: String,
    parameters: serde_json::Value,
    command: String,
    #[serde(default = "default_execution_mode")]
    execution_mode: String,
}

fn default_execution_mode() -> String {
    "parallel".to_string()
}

pub struct PluginTool {
    name: String,
    description: String,
    parameters: serde_json::Value,
    command: String,
    working_dir: PathBuf,
    execution_mode: ExecutionMode,
}

#[async_trait]
impl AgentTool for PluginTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> serde_json::Value {
        self.parameters.clone()
    }

    fn execution_mode(&self) -> ExecutionMode {
        self.execution_mode
    }

    async fn call(&self, args: serde_json::Value) -> ToolResult {
        use tokio::io::AsyncWriteExt;

        let mut child = match tokio::process::Command::new("bash")
            .args(["-c", &self.command])
            .current_dir(&self.working_dir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
        {
            Ok(child) => child,
            Err(e) => return ToolResult::error(format!("Failed to spawn command: {e}")),
        };

        let stdin_bytes = serde_json::to_vec(&args).unwrap_or_default();
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(&stdin_bytes).await;
        }

        let result =
            tokio::time::timeout(std::time::Duration::from_secs(60), child.wait_with_output())
                .await;

        match result {
            Err(_) => ToolResult::error("Command timed out after 60 seconds".to_string()),
            Ok(Err(e)) => ToolResult::error(format!("Command failed: {e}")),
            Ok(Ok(output)) => {
                if output.status.success() {
                    ToolResult::ok(String::from_utf8_lossy(&output.stdout).into_owned())
                } else {
                    ToolResult::error(String::from_utf8_lossy(&output.stderr).into_owned())
                }
            }
        }
    }
}

pub fn load_extensions(dirs: &[PathBuf]) -> Vec<PluginTool> {
    let mut plugins_by_name: HashMap<String, PluginTool> = HashMap::new();

    for dir in dirs {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
                continue;
            }

            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(e) => {
                    log::warn!("Failed to read plugin manifest {}: {e}", path.display());
                    continue;
                }
            };

            let manifest: PluginManifest = match serde_yaml::from_str(&content) {
                Ok(m) => m,
                Err(e) => {
                    log::warn!("Invalid plugin manifest {}: {e}", path.display());
                    continue;
                }
            };

            let execution_mode = match manifest.execution_mode.as_str() {
                "sequential" => ExecutionMode::Sequential,
                _ => ExecutionMode::Parallel,
            };

            let working_dir = dir.to_path_buf();



            log::info!("Loaded plugin: {} from {}", manifest.name, path.display());

            plugins_by_name.insert(
                manifest.name.clone(),
                PluginTool {
                    name: manifest.name,
                    description: manifest.description,
                    parameters: manifest.parameters,
                    command: manifest.command,
                    working_dir,
                    execution_mode,
                },
            );
        }
    }

    plugins_by_name.into_values().collect()
}

pub fn default_extension_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();

    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join(".ignis").join("extensions"));
    }

    dirs.push(PathBuf::from(".ignis/extensions"));

    dirs
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_plugin_manifest_deserialization() {
        let manifest_yaml = r#"
name: "test_tool"
description: "A tool for testing"
parameters:
  type: object
  properties:
    input:
      type: string
command: "echo test"
execution_mode: "sequential"
"#;
        let manifest: PluginManifest = serde_yaml::from_str(manifest_yaml).unwrap();
        assert_eq!(manifest.name, "test_tool");
        assert_eq!(manifest.description, "A tool for testing");
        assert_eq!(manifest.command, "echo test");
        assert_eq!(manifest.execution_mode, "sequential");
    }

    #[tokio::test]
    async fn test_plugin_tool_call() {
        let temp_dir = std::env::temp_dir();
        let plugin = PluginTool {
            name: "echo_tool".to_string(),
            description: "echoes input".to_string(),
            parameters: json!({}),
            command: "echo 'hello plugin'".to_string(),
            working_dir: temp_dir,
            execution_mode: ExecutionMode::Parallel,
        };

        let res = plugin.call(json!({})).await;
        assert!(!res.is_error);
        assert_eq!(res.content.trim(), "hello plugin");
    }
}
