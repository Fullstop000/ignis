//! User-defined skills: `SKILL.md` instruction sets discovered from disk,
//! advertised to the model, loadable on demand, and toggleable at runtime.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Largest skill body we will load (≈12k tokens). Oversized skills are skipped
/// so loading or inlining one can never blow the context window.
pub const MAX_SKILL_BODY_CHARS: usize = 50_000;

/// Slash-command names a skill may not shadow.
const BASE_SLASH_NAMES: &[&str] = &["resume", "clear", "compact", "copy", "model", "skills"];

/// Which root a skill was discovered under (for the `/skills` source tag).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillScope {
    Global,
    Project,
}

#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: Option<String>,
    pub dir: PathBuf,
    pub body: String,
    pub scope: SkillScope,
}

/// A valid skill name: starts alphanumeric, then alphanumeric / `_` / `-`.
/// It becomes a slash command, a tool argument, and a persisted id.
pub fn valid_skill_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    name.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Parse a `SKILL.md` body. Returns `(name, description, body)` or an `Err`
/// reason explaining why the file is skipped.
pub(crate) fn parse_skill_md(content: &str) -> Result<(String, Option<String>, String), String> {
    // Strip a leading UTF-8 BOM (some Windows editors emit one) so the opening
    // `---` fence still matches.
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    let mut lines = content.lines();
    if lines.next().map(str::trim) != Some("---") {
        return Err("missing opening frontmatter fence".to_string());
    }
    let mut name: Option<String> = None;
    let mut description: Option<String> = None;
    let mut closed = false;
    for line in lines.by_ref() {
        if line.trim() == "---" {
            closed = true;
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            let val = v.trim().trim_matches('"').trim_matches('\'').to_string();
            match k.trim() {
                "name" => name = Some(val),
                "description" if !val.is_empty() => description = Some(val),
                _ => {}
            }
        }
    }
    if !closed {
        return Err("missing closing frontmatter fence".to_string());
    }
    let body = lines.collect::<Vec<_>>().join("\n");
    let body = body.trim();

    let name = name.ok_or("missing required `name`")?;
    if !valid_skill_name(&name) {
        return Err(format!("invalid skill name `{name}`"));
    }
    if BASE_SLASH_NAMES.contains(&name.as_str()) {
        return Err(format!("name `{name}` collides with a built-in command"));
    }
    if body.is_empty() {
        return Err("empty skill body".to_string());
    }
    if body.len() > MAX_SKILL_BODY_CHARS {
        return Err(format!("skill body exceeds {MAX_SKILL_BODY_CHARS} chars"));
    }
    Ok((name, description, body.to_string()))
}

