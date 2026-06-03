//! Tiny, dependency-free git-branch probe for the TUI footer (oh-my-zsh
//! style). Reads `.git/HEAD` directly rather than shelling out to `git`, so
//! it stays cheap enough to refresh on every turn boundary. Returns the branch
//! name for a normal checkout, a short SHA for a detached HEAD, or `None` when
//! the cwd isn't inside a work tree.

use std::path::Path;

/// Parse the contents of a `HEAD` file into a display string.
///
/// - `ref: refs/heads/<branch>` → `<branch>` (slashes in the branch kept).
/// - a bare 40-char object id → the 7-char short SHA (detached HEAD).
/// - anything else → `None`.
fn parse_head(content: &str) -> Option<String> {
    let line = content.trim();
    if let Some(rest) = line.strip_prefix("ref:") {
        let r = rest.trim();
        let branch = r.strip_prefix("refs/heads/").unwrap_or(r);
        return (!branch.is_empty()).then(|| branch.to_string());
    }
    // Detached HEAD: a raw object id. Accept the usual 40-hex (and the
    // 64-hex SHA-256 case) and surface a short prefix.
    if line.len() >= 7 && line.chars().all(|c| c.is_ascii_hexdigit()) {
        return Some(line[..7].to_string());
    }
    None
}

/// Resolve the current branch (or short detached SHA) for `cwd`, walking up
/// parent directories to find the work tree's `.git`. Handles both a `.git`
/// directory (normal clone) and a `.git` *file* (`gitdir: …` pointer used by
/// worktrees and submodules).
pub(crate) fn branch(cwd: &Path) -> Option<String> {
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        let dot_git = d.join(".git");
        if let Ok(meta) = std::fs::symlink_metadata(&dot_git) {
            let git_dir = if meta.is_dir() {
                dot_git
            } else {
                // `.git` is a file: `gitdir: <path>` (possibly relative to d).
                let content = std::fs::read_to_string(&dot_git).ok()?;
                let target = content.trim().strip_prefix("gitdir:")?.trim();
                let p = Path::new(target);
                if p.is_absolute() {
                    p.to_path_buf()
                } else {
                    d.join(p)
                }
            };
            let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
            return parse_head(&head);
        }
        dir = d.parent();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn parse_head_reads_branch_from_ref() {
        assert_eq!(
            parse_head("ref: refs/heads/main\n").as_deref(),
            Some("main")
        );
    }

    #[test]
    fn parse_head_keeps_slashes_in_branch() {
        assert_eq!(
            parse_head("ref: refs/heads/feature/login\n").as_deref(),
            Some("feature/login")
        );
    }

    #[test]
    fn parse_head_detached_returns_short_sha() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(parse_head(sha).as_deref(), Some("0123456"));
    }

    #[test]
    fn parse_head_garbage_returns_none() {
        assert_eq!(parse_head(""), None);
        assert_eq!(parse_head("not a ref or sha"), None);
    }

    #[test]
    fn branch_reads_head_in_plain_repo() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join(".git")).unwrap();
        fs::write(tmp.path().join(".git/HEAD"), "ref: refs/heads/dev\n").unwrap();
        assert_eq!(branch(tmp.path()).as_deref(), Some("dev"));
    }

    #[test]
    fn branch_walks_up_from_subdirectory() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join(".git")).unwrap();
        fs::write(tmp.path().join(".git/HEAD"), "ref: refs/heads/up\n").unwrap();
        let sub = tmp.path().join("a/b/c");
        fs::create_dir_all(&sub).unwrap();
        assert_eq!(branch(&sub).as_deref(), Some("up"));
    }

    #[test]
    fn branch_follows_worktree_gitfile() {
        let tmp = tempfile::tempdir().unwrap();
        // Real gitdir (as a main repo's worktrees/<name> would be).
        let real = tmp.path().join("realgit");
        fs::create_dir_all(&real).unwrap();
        fs::write(real.join("HEAD"), "ref: refs/heads/wt\n").unwrap();
        // The worktree dir whose `.git` is a file pointing at `real`.
        let wt = tmp.path().join("worktree");
        fs::create_dir(&wt).unwrap();
        fs::write(wt.join(".git"), format!("gitdir: {}\n", real.display())).unwrap();
        assert_eq!(branch(&wt).as_deref(), Some("wt"));
    }

    #[test]
    fn branch_returns_none_outside_repo() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(branch(tmp.path()), None);
    }
}
