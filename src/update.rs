//! Self-update: find newer GitHub releases and replace the running binary.
//! Talks to GitHub through the system `curl` and unpacks with the system `tar` (as install.sh does; Windows 10+
//! ships both), so the app needs no HTTP or TLS crates.

use std::ffi::OsStr;
use std::fmt;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::room::to_hex;

pub const REPO: &str = "Abinsanth/corvo-lan";
pub const INSTALL_URL: &str = "https://raw.githubusercontent.com/Abinsanth/corvo-lan/release/install.sh";
pub const INSTALL_PS1_URL: &str = "https://raw.githubusercontent.com/Abinsanth/corvo-lan/release/install.ps1";
/// Where a Windows update moves the running exe, which can be renamed but not replaced.
pub const OLD_EXE: &str = "corvo-lan.old.exe";
/// The binary inside a release archive.
#[cfg(windows)]
const BIN_NAME: &str = "corvo-lan.exe";
#[cfg(not(windows))]
const BIN_NAME: &str = "corvo-lan";
/// Names the temp dir an install works in, beside the binary it replaces.
const TMP_PREFIX: &str = ".corvo-lan-update-";
/// Most PR titles shown per release.
const SUMMARY_LINES: usize = 3;

/// `major.minor.patch`, compared numerically (0.10.0 is newer than 0.9.0).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(pub u64, pub u64, pub u64);

