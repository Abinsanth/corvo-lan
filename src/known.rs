//! Peers we've met, remembered by key, so a familiar name with a new key stands out.

use std::fs;
use std::io::Write;
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
    /// A new key under a base we've only met with other keys, none of them online now.
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

    /// A missing or unreadable file is an empty store. A corrupt one is moved
    /// to `.bad` so it isn't overwritten, and the notice says so.
    pub fn load(path: PathBuf) -> (Store, Option<String>) {
        let Ok(bytes) = fs::read(&path) else {
            return (Store { path: Some(path), entries: Vec::new() }, None);
        };
        match serde_json::from_slice(&bytes) {
            Ok(entries) => (Store { path: Some(path), entries }, None),
            Err(_) => {
                let bad = path.with_extension("json.bad");
                let _ = fs::rename(&path, &bad);
                let notice = format!(
                    "{} was unreadable; moved it to {} and started a new list of known peers",
                    path.display(),
                    bad.display()
                );
                (Store { path: Some(path), entries: Vec::new() }, Some(notice))
            }
        }
    }

    pub fn judge(&self, key: PeerId, nick: &str, online: &[PeerId]) -> Judgement {
        if self.entries.iter().any(|e| e.key == key) {
            return Judgement::Known;
        }
        let base = names::base_of(nick).unwrap_or(nick);
        let others: Vec<&Entry> = self.entries.iter().filter(|e| e.base == base).collect();
        if others.is_empty() || others.iter().any(|e| online.contains(&e.key)) {
            return Judgement::FirstMeeting;
        }
        Judgement::KeyChanged { last_seen: others.iter().map(|e| e.last_seen).max().unwrap_or(0) }
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

    fn temp_file() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("corvo-known-{:016x}", rand::random::<u64>()));
        fs::create_dir_all(&dir).unwrap();
        dir.join("known_peers.json")
    }

    fn met(entries: &[(u64, &str, u64)]) -> Store {
        let mut s = Store::in_memory();
        for &(key, nick, at) in entries {
            s.record(PeerId::test(key), nick, at);
        }
        s
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
        let path = temp_file();
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
        let path = temp_file();
        fs::write(&path, "{ not json").unwrap();
        let (s, notice) = Store::load(path.clone());
        assert!(s.entries.is_empty());
        assert!(notice.unwrap().contains("known_peers.json.bad"));
        assert_eq!(fs::read_to_string(path.with_extension("json.bad")).unwrap(), "{ not json");
        assert!(!path.exists());
    }

    #[test]
    fn an_in_memory_store_writes_nothing() {
        let mut s = Store::in_memory();
        s.record(PeerId::test(1), "bobRoger", DAY);
        s.save(); // no path: nothing to do, no panic
        assert_eq!(s.entries.len(), 1);
    }
}
