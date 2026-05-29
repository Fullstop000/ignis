//! User-declared permission rules — the *rule layer* that sits beneath the
//! `Mode` axis and above the per-tool default. Three lists (`deny`, `ask`,
//! `allow`) of `Tool(pattern)` strings, evaluated `deny > ask > allow`.
//!
//! Sources merge into one `RuleSet`: the hand-authored `[permissions]` block in
//! `config.toml` and the machine-written "always allow" grants in `state.json`
//! (grants fold into `allow`). The matcher's one real safety property is
//! compound-command segmenting: an `allow` rule for `bash(git *)` will *not*
//! green-light `git x && rm -rf y`, because every segment must be covered.

use serde_json::Value;

use super::{builtin, Decision};

/// Path-argument tools whose pattern globs over the `path` field (with anchors).
const PATH_TOOLS: &[&str] = &["edit_file", "create_file", "read_file"];

/// bash "always allow" arity table: how many leading tokens to keep before the
/// trailing ` *`. Multiword keys are checked longest-first, so `npm run build`
/// keeps 3 and a plain `npm install` keeps 2. Unlisted commands keep 1.
const BASH_ARITY: &[(&str, usize)] = &[
    ("kubectl rollout", 3),
    ("npm run", 3),
    ("gcloud", 3),
    ("git", 2),
    ("cargo", 2),
    ("npm", 2),
    ("docker", 2),
    ("kubectl", 2),
    ("go", 2),
    ("python", 2),
    ("pip", 2),
    ("yarn", 2),
    ("pnpm", 2),
    ("make", 2),
    ("gh", 2),
    ("aws", 2),
    ("terraform", 2),
    ("helm", 2),
    ("systemctl", 2),
    ("apt", 2),
    ("brew", 2),
];

/// One parsed rule: a tool name and a glob pattern over that tool's primary
/// argument. A bare `"bash"` parses to `pattern = "*"` (matches every use).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Rule {
    tool: String,
    pattern: String,
}

impl Rule {
    /// Parse `"bash(git *)"` → `Rule{tool:"bash", pattern:"git *"}`; a bare
    /// `"bash"` → `pattern:"*"`. Returns `None` for malformed input (no closing
    /// paren, empty tool name) so the caller can warn-and-skip.
    fn parse(s: &str) -> Option<Rule> {
        let s = s.trim();
        if s.is_empty() {
            return None;
        }
        match s.split_once('(') {
            Some((tool, rest)) => {
                let pattern = rest.strip_suffix(')')?;
                let tool = tool.trim();
                if tool.is_empty() {
                    return None;
                }
                Some(Rule {
                    tool: tool.to_string(),
                    pattern: pattern.to_string(),
                })
            }
            // Bare tool name → match every use.
            None => Some(Rule {
                tool: s.to_string(),
                pattern: "*".to_string(),
            }),
        }
    }
}

/// The three ordered rule lists. Evaluated `deny` → `ask` → `allow`.
#[derive(Clone, Debug, Default)]
pub struct RuleSet {
    deny: Vec<Rule>,
    ask: Vec<Rule>,
    allow: Vec<Rule>,
}

impl RuleSet {
    /// Build from the three config string lists. Unparseable entries are logged
    /// with `tracing::warn!` and skipped — a typo in one rule never crashes
    /// startup or silently disables the rest.
    pub fn from_strings(allow: &[String], ask: &[String], deny: &[String]) -> RuleSet {
        RuleSet {
            deny: parse_list(deny),
            ask: parse_list(ask),
            allow: parse_list(allow),
        }
    }

