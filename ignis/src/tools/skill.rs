use crate::skills::SkillRegistry;
use crate::tools::tool::{AgentTool, ToolResult};
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

/// Loads a skill's full instructions by name. Registered top-level only.
pub struct SkillTool {
    registry: Arc<SkillRegistry>,
}

impl SkillTool {
    pub fn new(registry: Arc<SkillRegistry>) -> Self {
        Self { registry }
    }

    fn available(&self) -> String {
        self.registry
            .enabled_entries()
            .into_iter()
            .map(|(n, _)| n)
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[async_trait]
impl AgentTool for SkillTool {
    fn name(&self) -> &str {
        "skill"
    }

    fn description(&self) -> &str {
        "Load a specialized skill by name when the task matches one listed in \
         <available_skills>. Returns the skill's full instructions."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Skill name from <available_skills>" }
            },
            "required": ["name"]
        })
    }

    async fn call(&self, args: serde_json::Value) -> ToolResult {
        let Some(name) = args["name"].as_str() else {
            return ToolResult::error("Missing required parameter: name".to_string());
        };
        match self.registry.get_enabled(name) {
            // The body is the skill's instructions — emitted verbatim, not
            // XML-escaped (unlike the catalog's metadata), so code samples and
            // markdown in it survive intact. `name` is validated to a safe
            // charset, so the wrapper tag is well-formed.
            Some(skill) => ToolResult::ok(format!(
                "<skill name=\"{}\">\n{}\n\n(Skill directory: {} — read bundled files with \
                 read_file / list_dir if referenced.)\n</skill>",
                skill.name,
                skill.body,
                skill.dir.display()
            )),
            None => ToolResult::error(format!(
                "Skill '{name}' not found or disabled. Available: [{}]",
                self.available()
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn registry_with_react(disabled: HashSet<String>) -> Arc<SkillRegistry> {
        let tmp = crate::util::unique_temp_dir("ignis-skilltool");
        let cwd = tmp.join("proj");
        let dir = cwd.join(".ignis/skills/react");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), "---\nname: react\n---\nReact body.").unwrap();
        let reg = SkillRegistry::load(None, &cwd, disabled);
        std::fs::remove_dir_all(&tmp).ok();
        Arc::new(reg)
    }

    #[tokio::test]
    async fn loads_known_enabled_skill() {
        let tool = SkillTool::new(registry_with_react(HashSet::new()));
        let r = tool.call(serde_json::json!({ "name": "react" })).await;
        assert!(!r.is_error);
        assert!(r.content.contains("React body."));
        assert!(r.content.contains("<skill name=\"react\">"));
    }

    #[tokio::test]
    async fn unknown_skill_is_error() {
        let tool = SkillTool::new(registry_with_react(HashSet::new()));
        let r = tool.call(serde_json::json!({ "name": "ghost" })).await;
        assert!(r.is_error);
    }

    #[tokio::test]
    async fn disabled_skill_is_error() {
        let mut disabled = HashSet::new();
        disabled.insert("react".to_string());
        let tool = SkillTool::new(registry_with_react(disabled));
        let r = tool.call(serde_json::json!({ "name": "react" })).await;
        assert!(r.is_error);
    }

    #[tokio::test]
    async fn missing_name_is_error() {
        let tool = SkillTool::new(registry_with_react(HashSet::new()));
        let r = tool.call(serde_json::json!({})).await;
        assert!(r.is_error);
    }
}
