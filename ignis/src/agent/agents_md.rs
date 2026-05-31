//! Project instructions from `AGENTS.md`.
//!
//! Reads the global (`~/.ignis/AGENTS.md`) and project (`<cwd>/AGENTS.md`)
//! instruction files (the cross-tool project-instructions convention) and
//! formats them as a single user-message body. The agent loop prepends this as
//! a synthetic first user turn (see [`crate::agent::Agent`]) so the model treats
//! it as authoritative project context without it being persisted or rendered.
//!
//! Both files stack (matching Claude Code and Codex): global is emitted first
//! (machine-wide base); the project file is emitted last so it overrides the
//! global one on conflict.

use std::path::Path;

use crate::llm::Message;

const AGENTS_MD_FILENAME: &str = "AGENTS.md";
/// Per-file byte budget; a runaway file must not blow the context window.
const AGENTS_MD_MAX_BYTES: usize = 32 * 1024;

/// Load the global (`~/.ignis/AGENTS.md`) then project (`<cwd>/AGENTS.md`)
/// instructions and format them as a project-instructions user-message body.
/// Both stack, global first; returns `None` when neither file exists (or both
/// are empty). Symlinks are followed by the normal read; each file is capped
/// independently.
pub fn load(cwd: &Path, home: Option<&Path>) -> Option<String> {
    let mut sections: Vec<(&str, String)> = Vec::new();
    // Global base first (lowest precedence).
    if let Some(h) = home {
        if let Some(c) = read_file(&h.join(".ignis").join(AGENTS_MD_FILENAME)) {
            sections.push(("global (~/.ignis/AGENTS.md)", c));
        }
    }
    // Project file last (overrides the global one on conflict).
    if let Some(c) = read_file(&cwd.join(AGENTS_MD_FILENAME)) {
        sections.push(("project (./AGENTS.md)", c));
    }
    if sections.is_empty() {
        return None;
    }

    let mut body = String::from(
        "The following are project-specific instructions from AGENTS.md. Treat them as \
         authoritative for this project's conventions, second only to my direct instructions \
         in this conversation. When two files conflict, the later (more specific) one wins.\n",
    );
    for (label, content) in &sections {
        body.push_str("\n<!-- ");
        body.push_str(label);
        body.push_str(" -->\n");
        body.push_str(content);
        body.push('\n');
    }
    Some(body)
}

/// Read a single AGENTS.md file, trimmed and capped. `None` if it is absent or
/// empty. Symlinks are followed.
fn read_file(path: &Path) -> Option<String> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            // A bare missing file is the common case → stay silent. A path that
            // exists yet can't be read (dangling symlink, permissions) is worth
            // surfacing.
            if path.symlink_metadata().is_ok() {
                log::warn!(
                    "AGENTS.md present at {} but unreadable: {e}",
                    path.display()
                );
            }
            return None;
        }
    };
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(cap_on_char_boundary(trimmed, AGENTS_MD_MAX_BYTES))
}

/// Truncate `s` to at most `max` bytes on a UTF-8 char boundary, appending a
/// marker when truncation occurs.
fn cap_on_char_boundary(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    log::warn!("AGENTS.md exceeded {max} bytes; truncating");
    format!(
        "{}\n\n[… AGENTS.md truncated at {} KiB]",
        &s[..end],
        max / 1024
    )
}

/// Build the synthetic user turn that carries the project instructions.
fn project_instructions_message(block: &str) -> Message {
    Message {
        role: "user".to_string(),
        content: Some(block.to_string()),
        reasoning_content: None,
        name: None,
        tool_call_id: None,
        tool_calls: None,
        created_at_ms: None,
    }
}

