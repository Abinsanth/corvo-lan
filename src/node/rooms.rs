//! Private-room state and handlers. Knowing a room's name and key is
//! membership: every room frame goes to every peer, and only members can
//! recognise (`room_id`) and read it. See `crate::room` for the crypto.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use tokio::time::Instant;
use tracing::debug;

use super::{Node, NodeMsg, MAX_TEXT_LEN};
use crate::api::{Conv, Event};
use crate::names;
use crate::protocol::{now_millis, PeerId};
use crate::room::{self, ReplayGuard, RoomBody, RoomEnvelope, RoomId, RoomKeys};

pub const MAX_ROOMS: usize = 16;
/// How long a join waits for any member to answer its probe.
pub const JOIN_WAIT: Duration = Duration::from_secs(2);
/// After the first answer, how long to keep collecting the others.
pub const JOIN_SETTLE: Duration = Duration::from_millis(300);
pub const HERE_INTERVAL: Duration = Duration::from_secs(5);
/// A member silent this long is considered gone.
pub const MEMBER_TIMEOUT: Duration = Duration::from_secs(15);

/// What a worker thread was deriving keys for.
pub enum Job {
    Create { name: String, key: String },
    Join { name: String },
}

struct Room {
    name: String,
    keys: RoomKeys,
    /// Other members and when we last heard from them.
    members: HashMap<PeerId, Instant>,
    replay: ReplayGuard,
    told_alone: bool,
}

struct PendingJoin {
    name: String,
    keys: RoomKeys,
    replies: HashSet<PeerId>,
    settling: bool,
    replay: ReplayGuard,
}

#[derive(Default)]
pub(super) struct Rooms {
    joined: HashMap<RoomId, Room>,
    joining: HashMap<RoomId, PendingJoin>,
    /// Names whose keys are being derived right now.
    deriving: HashSet<String>,
    last_here: Option<Instant>,
}

impl Rooms {
    fn count(&self) -> usize {
        self.joined.len() + self.joining.len() + self.deriving.len()
    }

    fn holds(&self, name: &str) -> bool {
        self.deriving.contains(name)
            || self.joined.values().any(|r| r.name == name)
            || self.joining.values().any(|p| p.name == name)
    }

    fn id_of(&self, name: &str) -> Option<RoomId> {
        self.joined.iter().find(|(_, r)| r.name == name).map(|(id, _)| *id)
    }
}

impl Node {
    fn seal(&self, keys: &RoomKeys, body: RoomBody) -> crate::protocol::Frame {
        room::seal(keys, self.id, &RoomEnvelope { ts: now_millis(), body })
    }

    fn spawn_derive(&mut self, job: Job, name: String, key: String) {
        self.rooms.deriving.insert(name.clone());
        let tx = self.msg_tx.clone();
        tokio::spawn(async move {
            let keys = tokio::task::spawn_blocking(move || room::derive(&name, &key)).await;
            if let Ok(keys) = keys {
                let _ = tx.send(NodeMsg::Derived { job, keys }).await;
            }
        });
    }

    fn room_slot_free(&self) -> bool {
        if self.rooms.count() >= MAX_ROOMS {
            self.emit(Event::Error(format!("you're in {MAX_ROOMS} rooms (the max)")));
            return false;
        }
        true
    }

    pub(super) fn create_room(&mut self, base: &str, part: &str) {
        if let Err(e) = names::validate_base(base).and_then(|_| room::validate_part(part)) {
            self.emit(Event::Error(e));
            return;
        }
        if !self.room_slot_free() {
            return;
        }
        let Some(name) = names::pick_room(base, |n| self.rooms.holds(n)) else {
            self.emit(Event::Error(format!("you're already in every #{base}… room")));
            return;
        };
        let key = room::make_key(part);
        self.spawn_derive(Job::Create { name: name.clone(), key: key.clone() }, name, key);
    }