impl Version {
    /// `0.2.0` or `v0.2.0`; anything else (suffixes, missing parts) is `None`.
    pub fn parse(s: &str) -> Option<Version> {
        let mut parts = s.strip_prefix('v').unwrap_or(s).split('.');
        let mut next = || parts.next()?.parse::<u64>().ok();
        let v = Version(next()?, next()?, next()?);
        if parts.next().is_some() { None } else { Some(v) }
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

/// The version of this binary.
pub fn current() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("the crate version is major.minor.patch")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: Version,
    /// `YYYY-MM-DD`, or empty if GitHub didn't say.
    pub date: String,
    /// PR titles from the generated release notes.
    pub summary: Vec<String>,
}

/// The fields we use from `GET /repos/{REPO}/releases`.
#[derive(Deserialize)]
struct ApiRelease {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    published_at: Option<String>,
    body: Option<String>,
}

/// Published releases with a `v<major>.<minor>.<patch>` tag, newest first.
pub fn parse_releases(json: &str) -> Result<Vec<Release>, String> {
    let api: Vec<ApiRelease> = serde_json::from_str(json).map_err(|_| "unexpected reply from GitHub".to_string())?;
    let mut releases: Vec<Release> = api
        .into_iter()
        .filter(|r| !r.draft && !r.prerelease)
        .filter_map(|r| {
            Some(Release {
                version: Version::parse(&r.tag_name)?,
                date: r.published_at.as_deref().and_then(|d| d.get(..10)).unwrap_or_default().to_string(),
                summary: summarise(r.body.as_deref().unwrap_or_default()),
            })
        })
        .collect();
    releases.sort_by_key(|r| std::cmp::Reverse(r.version));
    Ok(releases)
}

/// GitHub's generated notes list PRs as "* Title by @user in https://…".
fn summarise(notes: &str) -> Vec<String> {
    notes
        .lines()
        .filter_map(|l| l.trim().strip_prefix("* "))
        .map(|l| l.split(" by @").next().unwrap_or(l).trim().to_string())
        .filter(|l| !l.is_empty())
        .take(SUMMARY_LINES)
        .collect()
}

/// The releases newer than `current`, newest first.
pub fn newer_than(current: Version, releases: Vec<Release>) -> Vec<Release> {
    let mut newer: Vec<Release> = releases.into_iter().filter(|r| r.version > current).collect();
    newer.sort_by_key(|r| std::cmp::Reverse(r.version));
    newer
}

/// The release asset for `std::env::consts::{OS, ARCH}`; same naming as install.sh and install.ps1.
pub fn asset_name(os: &str, arch: &str) -> Option<String> {
    match (os, arch) {
        ("macos", "x86_64" | "aarch64") => Some(format!("corvo-lan-{arch}-apple-darwin.tar.gz")),
        ("linux", "x86_64" | "aarch64") => Some(format!("corvo-lan-{arch}-unknown-linux-musl.tar.gz")),
        ("windows", "x86_64") => Some("corvo-lan-x86_64-pc-windows-msvc.zip".into()),
        _ => None,
    }
}

/// Whether `CORVO_LAN_NO_UPDATE_CHECK=<value>` turns the startup check off.
pub fn check_disabled_by(value: Option<&str>) -> bool {
    value.is_some_and(|v| !v.is_empty() && v != "0")
}

/// Where releases are listed and downloaded from. Tests point these at `file://`.
pub struct Urls {
    pub api: String,
    pub download: String,
}

impl Default for Urls {
    fn default() -> Self {
        Urls {
            api: format!("https://api.github.com/repos/{REPO}/releases?per_page=20"),
            download: format!("https://github.com/{REPO}/releases/download"),
        }
    }
}

/// Releases newer than this binary.
pub async fn check() -> Result<Vec<Release>, String> {
    check_with(&Urls::default(), current()).await
}

pub async fn check_with(urls: &Urls, current: Version) -> Result<Vec<Release>, String> {
    let agent = format!("User-Agent: corvo-lan/{current}");
    let accept = "Accept: application/vnd.github+json";
    let body = curl(["-fsSL", "--max-time", "10", "-H", accept, "-H", &agent, &urls.api]).await?;
    let json = String::from_utf8(body).map_err(|_| "unexpected reply from GitHub".to_string())?;
    Ok(newer_than(current, parse_releases(&json)?))
}

/// Replaces `target` with `version`'s build for this system.
pub async fn install(version: Version, target: &Path) -> Result<(), String> {
    let (os, arch) = (std::env::consts::OS, std::env::consts::ARCH);
    let asset = asset_name(os, arch).ok_or_else(|| format!("no release build for {os}/{arch}; build from source"))?;
    install_with(&Urls::default(), version, &asset, target).await
}

/// Downloads into a temp dir beside `target` (so the final rename is atomic and
/// on one filesystem), verifies, unpacks, then swaps the new binary in. Any
/// failure leaves `target` as it was.
pub async fn install_with(urls: &Urls, version: Version, asset: &str, target: &Path) -> Result<(), String> {
    let dir = target.parent().ok_or_else(|| cant_replace(target, "it has no parent directory"))?;
    sweep_leftovers(dir);
    let tmp = dir.join(format!("{TMP_PREFIX}{}", std::process::id()));
    if let Err(e) = std::fs::create_dir(&tmp) {
        let reason = if e.kind() == ErrorKind::PermissionDenied { "permission denied".into() } else { e.to_string() };
        return Err(cant_replace(target, &reason));
    }
    let result = install_in(&tmp, urls, version, asset, target).await;
    let _ = std::fs::remove_dir_all(&tmp);
    result
}

async fn install_in(tmp: &Path, urls: &Urls, version: Version, asset: &str, target: &Path) -> Result<(), String> {
    let url = format!("{}/v{version}/{asset}", urls.download);
    let archive = tmp.join(asset);
    let sums = tmp.join(format!("{asset}.sha256"));
    for (out, url) in [(&archive, url.clone()), (&sums, format!("{url}.sha256"))] {
        download(&url, out).await.map_err(|e| format!("couldn't download v{version}: {e}"))?;
    }

    let expected = std::fs::read_to_string(&sums).unwrap_or_default();
    let expected = expected.split_whitespace().next().unwrap_or_default();
    let actual = to_hex(&Sha256::digest(std::fs::read(&archive).map_err(|e| e.to_string())?));
    if expected != actual {
        return Err("download didn't match its checksum; nothing changed".into());
    }

    let status = tokio::process::Command::new(tar_program())
        .kill_on_drop(true)
        .arg("-xf")
        .arg(&archive)
        .arg("-C")
        .arg(tmp)
        .status()
        .await;
    match status {
        Ok(s) if s.success() => {}
        Ok(_) => return Err("couldn't unpack the download; nothing changed".into()),
        Err(e) if e.kind() == ErrorKind::NotFound => return Err("/update needs tar, which isn't installed".into()),
        Err(e) => return Err(format!("couldn't run tar: {e}")),
    }
    let new = tmp.join(BIN_NAME);
    if !new.is_file() {
        return Err("the download didn't contain corvo-lan; nothing changed".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&new, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
    }
    swap_in(&new, target).map_err(|e| cant_replace(target, &e.to_string()))
}

/// Windows' own bsdtar reads zip; a GNU tar from Git for Windows on PATH doesn't.
fn tar_program() -> PathBuf {
    #[cfg(windows)]
    if let Some(root) = std::env::var_os("SystemRoot") {
        return Path::new(&root).join("System32").join("tar.exe");
    }
    PathBuf::from("tar")
}

/// Puts `new` where `target` is. The running process keeps its open copy of
/// the old file; restarting picks up the new one.
#[cfg(not(windows))]
fn swap_in(new: &Path, target: &Path) -> std::io::Result<()> {
    std::fs::rename(new, target)
}

/// Windows won't replace a running exe but will rename it, so move it aside
/// first and put it back if the new one can't go in.
#[cfg(windows)]
fn swap_in(new: &Path, target: &Path) -> std::io::Result<()> {
    if !new.is_file() {
        return Err(std::io::Error::new(ErrorKind::NotFound, "the new version is missing"));
    }
    let old = target.with_file_name(OLD_EXE);
    let _ = std::fs::remove_file(&old);
    std::fs::rename(target, &old)?;
    if let Err(e) = std::fs::rename(new, target) {
        return match std::fs::rename(&old, target) {
            Ok(()) => Err(e),
            Err(_) => Err(std::io::Error::other(format!(
                "{e}; the old version is at {}, rename it back to {}",
                old.display(),
                target.display()
            ))),
        };
    }
    Ok(())
}

/// Deletes the exe a Windows update moved aside; it couldn't go while it ran.
pub fn remove_old_exe(dir: &Path) {
    let _ = std::fs::remove_file(dir.join(OLD_EXE));
}

async fn download(url: &str, out: &Path) -> Result<(), String> {
    let args = [OsStr::new("-fsSL"), OsStr::new("--max-time"), OsStr::new("120"), OsStr::new("-o"), out.as_os_str()];
    curl(args.into_iter().chain([OsStr::new(url)])).await.map(|_| ())
}

/// Removes temp dirs left by updates that were quit or crashed mid-download.
fn sweep_leftovers(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with(TMP_PREFIX) {
            let path = entry.path();
            let _ = std::fs::remove_dir_all(&path).or_else(|_| std::fs::remove_file(&path));
        }
        if entry.file_name() == OLD_EXE {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// How to reinstall by hand on this platform.
pub fn reinstall_hint() -> String {
    if cfg!(windows) { format!("irm {INSTALL_PS1_URL} | iex") } else { format!("curl -fsSL {INSTALL_URL} | sh") }
}

fn cant_replace(target: &Path, reason: &str) -> String {
    format!("can't replace {}: {reason}. Reinstall with: {}", target.display(), reinstall_hint())
}

/// Runs curl and returns what it wrote to stdout.
async fn curl<I, S>(args: I) -> Result<Vec<u8>, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let out = tokio::process::Command::new("curl").kill_on_drop(true).args(args).output().await.map_err(|e| {
        if e.kind() == ErrorKind::NotFound {
            "/update needs curl, which isn't installed".to_string()
        } else {
            format!("couldn't run curl: {e}")
        }
    })?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(curl_error(out.status.code(), &String::from_utf8_lossy(&out.stderr)))
    }
}

/// Turns a failed curl run into a message for people.
fn curl_error(code: Option<i32>, stderr: &str) -> String {
    match code {
        // DNS, connect, timeout, TLS handshake.
        Some(6 | 7 | 28 | 35) => "couldn't reach GitHub (offline?)".into(),
        Some(22) => match stderr.rsplit_once("error: ").and_then(|(_, n)| n.trim().parse::<u16>().ok()) {
            Some(status) => format!("GitHub said no (HTTP {status}); try again later"),
            None => "GitHub said no; try again later".into(),
        },
        Some(n) => format!("curl failed (exit {n})"),
        None => "curl was stopped".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rel(v: &str) -> Release {
        Release { version: Version::parse(v).unwrap(), date: "2026-01-01".into(), summary: vec![] }
    }

    const API: &str = r###"[
      {"tag_name": "v0.2.1", "draft": false, "prerelease": false, "published_at": "2026-10-20T09:00:00Z",
       "body": "## What's Changed\n* Fix long names wrapping by @Abinsanth in https://github.com/Abinsanth/corvo-lan/pull/12\n\n**Full Changelog**: x"},
      {"tag_name": "v0.10.0", "draft": false, "prerelease": false, "published_at": "2026-12-01T09:00:00Z",
       "body": "* TUI by @a in u\n* Room expiry by @a in u\n* Update command by @a in u\n* Fourth by @a in u"},
      {"tag_name": "v0.9.0", "draft": true, "prerelease": false, "published_at": null, "body": ""},
      {"tag_name": "v0.9.1", "draft": false, "prerelease": true, "published_at": "2026-11-01T09:00:00Z", "body": ""}
    ]"###;

    #[test]
    fn versions_parse_and_order_numerically() {
        assert_eq!(Version::parse("v0.2.0"), Some(Version(0, 2, 0)));
        assert_eq!(Version::parse("1.20.3"), Some(Version(1, 20, 3)));
        assert!(Version(0, 10, 0) > Version(0, 9, 0));
        assert_eq!(Version(0, 2, 0).to_string(), "0.2.0");
        assert_eq!(current().to_string(), env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn junk_and_suffixed_tags_are_skipped() {
        for tag in ["v0.3.0-rc1", "0.3", "v1.2.3.4", "latest", "v1.x.0", ""] {
            assert_eq!(Version::parse(tag), None, "{tag}");
        }
        let json =
            r#"[{"tag_name": "v0.3.0-rc1", "draft": false, "prerelease": false, "published_at": null, "body": null}]"#;
        assert_eq!(parse_releases(json), Ok(vec![]));
    }

    #[test]
    fn releases_skip_drafts_and_prereleases_newest_first() {
        let releases = parse_releases(API).unwrap();
        let versions: Vec<String> = releases.iter().map(|r| r.version.to_string()).collect();
        assert_eq!(versions, vec!["0.10.0", "0.2.1"]);
        assert_eq!(releases[0].date, "2026-12-01");
        assert_eq!(releases[0].summary, vec!["TUI", "Room expiry", "Update command"]);
        assert_eq!(releases[1].summary, vec!["Fix long names wrapping"]);
    }

    #[test]
    fn null_notes_give_an_empty_summary() {
        let json = r#"[{"tag_name": "v0.3.0", "draft": false, "prerelease": false, "published_at": "2026-11-02T00:00:00Z", "body": null}]"#;
        assert_eq!(parse_releases(json).unwrap()[0].summary, Vec::<String>::new());
    }

    #[test]
    fn unexpected_json_is_an_error() {
        assert_eq!(
            parse_releases(r#"{"message": "API rate limit exceeded"}"#),
            Err("unexpected reply from GitHub".into())
        );
        assert_eq!(parse_releases("not json"), Err("unexpected reply from GitHub".into()));
    }

    #[test]
    fn only_newer_releases_remain_newest_first() {
        let all = vec![rel("0.1.0"), rel("0.2.1"), rel("0.2.0"), rel("0.3.0")];
        let newer: Vec<Version> = newer_than(Version(0, 2, 0), all).into_iter().map(|r| r.version).collect();
        assert_eq!(newer, vec![Version(0, 3, 0), Version(0, 2, 1)]);
    }

    #[test]
    fn asset_names_match_the_release_workflow() {
        assert_eq!(asset_name("macos", "aarch64").as_deref(), Some("corvo-lan-aarch64-apple-darwin.tar.gz"));
        assert_eq!(asset_name("macos", "x86_64").as_deref(), Some("corvo-lan-x86_64-apple-darwin.tar.gz"));
        assert_eq!(asset_name("linux", "x86_64").as_deref(), Some("corvo-lan-x86_64-unknown-linux-musl.tar.gz"));
        assert_eq!(asset_name("linux", "aarch64").as_deref(), Some("corvo-lan-aarch64-unknown-linux-musl.tar.gz"));
        assert_eq!(asset_name("windows", "x86_64").as_deref(), Some("corvo-lan-x86_64-pc-windows-msvc.zip"));
        assert_eq!(asset_name("windows", "aarch64"), None);
        assert_eq!(asset_name("linux", "riscv64"), None);
    }

    #[test]
    fn the_env_var_turns_the_startup_check_off() {
        assert!(!check_disabled_by(None));
        assert!(!check_disabled_by(Some("")));
        assert!(!check_disabled_by(Some("0")));
        assert!(check_disabled_by(Some("1")));
        assert!(check_disabled_by(Some("yes")));
    }

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A fresh empty directory under the system temp dir.
    fn scratch(name: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("corvo-lan-test-{}-{n}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A fake release `v<version>` under `root`: a tarball holding a `corvo-lan`
    /// script that prints `marker`, and its checksum file.
    fn fake_release(root: &Path, version: &str, asset: &str, marker: &str) -> Urls {
        let dir = root.join(format!("v{version}"));
        std::fs::create_dir_all(&dir).unwrap();
        let src = root.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join(BIN_NAME), format!("#!/bin/sh\necho {marker}\n")).unwrap();
        let archive = dir.join(asset);
        let ok = std::process::Command::new(tar_program())
            .arg("-a")
            .arg("-cf")
            .arg(&archive)
            .arg("-C")
            .arg(&src)
            .arg(BIN_NAME)
            .status()
            .unwrap();
        assert!(ok.success());
        let sum = to_hex(&Sha256::digest(std::fs::read(&archive).unwrap()));
        std::fs::write(dir.join(format!("{asset}.sha256")), format!("{sum}  {asset}\n")).unwrap();
        Urls { api: String::new(), download: file_url(root) }
    }

    fn old_binary(dir: &Path) -> PathBuf {
        let target = dir.join(BIN_NAME);
        std::fs::write(&target, "old").unwrap();
        target
    }

    fn leftovers(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n != BIN_NAME)
            .collect()
    }

    /// The archive the fake releases use: tar picks the format from the name.
    #[cfg(windows)]
    const ASSET: &str = "corvo-lan-test.zip";
    #[cfg(not(windows))]
    const ASSET: &str = "corvo-lan-test.tar.gz";

    /// A `file://` URL curl accepts; Windows paths become `file:///C:/...`.
    fn file_url(path: &Path) -> String {
        let p = path.display().to_string().replace('\\', "/");
        if p.starts_with('/') { format!("file://{p}") } else { format!("file:///{p}") }
    }

    #[tokio::test]
    async fn check_reads_the_api_and_keeps_newer_releases() {
        let root = scratch("check");
        std::fs::write(root.join("releases.json"), API).unwrap();
        let urls = Urls { api: file_url(&root.join("releases.json")), download: String::new() };
        let newer = check_with(&urls, Version(0, 2, 0)).await.unwrap();
        let versions: Vec<Version> = newer.iter().map(|r| r.version).collect();
        assert_eq!(versions, vec![Version(0, 10, 0), Version(0, 2, 1)]);
    }

    #[tokio::test]
    async fn install_replaces_the_target() {
        let root = scratch("ok-release");
        let bin = scratch("ok-bin");
        let urls = fake_release(&root, "0.3.0", ASSET, "new");
        let target = old_binary(&bin);
        install_with(&urls, Version(0, 3, 0), ASSET, &target).await.unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "#!/bin/sh\necho new\n");
        #[cfg(unix)]
        assert_eq!(std::fs::metadata(&target).unwrap().permissions().mode() & 0o777, 0o755);
        // Windows can't delete the exe it just moved aside; the next start does.
        let expected: Vec<String> = if cfg!(windows) { vec![OLD_EXE.into()] } else { vec![] };
        assert_eq!(leftovers(&bin), expected);
    }

    #[tokio::test]
    async fn checksum_mismatch_leaves_target_unchanged() {
        let root = scratch("bad-release");
        let bin = scratch("bad-bin");
        let urls = fake_release(&root, "0.3.0", ASSET, "new");
        std::fs::write(root.join(format!("v0.3.0/{ASSET}.sha256")), format!("{}  {ASSET}\n", "0".repeat(64))).unwrap();
        let target = old_binary(&bin);
        let err = install_with(&urls, Version(0, 3, 0), ASSET, &target).await.unwrap_err();
        assert_eq!(err, "download didn't match its checksum; nothing changed");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "old");
        assert!(leftovers(&bin).is_empty());
    }

    #[tokio::test]
    async fn missing_archive_leaves_target_unchanged() {
        let root = scratch("empty-release");
        let bin = scratch("missing-bin");
        let urls = Urls { api: String::new(), download: file_url(&root) };
        let target = old_binary(&bin);
        let err = install_with(&urls, Version(0, 3, 0), ASSET, &target).await.unwrap_err();
        assert!(err.starts_with("couldn't download v0.3.0"), "{err}");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "old");
        assert!(leftovers(&bin).is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn read_only_directory_fails_before_downloading() {
        let bin = scratch("ro-bin");
        let target = old_binary(&bin);
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o555)).unwrap();
        // The download URL is unreachable on purpose: we must fail before trying it.
        let urls = Urls { api: String::new(), download: "file:///nonexistent".into() };
        let err = install_with(&urls, Version(0, 3, 0), ASSET, &target).await.unwrap_err();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(err.starts_with(&format!("can't replace {}: permission denied", target.display())), "{err}");
        assert!(err.ends_with(&format!("Reinstall with: {}", reinstall_hint())), "{err}");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "old");
    }

    #[tokio::test]
    async fn a_leftover_temp_dir_does_not_block_install() {
        let root = scratch("leftover-release");
        let bin = scratch("leftover-bin");
        let urls = fake_release(&root, "0.3.0", ASSET, "new");
        let target = old_binary(&bin);
        // Left by earlier runs (other process ids): one quit mid-download, one odd file.
        let stale = bin.join(".corvo-lan-update-1");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::write(stale.join("junk"), "x").unwrap();
        std::fs::write(bin.join(".corvo-lan-update-2"), "x").unwrap();
        std::fs::write(bin.join(format!(".corvo-lan-update-{}", std::process::id())), "x").unwrap();
        install_with(&urls, Version(0, 3, 0), ASSET, &target).await.unwrap();
        let expected: Vec<String> = if cfg!(windows) { vec![OLD_EXE.into()] } else { vec![] };
        assert_eq!(leftovers(&bin), expected);
    }

    #[tokio::test]
    async fn installing_twice_sweeps_the_previous_old_exe() {
        let root = scratch("twice-release");
        let bin = scratch("twice-bin");
        let urls = fake_release(&root, "0.3.0", ASSET, "new");
        let target = old_binary(&bin);
        install_with(&urls, Version(0, 3, 0), ASSET, &target).await.unwrap();
        install_with(&urls, Version(0, 3, 0), ASSET, &target).await.unwrap();
        let expected: Vec<String> = if cfg!(windows) { vec![OLD_EXE.into()] } else { vec![] };
        assert_eq!(leftovers(&bin), expected);
    }

    #[test]
    fn a_failed_swap_leaves_the_target_as_it_was() {
        let bin = scratch("swap-fail");
        let target = old_binary(&bin);
        assert!(swap_in(&bin.join("missing"), &target).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "old");
        assert!(leftovers(&bin).is_empty(), "{:?}", leftovers(&bin));
    }

    /// The second rename fails (the new file is held open), so the first must
    /// be undone.
    #[cfg(windows)]
    #[test]
    fn a_swap_that_fails_midway_is_undone() {
        use std::os::windows::fs::OpenOptionsExt;
        let bin = scratch("swap-midway-bin");
        let target = old_binary(&bin);
        let new = scratch("swap-midway-new").join("new.exe");
        std::fs::write(&new, "new").unwrap();
        let _held = std::fs::OpenOptions::new().read(true).share_mode(0).open(&new).unwrap();
        assert!(swap_in(&new, &target).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "old");
        assert!(!bin.join(OLD_EXE).exists());
    }

    #[test]
    fn the_old_exe_is_removed_at_startup() {
        let bin = scratch("old-exe");
        std::fs::write(bin.join(OLD_EXE), "old").unwrap();
        remove_old_exe(&bin);
        assert!(!bin.join(OLD_EXE).exists());
        remove_old_exe(&bin); // nothing there: no panic
    }

    #[test]
    fn the_reinstall_hint_fits_the_platform() {
        if cfg!(windows) {
            assert_eq!(reinstall_hint(), format!("irm {INSTALL_PS1_URL} | iex"));
        } else {
            assert_eq!(reinstall_hint(), format!("curl -fsSL {INSTALL_URL} | sh"));
        }
    }

    /// Updating again before restarting: the old exe is still running, so it
    /// can't be replaced. Nothing may change.
    #[cfg(windows)]
    #[tokio::test]
    async fn a_locked_old_exe_fails_cleanly() {
        use std::os::windows::fs::OpenOptionsExt;
        let root = scratch("locked-release");
        let bin = scratch("locked-bin");
        let urls = fake_release(&root, "0.3.0", ASSET, "new");
        let target = old_binary(&bin);
        std::fs::write(bin.join(OLD_EXE), "running").unwrap();
        let _held = std::fs::OpenOptions::new().read(true).share_mode(0).open(bin.join(OLD_EXE)).unwrap();
        let err = install_with(&urls, Version(0, 3, 0), ASSET, &target).await.unwrap_err();
        assert!(err.starts_with("can't replace"), "{err}");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "old");
    }

    #[test]
    fn curl_errors_read_like_people_talk() {
        for code in [6, 7, 28, 35] {
            assert_eq!(curl_error(Some(code), ""), "couldn't reach GitHub (offline?)");
        }
        let forbidden = "curl: (22) The requested URL returned error: 403";
        assert_eq!(curl_error(Some(22), forbidden), "GitHub said no (HTTP 403); try again later");
        assert_eq!(curl_error(Some(22), "curl: (22) weird"), "GitHub said no; try again later");
        assert_eq!(curl_error(Some(37), ""), "curl failed (exit 37)");
        assert_eq!(curl_error(None, ""), "curl was stopped");
    }
}
