//! `/update` for the UI: formats what `crate::update` finds and runs checks
//! and installs in the background so chat keeps flowing.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc::UnboundedSender;

use super::Notice;
use crate::update::{self, Release, Version};

/// Releases listed by `/update` before "…and N older".
const MAX_LISTED: usize = 5;
/// Cells in the download progress bar.
const BAR_CELLS: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateAction {
    Check,
    Install,
}

/// What the startup check does about a newer version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Ask whether to install it.
    Offer(Version),
    /// It's been out `days` days: install it now.
    Force { version: Version, days: i64 },
}

/// What `/update` prints for `newer` (newest first).
pub fn format_check(current: Version, newer: &[Release]) -> Vec<String> {
    if newer.is_empty() {
        return vec![format!("* corvo-lan {current} is the latest")];
    }
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
    lines
}

/// The startup check's move for `newer` (newest first), on day `today`.
pub fn startup_step(newer: &[Release], today: i64) -> Option<Step> {
    let latest = newer.first()?;
    Some(match update::days_out(latest, today) {
        Some(days) if update::must_update(latest, today) => Step::Force { version: latest.version, days },
        _ => Step::Offer(latest.version),
    })
}

/// `* downloading v0.6.0  [##########··········]  52%`, or without a size, how much has arrived.
pub fn progress_line(version: Version, done: u64, total: Option<u64>) -> String {
    match total.filter(|t| *t > 0) {
        Some(total) => {
            let pct = (done.min(total) * 100 / total) as usize;
            let filled = pct * BAR_CELLS / 100;
            format!("* downloading v{version}  [{}{}]  {pct}%", "#".repeat(filled), "·".repeat(BAR_CELLS - filled))
        }
        None => format!("* downloading v{version}  {:.1} MB", done as f64 / 1_000_000.0),
    }
}

/// Runs one update job at a time in the background, reporting through `out`.
pub struct Updater {
    out: UnboundedSender<Notice>,
    busy: Arc<AtomicBool>,
    /// The binary to replace; `None` if it couldn't be found.
    exe: Option<PathBuf>,
}

impl Updater {
    pub fn new(out: UnboundedSender<Notice>, exe: Option<PathBuf>) -> Self {
        Updater { out, busy: Arc::new(AtomicBool::new(false)), exe }
    }

    pub fn start(&self, action: UpdateAction) {
        if self.busy.swap(true, Ordering::SeqCst) {
            let _ = self.out.send(Notice::new("! an update check is already running"));
            return;
        }
        let (out, busy, exe) = (self.out.clone(), self.busy.clone(), self.exe.clone());
        tokio::spawn(async move {
            let _clear = ClearOnDrop(busy);
            run(action, exe.as_deref(), &out).await;
        });
    }

    /// One check right away: offers a newer version, or installs one that's
    /// been out `FORCE_AFTER_DAYS`. Says nothing if GitHub can't be reached.
    pub fn check_at_startup(&self) {
        let (out, busy, exe) = (self.out.clone(), self.busy.clone(), self.exe.clone());
        tokio::spawn(async move {
            let Ok(newer) = update::check().await else {
                return;
            };
            match startup_step(&newer, update::today()) {
                None => {}
                Some(Step::Offer(version)) => {
                    let _ = out.send(Notice::Offer(version));
                }
                Some(Step::Force { version, days }) => {
                    if busy.swap(true, Ordering::SeqCst) {
                        return;
                    }
                    let _clear = ClearOnDrop(busy);
                    let current = update::current();
                    let text = format!("* you're on v{current} and v{version} has been out {days} days — updating");
                    let _ = out.send(Notice::new(text));
                    install_latest(&newer[0], true, exe.as_deref(), &out).await;
                }
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

async fn run(action: UpdateAction, exe: Option<&Path>, out: &UnboundedSender<Notice>) {
    // Installing checks again, so it never acts on a stale list.
    let newer = match update::check().await {
        Ok(newer) => newer,
        Err(e) => {
            let _ = out.send(Notice::new(format!("! {e}")));
            return;
        }
    };
    match (action, newer.first()) {
        (UpdateAction::Install, Some(latest)) => install_latest(latest, false, exe, out).await,
        (_, latest) => {
            for line in format_check(update::current(), &newer) {
                let _ = out.send(Notice::new(line));
            }
            if let Some(latest) = latest {
                let _ = out.send(Notice::Offer(latest.version));
            }
        }
    }
}

/// Downloads and installs `release` with a progress line, then says it's installed.
async fn install_latest(release: &Release, forced: bool, exe: Option<&Path>, out: &UnboundedSender<Notice>) {
    let Some(target) = exe else {
        let _ = out.send(Notice::new("! can't find the running corvo-lan"));
        return;
    };
    let version = release.version;
    let last = Mutex::new(String::new());
    let progress = |done: u64, total: Option<u64>| {
        let line = progress_line(version, done, total);
        let mut last = last.lock().unwrap_or_else(|e| e.into_inner());
        if *last != line {
            last.clone_from(&line);
            let _ = out.send(Notice::Progress(line));
        }
    };
    match update::install(release, target, progress).await {
        Ok(()) => {
            let _ = out.send(Notice::Installed { version, forced });
        }
        Err(e) => {
            let _ = out.send(Notice::new(format!("! {e}")));
        }
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
            assets: vec![],
        }
    }

    const NOW: Version = Version(0, 2, 0);

    #[test]
    fn up_to_date() {
        assert_eq!(format_check(NOW, &[]), vec!["* corvo-lan 0.2.0 is the latest"]);
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
            ]
        );
    }

    #[test]
    fn a_release_without_notes_shows_just_the_date() {
        assert_eq!(format_check(NOW, &[rel("0.2.1", "2026-10-20", &[])])[1], "*   v0.2.1 (2026-10-20)");
    }

    #[test]
    fn long_lists_are_cut_at_five() {
        let newer: Vec<Release> = (1..=7).rev().map(|p| rel(&format!("0.2.{p}"), "2026-10-20", &[])).collect();
        let lines = format_check(NOW, &newer);
        assert_eq!(lines.len(), 1 + 5 + 1);
        assert_eq!(lines[5], "*   v0.2.3 (2026-10-20)");
        assert_eq!(lines[6], "*   …and 2 older");
    }

    #[test]
    fn startup_steps() {
        let today = update::day_number("2026-10-15").unwrap();
        assert_eq!(startup_step(&[], today), None);
        let out_since = |date| rel("0.6.0", date, &[]);
        assert_eq!(startup_step(&[out_since("2026-09-24")], today), Some(Step::Offer(Version(0, 6, 0))));
        assert_eq!(
            startup_step(&[out_since("2026-09-23"), rel("0.5.1", "2026-01-01", &[])], today),
            Some(Step::Force { version: Version(0, 6, 0), days: 22 })
        );
        assert_eq!(startup_step(&[out_since("")], today), Some(Step::Offer(Version(0, 6, 0))), "no date: just ask");
    }

    #[test]
    fn progress_lines() {
        let v = Version(0, 6, 0);
        assert_eq!(progress_line(v, 52, Some(100)), "* downloading v0.6.0  [##########··········]  52%");
        assert_eq!(progress_line(v, 0, Some(100)), "* downloading v0.6.0  [····················]  0%");
        assert_eq!(progress_line(v, 150, Some(100)), "* downloading v0.6.0  [####################]  100%");
        assert_eq!(progress_line(v, 1_500_000, None), "* downloading v0.6.0  1.5 MB");
        assert_eq!(progress_line(v, 10, Some(0)), "* downloading v0.6.0  0.0 MB", "a zero size is no size");
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
