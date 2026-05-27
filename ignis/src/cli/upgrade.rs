//! `ignis upgrade` (alias `update`) — download the latest release tarball
//! that matches this build's target triple and atomically replace the running
//! binary at `current_exe()`. Mirrors `install.sh` but works without a shell.
//!
//! Scope (v1): Linux x86_64 + macOS x86_64/aarch64. The target triples come
//! straight from `.github/workflows/release.yml`; if the running build doesn't
//! match one of those, we refuse with a build-from-source pointer. Windows is
//! deliberately out of scope until the release pipeline ships an installer.
//!
//! Network: one HTTPS GET to `api.github.com/repos/<repo>/releases/latest`
//! (anonymous, User-Agent set), then one HTTPS GET for the tarball. Extraction
//! shells out to `tar -xzf`, which is on every supported platform — keeps us
//! out of an extra `tar`/`flate2` Rust dep.

use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;
use std::path::{Path, PathBuf};

const REPO: &str = "Fullstop000/ignis";

/// The release-artifact target triple for this build. `None` means we don't
/// ship a prebuilt binary for the host and `ignis upgrade` should refuse.
pub const TARGET: Option<&str> = if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
    Some("x86_64-unknown-linux-gnu")
} else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
    Some("x86_64-apple-darwin")
} else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
    Some("aarch64-apple-darwin")
} else {
    None
};

#[derive(Debug, Parser)]
#[command(name = "ignis upgrade", about = "Update ignis to the latest release")]
pub struct UpgradeCmd {
    /// Install a specific tag (e.g. `v0.14.1`) instead of the latest.
    #[arg(long)]
    pub version: Option<String>,
    /// Reinstall even when already at the target version.
    #[arg(long)]
    pub force: bool,
    /// Don't download — just report whether an update is available.
    #[arg(long)]
    pub check: bool,
}

pub async fn run(args: Vec<String>) -> Result<()> {
    // clap expects argv[0] to be the program name; the dispatcher in main.rs
    // strips the subcommand word before calling us, so prepend a synthetic one.
    let mut argv = vec!["ignis upgrade".to_string()];
    argv.extend(args);
    // `try_parse_from` returns Err for `--help`/`--version` instead of the
    // usual exit-0-after-print. Detect those kinds and exit cleanly so
    // `ignis upgrade --help` reads like every other CLI.
    let cmd = match UpgradeCmd::try_parse_from(argv) {
        Ok(c) => c,
        Err(e) => {
            e.print().ok();
            std::process::exit(match e.kind() {
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion => 0,
                _ => 2,
            });
        }
    };

    let target = TARGET.ok_or_else(|| {
        anyhow!(
            "no prebuilt binary for this host; build from source: https://github.com/{}",
            REPO
        )
    })?;

    let current = env!("CARGO_PKG_VERSION");

    // Only hit the GitHub releases API when we actually need the latest tag —
    // `--version vX.Y.Z` should work even when the API is rate-limited or down.
    let needs_latest = cmd.check || cmd.version.is_none();
    let latest_tag = if needs_latest {
        Some(fetch_latest_tag().await?)
    } else {
        None
    };

    if cmd.check {
        let latest_ver = strip_v(latest_tag.as_deref().unwrap());
        if version_lt(current, latest_ver) {
            println!("update available: {} → {}", current, latest_ver);
        } else {
            println!("ignis is up to date ({})", current);
        }
        return Ok(());
    }

    let desired_tag = cmd
        .version
        .clone()
        .unwrap_or_else(|| latest_tag.expect("latest_tag fetched when --version is unset"));
    let desired_ver = strip_v(&desired_tag).to_string();

    if !cmd.force && desired_ver == current {
        println!("Already at {} — pass --force to reinstall.", current);
        return Ok(());
    }

    let url = format!(
        "https://github.com/{}/releases/download/{}/ignis-{}-{}.tar.gz",
        REPO, desired_tag, desired_tag, target
    );

    let tmp = mkdtemp("ignis-upgrade")?;
    // Best-effort cleanup. We don't use the `tempfile` crate to avoid pulling
    // it into the runtime build (it's already a dev-dependency).
    let _guard = TmpDir(tmp.clone());

    let tarball = tmp.join("ignis.tar.gz");
    println!("Downloading {}", url);
    download(&url, &tarball).await?;

    extract_tar_gz(&tarball, &tmp)?;
    let extracted = tmp
        .join(format!("ignis-{}-{}", desired_tag, target))
        .join("ignis");
    if !extracted.is_file() {
        bail!(
            "tarball layout unexpected — expected {} after extract",
            extracted.display()
        );
    }

    let dest = std::env::current_exe().context("locate current executable")?;
    atomic_replace(&extracted, &dest)?;
    println!("ignis upgraded to {} at {}", desired_ver, dest.display());
    Ok(())
}

/// `GET /releases/latest` → `tag_name`. GitHub requires a User-Agent.
async fn fetch_latest_tag() -> Result<String> {
    let url = format!("https://api.github.com/repos/{}/releases/latest", REPO);
    let body: serde_json::Value = reqwest::Client::new()
        .get(&url)
        .header(
            reqwest::header::USER_AGENT,
            format!("ignis-upgrade/{}", env!("CARGO_PKG_VERSION")),
        )
        .send()
        .await
        .context("fetch latest release")?
        .error_for_status()
        .context("github releases API")?
        .json()
        .await
        .context("parse latest release JSON")?;
    body.get("tag_name")
        .and_then(|t| t.as_str())
        .map(String::from)
        .ok_or_else(|| anyhow!("releases API: tag_name missing"))
}

