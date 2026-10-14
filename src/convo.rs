//! Conversations (rooms and DMs) built purely from `Event`s. UI-independent:
//! the line UI uses it now and the v0.3 TUI will reuse it unchanged.

use std::collections::{BTreeSet, VecDeque};

use crate::api::{Conv, Event};

/// Lines kept per conversation.
pub const MAX_LINES: usize = 1000;
pub const MAX_CONVS: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineKind {
    Msg { from: String, text: String },
    Mine { text: String },
    Notice(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    /// Unix millis.
    pub ts: u64,
    pub kind: LineKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conversation {
    pub conv: Conv,
    pub lines: VecDeque<Line>,
    pub unread: u32,
    /// Unread messages that mention me.
    pub mentions: u32,
    /// DM partner is no longer connected.
    pub offline: bool,
    /// Room members other than us.
    pub members: BTreeSet<String>,
}

#[derive(Debug, Default)]
pub struct Store {
    me: Option<String>,
    /// Oldest first.
    convs: Vec<Conversation>,
    current: Option<Conv>,
    online: BTreeSet<String>,
}

/// Whether `text` mentions `@me`: the whole name, ignoring case, not part of
/// a longer word on either side.
pub fn mentions(text: &str, me: &str) -> bool {
    let word = |c: char| c.is_alphanumeric() || c == '_';
    let (text, needle) = (text.to_lowercase(), format!("@{}", me.to_lowercase()));
    text.match_indices(&needle).any(|(i, _)| {
        !text[..i].chars().next_back().is_some_and(word) && !text[i + needle.len()..].chars().next().is_some_and(word)
    })
}

impl Store {
    pub fn new() -> Self {
        Store::default()
    }

    pub fn me(&self) -> Option<&str> {
        self.me.as_deref()
    }

    pub fn current(&self) -> Option<&Conv> {
        self.current.as_ref()
    }

    pub fn conversations(&self) -> &[Conversation] {
        &self.convs
    }

    pub fn get(&self, conv: &Conv) -> Option<&Conversation> {
        self.convs.iter().find(|c| &c.conv == conv)
    }

    /// People online right now, in name order.
    pub fn online(&self) -> impl Iterator<Item = &String> {
        self.online.iter()
    }

    /// Forgets a conversation (the TUI's `/close`); a later message reopens it.
    pub fn close(&mut self, conv: &Conv) {
        self.convs.retain(|c| &c.conv != conv);
        if self.current.as_ref() == Some(conv) {
            self.current = None;
        }
    }

    /// Views no conversation (the TUI's corvo tab), so every message counts as unread.
    pub fn clear_current(&mut self) {
        self.current = None;
    }

    pub fn is_online(&self, nick: &str) -> bool {
        self.online.contains(nick)
    }

    /// Makes `conv` current (creating it if needed) and marks it read.
    pub fn switch(&mut self, conv: Conv) {
        let c = self.entry(&conv);
        c.unread = 0;
        c.mentions = 0;
        self.current = Some(conv);
    }

    /// The last `n` lines of the current conversation, oldest first; marks it read.
    pub fn history(&mut self, n: usize) -> Vec<Line> {
        let Some(conv) = self.current.clone() else {
            return Vec::new();
        };
        let c = self.entry(&conv);
        c.unread = 0;
        c.mentions = 0;
        let skip = c.lines.len().saturating_sub(n);
        c.lines.iter().skip(skip).cloned().collect()
    }

    /// Folds one event into the store. `now` (unix millis) stamps notices.
    pub fn apply(&mut self, event: &Event, now: u64) {
        match event {
            Event::Assigned { nick, .. } => self.me = Some(nick.clone()),
            Event::PeerJoined { nick, .. } => {
                self.online.insert(nick.clone());
                if let Some(c) = self.find_mut(&Conv::Dm(nick.clone())) {
                    c.offline = false;
                }
            }
            Event::PeerLeft { nick, .. } => {
                self.online.remove(nick);
                if let Some(c) = self.find_mut(&Conv::Dm(nick.clone())) {
                    c.offline = true;
                }
            }
            Event::Renamed { old, new } => self.rename(old, new),
            Event::Message { conv, from, text, ts } => {
                let unread = self.current.as_ref() != Some(conv);
                let mention = self.me.as_deref().is_some_and(|me| mentions(text, me));
                let c = self.entry(conv);
                push(c, Line { ts: *ts, kind: LineKind::Msg { from: from.clone(), text: text.clone() } });
                if unread {
                    c.unread += 1;
                    c.mentions += u32::from(mention);
                }
            }
            Event::Sent { conv, text, ts } => {
                push(self.entry(conv), Line { ts: *ts, kind: LineKind::Mine { text: text.clone() } })
            }
            Event::RoomCreated { name, key, .. } => {
                let c = self.entry(&Conv::Room(name.clone()));
                push(
                    c,
                    Line {
                        ts: now,
                        kind: LineKind::Notice(format!("created #{name} — key {key} — share both to invite")),
                    },
                );
            }
            Event::RoomJoined { name, members, .. } => {
                let c = self.entry(&Conv::Room(name.clone()));
                c.members = members.iter().cloned().collect();
                push(c, Line { ts: now, kind: LineKind::Notice(format!("joined #{name}")) });
            }
            Event::RoomLeft { name } => {
                let conv = Conv::Room(name.clone());
                self.convs.retain(|c| c.conv != conv);
                if self.current.as_ref() == Some(&conv) {
                    self.current = None;
                }
            }
            Event::MemberJoined { room, nick } => {
                if let Some(c) = self.find_mut(&Conv::Room(room.clone())) {
                    c.members.insert(nick.clone());
                    push(c, Line { ts: now, kind: LineKind::Notice(format!("@{nick} joined")) });
                }
            }
            Event::MemberLeft { room, nick } => {
                if let Some(c) = self.find_mut(&Conv::Room(room.clone())) {
                    c.members.remove(nick);
                    push(c, Line { ts: now, kind: LineKind::Notice(format!("@{nick} left")) });
                }
            }
            Event::Peers { .. } | Event::Error(_) | Event::Info(_) => {}
            Event::JoinWaiting { .. }
            | Event::JoinEnded { .. }
            | Event::JoinRequest { .. }
            | Event::Approved { .. }
            | Event::RequestEnded { .. } => {}
        }
    }

    /// A peer re-rolled its name: follow it in room member lists and move its
    /// DM, merging into any older conversation already under the new name.
    fn rename(&mut self, old: &str, new: &str) {
        if self.online.remove(old) {
            self.online.insert(new.to_string());
        }
        for c in &mut self.convs {
            if c.members.remove(old) {
                c.members.insert(new.to_string());
            }
        }
        let (from, to) = (Conv::Dm(old.to_string()), Conv::Dm(new.to_string()));
        if self.current.as_ref() == Some(&from) {
            self.current = Some(to.clone());
        }
        let Some(i) = self.convs.iter().position(|c| c.conv == from) else {
            return;
        };
        let mut moved = self.convs.remove(i);
        let offline = !self.online.contains(new);
        match self.find_mut(&to) {
            Some(c) => {
                c.lines.extend(moved.lines);
                c.lines.make_contiguous().sort_by_key(|l| l.ts);
                let excess = c.lines.len().saturating_sub(MAX_LINES);
                c.lines.drain(..excess);
                c.unread += moved.unread;
                c.mentions += moved.mentions;
                c.offline = offline;
            }
            None => {
                moved.conv = to;
                moved.offline = offline;
                self.convs.insert(i, moved);
            }
        }
    }

    fn find_mut(&mut self, conv: &Conv) -> Option<&mut Conversation> {
        self.convs.iter_mut().find(|c| &c.conv == conv)
    }

    fn entry(&mut self, conv: &Conv) -> &mut Conversation {
        if let Some(i) = self.convs.iter().position(|c| &c.conv == conv) {
            return &mut self.convs[i];
        }
        if self.convs.len() >= MAX_CONVS {
            self.evict();
        }
        let offline = matches!(conv, Conv::Dm(n) if !self.online.contains(n));
        self.convs.push(Conversation {
            conv: conv.clone(),
            lines: VecDeque::new(),
            unread: 0,
            mentions: 0,
            offline,
            members: BTreeSet::new(),
        });
        self.convs.last_mut().expect("just pushed")
    }

    /// Drops the oldest offline DM, or failing that the oldest non-current conversation.
    fn evict(&mut self) {
        let current = self.current.clone();
        let victim = self
            .convs
            .iter()
            .position(|c| matches!(c.conv, Conv::Dm(_)) && c.offline)
            .or_else(|| self.convs.iter().position(|c| Some(&c.conv) != current.as_ref()));
        if let Some(i) = victim {
            self.convs.remove(i);
        }
    }
}

fn push(c: &mut Conversation, line: Line) {
    if c.lines.len() == MAX_LINES {
        c.lines.pop_front();
    }
    c.lines.push_back(line);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{LeaveReason, PeerId};

    fn room(n: &str) -> Conv {
        Conv::Room(n.into())
    }

    fn dm(n: &str) -> Conv {
        Conv::Dm(n.into())
    }

    fn msg(conv: Conv, from: &str, text: &str) -> Event {
        Event::Message { conv, from: from.into(), text: text.into(), ts: 1 }
    }

    #[test]
    fn messages_outside_the_current_conversation_count_as_unread() {
        let mut s = Store::new();
        s.switch(room("catPieR"));
        s.apply(&msg(room("catPieR"), "bobRoger", "in view"), 0);
        s.apply(&msg(dm("bobRoger"), "bobRoger", "psst"), 0);
        s.apply(&msg(dm("bobRoger"), "bobRoger", "psst again"), 0);
        assert_eq!(s.get(&room("catPieR")).unwrap().unread, 0);
        assert_eq!(s.get(&dm("bobRoger")).unwrap().unread, 2);
        s.switch(dm("bobRoger"));
        assert_eq!(s.get(&dm("bobRoger")).unwrap().unread, 0);
        assert_eq!(s.current(), Some(&dm("bobRoger")));
    }

    #[test]
    fn history_returns_the_tail_and_marks_read() {
        let mut s = Store::new();
        s.switch(room("catPieR"));
        s.switch(dm("bobRoger"));
        for i in 0..5 {
            s.apply(&msg(room("catPieR"), "bobRoger", &format!("m{i}")), 0);
        }
        s.apply(&Event::Sent { conv: room("catPieR"), text: "mine".into(), ts: 2 }, 0);
        s.switch(room("catPieR"));
        s.apply(&msg(room("catPieR"), "bobRoger", "late"), 0);
        let tail = s.history(2);
        assert_eq!(tail[0].kind, LineKind::Mine { text: "mine".into() });
        assert_eq!(tail[1].kind, LineKind::Msg { from: "bobRoger".into(), text: "late".into() });
        assert_eq!(s.get(&room("catPieR")).unwrap().unread, 0);
        assert_eq!(s.history(100).len(), 7);
    }

    #[test]
    fn lines_are_capped_per_conversation() {
        let mut s = Store::new();
        for i in 0..MAX_LINES + 5 {
            s.apply(&msg(dm("bobRoger"), "bobRoger", &i.to_string()), 0);
        }
        let c = s.get(&dm("bobRoger")).unwrap();
        assert_eq!(c.lines.len(), MAX_LINES);
        assert_eq!(c.lines[0].kind, LineKind::Msg { from: "bobRoger".into(), text: "5".into() });
    }

    #[test]
    fn dm_partners_go_offline_and_back() {
        let mut s = Store::new();
        let id = PeerId::test(1);
        s.apply(&Event::PeerJoined { id, nick: "bobRoger".into() }, 0);
        s.switch(dm("bobRoger"));
        assert!(!s.get(&dm("bobRoger")).unwrap().offline);
        s.apply(&Event::PeerLeft { id, nick: "bobRoger".into(), reason: LeaveReason::Lost }, 0);
        assert!(s.get(&dm("bobRoger")).unwrap().offline);
        assert!(!s.is_online("bobRoger"));
        s.apply(&Event::PeerJoined { id, nick: "bobRoger".into() }, 0);
        assert!(!s.get(&dm("bobRoger")).unwrap().offline);
        // A DM opened with someone not online starts offline.
        s.switch(dm("ghostMoon"));
        assert!(s.get(&dm("ghostMoon")).unwrap().offline);
    }

    #[test]
    fn rooms_track_members_and_vanish_on_leave() {
        let mut s = Store::new();
        s.apply(
            &Event::RoomJoined {
                name: "catPieR".into(),
                key: "k7m9xq".into(),
                max: 13,
                members: vec!["bobRoger".into()],
            },
            0,
        );
        s.switch(room("catPieR"));
        s.apply(&Event::MemberJoined { room: "catPieR".into(), nick: "carolFox".into() }, 0);
        s.apply(&Event::MemberLeft { room: "catPieR".into(), nick: "bobRoger".into() }, 0);
        let c = s.get(&room("catPieR")).unwrap();
        assert_eq!(c.members.iter().cloned().collect::<Vec<_>>(), vec!["carolFox".to_string()]);
        assert!(c.lines.iter().any(|l| l.kind == LineKind::Notice("@carolFox joined".into())));
        s.apply(&Event::RoomLeft { name: "catPieR".into() }, 0);
        assert!(s.get(&room("catPieR")).is_none());
        assert_eq!(s.current(), None);
    }

    #[test]
    fn full_store_evicts_the_oldest_offline_dm_first() {
        let mut s = Store::new();
        s.switch(dm("keepFox")); // offline, but older ones go first
        for i in 0..MAX_CONVS - 1 {
            s.apply(&Event::PeerJoined { id: PeerId::test(i as u64), nick: format!("p{i}Moon") }, 0);
            s.switch(dm(&format!("p{i}Moon")));
        }
        s.switch(room("catPieR")); // store is full: keepFox is the only offline DM
        assert_eq!(s.conversations().len(), MAX_CONVS);
        assert!(s.get(&dm("keepFox")).is_none());
        assert!(s.get(&room("catPieR")).is_some());
    }

    #[test]
    fn a_renamed_peer_keeps_its_dm_and_room_membership() {
        let mut s = Store::new();
        s.apply(&Event::PeerJoined { id: PeerId::test(1), nick: "bobRoger".into() }, 0);
        s.apply(
            &Event::RoomJoined {
                name: "catPieR".into(),
                key: "k7m9xq".into(),
                max: 13,
                members: vec!["bobRoger".into()],
            },
            0,
        );
        s.apply(&msg(dm("bobRoger"), "bobRoger", "psst"), 0);
        s.switch(dm("bobRoger"));
        s.apply(&Event::Renamed { old: "bobRoger".into(), new: "bobPie".into() }, 0);

        assert!(s.get(&dm("bobRoger")).is_none());
        let c = s.get(&dm("bobPie")).unwrap();
        assert_eq!(c.lines[0].kind, LineKind::Msg { from: "bobRoger".into(), text: "psst".into() });
        assert!(!c.offline);
        assert_eq!(s.current(), Some(&dm("bobPie")));
        assert!(s.is_online("bobPie") && !s.is_online("bobRoger"));
        let members: Vec<_> = s.get(&room("catPieR")).unwrap().members.iter().cloned().collect();
        assert_eq!(members, vec!["bobPie".to_string()]);
    }

    #[test]
    fn a_rename_onto_an_old_conversation_merges_them_in_order() {
        let mut s = Store::new();
        let at = |conv, text: &str, ts| Event::Message { conv, from: "x".into(), text: text.into(), ts };
        s.apply(&at(dm("bobPie"), "older bobPie", 1), 0); // someone who went offline
        s.apply(&Event::PeerJoined { id: PeerId::test(1), nick: "bobRoger".into() }, 0);
        s.apply(&at(dm("bobRoger"), "from bobRoger", 2), 0);
        s.apply(&Event::Renamed { old: "bobRoger".into(), new: "bobPie".into() }, 0);

        assert!(s.get(&dm("bobRoger")).is_none());
        let c = s.get(&dm("bobPie")).unwrap();
        let ts: Vec<u64> = c.lines.iter().map(|l| l.ts).collect();
        assert_eq!(ts, vec![1, 2]);
        assert_eq!(c.unread, 2);
        assert!(!c.offline);
    }

    #[test]
    fn mentions_need_the_whole_name() {
        assert!(mentions("hi @bobRoger", "bobRoger"));
        assert!(mentions("@BOBROGER, look", "bobRoger"));
        assert!(mentions("(@bobRoger)", "bobRoger"));
        assert!(mentions("line one\n@bobRoger", "bobRoger"));
        assert!(!mentions("@bobRogers hi", "bobRoger"));
        assert!(!mentions("@bobRoger_x", "bobRoger"));
        assert!(!mentions("x@bobRoger", "bobRoger"));
        assert!(!mentions("bobRoger", "bobRoger"));
        assert!(mentions("é @bobRoger", "bobRoger"), "non-ASCII before the name");
    }

    #[test]
    fn unread_mentions_are_counted_and_cleared_with_unread() {
        let mut s = Store::new();
        s.apply(&Event::Assigned { nick: "annMoon".into(), renamed: false }, 0);
        s.switch(room("catPieR"));
        s.apply(&msg(room("catPieR"), "bobRoger", "@annMoon in view"), 0);
        s.apply(&msg(dm("bobRoger"), "bobRoger", "@annMoon psst"), 0);
        s.apply(&msg(dm("bobRoger"), "bobRoger", "no mention"), 0);
        assert_eq!(s.get(&room("catPieR")).unwrap().mentions, 0, "seen as it arrived");
        assert_eq!(s.get(&dm("bobRoger")).unwrap().mentions, 1);
        s.switch(dm("bobRoger"));
        assert_eq!(s.get(&dm("bobRoger")).unwrap().mentions, 0);
    }

    #[test]
    fn mentions_before_i_have_a_name_do_not_count() {
        let mut s = Store::new();
        s.apply(&msg(dm("bobRoger"), "bobRoger", "@annMoon early"), 0);
        s.apply(&Event::Sent { conv: dm("bobRoger"), text: "@annMoon me".into(), ts: 2 }, 0);
        assert_eq!(s.get(&dm("bobRoger")).unwrap().mentions, 0);
    }

    #[test]
    fn a_rename_merge_adds_mentions() {
        let mut s = Store::new();
        s.apply(&Event::Assigned { nick: "annMoon".into(), renamed: false }, 0);
        s.apply(&msg(dm("bobPie"), "bobPie", "@annMoon old"), 0);
        s.apply(&Event::PeerJoined { id: PeerId::test(1), nick: "bobRoger".into() }, 0);
        s.apply(&msg(dm("bobRoger"), "bobRoger", "@annMoon new"), 0);
        s.apply(&Event::Renamed { old: "bobRoger".into(), new: "bobPie".into() }, 0);
        assert_eq!(s.get(&dm("bobPie")).unwrap().mentions, 2);
    }

    #[test]
    fn assigned_name_is_remembered() {
        let mut s = Store::new();
        s.apply(&Event::Assigned { nick: "aliceMoon".into(), renamed: false }, 0);
        assert_eq!(s.me(), Some("aliceMoon"));
    }

    #[test]
    fn closing_a_dm_forgets_it_and_a_new_message_reopens_it() {
        let mut s = Store::new();
        s.apply(&msg(dm("bobRoger"), "bobRoger", "hi"), 0);
        s.switch(dm("bobRoger"));
        s.close(&dm("bobRoger"));
        assert!(s.get(&dm("bobRoger")).is_none());
        assert_eq!(s.current(), None);
        s.apply(&msg(dm("bobRoger"), "bobRoger", "again"), 0);
        assert_eq!(s.get(&dm("bobRoger")).unwrap().lines.len(), 1);
    }

    #[test]
    fn online_people_are_listed_in_order_and_created_rooms_show_their_key() {
        let mut s = Store::new();
        s.apply(&Event::PeerJoined { id: PeerId::test(2), nick: "carolFox".into() }, 0);
        s.apply(&Event::PeerJoined { id: PeerId::test(1), nick: "bobRoger".into() }, 0);
        assert_eq!(s.online().cloned().collect::<Vec<_>>(), vec!["bobRoger", "carolFox"]);
        s.apply(&Event::RoomCreated { name: "catPieR".into(), key: "123-k7m9xq".into(), max: 13 }, 0);
        let c = s.get(&room("catPieR")).unwrap();
        assert_eq!(
            c.lines[0].kind,
            LineKind::Notice("created #catPieR — key 123-k7m9xq — share both to invite".into())
        );
    }
}