    pub(super) fn join_room(&mut self, name: &str, key: &str) {
        if !names::is_valid_room(name) {
            self.emit(Event::Error(format!("#{name} isn't a room name")));
            return;
        }
        if let Err(e) = room::validate_key(key) {
            self.emit(Event::Error(e));
            return;
        }
        if self.rooms.holds(name) {
            self.emit(Event::Error(format!("you're already in #{name}")));
            return;
        }
        if !self.room_slot_free() {
            return;
        }
        self.spawn_derive(Job::Join { name: name.to_string() }, name.to_string(), key.to_string());
    }

    pub(super) fn on_derived(&mut self, job: Job, keys: RoomKeys) {
        let name = match &job {
            Job::Create { name, .. } | Job::Join { name } => name.clone(),
        };
        self.rooms.deriving.remove(&name);
        if self.rooms.joined.contains_key(&keys.id) || self.rooms.joining.contains_key(&keys.id) {
            self.emit(Event::Error(format!("you're already in #{name}")));
            return;
        }
        match job {
            Job::Create { name, key } => {
                let room = Room {
                    name: name.clone(),
                    keys: keys.clone(),
                    members: HashMap::new(),
                    replay: ReplayGuard::default(),
                    told_alone: false,
                };
                self.rooms.joined.insert(keys.id, room);
                self.emit(Event::RoomCreated { name, key });
            }
            Job::Join { name } => {
                let probe = RoomBody::Probe { nonce: room::to_hex(&rand::random::<[u8; 16]>()) };
                self.broadcast(self.seal(&keys, probe));
                let id = keys.id;
                let pending =
                    PendingJoin { name, keys, replies: HashSet::new(), settling: false, replay: ReplayGuard::default() };
                self.rooms.joining.insert(id, pending);
                self.after(JOIN_WAIT, NodeMsg::JoinTimeout(id));
            }
        }
    }

    pub(super) fn on_join_settled(&mut self, id: RoomId) {
        let Some(p) = self.rooms.joining.remove(&id) else {
            return;
        };
        let now = Instant::now();
        let mut members: Vec<String> = p.replies.iter().filter_map(|m| self.nick_of(*m)).collect();
        members.sort();
        let room = Room {
            name: p.name.clone(),
            keys: p.keys,
            members: p.replies.into_iter().map(|m| (m, now)).collect(),
            replay: p.replay,
            told_alone: false,
        };
        self.broadcast(self.seal(&room.keys, RoomBody::Here { name: room.name.clone() }));
        self.rooms.joined.insert(id, room);
        self.emit(Event::RoomJoined { name: p.name, members });
    }

    pub(super) fn on_join_timeout(&mut self, id: RoomId) {
        if self.rooms.joining.get(&id).is_some_and(|p| !p.settling) {
            if let Some(p) = self.rooms.joining.remove(&id) {
                self.emit(Event::Error(format!("no room #{} with that key", p.name)));
            }
        }
    }

    pub(super) fn leave_room(&mut self, name: &str) {
        let Some(id) = self.rooms.id_of(name) else {
            self.emit(Event::Error(format!("you're not in #{name}")));
            return;
        };
        if let Some(room) = self.rooms.joined.remove(&id) {
            self.broadcast(self.seal(&room.keys, RoomBody::Leave));
            self.emit(Event::RoomLeft { name: room.name });
        }
    }

    pub(super) fn leave_all_rooms(&self) {
        for room in self.rooms.joined.values() {
            self.broadcast(self.seal(&room.keys, RoomBody::Leave));
        }
    }

    pub(super) fn send_room(&mut self, name: &str, text: String) {
        let Some(id) = self.rooms.id_of(name) else {
            self.emit(Event::Error(format!("you're not in #{name}")));
            return;
        };
        let room = &self.rooms.joined[&id];
        let frame = self.seal(&room.keys, RoomBody::Chat { text: text.clone() });
        self.broadcast(frame);
        let alone = room.members.is_empty() && !room.told_alone;
        self.emit(Event::Sent { conv: Conv::Room(name.to_string()), text, ts: now_millis() });
        if alone {
            if let Some(room) = self.rooms.joined.get_mut(&id) {
                room.told_alone = true;
            }
            self.emit(Event::Info(format!("nobody else is in #{name} yet")));
        }
    }