    /// Fold a runtime "always allow" grant (a `Tool(pattern)` string) into the
    /// live `allow` list. No-op on a malformed grant.
    pub fn add_grant(&mut self, grant: &str) {
        if let Some(rule) = Rule::parse(grant) {
            self.allow.push(rule);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.deny.is_empty() && self.ask.is_empty() && self.allow.is_empty()
    }

    /// Resolve a tool call against the rules: `deny` first (any match → `Deny`),
    /// then `ask` (any match → `Ask`), then `allow` (covered → `Allow`).
    /// `None` means no rule applied — the caller falls through to the rest of
    /// the pipeline.
    pub fn decide(&self, tool: &str, args: &Value) -> Option<Decision> {
        // deny / ask are liberal: any matching rule fires (for bash, a match on
        // *any* segment is enough — catch a hidden destructive segment anywhere).
        if let Some(r) = self.deny.iter().find(|r| r.matches_liberal(tool, args)) {
            return Some(Decision::deny(format!(
                "denied by permission rule `{}({})`",
                r.tool, r.pattern
            )));
        }
        if let Some(r) = self.ask.iter().find(|r| r.matches_liberal(tool, args)) {
            return Some(Decision::ask(format!(
                "permission rule asks before `{}({})`",
                r.tool, r.pattern
            )));
        }
        // allow is conservative for bash: every segment must be covered.
        if self.allow_covers(tool, args) {
            return Some(Decision::Allow);
        }
        None
    }

    /// `allow` semantics. For bash, *every* command segment must be covered by
    /// an allow rule or be read-only, and at least one segment must be matched
    /// by an actual rule (so an empty allow list never auto-allows). For other
    /// tools, a single matching allow rule is enough.
    fn allow_covers(&self, tool: &str, args: &Value) -> bool {
        if tool == "bash" {
            let bash_allows: Vec<&Rule> = self.allow.iter().filter(|r| r.tool == "bash").collect();
            if bash_allows.is_empty() {
                return false;
            }
            let Some(command) = args.get("command").and_then(|v| v.as_str()) else {
                return false;
            };
            let segments = builtin::split_command_segments(command);
            if segments.is_empty() {
                return false;
            }
            let mut hit_rule = false;
            for seg in &segments {
                if bash_allows.iter().any(|r| bash_glob(&r.pattern, seg)) {
                    hit_rule = true;
                } else if !builtin::is_read_only_bash(seg) {
                    return false; // an un-allowed, non-read-only segment
                }
            }
            hit_rule
        } else {
            self.allow.iter().any(|r| r.matches_liberal(tool, args))
        }
    }
}

impl Rule {
    /// Match used for deny/ask and for non-bash allow. For bash this matches if
    /// *any* segment matches the glob; for path tools it globs the `path` arg;
    /// for web_fetch it matches the host; otherwise it's a tool-name match.
    fn matches_liberal(&self, tool: &str, args: &Value) -> bool {
        if self.tool != tool {
            return false;
        }
        if tool == "bash" {
            let Some(command) = args.get("command").and_then(|v| v.as_str()) else {
                return false;
            };
            return builtin::split_command_segments(command)
                .iter()
                .any(|seg| bash_glob(&self.pattern, seg));
        }
        if PATH_TOOLS.contains(&tool) {
            let Some(path) = args.get("path").and_then(|v| v.as_str()) else {
                return false;
            };
            return path_glob(&self.pattern, path);
        }
        if tool == "web_fetch" {
            let Some(host_pat) = self.pattern.strip_prefix("domain:") else {
                return false;
            };
            let Some(url) = args.get("url").and_then(|v| v.as_str()) else {
                return false;
            };
            return match extract_host(url) {
                Some(host) => simple_glob(host_pat, &host),
                None => false,
            };
        }
        // Any other tool (agent, mcp__*, …): the only meaningful pattern is `*`.
        self.pattern == "*"
    }
}

fn parse_list(items: &[String]) -> Vec<Rule> {
    items
        .iter()
        .filter_map(|s| match Rule::parse(s) {
            Some(r) => Some(r),
            None => {
                tracing::warn!(rule = %s, "skipping malformed [permissions] rule");
                None
            }
        })
        .collect()
}

/// Derive the `Tool(pattern)` string an "always allow" click should persist.
/// bash → arity-trimmed prefix + ` *`; path tools → the concrete path; web_fetch
/// → `domain:HOST`; everything else → the bare tool name. `None` when the args
/// lack the field the suggestion needs (caller falls back to the bare name).
pub fn suggest_grant(tool: &str, args: &Value) -> Option<String> {
    if tool == "bash" {
        let command = args.get("command").and_then(|v| v.as_str())?;
        let tokens: Vec<&str> = command.split_whitespace().collect();
        if tokens.is_empty() {
            return None;
        }
        let keep = bash_arity(&tokens).min(tokens.len());
        let prefix = tokens[..keep].join(" ");
        return Some(format!("bash({prefix} *)"));
    }
    if PATH_TOOLS.contains(&tool) {
        let path = args.get("path").and_then(|v| v.as_str())?;
        return Some(format!("{tool}({path})"));
    }
    if tool == "web_fetch" {
        let url = args.get("url").and_then(|v| v.as_str())?;
        let host = extract_host(url)?;
        return Some(format!("web_fetch(domain:{host})"));
    }
    Some(tool.to_string())
}

/// Longest-prefix lookup in `BASH_ARITY`; defaults to 1 (keep the command name).
fn bash_arity(tokens: &[&str]) -> usize {
    let mut best: Option<(usize, usize)> = None; // (key_len_in_tokens, arity)
    for (key, arity) in BASH_ARITY {
        let kw: Vec<&str> = key.split(' ').collect();
        if tokens.len() >= kw.len()
            && tokens[..kw.len()] == kw[..]
            && best.is_none_or(|(bl, _)| kw.len() > bl)
        {
            best = Some((kw.len(), *arity));
        }
    }
    best.map(|(_, a)| a).unwrap_or(1)
}

/// bash glob: `*` → `.*` (any run incl. spaces), anchored `^…$`. A trailing
/// ` *` becomes optional (`( .*)?`) so `git *` also matches a bare `git`.
fn bash_glob(pattern: &str, segment: &str) -> bool {
    let (base, optional_tail) = match pattern.strip_suffix(" *") {
        Some(b) => (b, true),
        None => (pattern, false),
    };
    let mut re = String::from("^");
    re.push_str(&escape_with_star(base));
    if optional_tail {
        re.push_str("( .*)?");
    }
    re.push('$');
    regex_matches(&re, segment.trim())
}

/// Host glob for `domain:` patterns: `*` → `.*`, anchored.
fn simple_glob(pattern: &str, candidate: &str) -> bool {
    let re = format!("^{}$", escape_with_star(pattern));
    regex_matches(&re, candidate)
}

/// Path glob with anchors. `**/` → zero-or-more dirs, `**` → any run, `*` →
/// any run *within* a segment (no `/`). Anchors apply after normalizing the
/// pattern: `~/` expands to `$HOME`, `//x` is absolute, `/x` is project-root
/// relative (leading slash dropped), a bare filename becomes `**/<name>`.
fn path_glob(pattern: &str, path: &str) -> bool {
    let normalized_pattern = normalize_path_pattern(pattern);
    let candidate = path.trim().trim_start_matches("./");
    let re = format!("^{}$", path_pattern_to_regex(&normalized_pattern));
    regex_matches(&re, candidate)
}

fn normalize_path_pattern(pattern: &str) -> String {
    let p = pattern.trim();
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return format!("{}/{}", home.display(), rest);
        }
        return p.to_string();
    }
    if let Some(rest) = p.strip_prefix("//") {
        return format!("/{rest}"); // absolute
    }
    if let Some(rest) = p.strip_prefix('/') {
        return rest.to_string(); // project-root relative
    }
    if let Some(rest) = p.strip_prefix("./") {
        return rest.to_string();
    }
    if !p.contains('/') {
        return format!("**/{p}"); // bare filename → recursive
    }
    p.to_string()
}

