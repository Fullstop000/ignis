//! `enter_worktree` / `exit_worktree` — isolate a session's edits on a fresh
//! git worktree + branch, then keep or discard it.
//!
//! `enter_worktree` creates `<repo>/.ignis/worktrees/<name>` on a new branch and
//! redirects the whole toolset into it by swapping the shared [`SessionCwd`]
//! (see [`super::cwd`]); subsequent edits made by relative path land in the
//! worktree, not the user's working copy. `exit_worktree` restores the original
//! directory and either keeps the worktree (for review) or removes it — refusing
//! to destroy unsaved work unless `discard_changes` is set.
//!
//! Both are top-level only and session-scoped: `exit_worktree` only ever touches
//! the worktree `enter_worktree` created *this session* ([`WorktreeState`]); it
//! never removes a worktree made by hand.

use crate::tools::cwd::SessionCwd;
use crate::tools::tool::{ExecutionMode, StaticTool, ToolArgs, ToolOutcome, ToolParam};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Where session worktrees live, relative to the repo top level. Already
/// git-ignored (committed `.gitignore`), so worktree contents never pollute
/// `git status` in the main checkout.
const WORKTREE_DIR: &str = ".ignis/worktrees";

/// A worktree this session created via `enter_worktree`.
#[derive(Clone, Debug)]
pub struct ActiveWorktree {
    pub path: PathBuf,
    pub branch: String,
    /// The commit the branch was cut from — used to detect commits made only in
    /// the worktree when deciding whether `remove` would lose work.
    pub base_commit: String,
    /// Where the session was before `enter_worktree`, restored on exit.
    pub original_cwd: PathBuf,
}

/// Session-scoped: the (at most one) worktree entered this session, shared by
/// the enter/exit tools.
pub type WorktreeState = Arc<Mutex<Option<ActiveWorktree>>>;

pub fn new_state() -> WorktreeState {
    Arc::new(Mutex::new(None))
}

enum BaseRef {
    Head,
    Fresh,
}

fn parse_base_ref(s: Option<&str>) -> Result<BaseRef, String> {
    match s {
        None | Some("head") => Ok(BaseRef::Head),
        Some("fresh") => Ok(BaseRef::Fresh),
        Some(other) => Err(format!(
            "invalid base_ref '{other}': expected \"head\" or \"fresh\""
        )),
    }
}

