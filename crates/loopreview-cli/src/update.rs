//! `lr update`: self-update from GitHub Releases.
//!
//! Checks the project's latest GitHub release (tags `v*`, prereleases ignored),
//! compares its version against this build's [`CARGO_PKG_VERSION`], and — when a
//! newer one exists — downloads the archive built for the running platform,
//! verifies it against `checksums.txt` (sha256; a mismatch aborts), and replaces
//! both the `loopreview` and `lr` binaries in place.
//!
//! Networking prefers the `gh` CLI (`gh release download`) when it is installed;
//! otherwise it fetches the public release assets directly over HTTPS with
//! [`ureq`] (rustls). No authentication is used or needed — releases are public
//! and every download is checksum-verified.
//!
//! The replacement is platform-specific and deliberate:
//!
//! * **Unix** writes the new binary to a temp file beside the target, marks it
//!   executable, then `rename`s it over the old one. The running process keeps
//!   its old inode, so overwriting the file in place (which macOS answers with a
//!   `SIGKILL`) never happens.
//! * **Windows** cannot replace a running `.exe`, so the current one is renamed
//!   to `<name>.old` and the new one dropped in its place; the stale `.old` is
//!   swept on the next launch (see [`cleanup_stale_windows`]).
//!
//! The asset naming ([`asset_name`]) and target matrix ([`resolve_target`]) are
//! the single source of truth mirrored from `.github/workflows/release.yml`.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;

/// The GitHub repository releases are published to (`owner/repo`).
const REPO: &str = "loopkeep/loopreview";

/// The checksums manifest published alongside the archives.
const CHECKSUMS: &str = "checksums.txt";

/// `User-Agent` for direct API/asset requests (GitHub's API requires one).
const USER_AGENT: &str = concat!("loopreview/", env!("CARGO_PKG_VERSION"));

/// Upper bound on a single download, a guard against a runaway response. Release
/// archives are a few megabytes; this leaves ample headroom.
const MAX_DOWNLOAD: u64 = 1024 * 1024 * 1024;

/// The binaries an archive carries and this tool replaces, in the running
/// platform's naming. Both live side by side in the install directory.
#[cfg(windows)]
pub(crate) const BIN_NAMES: &[&str] = &["loopreview.exe", "lr.exe"];
/// The binaries an archive carries and this tool replaces.
#[cfg(not(windows))]
pub(crate) const BIN_NAMES: &[&str] = &["loopreview", "lr"];

/// Run `lr update` (or, with `check_only`, `lr update --check`).
pub fn run(check_only: bool) -> Result<()> {
    let current = env!("CARGO_PKG_VERSION");
    let target = current_target().ok_or_else(|| {
        anyhow!(
            "no prebuilt release exists for this platform ({} {}). Build from source with \
             `cargo install --path .` instead.",
            std::env::consts::ARCH,
            std::env::consts::OS,
        )
    })?;

    eprintln!("checking for updates (current v{current})…");
    let (mut backend, release) = resolve_release()?;
    let latest = release.tag_name.trim_start_matches('v').to_string();

    if !is_newer(current, &latest)? {
        println!("loopreview v{current} is already up to date.");
        return Ok(());
    }

    if check_only {
        println!("update available: v{current} → v{latest}");
        println!("run `lr update` to install it.");
        return Ok(());
    }

    let asset = asset_name(&latest, target);
    let work = TempDir::new()?;
    eprintln!("downloading v{latest} for {target}…");
    backend.download(&release, &asset, work.path())?;

    let archive_path = work.path().join(&asset);
    let checksums_path = work.path().join(CHECKSUMS);
    if !archive_path.exists() {
        bail!(
            "the release v{latest} has no asset named {asset} — this platform may not have been \
             built for that version yet."
        );
    }
    if !checksums_path.exists() {
        bail!("the release v{latest} is missing {CHECKSUMS}; refusing to install unverified.");
    }

    eprintln!("verifying checksum…");
    verify(&archive_path, &asset, &checksums_path)?;

    extract_binaries(&archive_path, work.path())
        .with_context(|| format!("extracting {}", archive_path.display()))?;

    let install_dir = install_dir()?;
    install_all(work.path(), &install_dir)?;

    println!("updated v{current} → v{latest}");
    Ok(())
}