/// Prepend the project-instructions turn (when present) ahead of `history` for
/// the model request. Returns `history` unchanged with no allocation when there
/// is no AGENTS.md, so the common case stays zero-copy. The result is never
/// stored in the session — it is rebuilt each turn from the current file(s).
pub(crate) fn prepend<'a>(
    project_instructions: Option<&str>,
    history: &'a [Message],
) -> std::borrow::Cow<'a, [Message]> {
    match project_instructions {
        Some(block) => {
            let mut msgs = Vec::with_capacity(history.len() + 1);
            msgs.push(project_instructions_message(block));
            msgs.extend_from_slice(history);
            std::borrow::Cow::Owned(msgs)
        }
        None => std::borrow::Cow::Borrowed(history),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_agents_md(dir: &Path, content: &str) {
        std::fs::write(dir.join(AGENTS_MD_FILENAME), content).unwrap();
    }

    #[test]
    fn absent_file_returns_none() {
        let dir = crate::util::unique_temp_dir("ignis-agents-absent");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(load(&dir, None).is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn basic_file_is_loaded_with_header() {
        let dir = crate::util::unique_temp_dir("ignis-agents-basic");
        std::fs::create_dir_all(&dir).unwrap();
        write_agents_md(&dir, "Always answer in haiku.");
        let out = load(&dir, None).expect("should load");
        assert!(out.contains("Always answer in haiku."));
        assert!(out.contains("authoritative"));
        assert!(out.contains("project (./AGENTS.md)"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn whitespace_only_file_returns_none() {
        let dir = crate::util::unique_temp_dir("ignis-agents-blank");
        std::fs::create_dir_all(&dir).unwrap();
        write_agents_md(&dir, "   \n\t\n  ");
        assert!(load(&dir, None).is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn oversized_file_is_truncated_with_marker() {
        let dir = crate::util::unique_temp_dir("ignis-agents-big");
        std::fs::create_dir_all(&dir).unwrap();
        write_agents_md(&dir, &"a".repeat(AGENTS_MD_MAX_BYTES + 5_000));
        let out = load(&dir, None).expect("should load");
        assert!(out.contains("truncated"));
        // Header + one capped section; the file body itself never exceeds the cap.
        assert!(out.len() < AGENTS_MD_MAX_BYTES + 1_000);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn cap_respects_char_boundary() {
        // 4-byte chars: cutting mid-char would panic if boundary isn't honored.
        let s = "😀".repeat(20);
        let capped = cap_on_char_boundary(&s, 10);
        assert!(capped.contains("truncated"));
    }

    #[cfg(unix)]
    #[test]
    fn dangling_symlink_returns_none_without_panic() {
        let dir = crate::util::unique_temp_dir("ignis-agents-dangling");
        std::fs::create_dir_all(&dir).unwrap();
        std::os::unix::fs::symlink(dir.join("nonexistent-target"), dir.join(AGENTS_MD_FILENAME))
            .unwrap();
        assert!(load(&dir, None).is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn symlink_to_real_file_is_followed() {
        let dir = crate::util::unique_temp_dir("ignis-agents-symlink");
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("real-instructions.md");
        std::fs::write(&target, "Use tabs, not spaces.").unwrap();
        std::os::unix::fs::symlink(&target, dir.join(AGENTS_MD_FILENAME)).unwrap();
        let out = load(&dir, None).expect("should follow symlink");
        assert!(out.contains("Use tabs, not spaces."));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn global_comes_before_project_and_both_stack() {
        let home = crate::util::unique_temp_dir("ignis-agents-home");
        let cwd = crate::util::unique_temp_dir("ignis-agents-cwd");
        std::fs::create_dir_all(home.join(".ignis")).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(home.join(".ignis").join(AGENTS_MD_FILENAME), "GLOBAL_RULE").unwrap();
        write_agents_md(&cwd, "PROJECT_RULE");
        let out = load(&cwd, Some(&home)).expect("should load both");
        let g = out.find("GLOBAL_RULE").expect("global present");
        let p = out.find("PROJECT_RULE").expect("project present");
        assert!(
            g < p,
            "global must come before project so project overrides"
        );
        assert!(out.contains("global (~/.ignis/AGENTS.md)"));
        assert!(out.contains("project (./AGENTS.md)"));
        std::fs::remove_dir_all(&home).unwrap();
        std::fs::remove_dir_all(&cwd).unwrap();
    }

    #[test]
    fn global_only_is_loaded_when_no_project_file() {
        let home = crate::util::unique_temp_dir("ignis-agents-global-only-home");
        let cwd = crate::util::unique_temp_dir("ignis-agents-global-only-cwd");
        std::fs::create_dir_all(home.join(".ignis")).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(home.join(".ignis").join(AGENTS_MD_FILENAME), "GLOBAL_ONLY").unwrap();
        let out = load(&cwd, Some(&home)).expect("should load global");
        assert!(out.contains("GLOBAL_ONLY"));
        assert!(!out.contains("project (./AGENTS.md)"));
        std::fs::remove_dir_all(&home).unwrap();
        std::fs::remove_dir_all(&cwd).unwrap();
    }

    fn user_msg(text: &str) -> Message {
        Message {
            role: "user".to_string(),
            content: Some(text.to_string()),
            reasoning_content: None,
            name: None,
            tool_call_id: None,
            tool_calls: None,
            created_at_ms: None,
        }
    }

    #[test]
    fn prepend_none_borrows_history_unchanged() {
        let history = vec![user_msg("hello")];
        let out = prepend(None, &history);
        assert!(matches!(out, std::borrow::Cow::Borrowed(_)));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].content.as_deref(), Some("hello"));
    }

    #[test]
    fn prepend_some_inserts_instructions_as_first_user_turn() {
        let history = vec![user_msg("what should I do?")];
        let out = prepend(Some("PROJECT RULES"), &history);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].role, "user");
        assert_eq!(out[0].content.as_deref(), Some("PROJECT RULES"));
        assert_eq!(out[1].content.as_deref(), Some("what should I do?"));
    }
}