/// Run `git` in `dir`, returning trimmed stdout on success or trimmed stderr as
/// the error.
async fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = tokio::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .await
        .map_err(|e| format!("failed to run git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// The repo top level for `cwd`, or an error if it isn't a git work tree.
async fn toplevel(cwd: &Path) -> Result<PathBuf, String> {
    git(cwd, &["rev-parse", "--show-toplevel"])
        .await
        .map(PathBuf::from)
        .map_err(|_| "not a git repository".to_string())
}

/// True when `cwd`'s work tree is a *linked worktree* (so creating another would
/// nest). A linked worktree's top-level `.git` is a file (a `gitdir:` pointer);
/// the main checkout's is a directory. Submodules also use a `.git` file, so
/// they're excluded via `--show-superproject-working-tree`.
async fn is_nested_worktree(cwd: &Path, top: &Path) -> bool {
    let dot_git_is_file = std::fs::metadata(top.join(".git"))
        .map(|m| m.is_file())
        .unwrap_or(false);
    if !dot_git_is_file {
        return false;
    }
    let superproject = git(cwd, &["rev-parse", "--show-superproject-working-tree"])
        .await
        .unwrap_or_default();
    superproject.is_empty() // empty => not a submodule => a real linked worktree
}

/// `origin/<default-branch>` (e.g. `origin/main`) for the `fresh` base.
async fn default_branch(cwd: &Path) -> Result<String, String> {
    git(
        cwd,
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
    )
    .await
    .map_err(|_| {
        "cannot resolve origin's default branch (no remote HEAD); use base_ref \"head\"".to_string()
    })
}

/// Reduce a user-supplied name to a single safe path/branch component.
fn sanitize(name: &str) -> Result<String, String> {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches('-').to_string();
    if cleaned.is_empty() {
        return Err("worktree name is empty after sanitizing".to_string());
    }
    Ok(cleaned)
}

/// A generated `wt-<hex>` name when none is given. Collisions are caught by the
/// "already exists" check in [`create`], so a coarse clock-derived suffix is
/// enough.
fn generated_name() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    format!("wt-{:06x}", nanos & 0xff_ffff)
}

/// Create the worktree + branch, returning the tracked handle. Does not switch
/// the session cwd — the caller does that on success.
async fn create(
    cwd: &Path,
    name: Option<&str>,
    base_ref: BaseRef,
) -> Result<ActiveWorktree, String> {
    let top = toplevel(cwd).await?;
    if is_nested_worktree(cwd, &top).await {
        return Err(format!(
            "already inside a worktree ({}); exit it before creating another",
            cwd.display()
        ));
    }

    let name = match name {
        Some(n) => sanitize(n)?,
        None => generated_name(),
    };
    let path = top.join(WORKTREE_DIR).join(&name);
    if path.exists() {
        return Err(format!("{} already exists", path.display()));
    }
    let path_str = path.to_str().ok_or("worktree path is not valid UTF-8")?;

    let base = match base_ref {
        BaseRef::Head => "HEAD".to_string(),
        BaseRef::Fresh => default_branch(cwd).await?,
    };
    let base_commit = git(cwd, &["rev-parse", &base])
        .await
        .map_err(|e| format!("cannot resolve base ref '{base}': {e}"))?;

    git(
        cwd,
        &["worktree", "add", "-b", &name, path_str, &base_commit],
    )
    .await
    .map_err(|e| format!("git worktree add failed: {e}"))?;

    Ok(ActiveWorktree {
        path,
        branch: name,
        base_commit,
        original_cwd: cwd.to_path_buf(),
    })
}

/// If the worktree holds unsaved work, a message describing it; else `None`.
async fn unsaved_work(active: &ActiveWorktree) -> Result<Option<String>, String> {
    let status = git(&active.path, &["status", "--porcelain"]).await?;
    let uncommitted = status.lines().filter(|l| !l.trim().is_empty()).count();
    let ahead = git(
        &active.path,
        &[
            "rev-list",
            "--count",
            &format!("{}..HEAD", active.base_commit),
        ],
    )
    .await
    .ok()
    .and_then(|s| s.parse::<usize>().ok())
    .unwrap_or(0);
    if uncommitted == 0 && ahead == 0 {
        return Ok(None);
    }
    Ok(Some(format!(
        "refusing to remove worktree with unsaved work: {uncommitted} uncommitted change(s), \
         {ahead} commit(s) not on the base branch. Pass discard_changes=true to remove anyway, \
         or use action=\"keep\"."
    )))
}

/// Remove the worktree and its branch. Run from the main repo (never inside the
/// worktree). Refuses to discard unsaved work unless `discard`.
async fn remove(active: &ActiveWorktree, discard: bool) -> Result<(), String> {
    if !discard {
        if let Some(msg) = unsaved_work(active).await? {
            return Err(msg);
        }
    }
    let repo = &active.original_cwd;
    let path_str = active
        .path
        .to_str()
        .ok_or("worktree path is not valid UTF-8")?;
    let mut args = vec!["worktree", "remove"];
    if discard {
        args.push("--force");
    }
    args.push(path_str);
    git(repo, &args)
        .await
        .map_err(|e| format!("git worktree remove failed: {e}"))?;
    // Branch is unmerged by design; force-delete. Best-effort — the worktree is
    // already gone, which is the load-bearing part.
    let _ = git(repo, &["branch", "-D", &active.branch]).await;
    Ok(())
}

pub struct EnterWorktreeTool {
    cwd: SessionCwd,
    state: WorktreeState,
}

impl EnterWorktreeTool {
    pub fn new(cwd: SessionCwd, state: WorktreeState) -> Self {
        Self { cwd, state }
    }
}

#[async_trait]
impl StaticTool for EnterWorktreeTool {
    const NAME: &'static str = "enter_worktree";
    const DESCRIPTION: &'static str =
        "Create a fresh git worktree on a new branch and switch this session into it, so your \
         edits stay isolated from the user's working copy and land as a reviewable branch. Use \
         this as your FIRST action when asked to work in a worktree; edits made by relative path \
         after this go into the worktree. Call exit_worktree(keep|remove) when done.";
    const PARAMETERS: &'static [ToolParam] = &[
        ToolParam {
            name: "name",
            ty: "string",
            description:
                "Optional name for the worktree dir + branch (.ignis/worktrees/<name>). A short \
                 id is generated if omitted.",
        },
        ToolParam {
            name: "base_ref",
            ty: "string",
            description:
                "Branch base: \"head\" (default, from current HEAD) or \"fresh\" (from origin's \
                 default branch).",
        },
    ];
    const REQUIRED: &'static [&'static str] = &[];
    const EXECUTION_MODE: ExecutionMode = ExecutionMode::Sequential;

    async fn run(&self, args: serde_json::Value) -> ToolOutcome {
        if let Some(active) = self.state.lock().unwrap().as_ref() {
            return Err(format!(
                "already in a worktree session at {} (branch `{}`); call exit_worktree first",
                active.path.display(),
                active.branch
            ));
        }
        let name = args.get("name").and_then(|v| v.as_str());
        let base_ref = parse_base_ref(args.get("base_ref").and_then(|v| v.as_str()))?;

        let cwd = self.cwd.get();
        let active = create(&cwd, name, base_ref).await?;

        self.cwd.set(active.path.clone());
        let msg = format!(
            "Entered worktree at {} on new branch `{}` (isolated from {}). Edits now land here. \
             Call exit_worktree with action \"keep\" (review later) or \"remove\" (discard) when done.",
            active.path.display(),
            active.branch,
            active.original_cwd.display()
        );
        *self.state.lock().unwrap() = Some(active);
        Ok(msg)
    }
}