/// On Windows, remove any `<name>.old` binary a previous update left behind.
///
/// Called once at startup (a no-op on every other platform). Best-effort: a
/// still-locked `.old` is simply left for the next launch.
pub fn cleanup_stale_windows() {
    #[cfg(windows)]
    {
        if let Ok(exe) = std::env::current_exe()
            && let Some(dir) = exe.parent()
        {
            for name in BIN_NAMES {
                let _ = fs::remove_file(dir.join(format!("{name}.old")));
            }
        }
    }
}

// -- version + naming (mirrors release.yml) --------------------------------

/// The release target triple for the running platform, or `None` when no
/// prebuilt archive is published for it.
fn current_target() -> Option<&'static str> {
    resolve_target(std::env::consts::ARCH, std::env::consts::OS)
}

/// Map a Rust `(arch, os)` pair to the release target triple. The five arms are
/// exactly `release.yml`'s build matrix; anything else has no prebuilt archive.
fn resolve_target(arch: &str, os: &str) -> Option<&'static str> {
    let triple = match (arch, os) {
        ("aarch64", "macos") => "aarch64-apple-darwin",
        ("x86_64", "macos") => "x86_64-apple-darwin",
        ("x86_64", "linux") => "x86_64-unknown-linux-gnu",
        ("aarch64", "linux") => "aarch64-unknown-linux-gnu",
        ("x86_64", "windows") => "x86_64-pc-windows-msvc",
        _ => return None,
    };
    Some(triple)
}

/// The release asset name for `version` and `target` — `.zip` on Windows,
/// `.tar.gz` elsewhere. Mirrors the naming block in `release.yml`.
fn asset_name(version: &str, target: &str) -> String {
    let ext = if is_windows_target(target) {
        "zip"
    } else {
        "tar.gz"
    };
    format!("loopreview-{version}-{target}.{ext}")
}

/// Whether a target triple names a Windows build (which ships a `.zip`).
fn is_windows_target(target: &str) -> bool {
    target.contains("windows")
}

/// True when `latest` (a semver, with or without a leading `v`) is strictly
/// newer than `current`.
fn is_newer(current: &str, latest: &str) -> Result<bool> {
    let current = semver::Version::parse(current.trim_start_matches('v'))
        .with_context(|| format!("parsing the current version {current:?}"))?;
    let latest = semver::Version::parse(latest.trim_start_matches('v'))
        .with_context(|| format!("parsing the release version {latest:?}"))?;
    Ok(latest > current)
}

// -- release metadata ------------------------------------------------------

/// A GitHub release, pared down to the fields an update needs.
#[derive(Debug, Deserialize)]
struct Release {
    /// The git tag (`v<version>`).
    tag_name: String,
    /// The published files (archives + `checksums.txt`).
    #[serde(default)]
    assets: Vec<Asset>,
}

/// One published file in a release.
#[derive(Debug, Deserialize)]
struct Asset {
    /// The file name (e.g. `loopreview-0.2.0-aarch64-apple-darwin.tar.gz`).
    name: String,
    /// The direct download URL (used only on the `ureq` path).
    browser_download_url: String,
}

impl Release {
    /// The download URL for the asset called `name`.
    fn asset_url(&self, name: &str) -> Result<&str> {
        self.assets
            .iter()
            .find(|a| a.name == name)
            .map(|a| a.browser_download_url.as_str())
            .ok_or_else(|| anyhow!("the release has no asset named {name}"))
    }
}

/// Parse the JSON GitHub returns for a release into [`Release`].
fn parse_release(json: &str) -> Result<Release> {
    serde_json::from_str(json).context("parsing the GitHub release metadata")
}

/// Which mechanism fetches releases: the `gh` CLI, or direct HTTPS.
enum Backend {
    /// The GitHub CLI (`gh`), preferred when installed.
    Gh,
    /// Direct public HTTPS via `ureq` (no authentication).
    Http,
}

