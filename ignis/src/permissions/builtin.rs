//! Built-in safety data: read-only auto-allow set, protected paths, circuit
//! breakers, banned-from-allow patterns. Static, curated, no user touchpoint.

use std::sync::OnceLock;

/// Curated set of bash commands that never need approval. Matched as the
/// **first whitespace-separated token** of the command (so `git status` matches
/// `git status -s`, but `cargo` does not match `cargo build`).
///
/// For multi-word commands like `git status`, we also accept the prefix-match
/// shape: a command starting with `git ` is checked against `git status`,
/// `git log`, etc. specifically.
///
/// Curated rather than pattern-based: smaller surface, no regex compile cost,
/// no foot-guns from a typo in a user-supplied pattern. v0.17.0 will add a
/// proper `Bash(git *)` user grammar; this set ships first for safety floor.
const READ_ONLY_BASH: &[&str] = &[
    // Pure read commands.
    "ls", "cat", "echo", "pwd", "head", "tail", "wc", "stat", "du", "df", "which", "whoami",
    "hostname", "uname", "date", "uptime", "id", "env", "printenv", "true", "false",
    // Search / inspection (read-only).
    "grep", "rg", "find", "fd", "ag", "ack", "locate", "tree",
    // File-info diff/inspection (read-only by default).
    "diff", "cmp", "file", "wc", "od", "xxd",
];

/// Multi-word read-only commands (first two whitespace-separated tokens).
/// `git status`, `git log`, etc. — git in particular is split because some
/// subcommands write (`git push`, `git reset`) and others read.
const READ_ONLY_BASH_MULTIWORD: &[&str] = &[
    "git status",
    "git log",
    "git diff",
    "git show",
    "git branch",
    "git remote",
    "git tag",
    "git ls-files",
    "git ls-tree",
    "git rev-parse",
    "git rev-list",
    "git config --get",
    "git config --list",
    "git blame",
    "git describe",
    "git stash list",
    "git stash show",
    "cargo --version",
    "cargo metadata",
    "cargo tree",
    "cargo search",
    "npm list",
    "npm view",
    "pip show",
    "pip list",
];

/// Return `true` if the bash command's leading tokens match the read-only set.
///
/// Logic: skip leading env-var assignments (`FOO=bar`) and `sudo`-style
/// wrappers (NOT honored — sudo always asks). Then check the first token
/// against `READ_ONLY_BASH`, then the first two against `READ_ONLY_BASH_MULTIWORD`.
///
/// Returns `false` for any command containing shell injection patterns
/// (`$()`, backticks, `&&`, `||`, `;`, `|`) — those need to be evaluated
/// segment-by-segment (v0.17.0). For v0.16.0 we conservatively make the user
/// approve compound commands.
pub fn is_read_only_bash(command: &str) -> bool {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return false;
    }
    // Compound / substitution → never auto-allow in v0.16.0.
    if trimmed.contains("&&")
        || trimmed.contains("||")
        || trimmed.contains(';')
        || trimmed.contains('|')
        || trimmed.contains("$(")
        || trimmed.contains('`')
        || trimmed.contains('>')
        || trimmed.contains('<')
    {
        return false;
    }
    let tokens: Vec<&str> = trimmed.split_whitespace().collect();
    if tokens.is_empty() {
        return false;
    }
    // Reject sudo upfront — never auto-allow a privileged escalation.
    if tokens[0] == "sudo" {
        return false;
    }
    // Single-token match.
    if READ_ONLY_BASH.contains(&tokens[0]) {
        return true;
    }
    // Two-token match.
    if tokens.len() >= 2 {
        let pair = format!("{} {}", tokens[0], tokens[1]);
        if READ_ONLY_BASH_MULTIWORD.iter().any(|m| *m == pair) {
            return true;
        }
        // Three-token match (e.g. `git config --get user.email`).
        if tokens.len() >= 3 {
            let triple = format!("{} {} {}", tokens[0], tokens[1], tokens[2]);
            if READ_ONLY_BASH_MULTIWORD.iter().any(|m| *m == triple) {
                return true;
            }
        }
    }
    false
}

/// Circuit-breaker patterns — destructive commands that ALWAYS ask, even
/// under `BypassPermissions`. Under AFK, these auto-deny (no user available
/// to authorize). The set is intentionally tiny: `rm -rf /`, `rm -rf ~`,
/// `rm -rf $HOME`, all variants. Bigger patterns belong in user-supplied
/// deny rules (v0.17.0); this is the "even if you said yes to everything,
/// this one still asks" floor.
pub fn is_circuit_breaker(command: &str) -> bool {
    circuit_breaker_label(command).is_some()
}