    /// A `Frame::Room` from peer `from`. Silently drops anything that isn't for
    /// one of our rooms, doesn't decrypt, is stale or is a replay.
    pub(super) fn on_room_frame(&mut self, from: PeerId, room_id: &str, nonce: &str, ct: &str, now: Instant) {
        let Some(id) = room::parse_room_id(room_id) else {
            return;
        };
        if let Some(room) = self.rooms.joined.get_mut(&id) {
            let Some((env, nonce)) = room::open(&room.keys, from, nonce, ct) else {
                debug!(%from, "room frame failed to open");
                return;
            };
            if !room.replay.accept(nonce, env.ts, now_millis()) {
                debug!(%from, "stale or replayed room frame");
                return;
            }
            let name = room.name.clone();
            match env.body {
                RoomBody::Chat { text } if text.len() > MAX_TEXT_LEN => {}
                RoomBody::Chat { text } => {
                    self.mark_present(id, from, now);
                    if let Some(nick) = self.nick_of(from) {
                        self.emit(Event::Message { conv: Conv::Room(name), from: nick, text, ts: now_millis() });
                    }
                }
                RoomBody::Probe { .. } => {
                    let keys = self.rooms.joined[&id].keys.clone();
                    self.send_to(from, self.seal(&keys, RoomBody::Here { name }));
                }
                RoomBody::Here { name: claimed } if claimed == name => self.mark_present(id, from, now),
                RoomBody::Here { .. } => {}
                RoomBody::Leave => self.drop_member(id, from),
            }
        } else if let Some(p) = self.rooms.joining.get_mut(&id) {
            let Some((env, nonce)) = room::open(&p.keys, from, nonce, ct) else {
                return;
            };
            if !p.replay.accept(nonce, env.ts, now_millis()) {
                return;
            }
            if matches!(&env.body, RoomBody::Here { name } if *name == p.name) {
                p.replies.insert(from);
                if !std::mem::replace(&mut p.settling, true) {
                    self.after(super::JOIN_SETTLE, NodeMsg::JoinSettled(id));
                }
            }
        }
    }

    fn mark_present(&mut self, id: RoomId, member: PeerId, now: Instant) {
        let Some(nick) = self.nick_of(member) else {
            return;
        };
        let Some(room) = self.rooms.joined.get_mut(&id) else {
            return;
        };
        if room.members.insert(member, now).is_none() {
            let room = room.name.clone();
            self.emit(Event::MemberJoined { room, nick });
        }
    }

    fn drop_member(&mut self, id: RoomId, member: PeerId) {
        let Some(room) = self.rooms.joined.get_mut(&id) else {
            return;
        };
        if room.members.remove(&member).is_some() {
            let room = room.name.clone();
            if let Some(nick) = self.nick_of(member) {
                self.emit(Event::MemberLeft { room, nick });
            }
        }
    }

    /// A peer disconnected: it leaves every room we share with it.
    pub(super) fn forget_member_everywhere(&mut self, member: PeerId) {
        let ids: Vec<RoomId> = self.rooms.joined.keys().copied().collect();
        for id in ids {
            self.drop_member(id, member);
        }
        for p in self.rooms.joining.values_mut() {
            p.replies.remove(&member);
        }
    }