/// Translate a normalized path pattern into a regex body (no anchors).
fn path_pattern_to_regex(pattern: &str) -> String {
    let chars: Vec<char> = pattern.chars().collect();
    let mut re = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '*' {
            if i + 1 < chars.len() && chars[i + 1] == '*' {
                if i + 2 < chars.len() && chars[i + 2] == '/' {
                    re.push_str("(?:.*/)?"); // `**/` → zero or more dirs
                    i += 3;
                } else {
                    re.push_str(".*"); // trailing `**`
                    i += 2;
                }
            } else {
                re.push_str("[^/]*"); // single `*` stays within a segment
                i += 1;
            }
        } else {
            push_escaped(&mut re, c);
            i += 1;
        }
    }
    re
}

/// Escape a glob into a regex body, translating `*` → `.*`. Used for bash and
/// host globs; path globbing uses `path_pattern_to_regex` instead (`**` vs `*`).
fn escape_with_star(glob: &str) -> String {
    let mut re = String::new();
    for c in glob.chars() {
        if c == '*' {
            re.push_str(".*");
        } else {
            push_escaped(&mut re, c);
        }
    }
    re
}

fn push_escaped(re: &mut String, c: char) {
    if "\\.+()[]{}^$|?".contains(c) {
        re.push('\\');
    }
    re.push(c);
}