/// Return the human-readable label of the matching circuit-breaker pattern,
/// or `None` if none matched. Used in the UI to explain why the picker fired.
pub fn circuit_breaker_label(command: &str) -> Option<&'static str> {
    let normalized = command.trim().replace('\t', " ");
    // Strip leading env-var assignments and `sudo` so `sudo rm -rf /` still
    // matches.
    let mut rest = normalized.as_str();
    loop {
        let stripped = rest.trim_start();
        // env var: KEY=VALUE at start
        if let Some(eq_idx) = stripped.find('=') {
            let key_part = &stripped[..eq_idx];
            if !key_part.is_empty()
                && key_part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_')
            {
                if let Some(space_idx) = stripped.find(char::is_whitespace) {
                    if space_idx < eq_idx {
                        // Whitespace before '=', so not an env var
                    } else {
                        rest = stripped[space_idx..].trim_start();
                        continue;
                    }
                }
            }
        }
        if let Some(stripped) = stripped.strip_prefix("sudo ") {
            rest = stripped;
            continue;
        }
        rest = stripped;
        break;
    }

    let collapsed: String = rest.split_whitespace().collect::<Vec<_>>().join(" ");

    // Match patterns. Each is a tuple of (label, predicate).
    type Pattern = (&'static str, fn(&str) -> bool);
    let patterns: &[Pattern] = &[
        ("rm -rf /", |c| {
            // `rm -rf /` or `rm -rf / <anything>` but not `rm -rf /tmp/...`
            let with_flags = c.starts_with("rm -rf ") || c.starts_with("rm -fr ");
            if !with_flags {
                return false;
            }
            let after = &c[7..];
            let first = after.split_whitespace().next().unwrap_or("");
            first == "/"
        }),
        ("rm -rf ~", |c| {
            let with_flags = c.starts_with("rm -rf ") || c.starts_with("rm -fr ");
            if !with_flags {
                return false;
            }
            let after = &c[7..];
            let first = after.split_whitespace().next().unwrap_or("");
            first == "~" || first == "~/"
        }),
        ("rm -rf $HOME", |c| {
            let with_flags = c.starts_with("rm -rf ") || c.starts_with("rm -fr ");
            if !with_flags {
                return false;
            }
            let after = &c[7..];
            let first = after.split_whitespace().next().unwrap_or("");
            first == "$HOME" || first == "\"$HOME\"" || first == "${HOME}" || first == "\"${HOME}\""
        }),
    ];

    patterns
        .iter()
        .find(|(_, p)| p(&collapsed))
        .map(|(label, _)| *label)
}

/// Protected path patterns — files/dirs the model should never silently edit
/// even under `BypassPermissions`. The patterns are absolute or basename-based;
/// matched against the path the model gave to `edit_file`/`create_file`.
///
/// The intent is "the model can't accidentally rewrite your shell init or
/// ignis's own config." Under `BypassPermissions` these still raise a picker;
/// under AFK they auto-deny.
pub fn is_protected_path(path: &str) -> bool {
    let path = path.trim();
    if path.is_empty() {
        return false;
    }

    // Strip leading `./` and any leading slash for basename-style checks.
    let normalized = path.trim_start_matches("./");

    // Anywhere-in-path matches (directory or file basename).
    let needles_anywhere: &[&str] = &["/.git/", "/.ignis/"];
    for needle in needles_anywhere {
        if normalized.contains(needle) {
            return true;
        }
    }

    // Basename matches (last path segment exactly equals).
    let basename = normalized.rsplit('/').next().unwrap_or(normalized);
    let protected_basenames: &[&str] = &[
        ".bashrc",
        ".bash_profile",
        ".bash_login",
        ".bash_logout",
        ".zshrc",
        ".zprofile",
        ".zlogin",
        ".zlogout",
        ".profile",
        ".gitconfig",
        ".ripgreprc",
    ];
    if protected_basenames.contains(&basename) {
        return true;
    }

    // Top-level directory matches (path STARTS with these — model gave a
    // relative path like `.git/config`).
    let prefixes: &[&str] = &[
        ".git/", ".ignis/", ".git",   // raw `.git` (no trailing slash)
        ".ignis", // raw `.ignis`
    ];
    for prefix in prefixes {
        if normalized == *prefix {
            return true;
        }
        if let Some(rest) = normalized.strip_prefix(prefix) {
            if rest.is_empty() || rest.starts_with('/') {
                return true;
            }
        }
    }

    // ignis's own runtime files (absolute).
    let home_marker = home_dir_cached();
    if let Some(home) = home_marker {
        let ignis_files = [
            format!("{}/.ignis/config.toml", home),
            format!("{}/.ignis/state.json", home),
        ];
        if ignis_files.iter().any(|f| normalized == *f) {
            return true;
        }
        if normalized.starts_with(&format!("{}/.ignis/", home)) {
            return true;
        }
    }

    false
}

fn home_dir_cached() -> Option<&'static str> {
    static CACHE: OnceLock<Option<String>> = OnceLock::new();
    CACHE
        .get_or_init(|| dirs::home_dir().and_then(|p| p.to_str().map(String::from)))
        .as_deref()
}