/// Pick a backend and fetch the latest release. `gh` is used when present, but a
/// `gh` failure (e.g. not authenticated) falls back to direct HTTPS — release
/// reads are public and need no token.
fn resolve_release() -> Result<(Backend, Release)> {
    if have_gh() {
        match Backend::Gh.latest_release() {
            Ok(release) => return Ok((Backend::Gh, release)),
            Err(err) => {
                eprintln!(
                    "note: `gh` could not read the release ({err:#}); \
                     falling back to a direct download."
                );
            }
        }
    }
    let release = Backend::Http.latest_release()?;
    Ok((Backend::Http, release))
}

impl Backend {
    /// Fetch the latest (non-prerelease) release's metadata.
    fn latest_release(&self) -> Result<Release> {
        let path = format!("repos/{REPO}/releases/latest");
        let json = match self {
            Backend::Gh => gh_api(&path)?,
            Backend::Http => http_get_string(&format!("https://api.github.com/{path}"))?,
        };
        parse_release(&json)
    }

    /// Download `asset` and [`CHECKSUMS`] into `dir`.
    fn download(&mut self, release: &Release, asset: &str, dir: &Path) -> Result<()> {
        match self {
            Backend::Gh => gh_download(&release.tag_name, asset, dir),
            Backend::Http => {
                for name in [asset, CHECKSUMS] {
                    let url = release.asset_url(name)?;
                    http_download(url, name, &dir.join(name))?;
                }
                Ok(())
            }
        }
    }
}

// -- `gh` subprocess -------------------------------------------------------