async fn download(url: &str, dest: &Path) -> Result<()> {
    let bytes = reqwest::Client::new()
        .get(url)
        .header(
            reqwest::header::USER_AGENT,
            format!("ignis-upgrade/{}", env!("CARGO_PKG_VERSION")),
        )
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .with_context(|| format!("HTTP error for {url}"))?
        .bytes()
        .await
        .with_context(|| format!("read body of {url}"))?;
    std::fs::write(dest, &bytes).with_context(|| format!("write {}", dest.display()))?;
    Ok(())
}

fn extract_tar_gz(tarball: &Path, into: &Path) -> Result<()> {
    let status = std::process::Command::new("tar")
        .arg("-xzf")
        .arg(tarball)
        .arg("-C")
        .arg(into)
        .status()
        .context("spawn tar")?;
    if !status.success() {
        bail!("tar -xzf failed (exit {:?})", status.code());
    }
    Ok(())
}

/// Copy `src` to a sibling of `dest` then `rename` over `dest`. The rename is
/// atomic on Unix and the kernel keeps the still-running process's text pages
/// mapped from the *old* inode, so this is safe to call against the running
/// `ignis` binary.
fn atomic_replace(src: &Path, dest: &Path) -> Result<()> {
    let dir = dest
        .parent()
        .ok_or_else(|| anyhow!("destination has no parent: {}", dest.display()))?;
    // Per-process suffix so two concurrent upgrades don't clobber each other's
    // staging file. The rename target is still `dest`, so the *result* is
    // last-writer-wins (fine for self-update).
    let tmp = dir.join(format!(".ignis.upgrade.{}.tmp", std::process::id()));
    std::fs::copy(src, &tmp).with_context(|| {
        format!(
            "copy new binary into {} (is the directory writable?)",
            dir.display()
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))
            .context("chmod 755 on new binary")?;
    }
    std::fs::rename(&tmp, dest).with_context(|| {
        format!(
            "replace {} (need write permission on its directory)",
            dest.display()
        )
    })?;
    Ok(())
}

fn strip_v(tag: &str) -> &str {
    tag.strip_prefix('v').unwrap_or(tag)
}

/// `current < other` by `MAJOR.MINOR.PATCH` numeric compare. Pre-release
/// suffixes (`-rc1` etc.) are ignored — we don't ship them yet, and a
/// semver dep would be a YAGNI add for one call site.
fn version_lt(current: &str, other: &str) -> bool {
    parse_semver(current) < parse_semver(other)
}

fn parse_semver(s: &str) -> (u32, u32, u32) {
    let core = s.split('-').next().unwrap_or(s);
    let mut it = core.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
    (
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
    )
}

/// Create a fresh empty directory under `std::env::temp_dir()` with a random
/// suffix. Inlined to avoid a `tempfile` runtime dep just for one call.
fn mkdtemp(prefix: &str) -> Result<PathBuf> {
    let base = std::env::temp_dir();
    for _ in 0..16 {
        let suffix: u64 = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
            ^ std::process::id() as u64;
        let path = base.join(format!("{prefix}-{suffix:x}"));
        match std::fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).context("create temp dir"),
        }
    }
    bail!(
        "could not create a unique temp dir under {}",
        base.display()
    );
}

struct TmpDir(PathBuf);
impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_v_handles_both_forms() {
        assert_eq!(strip_v("v0.14.1"), "0.14.1");
        assert_eq!(strip_v("0.14.1"), "0.14.1");
    }

    #[test]
    fn version_lt_compares_numerically() {
        assert!(version_lt("0.14.1", "0.14.2"));
        assert!(version_lt("0.14.1", "0.15.0"));
        assert!(version_lt("0.14.1", "1.0.0"));
        assert!(!version_lt("0.14.2", "0.14.1"));
        assert!(!version_lt("0.14.1", "0.14.1"));
        // String compare would say "0.9.0" > "0.14.1"; numeric must not.
        assert!(version_lt("0.9.0", "0.14.1"));
    }

    #[test]
    fn version_lt_ignores_prerelease_suffix() {
        // We don't ship pre-releases yet; suffix is stripped before compare.
        assert!(!version_lt("0.14.1-rc1", "0.14.1"));
        assert!(!version_lt("0.14.1", "0.14.1-rc1"));
    }

    #[test]
    fn target_matches_release_workflow_for_supported_hosts() {
        // If the host is one of the supported triples, the constant resolves;
        // otherwise it is `None`. Either way the constant exists and compiles.
        let known = [
            "x86_64-unknown-linux-gnu",
            "x86_64-apple-darwin",
            "aarch64-apple-darwin",
        ];
        if let Some(t) = TARGET {
            assert!(known.contains(&t), "unexpected target triple {t}");
        }
    }

    #[test]
    fn atomic_replace_swaps_file_contents() {
        let dir = crate::util::unique_temp_dir("ignis-upgrade-replace");
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("ignis");
        std::fs::write(&dest, b"old").unwrap();
        let src = dir.join("new-ignis");
        std::fs::write(&src, b"new contents").unwrap();

        atomic_replace(&src, &dest).unwrap();

        assert_eq!(std::fs::read(&dest).unwrap(), b"new contents");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o755);
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn upgrade_cmd_parses_flags() {
        let cmd = UpgradeCmd::try_parse_from(["ignis upgrade", "--version", "v0.14.1", "--force"])
            .unwrap();
        assert_eq!(cmd.version.as_deref(), Some("v0.14.1"));
        assert!(cmd.force);
        assert!(!cmd.check);

        let cmd = UpgradeCmd::try_parse_from(["ignis upgrade", "--check"]).unwrap();
        assert!(cmd.check);
        assert!(cmd.version.is_none());
    }
}