pub struct ExitWorktreeTool {
    cwd: SessionCwd,
    state: WorktreeState,
}

impl ExitWorktreeTool {
    pub fn new(cwd: SessionCwd, state: WorktreeState) -> Self {
        Self { cwd, state }
    }
}

#[async_trait]
impl StaticTool for ExitWorktreeTool {
    const NAME: &'static str = "exit_worktree";
    const DESCRIPTION: &'static str =
        "Leave the worktree entered with enter_worktree and return the session to the original \
         directory. action \"keep\" leaves the branch + worktree on disk to review later; \
         \"remove\" deletes them (refused if there is unsaved work unless discard_changes=true). \
         No-op if no worktree is active.";
    const PARAMETERS: &'static [ToolParam] = &[
        ToolParam {
            name: "action",
            ty: "string",
            description: "\"keep\" (leave intact) or \"remove\" (delete worktree + branch).",
        },
        ToolParam {
            name: "discard_changes",
            ty: "boolean",
            description:
                "Only with action=\"remove\": force-remove even with uncommitted changes or \
                 commits not on the base branch. Default false.",
        },
    ];
    const REQUIRED: &'static [&'static str] = &["action"];
    const EXECUTION_MODE: ExecutionMode = ExecutionMode::Sequential;

    async fn run(&self, args: serde_json::Value) -> ToolOutcome {
        let action = args.require_str("action")?;
        let active = self.state.lock().unwrap().clone();
        let Some(active) = active else {
            return Ok("No active worktree session; nothing to do.".to_string());
        };
        match action {
            "keep" => {
                self.cwd.set(active.original_cwd.clone());
                *self.state.lock().unwrap() = None;
                Ok(format!(
                    "Left worktree intact at {} on branch `{}`. Session returned to {}. Remove it \
                     later with `git worktree remove {}`.",
                    active.path.display(),
                    active.branch,
                    active.original_cwd.display(),
                    active.path.display()
                ))
            }
            "remove" => {
                let discard = args
                    .get("discard_changes")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                // On failure (e.g. unsaved work) leave state + cwd untouched so
                // the session stays in the worktree.
                remove(&active, discard).await?;
                self.cwd.set(active.original_cwd.clone());
                *self.state.lock().unwrap() = None;
                Ok(format!(
                    "Removed worktree {} and branch `{}`. Session returned to {}.",
                    active.path.display(),
                    active.branch,
                    active.original_cwd.display()
                ))
            }
            other => Err(format!(
                "invalid action '{other}': expected \"keep\" or \"remove\""
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::process::Command;

    fn sh(dir: &Path, args: &[&str]) {
        let out = Command::new(args[0])
            .args(&args[1..])
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "cmd {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A git repo with one commit on a known branch.
    fn init_repo() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path();
        sh(p, &["git", "init", "-q", "-b", "main"]);
        sh(p, &["git", "config", "user.email", "t@t.dev"]);
        sh(p, &["git", "config", "user.name", "Tester"]);
        std::fs::write(p.join("README.md"), "hi\n").unwrap();
        sh(p, &["git", "add", "."]);
        sh(p, &["git", "commit", "-qm", "init"]);
        tmp
    }

    fn tools(
        repo: &Path,
    ) -> (
        SessionCwd,
        WorktreeState,
        EnterWorktreeTool,
        ExitWorktreeTool,
    ) {
        let cwd = SessionCwd::from(repo);
        let state = new_state();
        let enter = EnterWorktreeTool::new(cwd.clone(), state.clone());
        let exit = ExitWorktreeTool::new(cwd.clone(), state.clone());
        (cwd, state, enter, exit)
    }

    #[tokio::test]
    async fn enter_creates_worktree_branch_and_switches_cwd() {
        let repo = init_repo();
        let (cwd, state, enter, _exit) = tools(repo.path());

        let msg = enter.run(json!({ "name": "feat-x" })).await.unwrap();
        assert!(msg.contains("feat-x"), "message names the branch: {msg}");

        let active = state.lock().unwrap().clone().unwrap();
        assert_eq!(active.branch, "feat-x");
        assert!(active.path.ends_with(".ignis/worktrees/feat-x"));
        assert!(active.path.exists(), "worktree dir created");
        // The whole toolset is now redirected into the worktree.
        assert_eq!(cwd.get(), active.path);
        // It really is a git branch.
        assert_eq!(
            super::git(&active.path, &["rev-parse", "--abbrev-ref", "HEAD"])
                .await
                .unwrap(),
            "feat-x"
        );
    }

    #[tokio::test]
    async fn exit_keep_leaves_worktree_and_restores_cwd() {
        let repo = init_repo();
        let (cwd, state, enter, exit) = tools(repo.path());
        enter.run(json!({ "name": "keepme" })).await.unwrap();
        let wt = state.lock().unwrap().clone().unwrap().path;

        let msg = exit.run(json!({ "action": "keep" })).await.unwrap();
        assert!(msg.contains("intact"), "{msg}");
        assert!(wt.exists(), "keep leaves the worktree on disk");
        assert_eq!(cwd.get(), repo.path(), "cwd restored to the repo root");
        assert!(state.lock().unwrap().is_none(), "session cleared");
    }

    #[tokio::test]
    async fn exit_remove_clean_deletes_worktree_and_branch() {
        let repo = init_repo();
        let (cwd, state, enter, exit) = tools(repo.path());
        enter.run(json!({ "name": "gone" })).await.unwrap();
        let wt = state.lock().unwrap().clone().unwrap().path;

        let msg = exit.run(json!({ "action": "remove" })).await.unwrap();
        assert!(msg.contains("Removed"), "{msg}");
        assert!(!wt.exists(), "remove deletes the worktree dir");
        assert_eq!(cwd.get(), repo.path());
        // Branch is gone too.
        let branches = super::git(repo.path(), &["branch", "--list", "gone"])
            .await
            .unwrap();
        assert!(branches.is_empty(), "branch deleted: {branches:?}");
    }

    #[tokio::test]
    async fn exit_remove_refuses_to_discard_uncommitted_work() {
        let repo = init_repo();
        let (cwd, state, enter, exit) = tools(repo.path());
        enter.run(json!({ "name": "dirty" })).await.unwrap();
        let wt = state.lock().unwrap().clone().unwrap().path;
        // Make an uncommitted change inside the worktree.
        std::fs::write(wt.join("new.txt"), "wip\n").unwrap();

        let err = exit.run(json!({ "action": "remove" })).await.unwrap_err();
        assert!(
            err.contains("uncommitted"),
            "must refuse to destroy work: {err}"
        );
        assert!(wt.exists(), "worktree left intact on refusal");
        assert_eq!(cwd.get(), wt, "still inside the worktree after refusal");
        assert!(state.lock().unwrap().is_some(), "session still active");

        // ...but discard_changes forces it through.
        let ok = exit
            .run(json!({ "action": "remove", "discard_changes": true }))
            .await
            .unwrap();
        assert!(ok.contains("Removed"));
        assert!(!wt.exists());
    }

    #[tokio::test]
    async fn exit_with_no_active_session_is_a_noop() {
        let repo = init_repo();
        let (_cwd, _state, _enter, exit) = tools(repo.path());
        let msg = exit.run(json!({ "action": "remove" })).await.unwrap();
        assert!(msg.contains("nothing to do"), "{msg}");
    }

    #[tokio::test]
    async fn enter_refuses_to_nest_in_an_existing_worktree() {
        let repo = init_repo();
        let (cwd, _state, enter, _exit) = tools(repo.path());
        enter.run(json!({ "name": "first" })).await.unwrap();
        // cwd is now inside the worktree. A fresh tool (no session state) must
        // still refuse via the git nesting guard.
        let fresh_state = new_state();
        let nested = EnterWorktreeTool::new(cwd.clone(), fresh_state);
        let err = nested.run(json!({ "name": "second" })).await.unwrap_err();
        assert!(
            err.contains("already inside a worktree"),
            "must not nest: {err}"
        );
    }

    #[tokio::test]
    async fn second_enter_in_same_session_is_refused() {
        let repo = init_repo();
        let (_cwd, _state, enter, _exit) = tools(repo.path());
        enter.run(json!({ "name": "one" })).await.unwrap();
        let err = enter.run(json!({ "name": "two" })).await.unwrap_err();
        assert!(err.contains("already in a worktree session"));
    }

    #[tokio::test]
    async fn enter_outside_a_git_repo_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let (_cwd, _state, enter, _exit) = tools(tmp.path());
        let err = enter.run(json!({})).await.unwrap_err();
        assert!(err.contains("not a git repository"), "{err}");
    }

    #[test]
    fn base_ref_parsing() {
        assert!(matches!(parse_base_ref(None), Ok(BaseRef::Head)));
        assert!(matches!(parse_base_ref(Some("head")), Ok(BaseRef::Head)));
        assert!(matches!(parse_base_ref(Some("fresh")), Ok(BaseRef::Fresh)));
        assert!(parse_base_ref(Some("nonsense")).is_err());
    }

    #[test]
    fn sanitize_makes_a_safe_component() {
        assert_eq!(sanitize("feat/login!").unwrap(), "feat-login");
        assert_eq!(sanitize("ok.name_1").unwrap(), "ok.name_1");
        assert!(sanitize("///").is_err());
    }
}