/// Whether the `gh` CLI is installed and runnable.
fn have_gh() -> bool {
    Command::new("gh")
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// `gh api <path>`, returning stdout on success.
fn gh_api(path: &str) -> Result<String> {
    let out = Command::new("gh")
        .args(["api", path, "-H", "Accept: application/vnd.github+json"])
        .stdin(Stdio::null())
        .output()
        .context("running `gh api`")?;
    if !out.status.success() {
        bail!(
            "`gh api {path}` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `gh release download <tag> --pattern <asset> --pattern checksums.txt`, into
/// `dir`. `gh`'s own progress is shown on the inherited stderr.
fn gh_download(tag: &str, asset: &str, dir: &Path) -> Result<()> {
    let status = Command::new("gh")
        .args(["release", "download", tag, "--repo", REPO, "--dir"])
        .arg(dir)
        .args(["--clobber", "--pattern", asset, "--pattern", CHECKSUMS])
        .stdin(Stdio::null())
        .status()
        .context("running `gh release download`")?;
    if !status.success() {
        bail!("`gh release download {tag}` failed");
    }
    Ok(())
}

// -- direct HTTPS (ureq / rustls) ------------------------------------------

/// GET `url` and return its body as a string (for small JSON responses).
fn http_get_string(url: &str) -> Result<String> {
    let mut resp = ureq::get(url)
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/vnd.github+json")
        .call()
        .with_context(|| format!("requesting {url}"))?;
    resp.body_mut()
        .with_config()
        .limit(MAX_DOWNLOAD)
        .read_to_string()
        .with_context(|| format!("reading the response from {url}"))
}

/// Stream `url` to `dest`, drawing a byte-count progress line on stderr.
fn http_download(url: &str, label: &str, dest: &Path) -> Result<()> {
    let mut resp = ureq::get(url)
        .header("User-Agent", USER_AGENT)
        .call()
        .with_context(|| format!("requesting {url}"))?;
    let total = resp.body().content_length();
    let mut reader = resp.body_mut().with_config().limit(MAX_DOWNLOAD).reader();
    let mut file = File::create(dest).with_context(|| format!("creating {}", dest.display()))?;

    let mut buf = [0u8; 64 * 1024];
    let mut done: u64 = 0;
    let mut last = Instant::now();
    loop {
        let n = reader
            .read(&mut buf)
            .with_context(|| format!("downloading {label}"))?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])
            .with_context(|| format!("writing {}", dest.display()))?;
        done += n as u64;
        if last.elapsed() >= Duration::from_millis(100) {
            draw_progress(label, done, total);
            last = Instant::now();
        }
    }
    draw_progress(label, done, total);
    eprintln!();
    Ok(())
}

/// Redraw the single-line download progress on stderr.
fn draw_progress(label: &str, done: u64, total: Option<u64>) {
    match total {
        Some(total) if total > 0 => {
            let pct = (done as f64 / total as f64 * 100.0).min(100.0);
            eprint!(
                "\r  {label}  {pct:5.1}%  {} / {}    ",
                human_bytes(done),
                human_bytes(total)
            );
        }
        _ => eprint!("\r  {label}  {}    ", human_bytes(done)),
    }
    let _ = io::stderr().flush();
}

/// A compact human-readable byte size (`1.5 MiB`).
fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

// -- checksum verification -------------------------------------------------

/// Verify `archive` against its entry in the `checksums.txt` at `checksums_path`.
/// A missing entry or a mismatch is a hard error — the install must not proceed.
fn verify(archive: &Path, asset: &str, checksums_path: &Path) -> Result<()> {
    let manifest = fs::read_to_string(checksums_path)
        .with_context(|| format!("reading {}", checksums_path.display()))?;
    let expected = parse_checksums(&manifest)
        .remove(asset)
        .ok_or_else(|| anyhow!("{CHECKSUMS} has no entry for {asset}"))?;
    let bytes = fs::read(archive).with_context(|| format!("reading {}", archive.display()))?;
    let actual = sha256_hex(&bytes);
    if actual.eq_ignore_ascii_case(&expected) {
        Ok(())
    } else {
        bail!(
            "checksum mismatch for {asset} — expected {expected}, computed {actual}. \
             The download was corrupted or tampered with; not installing."
        );
    }
}

/// Parse a `sha256sum`-format manifest into `filename -> lowercase hex digest`.
///
/// Each line is `<64-hex><whitespace>[*]<name>`; a leading path on the name is
/// dropped so a bare asset name matches. Lines without a valid digest are
/// skipped rather than failing the whole parse.
fn parse_checksums(text: &str) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.splitn(2, char::is_whitespace);
        let (Some(digest), Some(rest)) = (parts.next(), parts.next()) else {
            continue;
        };
        if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        let name = rest.trim_start().trim_start_matches('*').trim();
        let name = name.rsplit(['/', '\\']).next().unwrap_or(name);
        if !name.is_empty() {
            map.insert(name.to_string(), digest.to_ascii_lowercase());
        }
    }
    map
}

/// The lowercase hex sha256 of `bytes`.
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    to_hex(&hasher.finalize())
}

/// Lowercase hex encoding.
fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

// -- extraction ------------------------------------------------------------

/// Extract the two binaries ([`BIN_NAMES`]) from `archive` into `dest`.
#[cfg(not(windows))]
fn extract_binaries(archive: &Path, dest: &Path) -> Result<()> {
    let file = File::open(archive).with_context(|| format!("opening {}", archive.display()))?;
    let decoder = flate2::read::GzDecoder::new(file);
    let mut tar = tar::Archive::new(decoder);
    for entry in tar.entries().context("reading the archive")? {
        let mut entry = entry.context("reading an archive entry")?;
        let path = entry.path().context("reading an archive entry path")?;
        let Some(name) = path.file_name().and_then(|n| n.to_str()).map(str::to_owned) else {
            continue;
        };
        if BIN_NAMES.contains(&name.as_str()) {
            entry
                .unpack(dest.join(&name))
                .with_context(|| format!("extracting {name}"))?;
        }
    }
    Ok(())
}

