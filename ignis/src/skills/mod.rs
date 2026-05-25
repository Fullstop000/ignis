//! User-defined skills: `SKILL.md` instruction sets discovered from disk,
//! advertised to the model, loadable on demand, and toggleable at runtime.

use std::path::PathBuf;

/// Largest skill body we will load (≈12k tokens). Oversized skills are skipped
/// so loading or inlining one can never blow the context window.
pub const MAX_SKILL_BODY_CHARS: usize = 50_000;

/// Slash-command names a skill may not shadow.
#[allow(dead_code)]
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
#[allow(dead_code)]
pub(crate) fn parse_skill_md(content: &str) -> Result<(String, Option<String>, String), String> {
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
}
