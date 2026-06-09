use crate::skills::SkillRegistry;
use crate::tools::tool::{StaticTool, ToolArgs, ToolOutcome, ToolParam};
use async_trait::async_trait;
use std::sync::Arc;

/// Loads a skill's full instructions by name. Registered top-level only.
pub struct SkillTool {
    registry: Arc<SkillRegistry>,
}

impl SkillTool {
    /// Inherent mirror of the `StaticTool::NAME` const so callers (e.g. the
    /// tool-block renderer) can match on `SkillTool::NAME` without importing
    /// the trait — same pattern as `EditFileTool`/`CreateFileTool`.
    pub const NAME: &'static str = "skill";

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
impl StaticTool for SkillTool {
    const NAME: &'static str = "skill";
    const DESCRIPTION: &'static str =
        "Load a specialized skill by name when the task matches one listed in \
         <available_skills>. Returns the skill's full instructions, plus a list \
         of any supporting files it bundles.";
    const PARAMETERS: &'static [ToolParam] = &[ToolParam {
        name: "name",
        ty: "string",
        description: "Skill name from <available_skills>",
    }];
    const REQUIRED: &'static [&'static str] = &["name"];

    async fn run(&self, args: serde_json::Value) -> ToolOutcome {
        let name = args.require_str("name")?;
        match self.registry.get_enabled(name) {
            // The body is the skill's instructions — emitted verbatim, not
            // XML-escaped (unlike the catalog's metadata), so code samples and
            // markdown in it survive intact. `name` is validated to a safe
            // charset, so the wrapper tag is well-formed. The directory + file
            // list is appended only when the skill actually bundles files (see
            // `resources_note`), so pure-instruction skills stay clean.
            Some(skill) => Ok(format!(
                "<skill name=\"{}\">\n{}{}\n</skill>",
                skill.name,
                skill.body,
                skill.resources_note().unwrap_or_default()
            )),
            None => Err(format!(
                "Skill '{name}' not found or disabled. Available: [{}]",
                self.available()
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::tool::AgentTool;
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
    async fn loads_bundled_files_list() {
        // Keep the skill dir alive through the call (resources_note reads it live).
        let tmp = crate::util::unique_temp_dir("ignis-skilltool-bundled");
        let dir = tmp.join("proj/.ignis/skills/bundled");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nname: bundled\n---\nuse helper.sh",
        )
        .unwrap();
        std::fs::write(dir.join("helper.sh"), "echo hi").unwrap();
        let reg = Arc::new(SkillRegistry::load(None, &tmp.join("proj"), HashSet::new()));

        let tool = SkillTool::new(reg);
        let r = tool.call(serde_json::json!({ "name": "bundled" })).await;
        assert!(!r.is_error);
        assert!(r.content.contains("use helper.sh"));
        assert!(r.content.contains("helper.sh")); // bundled file advertised
        std::fs::remove_dir_all(&tmp).ok();
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
