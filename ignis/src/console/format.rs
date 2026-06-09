//! Pure formatters + sanitizers + small protocol enums shared across the
//! console module. Kept dependency-free (no ratatui, no `App`) so they're
//! easy to unit-test and to reuse from any submodule.

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum SelectionDirection {
    Previous,
    Next,
}

/// Requests the console runner sends down to the background agent task.
pub(crate) enum AgentRequest {
    Prompt {
        session_id: String,
        prompt: String,
    },
    Compact {
        session_id: String,
    },
    /// Switch the active provider/model/effort for subsequent prompts.
    SetModel {
        provider: String,
        model: String,
        effort: Option<String>,
    },
    /// Re-read `~/.ignis/config.toml` and replace the agent loop's in-memory
    /// `Config`. Used after `/connect` writes a fresh provider so the next
    /// prompt picks up the new `api_key` without a restart.
    ReloadConfig,
    /// Re-scan the skill roots and replace the runner's `SkillRegistry` clone.
    /// Sent when the user presses `r` in the `/skills` picker — the UI rebuilds
    /// `App.skills`, but the runner holds its own clone that would otherwise
    /// keep serving the stale skill set to the next prompt.
    ReloadSkills,
}

pub(crate) fn format_duration(ms: u128) -> String {
    if ms < 1000 {
        format!("{}ms", ms)
    } else {
        format!("{:.1}s", ms as f64 / 1000.0)
    }
}

/// Running-clock format for the live "Thinking…" timer: largest non-zero unit
/// down, seconds floored. `5s` · `1m 05s` · `1h 02m 05s`. (Tool durations keep
/// `format_duration`'s sub-second `Xms`/`X.Ys`.)
pub(crate) fn format_elapsed(ms: u128) -> String {
    let secs = (ms / 1000) as u64;
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}h {m:02}m {s:02}s")
    } else if m > 0 {
        format!("{m}m {s:02}s")
    } else {
        format!("{s}s")
    }
}

/// Human-friendly token count: `999`, `1.5k`, `120k`.
pub(crate) fn format_tokens(n: usize) -> String {
    if n < 1000 {
        n.to_string()
    } else {
        format!("{:.1}k", n as f64 / 1000.0)
    }
}

/// Compact context-window label: `128K`, `256K`, `1M`. Providers quote windows
/// in both binary (262144 = "256K") and decimal (200000 = "200K", 1000000 =
/// "1M") units, so prefer whichever lands on a clean number.
pub(crate) fn format_context(n: u64) -> String {
    const MIB: u64 = 1024 * 1024;
    if n != 0 && n.is_multiple_of(MIB) {
        format!("{}M", n / MIB)
    } else if n != 0 && n.is_multiple_of(1024) {
        format!("{}K", n / 1024) // binary, e.g. 262144 -> 256K
    } else if n >= 1_000_000 && n.is_multiple_of(1_000_000) {
        format!("{}M", n / 1_000_000)
    } else if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else {
        format!("{}K", (n as f64 / 1000.0).round() as u64) // decimal, e.g. 200000 -> 200K
    }
}

pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        // Take whole chars, never a byte slice — `&s[..max]` panics mid-codepoint.
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}

/// Make arbitrary text (tool output, file contents, pasted input) safe to feed
/// to ratatui: a literal `\t` desyncs layout (the terminal advances to a tab
/// stop, ratatui assumes width 1) and other control chars (CR, ANSI escapes)
/// corrupt the screen. Expand tabs to spaces and drop the rest.
pub(crate) fn sanitize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\t' => out.push_str("    "),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

pub(crate) fn next_selection(current: usize, len: usize, direction: SelectionDirection) -> usize {
    if len == 0 {
        return 0;
    }
    match direction {
        SelectionDirection::Previous => {
            if current == 0 {
                len - 1
            } else {
                current - 1
            }
        }
        SelectionDirection::Next => (current + 1) % len,
    }
}

#[cfg(test)]
mod tests {
    use super::format_elapsed;

    #[test]
    fn elapsed_drops_leading_zero_units() {
        assert_eq!(format_elapsed(0), "0s");
        assert_eq!(format_elapsed(999), "0s"); // sub-second floors to 0
        assert_eq!(format_elapsed(5_000), "5s");
        assert_eq!(format_elapsed(59_000), "59s");
        assert_eq!(format_elapsed(65_000), "1m 05s");
        assert_eq!(format_elapsed(600_000), "10m 00s");
        assert_eq!(format_elapsed(3_725_000), "1h 02m 05s"); // 1h 2m 5s
    }
}
