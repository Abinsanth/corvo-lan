//! Self-update: find newer GitHub releases and replace the running binary.
//! Talks to GitHub through the system `curl` (as install.sh does), so the app
//! needs no HTTP or TLS crates.

use std::fmt;

use serde::Deserialize;

pub const REPO: &str = "Abinsanth/lan-chat";
pub const INSTALL_URL: &str = "https://raw.githubusercontent.com/Abinsanth/lan-chat/release/install.sh";
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

/// The release asset for `std::env::consts::{OS, ARCH}`; same naming as install.sh.
pub fn asset_name(os: &str, arch: &str) -> Option<String> {
    let os = match os {
        "macos" => "apple-darwin",
        "linux" => "unknown-linux-musl",
        _ => return None,
    };
    let arch = match arch {
        "x86_64" | "aarch64" => arch,
        _ => return None,
    };
    Some(format!("lan-chat-{arch}-{os}.tar.gz"))
}

/// Whether `LAN_CHAT_NO_UPDATE_CHECK=<value>` turns the startup check off.
pub fn check_disabled_by(value: Option<&str>) -> bool {
    value.is_some_and(|v| !v.is_empty() && v != "0")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rel(v: &str) -> Release {
        Release { version: Version::parse(v).unwrap(), date: "2026-01-01".into(), summary: vec![] }
    }

    const API: &str = r###"[
      {"tag_name": "v0.2.1", "draft": false, "prerelease": false, "published_at": "2026-10-20T09:00:00Z",
       "body": "## What's Changed\n* Fix long names wrapping by @Abinsanth in https://github.com/Abinsanth/lan-chat/pull/12\n\n**Full Changelog**: x"},
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
        assert_eq!(asset_name("macos", "aarch64").as_deref(), Some("lan-chat-aarch64-apple-darwin.tar.gz"));
        assert_eq!(asset_name("macos", "x86_64").as_deref(), Some("lan-chat-x86_64-apple-darwin.tar.gz"));
        assert_eq!(asset_name("linux", "x86_64").as_deref(), Some("lan-chat-x86_64-unknown-linux-musl.tar.gz"));
        assert_eq!(asset_name("linux", "aarch64").as_deref(), Some("lan-chat-aarch64-unknown-linux-musl.tar.gz"));
        assert_eq!(asset_name("windows", "x86_64"), None);
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
}
