//! Conversations (rooms and DMs) built purely from `Event`s. UI-independent:
//! the line UI uses it now and the v0.3 TUI will reuse it unchanged.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::api::{Conv, Event, RequestEnd};

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
    /// A room's join key and head count limit.
    pub key: Option<String>,
    pub max: Option<u8>,
    /// People asking to join this room: nick → (approvals, approvals needed).
    pub waiting: BTreeMap<String, (u8, u8)>,
}

#[derive(Debug, Default)]
pub struct Store {
    me: Option<String>,
    /// Oldest first.
    convs: Vec<Conversation>,
    current: Option<Conv>,
    online: BTreeSet<String>,
    /// Rooms we've asked to join and are waiting to be let into.
    pending: BTreeSet<String>,
    /// Invitations to us as (from, room), oldest first, capped like the node's.
    invites: Vec<(String, String)>,
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

    pub fn invites(&self) -> &[(String, String)] {
        &self.invites
    }

    pub fn drop_invite(&mut self, room: &str) {
        self.invites.retain(|(_, r)| r != room);
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

    /// Rooms we're waiting to be let into.
    pub fn pending(&self) -> impl Iterator<Item = &String> {
        self.pending.iter()
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
            Event::RoomCreated { name, key, max } => {
                let c = self.entry(&Conv::Room(name.clone()));
                (c.key, c.max) = (Some(key.clone()), Some(*max));
                let text =
                    format!("created #{name} for up to {max} people — /invite @name, or share /chat #{name} {key}");
                push(c, Line { ts: now, kind: LineKind::Notice(text) });
            }
            Event::Invited { from, room } => {
                self.invites.retain(|(_, r)| r != room);
                if self.invites.iter().filter(|(f, _)| f == from).count() >= crate::node::MAX_INVITES_FROM_ONE {
                    let oldest = self.invites.iter().position(|(f, _)| f == from).expect("counted above");
                    self.invites.remove(oldest);
                }
                self.invites.push((from.clone(), room.clone()));
                if self.invites.len() > crate::node::MAX_INVITES {
                    self.invites.remove(0);
                }
            }
            Event::RoomJoined { name, key, max, members } => {
                self.invites.retain(|(_, r)| r != name);
                self.pending.remove(name);
                let c = self.entry(&Conv::Room(name.clone()));
                c.members = members.iter().cloned().collect();
                (c.key, c.max) = (Some(key.clone()), Some(*max));
                push(c, Line { ts: now, kind: LineKind::Notice(format!("joined #{name}")) });
            }
            Event::JoinWaiting { name, .. } => {
                self.invites.retain(|(_, r)| r != name);
                self.pending.insert(name.clone());
            }
            Event::JoinEnded { name, .. } => {
                self.invites.retain(|(_, r)| r != name);
                self.pending.remove(name);
            }
            Event::JoinRequest { room, nick, have, need } => {
                let unread = self.current.as_ref() != Some(&Conv::Room(room.clone()));
                if let Some(c) = self.find_mut(&Conv::Room(room.clone())) {
                    c.waiting.insert(nick.clone(), (*have, *need));
                    let text = format!("@{nick} wants to join — /approve @{nick} or /deny @{nick}");
                    push(c, Line { ts: now, kind: LineKind::Notice(text) });
                    c.unread += u32::from(unread);
                }
            }
            Event::Approved { room, nick, by, have, need } => {
                if let Some(c) = self.find_mut(&Conv::Room(room.clone())) {
                    c.waiting.insert(nick.clone(), (*have, *need));
                    let text = format!("@{by} approved @{nick} ({have}/{need})");
                    push(c, Line { ts: now, kind: LineKind::Notice(text) });
                }
            }
            Event::RequestEnded { room, nick, why } => {
                if let Some(c) = self.find_mut(&Conv::Room(room.clone())) {
                    c.waiting.remove(nick);
                    let text = match why {
                        RequestEnd::Admitted => None,
                        RequestEnd::Denied { by } => Some(format!("@{by} turned @{nick} away")),
                        RequestEnd::Full => Some(format!("@{nick} couldn't join — the room is full")),
                        RequestEnd::Gone => Some(format!("@{nick} stopped waiting")),
                    };
                    if let Some(text) = text {
                        push(c, Line { ts: now, kind: LineKind::Notice(text) });
                    }
                }
            }
            Event::RoomLeft { name } => {
                self.pending.remove(name);
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
            Event::Rekeyed { room } => {
                if let Some(c) = self.find_mut(&Conv::Room(room.clone())) {
                    push(c, Line { ts: now, kind: LineKind::Notice("key changed".into()) });
                }
            }
            Event::Peers { .. }
            | Event::Error(_)
            | Event::Info(_)
            | Event::NamePreview { .. }
            | Event::InviteDeclined { .. } => {}
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
            if let Some(votes) = c.waiting.remove(old) {
                c.waiting.insert(new.to_string(), votes);
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
            key: None,
            max: None,
            waiting: BTreeMap::new(),
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
    use crate::api::{JoinEnd, LeaveReason, PeerId, RequestEnd};

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
        s.switch(room("catpie"));
        s.apply(&msg(room("catpie"), "bobroger", "in view"), 0);
        s.apply(&msg(dm("bobroger"), "bobroger", "psst"), 0);
        s.apply(&msg(dm("bobroger"), "bobroger", "psst again"), 0);
        assert_eq!(s.get(&room("catpie")).unwrap().unread, 0);
        assert_eq!(s.get(&dm("bobroger")).unwrap().unread, 2);
        s.switch(dm("bobroger"));
        assert_eq!(s.get(&dm("bobroger")).unwrap().unread, 0);
        assert_eq!(s.current(), Some(&dm("bobroger")));
    }

    #[test]
    fn history_returns_the_tail_and_marks_read() {
        let mut s = Store::new();
        s.switch(room("catpie"));
        s.switch(dm("bobroger"));
        for i in 0..5 {
            s.apply(&msg(room("catpie"), "bobroger", &format!("m{i}")), 0);
        }
        s.apply(&Event::Sent { conv: room("catpie"), text: "mine".into(), ts: 2 }, 0);
        s.switch(room("catpie"));
        s.apply(&msg(room("catpie"), "bobroger", "late"), 0);
        let tail = s.history(2);
        assert_eq!(tail[0].kind, LineKind::Mine { text: "mine".into() });
        assert_eq!(tail[1].kind, LineKind::Msg { from: "bobroger".into(), text: "late".into() });
        assert_eq!(s.get(&room("catpie")).unwrap().unread, 0);
        assert_eq!(s.history(100).len(), 7);
    }

    #[test]
    fn lines_are_capped_per_conversation() {
        let mut s = Store::new();
        for i in 0..MAX_LINES + 5 {
            s.apply(&msg(dm("bobroger"), "bobroger", &i.to_string()), 0);
        }
        let c = s.get(&dm("bobroger")).unwrap();
        assert_eq!(c.lines.len(), MAX_LINES);
        assert_eq!(c.lines[0].kind, LineKind::Msg { from: "bobroger".into(), text: "5".into() });
    }

    #[test]
    fn dm_partners_go_offline_and_back() {
        let mut s = Store::new();
        let id = PeerId::test(1);
        s.apply(&Event::PeerJoined { id, nick: "bobroger".into() }, 0);
        s.switch(dm("bobroger"));
        assert!(!s.get(&dm("bobroger")).unwrap().offline);
        s.apply(&Event::PeerLeft { id, nick: "bobroger".into(), reason: LeaveReason::Lost }, 0);
        assert!(s.get(&dm("bobroger")).unwrap().offline);
        assert!(!s.is_online("bobroger"));
        s.apply(&Event::PeerJoined { id, nick: "bobroger".into() }, 0);
        assert!(!s.get(&dm("bobroger")).unwrap().offline);
        // A DM opened with someone not online starts offline.
        s.switch(dm("ghostmoon"));
        assert!(s.get(&dm("ghostmoon")).unwrap().offline);
    }

    #[test]
    fn a_rekey_is_noted_in_the_room() {
        let mut s = Store::new();
        s.apply(&Event::RoomCreated { name: "catpie".into(), key: "k7m9xq".into(), max: 13 }, 0);
        s.apply(&Event::Rekeyed { room: "catpie".into() }, 0);
        let c = s.get(&room("catpie")).unwrap();
        assert!(c.lines.iter().any(|l| l.kind == LineKind::Notice("key changed".into())));
    }

    #[test]
    fn rooms_track_members_and_vanish_on_leave() {
        let mut s = Store::new();
        s.apply(
            &Event::RoomJoined {
                name: "catpie".into(),
                key: "k7m9xq".into(),
                max: 13,
                members: vec!["bobroger".into()],
            },
            0,
        );
        s.switch(room("catpie"));
        s.apply(&Event::MemberJoined { room: "catpie".into(), nick: "carolwolf".into() }, 0);
        s.apply(&Event::MemberLeft { room: "catpie".into(), nick: "bobroger".into() }, 0);
        let c = s.get(&room("catpie")).unwrap();
        assert_eq!(c.members.iter().cloned().collect::<Vec<_>>(), vec!["carolwolf".to_string()]);
        assert!(c.lines.iter().any(|l| l.kind == LineKind::Notice("@carolwolf joined".into())));
        s.apply(&Event::RoomLeft { name: "catpie".into() }, 0);
        assert!(s.get(&room("catpie")).is_none());
        assert_eq!(s.current(), None);
    }

    #[test]
    fn full_store_evicts_the_oldest_offline_dm_first() {
        let mut s = Store::new();
        s.switch(dm("keepwolf")); // offline, but older ones go first
        for i in 0..MAX_CONVS - 1 {
            s.apply(&Event::PeerJoined { id: PeerId::test(i as u64), nick: format!("p{i}moon") }, 0);
            s.switch(dm(&format!("p{i}moon")));
        }
        s.switch(room("catpie")); // store is full: keepwolf is the only offline DM
        assert_eq!(s.conversations().len(), MAX_CONVS);
        assert!(s.get(&dm("keepwolf")).is_none());
        assert!(s.get(&room("catpie")).is_some());
    }

    #[test]
    fn a_renamed_peer_keeps_its_dm_and_room_membership() {
        let mut s = Store::new();
        s.apply(&Event::PeerJoined { id: PeerId::test(1), nick: "bobroger".into() }, 0);
        s.apply(
            &Event::RoomJoined {
                name: "catpie".into(),
                key: "k7m9xq".into(),
                max: 13,
                members: vec!["bobroger".into()],
            },
            0,
        );
        s.apply(&msg(dm("bobroger"), "bobroger", "psst"), 0);
        s.switch(dm("bobroger"));
        s.apply(&Event::Renamed { old: "bobroger".into(), new: "bobpie".into() }, 0);

        assert!(s.get(&dm("bobroger")).is_none());
        let c = s.get(&dm("bobpie")).unwrap();
        assert_eq!(c.lines[0].kind, LineKind::Msg { from: "bobroger".into(), text: "psst".into() });
        assert!(!c.offline);
        assert_eq!(s.current(), Some(&dm("bobpie")));
        assert!(s.is_online("bobpie") && !s.is_online("bobroger"));
        let members: Vec<_> = s.get(&room("catpie")).unwrap().members.iter().cloned().collect();
        assert_eq!(members, vec!["bobpie".to_string()]);
    }

    #[test]
    fn a_rename_onto_an_old_conversation_merges_them_in_order() {
        let mut s = Store::new();
        let at = |conv, text: &str, ts| Event::Message { conv, from: "x".into(), text: text.into(), ts };
        s.apply(&at(dm("bobpie"), "older bobpie", 1), 0); // someone who went offline
        s.apply(&Event::PeerJoined { id: PeerId::test(1), nick: "bobroger".into() }, 0);
        s.apply(&at(dm("bobroger"), "from bobroger", 2), 0);
        s.apply(&Event::Renamed { old: "bobroger".into(), new: "bobpie".into() }, 0);

        assert!(s.get(&dm("bobroger")).is_none());
        let c = s.get(&dm("bobpie")).unwrap();
        let ts: Vec<u64> = c.lines.iter().map(|l| l.ts).collect();
        assert_eq!(ts, vec![1, 2]);
        assert_eq!(c.unread, 2);
        assert!(!c.offline);
    }

    #[test]
    fn mentions_need_the_whole_name() {
        assert!(mentions("hi @bobroger", "bobroger"));
        assert!(mentions("@BOBROGER, look", "bobroger"));
        assert!(mentions("(@bobroger)", "bobroger"));
        assert!(mentions("line one\n@bobroger", "bobroger"));
        assert!(!mentions("@bobrogers hi", "bobroger"));
        assert!(!mentions("@bobroger_x", "bobroger"));
        assert!(!mentions("x@bobroger", "bobroger"));
        assert!(!mentions("bobroger", "bobroger"));
        assert!(mentions("é @bobroger", "bobroger"), "non-ASCII before the name");
    }

    #[test]
    fn unread_mentions_are_counted_and_cleared_with_unread() {
        let mut s = Store::new();
        s.apply(&Event::Assigned { nick: "annmoon".into(), renamed: false }, 0);
        s.switch(room("catpie"));
        s.apply(&msg(room("catpie"), "bobroger", "@annmoon in view"), 0);
        s.apply(&msg(dm("bobroger"), "bobroger", "@annmoon psst"), 0);
        s.apply(&msg(dm("bobroger"), "bobroger", "no mention"), 0);
        assert_eq!(s.get(&room("catpie")).unwrap().mentions, 0, "seen as it arrived");
        assert_eq!(s.get(&dm("bobroger")).unwrap().mentions, 1);
        s.switch(dm("bobroger"));
        assert_eq!(s.get(&dm("bobroger")).unwrap().mentions, 0);
    }

    #[test]
    fn mentions_before_i_have_a_name_do_not_count() {
        let mut s = Store::new();
        s.apply(&msg(dm("bobroger"), "bobroger", "@annmoon early"), 0);
        s.apply(&Event::Sent { conv: dm("bobroger"), text: "@annmoon me".into(), ts: 2 }, 0);
        assert_eq!(s.get(&dm("bobroger")).unwrap().mentions, 0);
    }

    #[test]
    fn a_rename_merge_adds_mentions() {
        let mut s = Store::new();
        s.apply(&Event::Assigned { nick: "annmoon".into(), renamed: false }, 0);
        s.apply(&msg(dm("bobpie"), "bobpie", "@annmoon old"), 0);
        s.apply(&Event::PeerJoined { id: PeerId::test(1), nick: "bobroger".into() }, 0);
        s.apply(&msg(dm("bobroger"), "bobroger", "@annmoon new"), 0);
        s.apply(&Event::Renamed { old: "bobroger".into(), new: "bobpie".into() }, 0);
        assert_eq!(s.get(&dm("bobpie")).unwrap().mentions, 2);
    }

    #[test]
    fn assigned_name_is_remembered() {
        let mut s = Store::new();
        s.apply(&Event::Assigned { nick: "alicemoon".into(), renamed: false }, 0);
        assert_eq!(s.me(), Some("alicemoon"));
    }

    #[test]
    fn closing_a_dm_forgets_it_and_a_new_message_reopens_it() {
        let mut s = Store::new();
        s.apply(&msg(dm("bobroger"), "bobroger", "hi"), 0);
        s.switch(dm("bobroger"));
        s.close(&dm("bobroger"));
        assert!(s.get(&dm("bobroger")).is_none());
        assert_eq!(s.current(), None);
        s.apply(&msg(dm("bobroger"), "bobroger", "again"), 0);
        assert_eq!(s.get(&dm("bobroger")).unwrap().lines.len(), 1);
    }

    #[test]
    fn online_people_are_listed_in_order_and_created_rooms_show_their_key() {
        let mut s = Store::new();
        s.apply(&Event::PeerJoined { id: PeerId::test(2), nick: "carolwolf".into() }, 0);
        s.apply(&Event::PeerJoined { id: PeerId::test(1), nick: "bobroger".into() }, 0);
        assert_eq!(s.online().cloned().collect::<Vec<_>>(), vec!["bobroger", "carolwolf"]);
        s.apply(&Event::RoomCreated { name: "catpie".into(), key: "k7m9xq".into(), max: 5 }, 0);
        let c = s.get(&room("catpie")).unwrap();
        assert_eq!(
            c.lines[0].kind,
            LineKind::Notice(
                "created #catpie for up to 5 people — /invite @name, or share /chat #catpie k7m9xq".into()
            )
        );
        assert_eq!((c.key.as_deref(), c.max), (Some("k7m9xq"), Some(5)));
    }

    fn notices(s: &Store, conv: &Conv) -> Vec<String> {
        s.get(conv)
            .unwrap()
            .lines
            .iter()
            .filter_map(|l| if let LineKind::Notice(n) = &l.kind { Some(n.clone()) } else { None })
            .collect()
    }

    #[test]
    fn rooms_keep_a_waiting_list_with_approvals() {
        let mut s = Store::new();
        s.apply(&Event::RoomCreated { name: "catpie".into(), key: "k7m9xq".into(), max: 13 }, 0);
        let r = || "catpie".to_string();
        s.apply(&Event::JoinRequest { room: r(), nick: "bobroger".into(), have: 0, need: 2 }, 0);
        s.apply(&Event::JoinRequest { room: r(), nick: "daveoak".into(), have: 0, need: 2 }, 0);
        let c = s.get(&room("catpie")).unwrap();
        assert_eq!(c.waiting.get("bobroger"), Some(&(0, 2)));
        assert_eq!(c.unread, 2, "requests are worth a look from another tab");
        s.apply(&Event::Approved { room: r(), nick: "bobroger".into(), by: "carolwolf".into(), have: 1, need: 2 }, 0);
        s.apply(&Event::Renamed { old: "bobroger".into(), new: "bobpie".into() }, 0);
        assert_eq!(s.get(&room("catpie")).unwrap().waiting.get("bobpie"), Some(&(1, 2)));
        s.apply(&Event::RequestEnded { room: r(), nick: "bobpie".into(), why: RequestEnd::Admitted }, 0);
        s.apply(&Event::RequestEnded { room: r(), nick: "daveoak".into(), why: RequestEnd::Full }, 0);
        assert!(s.get(&room("catpie")).unwrap().waiting.is_empty());
        for (nick, why) in [("eveowl", RequestEnd::Denied { by: "carolwolf".into() }), ("fredelk", RequestEnd::Gone)] {
            s.apply(&Event::JoinRequest { room: r(), nick: nick.into(), have: 0, need: 1 }, 0);
            s.apply(&Event::RequestEnded { room: r(), nick: nick.into(), why }, 0);
        }
        assert_eq!(
            notices(&s, &room("catpie"))[1..],
            [
                "@bobroger wants to join — /approve @bobroger or /deny @bobroger",
                "@daveoak wants to join — /approve @daveoak or /deny @daveoak",
                "@carolwolf approved @bobroger (1/2)",
                "@daveoak couldn't join — the room is full",
                "@eveowl wants to join — /approve @eveowl or /deny @eveowl",
                "@carolwolf turned @eveowl away",
                "@fredelk wants to join — /approve @fredelk or /deny @fredelk",
                "@fredelk stopped waiting",
            ]
        );
    }

    #[test]
    fn our_own_requests_wait_until_they_end() {
        let mut s = Store::new();
        let waiting = || Event::JoinWaiting { name: "catpie".into(), members: 4, needs: 2 };
        s.apply(&waiting(), 0);
        assert_eq!(s.pending().cloned().collect::<Vec<_>>(), vec!["catpie"]);
        s.apply(&Event::JoinEnded { name: "catpie".into(), why: JoinEnd::TimedOut }, 0);
        assert_eq!(s.pending().count(), 0);
        s.apply(&waiting(), 0);
        s.apply(&Event::RoomLeft { name: "catpie".into() }, 0);
        assert_eq!(s.pending().count(), 0);
        s.apply(&waiting(), 0);
        s.apply(&Event::RoomJoined { name: "catpie".into(), key: "k7m9xq".into(), max: 13, members: vec![] }, 0);
        assert_eq!(s.pending().count(), 0);
        assert_eq!(s.get(&room("catpie")).unwrap().key.as_deref(), Some("k7m9xq"));
    }
}
