//! `/update` for the line UI: formats what `crate::update` finds and runs
//! checks and installs in the background so chat keeps flowing.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::time::Duration;

use crate::update::{self, Release, Version};

/// Releases listed by `/update` before "…and N older".
const MAX_LISTED: usize = 5;
/// Lets the startup lines ("looking for peers…", our name) print first.
const STARTUP_DELAY: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateAction {
    Check,
    Install,
}

/// What `/update` prints for `newer` (newest first).
pub fn format_check(current: Version, newer: &[Release]) -> Vec<String> {
    let Some(latest) = newer.first() else {
        return vec![format!("* lan-chat {current} is the latest")];
    };
    let mut lines = vec![format!("* you have {current}. Newer:")];
    for r in newer.iter().take(MAX_LISTED) {
        let mut line = format!("*   v{} ({})", r.version, r.date);
        if !r.summary.is_empty() {
            line.push_str(&format!(": {}", r.summary.join(", ")));
        }
        lines.push(line);
    }
    if newer.len() > MAX_LISTED {
        lines.push(format!("*   …and {} older", newer.len() - MAX_LISTED));
    }
    lines.push(format!("* type /update yes to install v{}", latest.version));
    lines
}

/// The startup notice, if anything is newer.
pub fn format_notice(newer: &[Release]) -> Option<String> {
    newer.first().map(|r| format!("* v{} is available. Type /update", r.version))
}

/// Runs one update job at a time in the background, printing through `out`.
pub struct Updater {
    out: Sender<String>,
    busy: Arc<AtomicBool>,
}

impl Updater {
    pub fn new(out: Sender<String>) -> Self {
        Updater { out, busy: Arc::new(AtomicBool::new(false)) }
    }

    pub fn start(&self, action: UpdateAction) {
        if self.busy.swap(true, Ordering::SeqCst) {
            let _ = self.out.send("! an update check is already running".into());
            return;
        }
        let (out, busy) = (self.out.clone(), self.busy.clone());
        tokio::spawn(async move {
            let _clear = ClearOnDrop(busy);
            run(action, &out).await;
        });
    }

    /// One quiet check; prints a notice only if something is newer.
    pub fn check_at_startup(&self) {
        let out = self.out.clone();
        tokio::spawn(async move {
            tokio::time::sleep(STARTUP_DELAY).await;
            if let Some(notice) = update::check().await.ok().and_then(|newer| format_notice(&newer)) {
                let _ = out.send(notice);
            }
        });
    }
}

/// Frees the updater for the next job however this one ends, panics included.
struct ClearOnDrop(Arc<AtomicBool>);

impl Drop for ClearOnDrop {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

async fn run(action: UpdateAction, out: &Sender<String>) {
    let say = |s: String| {
        let _ = out.send(s);
    };
    // Installing checks again, so it never acts on a stale list.
    let newer = match update::check().await {
        Ok(newer) => newer,
        Err(e) => return say(format!("! {e}")),
    };
    let current = update::current();
    let latest = match (action, newer.first()) {
        (UpdateAction::Install, Some(latest)) => latest.version,
        _ => {
            for line in format_check(current, &newer) {
                say(line);
            }
            return;
        }
    };
    let target = match std::env::current_exe().and_then(|p| p.canonicalize()) {
        Ok(path) => path,
        Err(e) => return say(format!("! can't find the running lan-chat: {e}")),
    };
    say(format!("* downloading v{latest}…"));
    match update::install(latest, &target).await {
        Ok(()) => say(format!("* installed v{latest}. Restart lan-chat to use it.")),
        Err(e) => say(format!("! {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rel(v: &str, date: &str, summary: &[&str]) -> Release {
        Release {
            version: Version::parse(v).unwrap(),
            date: date.into(),
            summary: summary.iter().map(|s| s.to_string()).collect(),
        }
    }

    const NOW: Version = Version(0, 2, 0);

    #[test]
    fn up_to_date() {
        assert_eq!(format_check(NOW, &[]), vec!["* lan-chat 0.2.0 is the latest"]);
        assert_eq!(format_notice(&[]), None);
    }

    #[test]
    fn newer_releases_are_listed_with_the_install_hint() {
        let newer =
            [rel("0.3.0", "2026-11-02", &["TUI", "room expiry"]), rel("0.2.1", "2026-10-20", &["fix wrapping"])];
        assert_eq!(
            format_check(NOW, &newer),
            vec![
                "* you have 0.2.0. Newer:",
                "*   v0.3.0 (2026-11-02): TUI, room expiry",
                "*   v0.2.1 (2026-10-20): fix wrapping",
                "* type /update yes to install v0.3.0",
            ]
        );
        assert_eq!(format_notice(&newer).as_deref(), Some("* v0.3.0 is available. Type /update"));
    }

    #[test]
    fn a_release_without_notes_shows_just_the_date() {
        assert_eq!(format_check(NOW, &[rel("0.2.1", "2026-10-20", &[])])[1], "*   v0.2.1 (2026-10-20)");
    }

    #[test]
    fn long_lists_are_cut_at_five() {
        let newer: Vec<Release> = (1..=7).rev().map(|p| rel(&format!("0.2.{p}"), "2026-10-20", &[])).collect();
        let lines = format_check(NOW, &newer);
        assert_eq!(lines.len(), 1 + 5 + 1 + 1);
        assert_eq!(lines[5], "*   v0.2.3 (2026-10-20)");
        assert_eq!(lines[6], "*   …and 2 older");
        assert_eq!(lines[7], "* type /update yes to install v0.2.7");
    }

    #[test]
    fn the_busy_flag_clears_even_if_the_job_panics() {
        let busy = Arc::new(AtomicBool::new(true));
        let flag = busy.clone();
        let result = std::panic::catch_unwind(move || {
            let _clear = ClearOnDrop(flag);
            panic!("a bug in the update job");
        });
        assert!(result.is_err());
        assert!(!busy.load(Ordering::SeqCst));
    }
}
