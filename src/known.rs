//! Peers we've met, remembered by key, so a familiar name with a new key stands out.

use std::fs;
use std::io::{ErrorKind, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::names;
use crate::protocol::PeerId;

/// Entries kept; past this the longest-unseen one is dropped.
pub const MAX_ENTRIES: usize = 1000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub key: PeerId,
    pub base: String,
    pub last_nick: String,
    /// Unix millis.
    pub last_seen: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Judgement {
    Known,
    FirstMeeting,
    /// A key under a base we've met with other keys, none of them online now.
    KeyChanged {
        last_seen: u64,
    },
}

#[derive(Debug, Default)]
pub struct Store {
    /// `None` keeps everything in memory (tests, throwaway identities).
    path: Option<PathBuf>,
    entries: Vec<Entry>,
}

impl Store {
    pub fn in_memory() -> Store {
        Store::default()
    }

    /// A missing file is an empty store. A file that can't be read is left
    /// alone and this session keeps peers in memory only. A corrupt one is
    /// moved to `.bad` so it isn't overwritten. Each case but the first gives
    /// a notice. At most `MAX_ENTRIES` (the most recently seen) are kept.
    pub fn load(path: PathBuf) -> (Store, Option<String>) {
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == ErrorKind::NotFound => {
                return (Store { path: Some(path), entries: Vec::new() }, None);
            }
            Err(e) => {
                let notice = format!("couldn't read {} ({e}); known peers won't be saved this session", path.display());
                return (Store::in_memory(), Some(notice));
            }
        };
        match serde_json::from_slice::<Vec<Entry>>(&bytes) {
            Ok(mut entries) => {
                if entries.len() > MAX_ENTRIES {
                    entries.sort_by_key(|e| std::cmp::Reverse(e.last_seen));
                    entries.truncate(MAX_ENTRIES);
                }
                (Store { path: Some(path), entries }, None)
            }
            Err(_) => {
                let bad = path.with_extension("json.bad");
                match fs::rename(&path, &bad) {
                    Ok(()) => {
                        let notice = format!(
                            "{} was unreadable; moved it to {} and started a new list of known peers",
                            path.display(),
                            bad.display()
                        );
                        (Store { path: Some(path), entries: Vec::new() }, Some(notice))
                    }
                    Err(e) => {
                        let notice = format!(
                            "{} is unreadable and couldn't be moved aside ({e}); left it alone, and known peers \
                             won't be saved this session",
                            path.display()
                        );
                        (Store::in_memory(), Some(notice))
                    }
                }
            }
        }
    }

    pub fn judge(&self, key: PeerId, nick: &str, online: &[PeerId]) -> Judgement {
        let base = names::base_of(nick).unwrap_or(nick);
        let stored = self.entries.iter().find(|e| e.key == key);
        if stored.is_some_and(|e| e.base == base) {
            return Judgement::Known;
        }
        let others: Vec<&Entry> = self.entries.iter().filter(|e| e.base == base && e.key != key).collect();
        let clear = others.is_empty() || others.iter().any(|e| online.contains(&e.key));
        match (stored.is_some(), clear) {
            (true, true) => Judgement::Known,
            (false, true) => Judgement::FirstMeeting,
            (_, false) => Judgement::KeyChanged { last_seen: others.iter().map(|e| e.last_seen).max().unwrap_or(0) },
        }
    }

    /// Remembers `key` as `nick`, seen at `now`.
    pub fn record(&mut self, key: PeerId, nick: &str, now: u64) {
        let base = names::base_of(nick).unwrap_or(nick).to_string();
        match self.entries.iter_mut().find(|e| e.key == key) {
            Some(e) => {
                e.base = base;
                e.last_nick = nick.into();
                e.last_seen = now;
            }
            None => self.entries.push(Entry { key, base, last_nick: nick.into(), last_seen: now }),
        }
        if self.entries.len() > MAX_ENTRIES
            && let Some(oldest) = self.entries.iter().enumerate().min_by_key(|(_, e)| e.last_seen).map(|(i, _)| i)
        {
            self.entries.swap_remove(oldest);
        }
    }

    /// Writes atomically. A failure is logged, not shown: chat carries on.
    pub fn save(&self) {
        let Some(path) = &self.path else {
            return;
        };
        if let Err(e) = self.write(path) {
            warn!("could not save {}: {e}", path.display());
        }
    }

    fn write(&self, path: &Path) -> std::io::Result<()> {
        let tmp = path.with_extension("json.tmp");
        let mut file = fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        file.write_all(&serde_json::to_vec_pretty(&self.entries).expect("entries always serialize"))?;
        file.sync_all()?;
        fs::rename(&tmp, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    const DAY: u64 = 86_400_000;

    use crate::testutil::Scratch;

    /// The store's file inside a scratch folder; keep the `Scratch` alive.
    fn temp_file() -> (Scratch, PathBuf) {
        let scratch = Scratch::new("corvo-known");
        let path = scratch.path().join("known_peers.json");
        (scratch, path)
    }

    fn met(entries: &[(u64, &str, u64)]) -> Store {
        let mut s = Store::in_memory();
        for &(key, nick, at) in entries {
            s.record(PeerId::test(key), nick, at);
        }
        s
    }

    #[test]
    fn a_known_key_under_a_base_met_with_other_keys_is_judged_there() {
        let s = met(&[(1, "eveFox", DAY), (2, "bobRoger", 2 * DAY)]);
        assert_eq!(s.judge(PeerId::test(1), "bobMoon", &[]), Judgement::KeyChanged { last_seen: 2 * DAY });
        assert_eq!(s.judge(PeerId::test(1), "bobMoon", &[PeerId::test(2)]), Judgement::Known);
    }

    #[test]
    fn a_known_key_under_a_new_base_nobody_else_used_is_silent() {
        let s = met(&[(1, "eveFox", DAY)]);
        assert_eq!(s.judge(PeerId::test(1), "carolFox", &[]), Judgement::Known);
    }

    #[test]
    fn a_known_key_is_silent() {
        let s = met(&[(1, "bobRoger", DAY)]);
        assert_eq!(s.judge(PeerId::test(1), "bobOtter", &[]), Judgement::Known);
    }

    #[test]
    fn a_new_key_under_a_new_base_is_a_first_meeting() {
        let s = met(&[(1, "bobRoger", DAY)]);
        assert_eq!(s.judge(PeerId::test(2), "carolFox", &[]), Judgement::FirstMeeting);
        assert_eq!(Store::in_memory().judge(PeerId::test(2), "carolFox", &[]), Judgement::FirstMeeting);
    }

    #[test]
    fn a_new_key_under_a_base_met_before_is_a_key_change() {
        let s = met(&[(1, "bobRoger", DAY), (3, "bobOtter", 3 * DAY)]);
        assert_eq!(s.judge(PeerId::test(2), "bobMoon", &[]), Judgement::KeyChanged { last_seen: 3 * DAY });
    }

    #[test]
    fn two_machines_with_the_same_base_online_is_normal() {
        let s = met(&[(1, "bobRoger", DAY)]);
        assert_eq!(s.judge(PeerId::test(2), "bobMoon", &[PeerId::test(1)]), Judgement::FirstMeeting);
    }

    #[test]
    fn the_warning_fires_once() {
        let mut s = met(&[(1, "bobRoger", DAY)]);
        assert!(matches!(s.judge(PeerId::test(2), "bobMoon", &[]), Judgement::KeyChanged { .. }));
        s.record(PeerId::test(2), "bobMoon", 2 * DAY);
        assert_eq!(s.judge(PeerId::test(2), "bobMoon", &[]), Judgement::Known);
    }

    #[test]
    fn recording_updates_the_name_and_time() {
        let s = met(&[(1, "bobRoger", DAY), (1, "bobOtter", 2 * DAY)]);
        assert_eq!(
            s.entries,
            vec![Entry { key: PeerId::test(1), base: "bob".into(), last_nick: "bobOtter".into(), last_seen: 2 * DAY }]
        );
    }

    #[test]
    fn a_full_store_drops_the_longest_unseen() {
        let mut s = Store::in_memory();
        for i in 0..MAX_ENTRIES as u64 {
            s.record(PeerId::test(i + 10), "bobRoger", i + 10);
        }
        s.record(PeerId::test(1), "carolFox", 5_000);
        assert_eq!(s.entries.len(), MAX_ENTRIES);
        assert!(!s.entries.iter().any(|e| e.key == PeerId::test(10)), "the oldest went");
        assert!(s.entries.iter().any(|e| e.key == PeerId::test(1)));
    }

    #[test]
    fn saving_and_loading_round_trips_privately() {
        let (_scratch, path) = temp_file();
        let (mut s, notice) = Store::load(path.clone());
        assert_eq!(notice, None, "a missing file is just empty");
        s.record(PeerId::test(1), "bobRoger", DAY);
        s.save();
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let (again, _) = Store::load(path);
        assert_eq!(again.judge(PeerId::test(1), "bobRoger", &[]), Judgement::Known);
    }

    #[test]
    fn a_corrupt_file_is_moved_aside() {
        let (_scratch, path) = temp_file();
        fs::write(&path, "{ not json").unwrap();
        let (s, notice) = Store::load(path.clone());
        assert!(s.entries.is_empty());
        assert!(notice.unwrap().contains("known_peers.json.bad"));
        assert_eq!(fs::read_to_string(path.with_extension("json.bad")).unwrap(), "{ not json");
        assert!(!path.exists());
    }

    #[test]
    fn a_file_that_cant_be_read_is_left_alone() {
        let (_scratch, path) = temp_file();
        fs::create_dir(&path).unwrap(); // reading a folder fails, and not with NotFound
        let (mut s, notice) = Store::load(path.clone());
        assert!(notice.unwrap().contains("couldn't read"));
        assert_eq!(s.path, None, "kept in memory only");
        s.record(PeerId::test(1), "bobRoger", DAY);
        s.save();
        assert!(path.is_dir(), "never replaced");
    }

    #[test]
    fn a_corrupt_file_that_cant_be_moved_aside_is_left_alone() {
        let (_scratch, path) = temp_file();
        fs::write(&path, "{ not json").unwrap();
        let bad = path.with_extension("json.bad");
        fs::create_dir(&bad).unwrap();
        fs::write(bad.join("blocker"), "x").unwrap(); // a non-empty folder can't be renamed over
        let (mut s, notice) = Store::load(path.clone());
        assert!(notice.unwrap().contains("left it alone"));
        assert_eq!(s.path, None);
        s.record(PeerId::test(1), "bobRoger", DAY);
        s.save();
        assert_eq!(fs::read_to_string(&path).unwrap(), "{ not json");
    }

    #[test]
    fn loading_keeps_only_the_most_recently_seen() {
        let (_scratch, path) = temp_file();
        let entries: Vec<Entry> = (0..MAX_ENTRIES as u64 + 5)
            .map(|i| Entry { key: PeerId::test(i + 1), base: "bob".into(), last_nick: "bobRoger".into(), last_seen: i })
            .collect();
        fs::write(&path, serde_json::to_vec(&entries).unwrap()).unwrap();
        let (s, notice) = Store::load(path);
        assert_eq!(notice, None);
        assert_eq!(s.entries.len(), MAX_ENTRIES);
        assert!(!s.entries.iter().any(|e| e.last_seen < 5), "the five oldest went");
    }

    #[test]
    fn an_in_memory_store_writes_nothing() {
        let mut s = Store::in_memory();
        s.record(PeerId::test(1), "bobRoger", DAY);
        s.save(); // no path: nothing to do, no panic
        assert_eq!(s.entries.len(), 1);
    }
}