/// Extract the two binaries ([`BIN_NAMES`]) from a `.zip` into `dest`.
#[cfg(windows)]
fn extract_binaries(archive: &Path, dest: &Path) -> Result<()> {
    let file = File::open(archive).with_context(|| format!("opening {}", archive.display()))?;
    let mut zip = zip::ZipArchive::new(file).context("reading the zip archive")?;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).context("reading a zip entry")?;
        let Some(name) = entry
            .enclosed_name()
            .and_then(|p| p.file_name().and_then(|n| n.to_str()).map(str::to_owned))
        else {
            continue;
        };
        if BIN_NAMES.contains(&name.as_str()) {
            let out = dest.join(&name);
            let mut sink =
                File::create(&out).with_context(|| format!("creating {}", out.display()))?;
            io::copy(&mut entry, &mut sink).with_context(|| format!("extracting {name}"))?;
        }
    }
    Ok(())
}

// -- installation ----------------------------------------------------------

/// The directory holding the running binaries: the parent of the resolved
/// (symlinks followed) current executable.
fn install_dir() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("locating the running executable")?;
    let exe = fs::canonicalize(&exe).with_context(|| format!("resolving {}", exe.display()))?;
    exe.parent().map(Path::to_path_buf).ok_or_else(|| {
        anyhow!(
            "the running executable {} has no parent directory",
            exe.display()
        )
    })
}

/// Install each freshly extracted binary in `staging` over its counterpart in
/// `install_dir`. A binary missing from either side is warned about and skipped
/// (the other is still updated); at least one always exists — we are running it.
fn install_all(staging: &Path, install_dir: &Path) -> Result<()> {
    for name in BIN_NAMES {
        let new = staging.join(name);
        let target = install_dir.join(name);
        if !new.exists() {
            eprintln!("warning: the archive did not contain {name}; leaving it unchanged.");
            continue;
        }
        if !target.exists() {
            eprintln!(
                "warning: {name} is not installed next to the running binary ({}); \
                 skipping it.",
                install_dir.display()
            );
            continue;
        }
        install_one(&new, &target)?;
    }
    Ok(())
}

/// Replace `target` with `new` via a temp file plus `rename` (a fresh inode, so
/// the running process is never overwritten in place).
#[cfg(not(windows))]
fn install_one(new: &Path, target: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let dir = target
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", target.display()))?;
    let name = target
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("loopreview");
    let tmp = dir.join(format!(".{name}.new.{}", std::process::id()));

    fs::copy(new, &tmp).map_err(|e| write_error(dir, target, e))?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))
        .map_err(|e| write_error(dir, target, e))?;
    fs::rename(&tmp, target).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        write_error(dir, target, e)
    })?;
    Ok(())
}

/// Replace `target` with `new`: a running `.exe` cannot be overwritten, so it is
/// renamed to `<name>.old` (swept on the next launch) and `new` moved into place.
#[cfg(windows)]
fn install_one(new: &Path, target: &Path) -> Result<()> {
    let dir = target
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", target.display()))?;
    let old = target.with_file_name(format!(
        "{}.old",
        target
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("loopreview")
    ));
    let _ = fs::remove_file(&old);
    fs::rename(target, &old).map_err(|e| write_error(dir, target, e))?;
    fs::copy(new, target).map_err(|e| {
        // Roll back so the user is not left without a binary.
        let _ = fs::rename(&old, target);
        write_error(dir, target, e)
    })?;
    Ok(())
}

/// A friendly error for a failed write into the install directory. No `sudo`
/// suggestion — the fix is to own or relocate the install.
fn write_error(dir: &Path, target: &Path, source: io::Error) -> anyhow::Error {
    anyhow!(
        "could not replace {} ({source}). The directory {} must be writable by your user — \
         re-install loopreview to a location you own, or adjust that directory's permissions.",
        target.display(),
        dir.display()
    )
}

// -- temp dir --------------------------------------------------------------

