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
    // musl, not gnu: the release pipeline ships a static musl build for Linux
    // (see `.github/workflows/release.yml`) so the binary runs on older glibc.
    // This MUST match the asset name or the download 404s.
    Some("x86_64-unknown-linux-musl")
} else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
    Some("x86_64-apple-darwin")
} else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
    Some("aarch64-apple-darwin")
} else {
    None
};

#[derive(Debug, Parser)]
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

pub async fn run(cmd: UpgradeCmd) -> Result<()> {
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

/// Resolve the latest release tag via the `/releases/latest` HTML redirect
/// instead of the JSON API. The API is rate-limited to 60 req/hr per IP for
/// unauthenticated callers, and shared IPs (WSL, corp NAT, CI) hit that wall
/// constantly; the redirect endpoint isn't subject to the same limit. reqwest
/// follows the 302 by default, so we just inspect the final URL's last path
/// segment — e.g. `…/releases/tag/v0.15.1` → `v0.15.1`.
async fn fetch_latest_tag() -> Result<String> {
    let url = format!("https://github.com/{}/releases/latest", REPO);
    let resp = reqwest::Client::new()
        .get(&url)
        .header(
            reqwest::header::USER_AGENT,
            format!("ignis-upgrade/{}", env!("CARGO_PKG_VERSION")),
        )
        .send()
        .await
        .context("fetch latest release")?
        .error_for_status()
        .context("github releases page")?;
    tag_from_release_url(resp.url().as_str())
}

/// Extract the tag from a `…/releases/tag/<tag>` URL. Returns an error rather
/// than the literal "latest" / an empty string if the redirect didn't land
/// on a tag page (e.g. the repo has no releases yet). Defensively strips any
/// trailing query string / fragment — GitHub doesn't add them today, but if
/// they ever do we don't want `"v1.0.0?ref=foo"` to land in the asset URL.
fn tag_from_release_url(url: &str) -> Result<String> {
    let last = url
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .split(['?', '#'])
        .next()
        .unwrap_or_default();
    if last.is_empty() || last == "latest" || last == "releases" {
        bail!("could not extract release tag from {url}");
    }
    Ok(last.to_string())
}

/// Retry transient failures (connection resets, timeouts, 5xx) but not
/// deterministic ones: a 4xx like 404 means a wrong asset URL and won't fix
/// itself, so retrying just delays the error 3×. `None` (no HTTP status) is a
/// connection/transport error — retry it.
fn is_retryable(status: Option<reqwest::StatusCode>) -> bool {
    status.is_none_or(|s| s.is_server_error())
}

async fn download(url: &str, dest: &Path) -> Result<()> {
    const MAX: u32 = 3;
    let client = reqwest::Client::new();
    // GitHub's release-asset CDN intermittently resets the TLS connection, so a
    // single-shot GET surfaces a transient blip as a hard failure. Retry the
    // transport before giving up; the next attempt almost always succeeds.
    for attempt in 1..=MAX {
        let fetch = async {
            client
                .get(url)
                .header(
                    reqwest::header::USER_AGENT,
                    format!("ignis-upgrade/{}", env!("CARGO_PKG_VERSION")),
                )
                .send()
                .await?
                .error_for_status()?
                .bytes()
                .await
        }
        .await;
        match fetch {
            Ok(bytes) => {
                return std::fs::write(dest, &bytes)
                    .with_context(|| format!("write {}", dest.display()));
            }
            Err(e) if attempt < MAX && is_retryable(e.status()) => {
                eprintln!("download attempt {attempt}/{MAX} failed: {e}; retrying in 2s…");
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
            Err(e) => return Err(e).with_context(|| format!("download {url}")),
        }
    }
    unreachable!("loop returns on the final attempt")
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
    fn tag_from_release_url_extracts_tag() {
        assert_eq!(
            tag_from_release_url("https://github.com/Fullstop000/ignis/releases/tag/v0.15.1")
                .unwrap(),
            "v0.15.1"
        );
        // Trailing slash is tolerated.
        assert_eq!(
            tag_from_release_url("https://github.com/Fullstop000/ignis/releases/tag/v0.15.1/")
                .unwrap(),
            "v0.15.1"
        );
    }

    #[test]
    fn tag_from_release_url_strips_query_and_fragment() {
        // Defensive: GitHub doesn't add these today, but our extractor
        // shouldn't slot them into the asset URL if it ever changes.
        assert_eq!(
            tag_from_release_url("https://github.com/x/y/releases/tag/v1.2.3?ref=foo").unwrap(),
            "v1.2.3"
        );
        assert_eq!(
            tag_from_release_url("https://github.com/x/y/releases/tag/v1.2.3#changelog").unwrap(),
            "v1.2.3"
        );
    }

    #[test]
    fn tag_from_release_url_errors_on_unredirected_or_missing_tag() {
        // The HTML redirect didn't fire (got back `…/releases/latest` itself).
        assert!(tag_from_release_url("https://github.com/x/y/releases/latest").is_err());
        // Empty repo with no releases — redirect lands on `…/releases`.
        assert!(tag_from_release_url("https://github.com/x/y/releases").is_err());
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
        // Pin the exact triple per host: `TARGET` becomes the asset filename, so
        // it must match `.github/workflows/release.yml` byte-for-byte or the
        // download 404s. Linux is musl (static build), NOT gnu — a weaker
        // "is it in a known set" check let that drift slip through once.
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        assert_eq!(TARGET, Some("x86_64-unknown-linux-musl"));
        #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
        assert_eq!(TARGET, Some("x86_64-apple-darwin"));
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        assert_eq!(TARGET, Some("aarch64-apple-darwin"));
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
    fn is_retryable_retries_transient_not_client_errors() {
        use reqwest::StatusCode;
        // Connection/transport error (no HTTP status) — the TLS-reset case.
        assert!(is_retryable(None));
        // 5xx is transient.
        assert!(is_retryable(Some(StatusCode::INTERNAL_SERVER_ERROR)));
        assert!(is_retryable(Some(StatusCode::SERVICE_UNAVAILABLE)));
        // 4xx is deterministic — a 404 (wrong asset URL) must fail fast, not
        // retry 3×.
        assert!(!is_retryable(Some(StatusCode::NOT_FOUND)));
        assert!(!is_retryable(Some(StatusCode::FORBIDDEN)));
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