#[cfg(test)]
mod tests {
    use super::*;

    // -------- read-only bash set --------

    #[test]
    fn read_only_basic_commands() {
        for cmd in [
            "ls",
            "ls -la",
            "cat README.md",
            "pwd",
            "echo hello",
            "head -n 5 file.txt",
            "grep -rn foo src/",
            "rg foo",
            "find . -name '*.rs'",
            "git status",
            "git log -n 5",
            "git diff HEAD",
            "git show abc123",
            "cargo --version",
            "cargo tree",
        ] {
            assert!(is_read_only_bash(cmd), "expected read-only: {cmd}");
        }
    }

    #[test]
    fn read_only_rejects_mutating_commands() {
        for cmd in [
            "rm file.txt",
            "mv a b",
            "cp a b",
            "git push",
            "git reset --hard",
            "git checkout main",
            "cargo build",
            "npm install",
            "touch foo",
            "mkdir bar",
            "chmod 755 x",
            "curl -X POST url",
        ] {
            assert!(!is_read_only_bash(cmd), "should NOT be read-only: {cmd}");
        }
    }

    #[test]
    fn read_only_rejects_compound_commands() {
        // Compound commands need v0.17.0 segment-split logic; v0.16.0 plays safe.
        for cmd in [
            "ls && cat foo",
            "ls; cat foo",
            "ls | grep foo",
            "ls > out.txt",
            "echo $(date)",
            "echo `date`",
        ] {
            assert!(
                !is_read_only_bash(cmd),
                "should NOT auto-allow compound cmd: {cmd}"
            );
        }
    }

    #[test]
    fn read_only_rejects_sudo() {
        assert!(!is_read_only_bash("sudo ls"));
        assert!(!is_read_only_bash("sudo cat /etc/passwd"));
    }

    #[test]
    fn read_only_rejects_empty_and_whitespace() {
        assert!(!is_read_only_bash(""));
        assert!(!is_read_only_bash("   "));
    }

    // -------- circuit breakers --------

    #[test]
    fn circuit_breaker_matches_canonical_forms() {
        for cmd in [
            "rm -rf /",
            "rm -rf /  ",
            "  rm -rf / ",
            "rm -fr /",
            "rm -rf ~",
            "rm -rf ~/",
            "rm -rf $HOME",
            "rm -rf \"$HOME\"",
            "rm -rf ${HOME}",
            "rm -rf \"${HOME}\"",
            "sudo rm -rf /",
            "sudo rm -rf $HOME",
        ] {
            assert!(is_circuit_breaker(cmd), "expected circuit breaker: {cmd}");
        }
    }

    #[test]
    fn circuit_breaker_does_not_match_safe_rms() {
        for cmd in [
            "rm -rf /tmp/foo",
            "rm -rf ./build",
            "rm -rf ~/projects/scratch",
            "rm foo.txt",
            "rm -r src",
            "rm -f x",
        ] {
            assert!(!is_circuit_breaker(cmd), "should NOT trip: {cmd}");
        }
    }

    #[test]
    fn circuit_breaker_label_returns_pattern_name() {
        assert_eq!(circuit_breaker_label("rm -rf /"), Some("rm -rf /"));
        assert_eq!(circuit_breaker_label("rm -rf ~"), Some("rm -rf ~"));
        assert_eq!(circuit_breaker_label("rm -rf $HOME"), Some("rm -rf $HOME"));
        assert_eq!(circuit_breaker_label("rm foo.txt"), None);
    }

    // -------- protected paths --------

    #[test]
    fn protected_blocks_git_internals() {
        for path in [
            ".git",
            ".git/config",
            ".git/HEAD",
            "./.git/config",
            "subdir/.git/config",
            "/abs/path/.git/config",
        ] {
            assert!(is_protected_path(path), "expected protected: {path}");
        }
    }

    #[test]
    fn protected_blocks_ignis_internals() {
        for path in [
            ".ignis",
            ".ignis/config.toml",
            ".ignis/state.json",
            "./.ignis/skills/foo/SKILL.md",
        ] {
            assert!(is_protected_path(path), "expected protected: {path}");
        }
    }

    #[test]
    fn protected_blocks_shell_init() {
        for path in [
            ".bashrc",
            "./.bashrc",
            "/home/zht/.bashrc",
            ".zshrc",
            ".profile",
            ".gitconfig",
        ] {
            assert!(is_protected_path(path), "expected protected: {path}");
        }
    }

    #[test]
    fn protected_allows_normal_paths() {
        for path in [
            "src/main.rs",
            "README.md",
            "Cargo.toml",
            "tests/cli.rs",
            "docs/foo.md",
        ] {
            assert!(!is_protected_path(path), "should NOT protect: {path}");
        }
    }
}