/// A working directory removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    /// Create a uniquely named temp directory under the system temp dir.
    fn new() -> Result<TempDir> {
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir =
            std::env::temp_dir().join(format!("loopreview-update-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        Ok(TempDir(dir))
    }

    /// The directory's path.
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_every_release_target() {
        assert_eq!(
            resolve_target("aarch64", "macos"),
            Some("aarch64-apple-darwin")
        );
        assert_eq!(
            resolve_target("x86_64", "macos"),
            Some("x86_64-apple-darwin")
        );
        assert_eq!(
            resolve_target("x86_64", "linux"),
            Some("x86_64-unknown-linux-gnu")
        );
        assert_eq!(
            resolve_target("aarch64", "linux"),
            Some("aarch64-unknown-linux-gnu")
        );
        assert_eq!(
            resolve_target("x86_64", "windows"),
            Some("x86_64-pc-windows-msvc")
        );
    }

    #[test]
    fn unsupported_platforms_have_no_target() {
        assert_eq!(resolve_target("aarch64", "windows"), None);
        assert_eq!(resolve_target("riscv64", "linux"), None);
        assert_eq!(resolve_target("x86_64", "freebsd"), None);
    }

    #[test]
    fn asset_names_match_release_yml() {
        assert_eq!(
            asset_name("0.2.0", "aarch64-apple-darwin"),
            "loopreview-0.2.0-aarch64-apple-darwin.tar.gz"
        );
        assert_eq!(
            asset_name("0.2.0", "x86_64-apple-darwin"),
            "loopreview-0.2.0-x86_64-apple-darwin.tar.gz"
        );
        assert_eq!(
            asset_name("0.2.0", "x86_64-unknown-linux-gnu"),
            "loopreview-0.2.0-x86_64-unknown-linux-gnu.tar.gz"
        );
        assert_eq!(
            asset_name("0.2.0", "aarch64-unknown-linux-gnu"),
            "loopreview-0.2.0-aarch64-unknown-linux-gnu.tar.gz"
        );
        assert_eq!(
            asset_name("0.2.0", "x86_64-pc-windows-msvc"),
            "loopreview-0.2.0-x86_64-pc-windows-msvc.zip"
        );
    }

    #[test]
    fn only_windows_targets_are_zips() {
        assert!(is_windows_target("x86_64-pc-windows-msvc"));
        assert!(!is_windows_target("aarch64-apple-darwin"));
        assert!(!is_windows_target("x86_64-unknown-linux-gnu"));
    }

    #[test]
    fn newer_version_comparison() {
        assert!(is_newer("0.1.0", "0.2.0").unwrap());
        assert!(is_newer("0.1.0", "v0.1.1").unwrap());
        assert!(is_newer("0.1.0", "1.0.0").unwrap());
        assert!(!is_newer("0.2.0", "0.2.0").unwrap());
        assert!(!is_newer("0.2.0", "v0.2.0").unwrap());
        assert!(!is_newer("0.2.0", "0.1.9").unwrap());
    }

    #[test]
    fn newer_version_rejects_garbage() {
        assert!(is_newer("0.1.0", "not-a-version").is_err());
        assert!(is_newer("nope", "0.1.0").is_err());
    }

    #[test]
    fn parses_sha256sum_manifest() {
        let digest = "a".repeat(64);
        let other = "b".repeat(64);
        // Two-space (text mode), star (binary mode), and a path-prefixed name.
        let manifest = format!(
            "{digest}  loopreview-0.2.0-aarch64-apple-darwin.tar.gz\n\
             {other} *loopreview-0.2.0-x86_64-pc-windows-msvc.zip\n\
             {digest}  dist/checksums.txt\n\
             \n\
             garbage line without a digest\n"
        );
        let map = parse_checksums(&manifest);
        assert_eq!(
            map.get("loopreview-0.2.0-aarch64-apple-darwin.tar.gz"),
            Some(&digest)
        );
        assert_eq!(
            map.get("loopreview-0.2.0-x86_64-pc-windows-msvc.zip"),
            Some(&other)
        );
        // The path prefix on the name is dropped.
        assert_eq!(map.get("checksums.txt"), Some(&digest));
        assert!(!map.contains_key("garbage"));
    }

    #[test]
    fn uppercase_digests_are_normalized() {
        let manifest = format!("{}  file.tar.gz", "A".repeat(64));
        let map = parse_checksums(&manifest);
        assert_eq!(map.get("file.tar.gz"), Some(&"a".repeat(64)));
    }

    #[test]
    fn sha256_of_known_input() {
        // Empty input has a well-known sha256.
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn verify_accepts_match_and_rejects_mismatch() {
        let dir = TempDir::new().unwrap();
        let archive = dir
            .path()
            .join("loopreview-0.2.0-x86_64-unknown-linux-gnu.tar.gz");
        fs::write(&archive, b"hello world").unwrap();
        let digest = sha256_hex(b"hello world");
        let asset = "loopreview-0.2.0-x86_64-unknown-linux-gnu.tar.gz";

        let good = dir.path().join("good.txt");
        fs::write(&good, format!("{digest}  {asset}\n")).unwrap();
        assert!(verify(&archive, asset, &good).is_ok());

        let bad = dir.path().join("bad.txt");
        fs::write(&bad, format!("{}  {asset}\n", "0".repeat(64))).unwrap();
        assert!(verify(&archive, asset, &bad).is_err());

        let missing = dir.path().join("missing.txt");
        fs::write(&missing, format!("{digest}  someone-else.tar.gz\n")).unwrap();
        assert!(verify(&archive, asset, &missing).is_err());
    }

    #[test]
    fn parses_release_json_and_finds_assets() {
        let json = r#"{
            "tag_name": "v0.2.0",
            "prerelease": false,
            "assets": [
                {
                    "name": "loopreview-0.2.0-aarch64-apple-darwin.tar.gz",
                    "browser_download_url": "https://example.com/mac.tar.gz"
                },
                {
                    "name": "checksums.txt",
                    "browser_download_url": "https://example.com/checksums.txt"
                }
            ]
        }"#;
        let release = parse_release(json).unwrap();
        assert_eq!(release.tag_name, "v0.2.0");
        assert_eq!(
            release
                .asset_url("loopreview-0.2.0-aarch64-apple-darwin.tar.gz")
                .unwrap(),
            "https://example.com/mac.tar.gz"
        );
        assert_eq!(
            release.asset_url("checksums.txt").unwrap(),
            "https://example.com/checksums.txt"
        );
        assert!(release.asset_url("nope.tar.gz").is_err());
    }

    #[test]
    fn human_bytes_scales() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(1024 * 1024), "1.0 MiB");
    }

    #[cfg(not(windows))]
    #[test]
    fn install_one_replaces_via_rename() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let dir = TempDir::new().unwrap();
        let target = dir.path().join("loopreview");
        fs::write(&target, b"old binary").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
        let target_inode = fs::metadata(&target).unwrap().ino();

        let staged = dir.path().join("staged");
        fs::write(&staged, b"new binary").unwrap();

        install_one(&staged, &target).unwrap();

        assert_eq!(fs::read(&target).unwrap(), b"new binary");
        // A fresh inode: the replacement did not overwrite in place.
        assert_ne!(fs::metadata(&target).unwrap().ino(), target_inode);
        // Executable bit is set on the installed binary.
        assert_ne!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o111,
            0
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn extract_binaries_pulls_both_from_tar_gz() {
        use std::io::Write as _;

        let dir = TempDir::new().unwrap();
        let archive = dir.path().join("bundle.tar.gz");
        {
            let file = File::create(&archive).unwrap();
            let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
            let mut builder = tar::Builder::new(encoder);
            for (name, body) in [("loopreview", b"lp!!!!!!".as_slice()), ("lr", b"lr!!!!!!")] {
                let mut header = tar::Header::new_gnu();
                header.set_size(body.len() as u64);
                header.set_mode(0o755);
                header.set_cksum();
                builder.append_data(&mut header, name, body).unwrap();
            }
            builder
                .into_inner()
                .unwrap()
                .finish()
                .unwrap()
                .flush()
                .unwrap();
        }

        let out = dir.path().join("out");
        fs::create_dir_all(&out).unwrap();
        extract_binaries(&archive, &out).unwrap();
        assert_eq!(fs::read(out.join("loopreview")).unwrap(), b"lp!!!!!!");
        assert_eq!(fs::read(out.join("lr")).unwrap(), b"lr!!!!!!");
    }
}