    /// Every second: expire silent members; every `HERE_INTERVAL`: announce presence.
    pub(super) fn on_tick(&mut self, now: Instant) {
        let expired: Vec<(RoomId, PeerId)> = self
            .rooms
            .joined
            .iter()
            .flat_map(|(id, r)| {
                r.members
                    .iter()
                    .filter(|(_, seen)| now.saturating_duration_since(**seen) >= MEMBER_TIMEOUT)
                    .map(|(m, _)| (*id, *m))
            })
            .collect();
        for (id, member) in expired {
            self.drop_member(id, member);
        }
        if self.rooms.last_here.is_none_or(|t| now.saturating_duration_since(t) >= HERE_INTERVAL) {
            self.rooms.last_here = Some(now);
            for room in self.rooms.joined.values() {
                self.broadcast(self.seal(&room.keys, RoomBody::Here { name: room.name.clone() }));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{Harness, BOB, CAROL};
    use super::*;
    use crate::api::{Command, Target};
    use crate::protocol::Frame;

    const NAME: &str = "catPieR";
    const KEY: &str = "123-k7m9xq";

    fn keys() -> RoomKeys {
        room::derive(NAME, KEY)
    }

    /// A room frame as `from` would send it.
    fn frame_from(from: PeerId, keys: &RoomKeys, body: RoomBody) -> Frame {
        room::seal(keys, from, &RoomEnvelope { ts: now_millis(), body })
    }

    fn deliver(h: &mut Harness, from: PeerId, frame: Frame) {
        let conn = h.node.peers[&from].conn;
        h.node.on_frame(conn, from, frame);
    }

    fn open_body(keys: &RoomKeys, sender: PeerId, frame: &Frame) -> RoomBody {
        let Frame::Room { nonce, ct, .. } = frame else { panic!("not a room frame: {frame:?}") };
        room::open(keys, sender, nonce, ct).expect("opens").0.body
    }

    fn with_peers() -> Harness {
        let mut h = Harness::new();
        h.connect(BOB, "bobRoger", true);
        h.connect(CAROL, "carolFox", true);
        h.events();
        h
    }

    async fn create(h: &mut Harness) -> String {
        h.node.handle_command(Command::CreateRoom { base: "cat".into(), key_part: "123".into() });
        h.pump().await;
        match h.events().as_slice() {
            [Event::RoomCreated { name, key }] => {
                assert!(name.starts_with("cat") && names::is_valid_room(name), "{name}");
                assert!(key.starts_with("123-") && room::validate_key(key).is_ok(), "{key}");
                name.clone()
            }
            other => panic!("expected RoomCreated, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn creating_a_room_makes_us_its_only_member() {
        let mut h = with_peers();
        let name = create(&mut h).await;
        h.node.handle_command(Command::Send { to: Target::Room(name.clone()), text: "anyone?".into() });
        // Sent to everyone (only members can read it), recorded, and we're told we're alone.
        assert!(matches!(h.sent(BOB).as_slice(), [Frame::Room { .. }]));
        assert!(matches!(h.sent(CAROL).as_slice(), [Frame::Room { .. }]));
        let events = h.events();
        assert!(matches!(&events[0], Event::Sent { conv: Conv::Room(n), .. } if *n == name));
        assert_eq!(events[1], Event::Info(format!("nobody else is in #{name} yet")));
    }

    #[test]
    fn bad_room_input_is_rejected_up_front() {
        let mut h = with_peers();
        h.node.handle_command(Command::CreateRoom { base: "chat".into(), key_part: "1".into() });
        h.node.handle_command(Command::CreateRoom { base: "cat".into(), key_part: "".into() });
        h.node.handle_command(Command::JoinRoom { name: "catPie".into(), key: KEY.into() });
        h.node.handle_command(Command::JoinRoom { name: NAME.into(), key: "123".into() });
        h.node.handle_command(Command::LeaveRoom { name: NAME.into() });
        h.node.handle_command(Command::Send { to: Target::Room(NAME.into()), text: "x".into() });
        let errors: Vec<String> =
            h.events().into_iter().map(|e| if let Event::Error(m) = e { m } else { panic!("{e:?}") }).collect();
        assert_eq!(errors[0], "'chat' is reserved");
        assert!(errors[1].starts_with("key must be"));
        assert_eq!(errors[2], "#catPie isn't a room name");
        assert!(errors[3].starts_with("key must look like"));
        assert_eq!(errors[4], "you're not in #catPieR");
        assert_eq!(errors[5], "you're not in #catPieR");
        assert!(h.sent(BOB).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn joining_collects_members_then_announces_itself() {
        let mut h = with_peers();
        let keys = keys();
        h.node.handle_command(Command::JoinRoom { name: NAME.into(), key: KEY.into() });
        h.pump().await; // Derived
        // The probe goes to everyone.
        assert!(matches!(open_body(&keys, crate::node::tests::ME, &h.sent(BOB)[0]), RoomBody::Probe { .. }));
        h.sent(CAROL);
        // Both are members and answer.
        deliver(&mut h, BOB, frame_from(BOB, &keys, RoomBody::Here { name: NAME.into() }));
        deliver(&mut h, CAROL, frame_from(CAROL, &keys, RoomBody::Here { name: NAME.into() }));
        h.pump().await; // JoinSettled (the JoinTimeout comes later and is ignored)
        assert_eq!(
            h.events(),
            vec![Event::RoomJoined { name: NAME.into(), members: vec!["bobRoger".into(), "carolFox".into()] }]
        );
        assert!(matches!(open_body(&keys, crate::node::tests::ME, &h.sent(BOB)[0]), RoomBody::Here { .. }));
        h.pump().await; // JoinTimeout: no effect now
        assert!(h.events().is_empty());

        // Now a member: room chat from bob arrives as a room message.
        deliver(&mut h, BOB, frame_from(BOB, &keys, RoomBody::Chat { text: "welcome".into() }));
        assert!(matches!(
            h.events().as_slice(),
            [Event::Message { conv: Conv::Room(n), from, text, .. }] if n == NAME && from == "bobRoger" && text == "welcome"
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn joining_with_a_wrong_key_times_out() {
        let mut h = with_peers();
        h.node.handle_command(Command::JoinRoom { name: NAME.into(), key: "999-k7m9xq".into() });
        h.pump().await; // Derived
        // bob is in the real room; his Here is under the real key, so we can't read it.
        deliver(&mut h, BOB, frame_from(BOB, &keys(), RoomBody::Here { name: NAME.into() }));
        h.pump().await; // JoinTimeout
        assert_eq!(h.events(), vec![Event::Error(format!("no room #{NAME} with that key"))]);
    }

    #[tokio::test]
    async fn members_answer_probes_privately_and_track_presence() {
        let mut h = with_peers();
        let name = create(&mut h).await;
        let id = h.node.rooms.id_of(&name).unwrap();
        let keys = h.node.rooms.joined[&id].keys.clone();

        deliver(&mut h, BOB, frame_from(BOB, &keys, RoomBody::Probe { nonce: "00".into() }));
        assert!(matches!(open_body(&keys, crate::node::tests::ME, &h.sent(BOB)[0]), RoomBody::Here { name: n } if n == name));
        assert!(h.sent(CAROL).is_empty(), "the reply goes only to the prober");
        assert!(h.events().is_empty(), "probing doesn't make you a member");

        deliver(&mut h, BOB, frame_from(BOB, &keys, RoomBody::Here { name: name.clone() }));
        assert_eq!(h.events(), vec![Event::MemberJoined { room: name.clone(), nick: "bobRoger".into() }]);
        deliver(&mut h, BOB, frame_from(BOB, &keys, RoomBody::Here { name: name.clone() }));
        assert!(h.events().is_empty(), "already a member");

        deliver(&mut h, BOB, frame_from(BOB, &keys, RoomBody::Leave));
        assert_eq!(h.events(), vec![Event::MemberLeft { room: name, nick: "bobRoger".into() }]);
    }

    #[tokio::test]
    async fn silent_members_expire_and_presence_is_announced() {
        let mut h = with_peers();
        let name = create(&mut h).await;
        let id = h.node.rooms.id_of(&name).unwrap();
        let keys = h.node.rooms.joined[&id].keys.clone();
        let t0 = Instant::now();
        h.node.on_room_frame(BOB, &room::to_hex(&id), "", "", t0); // junk: ignored
        let Frame::Room { room_id, nonce, ct } = frame_from(BOB, &keys, RoomBody::Here { name: name.clone() }) else {
            unreachable!()
        };
        h.node.on_room_frame(BOB, &room_id, &nonce, &ct, t0);
        h.events();

        h.node.on_tick(t0 + Duration::from_secs(1)); // first tick announces
        assert!(matches!(open_body(&keys, crate::node::tests::ME, &h.sent(CAROL)[0]), RoomBody::Here { .. }));
        h.node.on_tick(t0 + Duration::from_secs(2)); // too soon to announce again
        assert!(h.sent(CAROL).is_empty());
        assert!(h.events().is_empty());
        h.node.on_tick(t0 + MEMBER_TIMEOUT);
        assert_eq!(h.events(), vec![Event::MemberLeft { room: name, nick: "bobRoger".into() }]);
    }

    #[tokio::test]
    async fn replays_unknown_rooms_and_wrong_senders_are_dropped() {
        let mut h = with_peers();
        let name = create(&mut h).await;
        let id = h.node.rooms.id_of(&name).unwrap();
        let keys = h.node.rooms.joined[&id].keys.clone();

        let chat = frame_from(BOB, &keys, RoomBody::Chat { text: "once".into() });
        deliver(&mut h, BOB, chat.clone());
        deliver(&mut h, BOB, chat); // replay
        let messages = h.events().into_iter().filter(|e| matches!(e, Event::Message { .. })).count();
        assert_eq!(messages, 1);

        // Sealed as bob but arriving on carol's connection: fails to open.
        deliver(&mut h, CAROL, frame_from(BOB, &keys, RoomBody::Chat { text: "forged".into() }));
        // A room we're not in.
        deliver(&mut h, BOB, frame_from(BOB, &room::derive("dogPieR", KEY), RoomBody::Chat { text: "x".into() }));
        assert!(h.events().is_empty());
    }

    #[tokio::test]
    async fn leaving_tells_members_and_disconnects_drop_members() {
        let mut h = with_peers();
        let name = create(&mut h).await;
        let id = h.node.rooms.id_of(&name).unwrap();
        let keys = h.node.rooms.joined[&id].keys.clone();
        deliver(&mut h, BOB, frame_from(BOB, &keys, RoomBody::Here { name: name.clone() }));
        h.events();

        let conn = h.node.peers[&BOB].conn;
        h.node.handle_msg(NodeMsg::PeerGone { conn, id: BOB, reason: crate::api::LeaveReason::Lost });
        assert!(h.events().contains(&Event::MemberLeft { room: name.clone(), nick: "bobRoger".into() }));

        h.node.handle_command(Command::LeaveRoom { name: name.clone() });
        assert!(matches!(open_body(&keys, crate::node::tests::ME, &h.sent(CAROL)[0]), RoomBody::Leave));
        assert_eq!(h.events(), vec![Event::RoomLeft { name: name.clone() }]);
        assert!(h.node.rooms.id_of(&name).is_none());
    }

    #[tokio::test]
    async fn room_count_is_capped() {
        let mut h = with_peers();
        for _ in 0..MAX_ROOMS {
            h.node.handle_command(Command::CreateRoom { base: "cat".into(), key_part: "1".into() });
        }
        h.node.handle_command(Command::CreateRoom { base: "cat".into(), key_part: "1".into() });
        assert_eq!(h.events(), vec![Event::Error(format!("you're in {MAX_ROOMS} rooms (the max)"))]);
        for _ in 0..MAX_ROOMS {
            h.pump().await;
        }
        let created = h.events().into_iter().filter(|e| matches!(e, Event::RoomCreated { .. })).count();
        assert_eq!(created, MAX_ROOMS);
    }
}