/// In-memory skill registry: immutable skill data plus a live disabled set.
/// Shared as `Arc<SkillRegistry>` by the agent, the `skill` tool, the slash
/// layer, and the `/skills` picker.
pub struct SkillRegistry {
    skills: Vec<Skill>, // unique by name, sorted by name
    disabled: Mutex<HashSet<String>>,
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

impl SkillRegistry {
    /// Discover skills under the four roots, later overriding earlier on a name
    /// collision (project beats global; `.ignis` beats `.agents`).
    pub fn load(home: Option<&Path>, cwd: &Path, disabled: HashSet<String>) -> Self {
        let mut roots: Vec<(PathBuf, SkillScope)> = Vec::new();
        if let Some(h) = home {
            roots.push((h.join(".agents/skills"), SkillScope::Global));
            roots.push((h.join(".ignis/skills"), SkillScope::Global));
        }
        roots.push((cwd.join(".agents/skills"), SkillScope::Project));
        roots.push((cwd.join(".ignis/skills"), SkillScope::Project));

        let mut map: BTreeMap<String, Skill> = BTreeMap::new();
        for (root, scope) in roots {
            let entries = match std::fs::read_dir(&root) {
                Ok(e) => e,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let dir = entry.path();
                if !dir.is_dir() {
                    continue;
                }
                let md = dir.join("SKILL.md");
                let content = match std::fs::read_to_string(&md) {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                match parse_skill_md(&content) {
                    Ok((name, description, body)) => {
                        let skill = Skill {
                            name: name.clone(),
                            description,
                            dir: dir.clone(),
                            body,
                            scope,
                        };
                        if let Some(prev) = map.insert(name.clone(), skill) {
                            log::warn!(
                                "skill `{name}` at {} overrides {}",
                                dir.display(),
                                prev.dir.display()
                            );
                        }
                    }
                    Err(reason) => log::warn!("skipping {}: {reason}", md.display()),
                }
            }
        }
        Self {
            skills: map.into_values().collect(),
            disabled: Mutex::new(disabled),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    pub fn len(&self) -> usize {
        self.skills.len()
    }

    /// Every discovered skill (the `/skills` picker shows disabled ones too).
    pub fn all(&self) -> &[Skill] {
        &self.skills
    }

    pub fn is_enabled(&self, name: &str) -> bool {
        !self.disabled.lock().unwrap().contains(name)
    }

    /// The enabled skill of this name, or `None` if unknown or disabled.
    pub fn get_enabled(&self, name: &str) -> Option<&Skill> {
        if !self.is_enabled(name) {
            return None;
        }
        self.skills.iter().find(|s| s.name == name)
    }

    /// `(name, description)` for each enabled skill — used to build slash entries.
    pub fn enabled_entries(&self) -> Vec<(String, Option<String>)> {
        self.skills
            .iter()
            .filter(|s| self.is_enabled(&s.name))
            .map(|s| (s.name.clone(), s.description.clone()))
            .collect()
    }

    /// Flip a skill's enabled state, persist the new disabled set, and return
    /// the resulting enabled state. The lock is dropped before disk I/O.
    pub fn toggle(&self, name: &str) -> bool {
        let (now_enabled, snapshot) = {
            let mut d = self.disabled.lock().unwrap();
            let now_enabled = if d.remove(name) {
                true
            } else {
                d.insert(name.to_string());
                false
            };
            let mut snapshot: Vec<String> = d.iter().cloned().collect();
            snapshot.sort();
            (now_enabled, snapshot)
        };
        if let Err(e) = crate::state::persist_disabled_skills(&snapshot) {
            log::warn!("failed to persist skill state: {e}");
        }
        now_enabled
    }

    /// The `<available_skills>` system-prompt block, enabled skills only.
    /// `None` when no skill is enabled.
    pub fn catalog_prompt(&self) -> Option<String> {
        let enabled: Vec<&Skill> = self
            .skills
            .iter()
            .filter(|s| self.is_enabled(&s.name))
            .collect();
        if enabled.is_empty() {
            return None;
        }
        let mut out = String::new();
        out.push_str("Skills provide specialized instructions for specific tasks.\n");
        out.push_str("Use the skill tool to load a skill when a task matches its description.\n\n");
        out.push_str("<available_skills>\n");
        for s in enabled {
            out.push_str("  <skill>\n");
            out.push_str(&format!("    <name>{}</name>\n", s.name));
            if let Some(d) = &s.description {
                out.push_str(&format!(
                    "    <description>{}</description>\n",
                    xml_escape(d)
                ));
            }
            out.push_str(&format!(
                "    <location>{}</location>\n",
                xml_escape(&s.dir.display().to_string())
            ));
            out.push_str("  </skill>\n");
        }
        out.push_str("</available_skills>");
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn md(front: &str, body: &str) -> String {
        format!("---\n{front}\n---\n{body}")
    }

    #[test]
    fn parses_name_description_body() {
        let (n, d, b) = parse_skill_md(&md(
            "name: react\ndescription: Use for React.",
            "Hello body.",
        ))
        .unwrap();
        assert_eq!(n, "react");
        assert_eq!(d.as_deref(), Some("Use for React."));
        assert_eq!(b, "Hello body.");
    }

    #[test]
    fn name_only_is_ok_description_none() {
        let (n, d, _) = parse_skill_md(&md("name: rust-style", "body")).unwrap();
        assert_eq!(n, "rust-style");
        assert!(d.is_none());
    }

    #[test]
    fn strips_quotes_from_values() {
        let (n, d, _) =
            parse_skill_md(&md("name: \"react\"\ndescription: 'quoted'", "body")).unwrap();
        assert_eq!(n, "react");
        assert_eq!(d.as_deref(), Some("quoted"));
    }

    #[test]
    fn missing_name_is_skipped() {
        assert!(parse_skill_md(&md("description: x", "body")).is_err());
    }

    #[test]
    fn no_opening_fence_is_skipped() {
        assert!(parse_skill_md("name: react\nbody").is_err());
    }

    #[test]
    fn bom_prefixed_frontmatter_parses() {
        let (n, _, b) = parse_skill_md(&format!("\u{feff}{}", md("name: react", "body"))).unwrap();
        assert_eq!(n, "react");
        assert_eq!(b, "body");
    }

    #[test]
    fn no_closing_fence_is_skipped() {
        assert!(parse_skill_md("---\nname: react\nbody with no close").is_err());
    }

    #[test]
    fn empty_body_is_skipped() {
        assert!(parse_skill_md(&md("name: react", "   \n  ")).is_err());
    }

    #[test]
    fn invalid_name_is_skipped() {
        assert!(parse_skill_md(&md("name: has space", "body")).is_err());
        assert!(parse_skill_md(&md("name: -leading", "body")).is_err());
        assert!(parse_skill_md(&md("name: sla/sh", "body")).is_err());
    }

    #[test]
    fn base_command_collision_is_skipped() {
        assert!(parse_skill_md(&md("name: model", "body")).is_err());
        assert!(parse_skill_md(&md("name: skills", "body")).is_err());
    }

    #[test]
    fn oversized_body_is_skipped() {
        let big = "x".repeat(MAX_SKILL_BODY_CHARS + 1);
        assert!(parse_skill_md(&md("name: react", &big)).is_err());
    }

    #[test]
    fn valid_name_charset() {
        assert!(valid_skill_name("a"));
        assert!(valid_skill_name("react-patterns_2"));
        assert!(!valid_skill_name(""));
        assert!(!valid_skill_name("-x"));
        assert!(!valid_skill_name("a b"));
    }

    fn write_skill(root: &std::path::Path, name: &str, body: &str) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: desc {name}\n---\n{body}"),
        )
        .unwrap();
    }

    #[test]
    fn discovers_project_and_global_and_sorts_by_name() {
        let tmp = crate::util::unique_temp_dir("ignis-skills-load");
        let home = tmp.join("home");
        let cwd = tmp.join("proj");
        write_skill(&home.join(".ignis/skills"), "zebra", "z");
        write_skill(&cwd.join(".ignis/skills"), "alpha", "a");

        let reg = SkillRegistry::load(Some(&home), &cwd, HashSet::new());
        let names: Vec<&str> = reg.all().iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "zebra"]);
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn project_overrides_global_same_name() {
        let tmp = crate::util::unique_temp_dir("ignis-skills-override");
        let home = tmp.join("home");
        let cwd = tmp.join("proj");
        write_skill(&home.join(".ignis/skills"), "dup", "global-body");
        write_skill(&cwd.join(".ignis/skills"), "dup", "project-body");

        let reg = SkillRegistry::load(Some(&home), &cwd, HashSet::new());
        assert_eq!(reg.len(), 1);
        let s = reg.get_enabled("dup").unwrap();
        assert_eq!(s.body, "project-body");
        assert_eq!(s.scope, SkillScope::Project);
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn ignis_overrides_agents_within_a_level() {
        let tmp = crate::util::unique_temp_dir("ignis-skills-ignis-wins");
        let cwd = tmp.join("proj");
        write_skill(&cwd.join(".agents/skills"), "dup", "agents-body");
        write_skill(&cwd.join(".ignis/skills"), "dup", "ignis-body");

        let reg = SkillRegistry::load(None, &cwd, HashSet::new());
        assert_eq!(reg.get_enabled("dup").unwrap().body, "ignis-body");
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn disabled_skills_are_hidden_from_catalog_and_get() {
        let tmp = crate::util::unique_temp_dir("ignis-skills-disabled");
        let cwd = tmp.join("proj");
        write_skill(&cwd.join(".ignis/skills"), "react", "body");

        let mut disabled = HashSet::new();
        disabled.insert("react".to_string());
        let reg = SkillRegistry::load(None, &cwd, disabled);
        assert!(reg.get_enabled("react").is_none());
        assert!(!reg.is_enabled("react"));
        assert!(reg.catalog_prompt().is_none());
        assert_eq!(reg.all().len(), 1);
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn catalog_lists_enabled_and_escapes_metadata() {
        let tmp = crate::util::unique_temp_dir("ignis-skills-catalog");
        let cwd = tmp.join("proj");
        let dir = cwd.join(".ignis/skills/react");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nname: react\ndescription: use <tag> & stuff\n---\nbody",
        )
        .unwrap();

        let reg = SkillRegistry::load(None, &cwd, HashSet::new());
        let cat = reg.catalog_prompt().unwrap();
        assert!(cat.contains("<name>react</name>"));
        assert!(cat.contains("use &lt;tag&gt; &amp; stuff"));
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn toggle_round_trips_through_state() {
        let _env = crate::util::ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = crate::util::unique_temp_dir("ignis-skills-toggle");
        let cwd = tmp.join("proj");
        write_skill(&cwd.join(".ignis/skills"), "react", "body");
        let prev = std::env::var_os("HOME");
        std::env::set_var("HOME", &tmp);

        let reg = SkillRegistry::load(None, &cwd, HashSet::new());
        assert!(reg.is_enabled("react"));
        assert!(!reg.toggle("react")); // now disabled
        assert!(!reg.is_enabled("react"));
        assert_eq!(
            crate::state::load_state().disabled_skills,
            vec!["react".to_string()]
        );
        assert!(reg.toggle("react")); // back on
        assert!(crate::state::load_state().disabled_skills.is_empty());

        match prev {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        std::fs::remove_dir_all(&tmp).ok();
    }
}
