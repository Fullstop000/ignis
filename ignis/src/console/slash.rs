//! Slash-command catalog + autocomplete + the `/skill-name` prompt builder.
//! The built-in commands live in `SLASH_COMMANDS`; user-installed skills are
//! appended at runtime by `slash_suggestions`, so they show in autocomplete
//! the same way native commands do.
use std::borrow::Cow;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SlashCommand {
    pub(crate) name: Cow<'static, str>,
    pub(crate) description: Cow<'static, str>,
}

const SLASH_COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: Cow::Borrowed("/resume"),
        description: Cow::Borrowed("List and resume sessions"),
    },
    SlashCommand {
        name: Cow::Borrowed("/clear"),
        description: Cow::Borrowed("Start a new session"),
    },
    SlashCommand {
        name: Cow::Borrowed("/compact"),
        description: Cow::Borrowed("Summarize earlier history to free up context"),
    },
    SlashCommand {
        name: Cow::Borrowed("/copy"),
        description: Cow::Borrowed("Copy the last assistant message to clipboard"),
    },
    SlashCommand {
        name: Cow::Borrowed("/model"),
        description: Cow::Borrowed("Switch model and reasoning effort"),
    },
    SlashCommand {
        name: Cow::Borrowed("/skills"),
        description: Cow::Borrowed("Manage skills (enable/disable)"),
    },
    SlashCommand {
        name: Cow::Borrowed("/mcp"),
        description: Cow::Borrowed("Manage MCP servers (enable/disable)"),
    },
    SlashCommand {
        name: Cow::Borrowed("/afk"),
        description: Cow::Borrowed("Toggle AFK mode (auto-approve tools + dismiss ask_user)"),
    },
    SlashCommand {
        name: Cow::Borrowed("/telemetry"),
        description: Cow::Borrowed("Show OpenTelemetry export status"),
    },
];

pub(crate) fn slash_suggestions(
    input: &str,
    skills: Option<&crate::skills::SkillRegistry>,
) -> Vec<SlashCommand> {
    let trimmed = input.trim_start();
    if !trimmed.starts_with('/') || trimmed.contains(' ') {
        return Vec::new();
    }

    let mut candidates: Vec<SlashCommand> = SLASH_COMMANDS.to_vec();
    if let Some(reg) = skills {
        for (name, desc) in reg.enabled_entries() {
            candidates.push(SlashCommand {
                name: Cow::Owned(format!("/{name}")),
                description: Cow::Owned(desc.unwrap_or_else(|| format!("Load the {name} skill"))),
            });
        }
    }

    let query = trimmed.trim_start_matches('/').to_ascii_lowercase();
    let mut matches: Vec<(usize, usize, SlashCommand)> = candidates
        .into_iter()
        .enumerate()
        .filter_map(|(idx, command)| {
            if query.is_empty() {
                return Some((0, idx, command));
            }
            let name = command.name.trim_start_matches('/').to_ascii_lowercase();
            let description = command.description.to_ascii_lowercase();
            if name.starts_with(&query) {
                Some((0, idx, command))
            } else if name.contains(&query) {
                Some((1, idx, command))
            } else if description.contains(&query) {
                Some((2, idx, command))
            } else {
                None
            }
        })
        .collect();
    matches.sort_by_key(|(rank, idx, _)| (*rank, *idx));
    matches.into_iter().map(|(_, _, command)| command).collect()
}
/// Build the prompt sent when a user force-loads a skill via `/skill-name`.
/// `args` is the remainder of the input after the command (may be empty).
/// The body is presented as already-loaded instructions so the model follows
/// them directly instead of re-invoking the `skill` tool.
pub(crate) fn build_skill_prompt(name: &str, body: &str, args: &str) -> String {
    let head = format!(
        "The \"{name}\" skill is now active — its instructions are below. Follow them \
         for this task; they are already loaded, so do not call the skill tool.\n\n{body}"
    );
    let args = args.trim();
    if args.is_empty() {
        head
    } else {
        format!("{head}\n\n---\n{args}")
    }
}
#[cfg(test)]
mod slash_skill_tests {
    use super::slash_suggestions;

    #[test]
    fn slash_suggestions_include_enabled_skills_exclude_disabled() {
        let tmp = crate::util::unique_temp_dir("ignis-slash-skills");
        let cwd = tmp.join("proj");
        for n in ["alpha", "beta"] {
            let dir = cwd.join(".ignis/skills").join(n);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("SKILL.md"), format!("---\nname: {n}\n---\nbody")).unwrap();
        }
        let mut disabled = std::collections::HashSet::new();
        disabled.insert("beta".to_string());
        let reg = crate::skills::SkillRegistry::load(None, &cwd, disabled);

        let names: Vec<String> = slash_suggestions("/", Some(&reg))
            .into_iter()
            .map(|c| c.name.into_owned())
            .collect();
        assert!(names.iter().any(|n| n == "/alpha"));
        assert!(!names.iter().any(|n| n == "/beta")); // disabled
        assert!(names.iter().any(|n| n == "/skills"));
        assert!(names.iter().any(|n| n == "/mcp"));
        assert!(names.iter().any(|n| n == "/telemetry"));
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn build_skill_prompt_with_and_without_args() {
        let body = "Follow these rules.";
        let with = super::build_skill_prompt("react", body, "fix this");
        assert!(with.contains("\"react\" skill"));
        assert!(with.contains("do not call the skill tool")); // suppress redundant tool call
        assert!(with.contains("Follow these rules."));
        assert!(with.contains("fix this"));

        let bare = super::build_skill_prompt("react", body, "");
        assert!(bare.contains("\"react\" skill"));
        assert!(bare.contains("Follow these rules."));
        assert!(!bare.contains("---")); // no args tail
    }
}