fn regex_matches(re: &str, candidate: &str) -> bool {
    match regex::Regex::new(re) {
        Ok(rx) => rx.is_match(candidate),
        Err(e) => {
            tracing::warn!(regex = %re, error = %e, "permission rule produced an invalid regex");
            false
        }
    }
}

/// Pull the host out of an http(s) URL: strip the scheme, take everything up to
/// the first `/`, `:` (port) or `?`.
fn extract_host(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let host: String = rest
        .chars()
        .take_while(|c| !matches!(c, '/' | ':' | '?'))
        .collect();
    if host.is_empty() {
        None
    } else {
        Some(host)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rs(allow: &[&str], ask: &[&str], deny: &[&str]) -> RuleSet {
        let conv = |xs: &[&str]| xs.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        RuleSet::from_strings(&conv(allow), &conv(ask), &conv(deny))
    }

    // -------- parse --------

    #[test]
    fn parse_tool_and_pattern() {
        let r = Rule::parse("bash(git *)").unwrap();
        assert_eq!(r.tool, "bash");
        assert_eq!(r.pattern, "git *");
    }

    #[test]
    fn parse_bare_name_is_star() {
        let r = Rule::parse("bash").unwrap();
        assert_eq!(r.tool, "bash");
        assert_eq!(r.pattern, "*");
    }

    #[test]
    fn parse_trims_whitespace() {
        let r = Rule::parse("  edit_file(src/**)  ").unwrap();
        assert_eq!(r.tool, "edit_file");
        assert_eq!(r.pattern, "src/**");
    }

    #[test]
    fn parse_rejects_malformed() {
        assert!(Rule::parse("bash(").is_none());
        assert!(Rule::parse("(x)").is_none());
        assert!(Rule::parse("").is_none());
        assert!(Rule::parse("   ").is_none());
    }

    #[test]
    fn from_strings_skips_malformed_keeps_valid() {
        let set = rs(&["bash(git *)", "garbage(", ""], &[], &[]);
        // The valid rule still works despite the junk entries.
        assert_eq!(
            set.decide("bash", &json!({"command": "git status"})),
            Some(Decision::Allow)
        );
    }

    // -------- bash allow (compound-segment safety) --------

    #[test]
    fn bash_allow_matches_glob() {
        let set = rs(&["bash(git *)"], &[], &[]);
        assert_eq!(
            set.decide("bash", &json!({"command": "git push origin main"})),
            Some(Decision::Allow)
        );
    }

    #[test]
    fn bash_allow_trailing_star_covers_bare_command() {
        let set = rs(&["bash(git *)"], &[], &[]);
        assert_eq!(
            set.decide("bash", &json!({"command": "git"})),
            Some(Decision::Allow)
        );
    }

    #[test]
    fn bash_allow_does_not_green_light_unallowed_segment() {
        // The core safety win: `git *` allowed, but a chained `rm -rf y` is not.
        let set = rs(&["bash(git *)"], &[], &[]);
        assert_eq!(
            set.decide("bash", &json!({"command": "git x && rm -rf y"})),
            None
        );
    }

    #[test]
    fn bash_allow_permits_read_only_second_segment() {
        let set = rs(&["bash(git *)"], &[], &[]);
        assert_eq!(
            set.decide("bash", &json!({"command": "git status && ls"})),
            Some(Decision::Allow)
        );
    }

    #[test]
    fn bash_allow_requires_a_rule_hit() {
        // No bash allow rule → never an Allow even for an innocent command.
        let set = rs(&[], &[], &[]);
        assert_eq!(set.decide("bash", &json!({"command": "git status"})), None);
    }

    // -------- bash deny / ask (liberal: any segment) --------

    #[test]
    fn bash_deny_matches_any_segment() {
        let set = rs(&[], &[], &["bash(rm -rf *)"]);
        assert!(matches!(
            set.decide("bash", &json!({"command": "ls && rm -rf foo"})),
            Some(Decision::Deny { .. })
        ));
    }

    #[test]
    fn bash_deny_ignores_unrelated_command() {
        let set = rs(&[], &[], &["bash(rm -rf *)"]);
        assert_eq!(set.decide("bash", &json!({"command": "ls -la"})), None);
    }

    // -------- precedence --------

    #[test]
    fn deny_beats_allow() {
        let set = rs(&["bash(git *)"], &[], &["bash(git *)"]);
        assert!(matches!(
            set.decide("bash", &json!({"command": "git status"})),
            Some(Decision::Deny { .. })
        ));
    }

    #[test]
    fn ask_beats_allow() {
        let set = rs(&["bash(git *)"], &["bash(git push *)"], &[]);
        assert!(matches!(
            set.decide("bash", &json!({"command": "git push origin"})),
            Some(Decision::Ask { .. })
        ));
        // A non-push git command still falls under allow.
        assert_eq!(
            set.decide("bash", &json!({"command": "git status"})),
            Some(Decision::Allow)
        );
    }

    // -------- path anchors --------

    #[test]
    fn path_double_star_crosses_slashes() {
        let set = rs(&["edit_file(src/**)"], &[], &[]);
        assert_eq!(
            set.decide("edit_file", &json!({"path": "src/a/b.rs"})),
            Some(Decision::Allow)
        );
    }

    #[test]
    fn path_project_root_anchor_strips_leading_slash() {
        let set = rs(&["edit_file(/src/**)"], &[], &[]);
        assert_eq!(
            set.decide("edit_file", &json!({"path": "src/main.rs"})),
            Some(Decision::Allow)
        );
    }

    #[test]
    fn path_single_star_does_not_cross_slash() {
        let set = rs(&["edit_file(src/*.rs)"], &[], &[]);
        assert_eq!(
            set.decide("edit_file", &json!({"path": "src/main.rs"})),
            Some(Decision::Allow)
        );
        assert_eq!(
            set.decide("edit_file", &json!({"path": "src/a/b.rs"})),
            None
        );
    }

    #[test]
    fn bare_filename_is_recursive() {
        let set = rs(&[], &[], &["read_file(.env)"]);
        assert!(matches!(
            set.decide("read_file", &json!({"path": ".env"})),
            Some(Decision::Deny { .. })
        ));
        assert!(matches!(
            set.decide("read_file", &json!({"path": "config/.env"})),
            Some(Decision::Deny { .. })
        ));
        assert_eq!(set.decide("read_file", &json!({"path": "foo.txt"})), None);
    }

    #[test]
    fn double_star_secrets_dir_matches_anywhere() {
        let set = rs(&[], &[], &["read_file(**/secrets/**)"]);
        assert!(matches!(
            set.decide("read_file", &json!({"path": "a/b/secrets/key.pem"})),
            Some(Decision::Deny { .. })
        ));
        assert!(matches!(
            set.decide("read_file", &json!({"path": "secrets/key.pem"})),
            Some(Decision::Deny { .. })
        ));
    }

    #[test]
    fn home_anchor_expands_tilde() {
        let Some(home) = dirs::home_dir() else {
            return;
        };
        let set = rs(&["read_file(~/.zshrc)"], &[], &[]);
        let path = format!("{}/.zshrc", home.display());
        assert_eq!(
            set.decide("read_file", &json!({ "path": path })),
            Some(Decision::Allow)
        );
    }

    // -------- domain --------

    #[test]
    fn domain_glob_matches_subdomain() {
        let set = rs(&["web_fetch(domain:*.github.com)"], &[], &[]);
        assert_eq!(
            set.decide("web_fetch", &json!({"url": "https://api.github.com/x"})),
            Some(Decision::Allow)
        );
        assert_eq!(
            set.decide("web_fetch", &json!({"url": "https://evil.com/x"})),
            None
        );
    }

    #[test]
    fn domain_wildcard_matches_all_hosts() {
        let set = rs(&["web_fetch(domain:*)"], &[], &[]);
        assert_eq!(
            set.decide("web_fetch", &json!({"url": "http://anything.example/y"})),
            Some(Decision::Allow)
        );
    }

    // -------- bare tool-name rules --------

    #[test]
    fn bare_tool_deny_blocks_every_use() {
        let set = rs(&[], &[], &["agent"]);
        assert!(matches!(
            set.decide("agent", &json!({"task": "x"})),
            Some(Decision::Deny { .. })
        ));
    }

    #[test]
    fn bare_tool_allow_permits_every_use() {
        let set = rs(&["read_file"], &[], &[]);
        assert_eq!(
            set.decide("read_file", &json!({"path": "anything/at/all"})),
            Some(Decision::Allow)
        );
    }

    // -------- grants fold into allow --------

    #[test]
    fn grant_folds_into_allow() {
        let mut set = rs(&[], &[], &[]);
        set.add_grant("bash(git status *)");
        assert_eq!(
            set.decide("bash", &json!({"command": "git status -s"})),
            Some(Decision::Allow)
        );
    }

    // -------- suggest_grant (arity) --------

    #[test]
    fn suggest_bash_arity_trims_to_table() {
        assert_eq!(
            suggest_grant("bash", &json!({"command": "git status --porcelain"})),
            Some("bash(git status *)".to_string())
        );
        assert_eq!(
            suggest_grant("bash", &json!({"command": "npm run build --watch"})),
            Some("bash(npm run build *)".to_string())
        );
        assert_eq!(
            suggest_grant("bash", &json!({"command": "gcloud compute instances list"})),
            Some("bash(gcloud compute instances *)".to_string())
        );
        assert_eq!(
            suggest_grant("bash", &json!({"command": "ls -la"})),
            Some("bash(ls *)".to_string())
        );
    }

    #[test]
    fn suggest_path_uses_concrete_path() {
        assert_eq!(
            suggest_grant("edit_file", &json!({"path": "src/main.rs"})),
            Some("edit_file(src/main.rs)".to_string())
        );
    }

    #[test]
    fn suggest_web_fetch_uses_host() {
        assert_eq!(
            suggest_grant("web_fetch", &json!({"url": "https://api.github.com/x"})),
            Some("web_fetch(domain:api.github.com)".to_string())
        );
    }

    #[test]
    fn suggest_other_tool_is_bare_name() {
        assert_eq!(
            suggest_grant("agent", &json!({})),
            Some("agent".to_string())
        );
    }

    #[test]
    fn suggest_returns_none_without_required_arg() {
        assert_eq!(suggest_grant("bash", &json!({})), None);
        assert_eq!(suggest_grant("edit_file", &json!({})), None);
    }
}
