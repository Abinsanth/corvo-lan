//! Private-room state and handlers. Knowing a room's name and key is
//! membership: every room frame goes to every peer, and only members can
//! recognise (`room_id`) and read it. See `crate::room` for the crypto.
//!
//! Peer ids are self-claimed until v0.4, so a non-member could replay frames
//! it captured under a member's id. A sender therefore only becomes a member
//! after answering a fresh probe (its `Here` echoes our nonce), and from then
//! on frames older than that sender's floor are dropped.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use tokio::time::Instant;
use tracing::debug;

use super::{MAX_TEXT_LEN, Node, NodeMsg};
use crate::api::{Conv, Event};
use crate::names;
use crate::protocol::{PeerId, now_millis};
use crate::room::{self, ReplayGuard, RoomBody, RoomEnvelope, RoomId, RoomKeys};

pub const MAX_ROOMS: usize = 16;
/// How long a join waits for any member to answer its probe.
pub const JOIN_WAIT: Duration = Duration::from_secs(2);
/// After the first answer, how long to keep collecting the others.
pub const JOIN_SETTLE: Duration = Duration::from_millis(300);
pub const HERE_INTERVAL: Duration = Duration::from_secs(5);
/// A member silent this long is considered gone.
pub const MEMBER_TIMEOUT: Duration = Duration::from_secs(15);
/// How long an unknown sender has to answer our challenge.
pub const CHALLENGE_TIMEOUT: Duration = Duration::from_secs(5);
/// Frames from an unproven sender held while we wait for its answer.
const MAX_BUFFERED: usize = 32;
/// At proof time, accept that sender's frames stamped up to this much earlier
/// (its first messages race its answer to our challenge).
const PROOF_GRACE_MS: u64 = 3_000;
/// After that, a sender's floor trails its newest frame by this much.
const REORDER_GRACE_MS: u64 = 2_000;

/// Numbers each create or join, so a timer or key derivation left over from
/// an earlier (left or failed) attempt at the same room is recognised and ignored.
pub type Attempt = u64;

/// What a worker thread was deriving keys for.
pub enum Job {
    Create { name: String, key: String },
    Join { name: String },
}

struct Member {
    seen: Instant,
    /// Frames from this member stamped earlier than this are replays.
    floor: u64,
}

/// We asked an unknown sender to prove it's live; its frames wait here.
struct Challenge {
    nonce: String,
    started: Instant,
    buffered: Vec<RoomEnvelope>,
}

struct Room {
    name: String,
    keys: RoomKeys,
    /// Other members, all proven.
    members: HashMap<PeerId, Member>,
    challenges: HashMap<PeerId, Challenge>,
    replay: ReplayGuard,
    told_alone: bool,
}

struct PendingJoin {
    attempt: Attempt,
    name: String,
    keys: RoomKeys,
    /// Our probe; only answers echoing it count.
    probe: String,
    /// Who answered, and the timestamp of their answer.
    replies: HashMap<PeerId, u64>,
    settling: bool,
    replay: ReplayGuard,
}

#[derive(Default)]
pub(super) struct Rooms {
    joined: HashMap<RoomId, Room>,
    joining: HashMap<RoomId, PendingJoin>,
    /// Names whose keys are being derived right now, and for which attempt.
    deriving: HashMap<String, Attempt>,
    last_attempt: Attempt,
    /// Peers we've already told the user have a skewed clock.
    skew_warned: HashSet<PeerId>,
    last_here: Option<Instant>,
}

impl Rooms {
    fn count(&self) -> usize {
        self.joined.len() + self.joining.len() + self.deriving.len()
    }

    fn holds(&self, name: &str) -> bool {
        self.deriving.contains_key(name)
            || self.joined.values().any(|r| r.name == name)
            || self.joining.values().any(|p| p.name == name)
    }

    fn id_of(&self, name: &str) -> Option<RoomId> {
        self.joined.iter().find(|(_, r)| r.name == name).map(|(id, _)| *id)
    }
}

fn fresh_nonce() -> String {
    room::to_hex(&rand::random::<[u8; 16]>())
}

fn skewed(ts: u64) -> bool {
    ts.abs_diff(now_millis()) > room::MAX_SKEW_MS
}

impl Node {
    fn seal(&self, keys: &RoomKeys, body: RoomBody) -> crate::protocol::Frame {
        room::seal(keys, self.id, &RoomEnvelope { ts: now_millis(), body })
    }

    fn spawn_derive(&mut self, job: Job, name: String, key: String) {
        self.rooms.last_attempt += 1;
        let attempt = self.rooms.last_attempt;
        self.rooms.deriving.insert(name.clone(), attempt);
        let tx = self.msg_tx.clone();
        tokio::spawn(async move {
            let keys = tokio::task::spawn_blocking(move || room::derive(&name, &key)).await;
            if let Ok(keys) = keys {
                let _ = tx.send(NodeMsg::Derived { job, attempt, keys }).await;
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

    pub(super) fn on_derived(&mut self, job: Job, attempt: Attempt, keys: RoomKeys) {
        let name = match &job {
            Job::Create { name, .. } | Job::Join { name } => name.clone(),
        };
        // Otherwise the user left (or left and retried) while we were deriving.
        if self.rooms.deriving.get(&name) != Some(&attempt) {
            return;
        }
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
                    challenges: HashMap::new(),
                    replay: ReplayGuard::default(),
                    told_alone: false,
                };
                self.rooms.joined.insert(keys.id, room);
                self.emit(Event::RoomCreated { name, key });
            }
            Job::Join { name } => {
                let probe = fresh_nonce();
                self.broadcast(self.seal(&keys, RoomBody::Probe { nonce: probe.clone() }));
                let id = keys.id;
                let pending = PendingJoin {
                    attempt,
                    name,
                    keys,
                    probe,
                    replies: HashMap::new(),
                    settling: false,
                    replay: ReplayGuard::default(),
                };
                self.rooms.joining.insert(id, pending);
                self.after(JOIN_WAIT, NodeMsg::JoinTimeout(id, attempt));
            }
        }
    }

    pub(super) fn on_join_settled(&mut self, id: RoomId, attempt: Attempt) {
        if self.rooms.joining.get(&id).is_none_or(|p| p.attempt != attempt) {
            return;
        }
        let Some(p) = self.rooms.joining.remove(&id) else {
            return;
        };
        let now = Instant::now();
        let mut members: Vec<String> = p.replies.keys().filter_map(|m| self.nick_of(*m)).collect();
        members.sort();
        let room = Room {
            name: p.name.clone(),
            keys: p.keys,
            members: p
                .replies
                .into_iter()
                .map(|(m, ts)| (m, Member { seen: now, floor: ts.saturating_sub(PROOF_GRACE_MS) }))
                .collect(),
            challenges: HashMap::new(),
            replay: p.replay,
            told_alone: false,
        };
        self.broadcast(self.seal(&room.keys, RoomBody::Here { name: room.name.clone(), echo: None }));
        self.rooms.joined.insert(id, room);
        self.emit(Event::RoomJoined { name: p.name, members });
    }

    pub(super) fn on_join_timeout(&mut self, id: RoomId, attempt: Attempt) {
        if self.rooms.joining.get(&id).is_some_and(|p| p.attempt == attempt && !p.settling)
            && let Some(p) = self.rooms.joining.remove(&id)
        {
            self.emit(Event::Error(format!("no room #{} with that key", p.name)));
        }
    }

    /// Also cancels a join of `name` that hasn't finished yet.
    pub(super) fn leave_room(&mut self, name: &str) {
        let pending = self.rooms.joining.iter().find(|(_, p)| p.name == name).map(|(id, _)| *id);
        if let Some(id) = pending {
            self.rooms.joining.remove(&id);
            self.emit(Event::RoomLeft { name: name.to_string() });
            return;
        }
        if self.rooms.deriving.remove(name).is_some() {
            self.emit(Event::RoomLeft { name: name.to_string() });
            return;
        }
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
            if skewed(env.ts) {
                self.warn_skew(from, env.ts);
                return;
            }
            if !room.replay.accept(nonce, env.ts, now_millis()) {
                debug!(%from, "replayed room frame");
                return;
            }
            self.on_room_envelope(id, from, env, now);
        } else if let Some(p) = self.rooms.joining.get_mut(&id) {
            let Some((env, nonce)) = room::open(&p.keys, from, nonce, ct) else {
                return;
            };
            if skewed(env.ts) {
                self.warn_skew(from, env.ts);
                return;
            }
            if !p.replay.accept(nonce, env.ts, now_millis()) {
                return;
            }
            let answers_us =
                matches!(&env.body, RoomBody::Here { name, echo: Some(e) } if *name == p.name && *e == p.probe);
            if answers_us {
                p.replies.insert(from, env.ts);
                if !std::mem::replace(&mut p.settling, true) {
                    let attempt = p.attempt;
                    self.after(JOIN_SETTLE, NodeMsg::JoinSettled(id, attempt));
                }
            }
        }
    }

    /// An opened, fresh, non-duplicate envelope for joined room `id`.
    fn on_room_envelope(&mut self, id: RoomId, from: PeerId, env: RoomEnvelope, now: Instant) {
        let Some(room) = self.rooms.joined.get_mut(&id) else {
            return;
        };
        // Anyone may probe, member or not: answer privately, echoing their nonce.
        if let RoomBody::Probe { nonce } = &env.body {
            let reply = RoomBody::Here { name: room.name.clone(), echo: Some(nonce.clone()) };
            let keys = room.keys.clone();
            self.send_to(from, self.seal(&keys, reply));
            return;
        }
        if room.members.contains_key(&from) {
            self.on_member_envelope(id, from, env, now);
            return;
        }
        // Unknown sender: is this the answer to our challenge?
        let proven = matches!(
            (&env.body, room.challenges.get(&from)),
            (RoomBody::Here { name, echo: Some(e) }, Some(c)) if *name == room.name && *e == c.nonce
        );
        if proven {
            let mut buffered = room.challenges.remove(&from).map(|c| c.buffered).unwrap_or_default();
            room.members.insert(from, Member { seen: now, floor: env.ts.saturating_sub(PROOF_GRACE_MS) });
            let name = room.name.clone();
            if let Some(nick) = self.nick_of(from) {
                self.emit(Event::MemberJoined { room: name, nick });
            }
            buffered.sort_by_key(|e| e.ts);
            for env in buffered {
                self.on_member_envelope(id, from, env, now);
            }
            return;
        }
        if matches!(env.body, RoomBody::Leave) {
            room.challenges.remove(&from);
            return;
        }
        // Challenge it (once) and hold its chat until it answers.
        let challenge = match room.challenges.get_mut(&from) {
            Some(c) => c,
            None => {
                let nonce = fresh_nonce();
                let probe = RoomBody::Probe { nonce: nonce.clone() };
                let frame = room::seal(&room.keys, self.id, &RoomEnvelope { ts: now_millis(), body: probe });
                room.challenges.insert(from, Challenge { nonce, started: now, buffered: Vec::new() });
                self.send_to(from, frame);
                match self.rooms.joined.get_mut(&id).and_then(|r| r.challenges.get_mut(&from)) {
                    Some(c) => c,
                    None => return,
                }
            }
        };
        if matches!(env.body, RoomBody::Chat { .. }) && challenge.buffered.len() < MAX_BUFFERED {
            challenge.buffered.push(env);
        }
    }

    /// An envelope from a proven member: enforce its floor, then act on it.
    fn on_member_envelope(&mut self, id: RoomId, from: PeerId, env: RoomEnvelope, now: Instant) {
        let Some(room) = self.rooms.joined.get_mut(&id) else {
            return;
        };
        let Some(member) = room.members.get_mut(&from) else {
            return;
        };
        if env.ts < member.floor {
            debug!(%from, "room frame older than the sender's floor");
            return;
        }
        member.floor = member.floor.max(env.ts.saturating_sub(REORDER_GRACE_MS));
        member.seen = now;
        let name = room.name.clone();
        match env.body {
            RoomBody::Chat { text } if text.len() > MAX_TEXT_LEN => {}
            RoomBody::Chat { text } => {
                if let Some(nick) = self.nick_of(from) {
                    self.emit(Event::Message { conv: Conv::Room(name), from: nick, text, ts: now_millis() });
                }
            }
            RoomBody::Leave => self.drop_member(id, from),
            RoomBody::Here { .. } | RoomBody::Probe { .. } => {}
        }
    }

    /// A frame that opened proves its sender has the key, so a stale one means
    /// their clock is off, not a wrong key; say so once instead of failing silently.
    fn warn_skew(&mut self, from: PeerId, ts: u64) {
        if !self.rooms.skew_warned.insert(from) {
            return;
        }
        if let Some(nick) = self.nick_of(from) {
            let minutes = ts.abs_diff(now_millis()) / 60_000;
            self.emit(Event::Info(format!("@{nick}'s clock is {minutes} min off; their room messages are ignored")));
        }
    }

    fn drop_member(&mut self, id: RoomId, member: PeerId) {
        let Some(room) = self.rooms.joined.get_mut(&id) else {
            return;
        };
        room.challenges.remove(&member);
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

    /// Every second: expire silent members and unanswered challenges; every
    /// `HERE_INTERVAL`: announce presence.
    pub(super) fn on_tick(&mut self, now: Instant) {
        let expired: Vec<(RoomId, PeerId)> = self
            .rooms
            .joined
            .iter()
            .flat_map(|(id, r)| {
                r.members
                    .iter()
                    .filter(|(_, m)| now.saturating_duration_since(m.seen) >= MEMBER_TIMEOUT)
                    .map(|(p, _)| (*id, *p))
            })
            .collect();
        for (id, member) in expired {
            self.drop_member(id, member);
        }
        for room in self.rooms.joined.values_mut() {
            room.challenges.retain(|_, c| now.saturating_duration_since(c.started) < CHALLENGE_TIMEOUT);
        }
        if self.rooms.last_here.is_none_or(|t| now.saturating_duration_since(t) >= HERE_INTERVAL) {
            self.rooms.last_here = Some(now);
            for room in self.rooms.joined.values() {
                self.broadcast(self.seal(&room.keys, RoomBody::Here { name: room.name.clone(), echo: None }));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{BOB, CAROL, Harness, ME};
    use super::*;
    use crate::api::{Command, Target};
    use crate::protocol::Frame;

    const NAME: &str = "catPieR";
    const KEY: &str = "123-k7m9xq";

    fn keys() -> RoomKeys {
        room::derive(NAME, KEY)
    }

    fn here(name: &str, echo: Option<&str>) -> RoomBody {
        RoomBody::Here { name: name.into(), echo: echo.map(String::from) }
    }

    /// A room frame as `from` would send it, stamped `ts`.
    fn frame_at(from: PeerId, keys: &RoomKeys, ts: u64, body: RoomBody) -> Frame {
        room::seal(keys, from, &RoomEnvelope { ts, body })
    }

    fn frame_from(from: PeerId, keys: &RoomKeys, body: RoomBody) -> Frame {
        frame_at(from, keys, now_millis(), body)
    }

    fn deliver(h: &mut Harness, from: PeerId, frame: Frame) {
        let conn = h.node.peers[&from].conn;
        h.node.on_frame(conn, from, frame);
    }

    fn open_body(keys: &RoomKeys, frame: &Frame) -> RoomBody {
        let Frame::Room { nonce, ct, .. } = frame else { panic!("not a room frame: {frame:?}") };
        room::open(keys, ME, nonce, ct).expect("opens").0.body
    }

    fn probe_nonce(keys: &RoomKeys, frame: &Frame) -> String {
        match open_body(keys, frame) {
            RoomBody::Probe { nonce } => nonce,
            other => panic!("expected a probe, got {other:?}"),
        }
    }

    fn with_peers() -> Harness {
        let mut h = Harness::new();
        h.connect(BOB, "bobRoger", true);
        h.connect(CAROL, "carolFox", true);
        h.events();
        h
    }

    async fn create(h: &mut Harness) -> (String, RoomKeys) {
        h.node.handle_command(Command::CreateRoom { base: "cat".into(), key_part: "123".into() });
        h.pump().await;
        let name = match h.events().as_slice() {
            [Event::RoomCreated { name, key }] => {
                assert!(name.starts_with("cat") && names::is_valid_room(name), "{name}");
                assert!(key.starts_with("123-") && room::validate_key(key).is_ok(), "{key}");
                name.clone()
            }
            other => panic!("expected RoomCreated, got {other:?}"),
        };
        let id = h.node.rooms.id_of(&name).unwrap();
        let keys = h.node.rooms.joined[&id].keys.clone();
        (name, keys)
    }

    /// `member` announces itself, answers our challenge, and becomes a member.
    fn verify(h: &mut Harness, member: PeerId, name: &str, keys: &RoomKeys) {
        deliver(h, member, frame_from(member, keys, here(name, None)));
        let challenge = probe_nonce(keys, h.sent(member).last().expect("challenge sent"));
        deliver(h, member, frame_from(member, keys, here(name, Some(&challenge))));
    }

    #[tokio::test]
    async fn creating_a_room_makes_us_its_only_member() {
        let mut h = with_peers();
        let (name, _) = create(&mut h).await;
        h.node.handle_command(Command::Send { to: Target::Room(name.clone()), text: "anyone?".into() });
        // Sent to everyone (only members can read it), recorded, and we're told we're alone.
        assert!(matches!(h.sent(BOB).as_slice(), [Frame::Room { .. }]));
        assert!(matches!(h.sent(CAROL).as_slice(), [Frame::Room { .. }]));
        let events = h.events();
        assert!(matches!(&events[0], Event::Sent { conv: Conv::Room(n), .. } if *n == name));
        assert_eq!(events[1], Event::Info(format!("nobody else is in #{name} yet")));
    }

    #[tokio::test]
    async fn escaped_text_is_what_counts_against_the_limit() {
        let mut h = with_peers();
        let (name, _) = create(&mut h).await;
        // Each control character escapes to 6 bytes, and room frames are hex on top.
        h.node.handle_command(Command::Send { to: Target::Room(name.clone()), text: "\u{1}".repeat(MAX_TEXT_LEN) });
        assert!(h.sent(BOB).is_empty());
        assert!(matches!(h.events().as_slice(), [Event::Error(e)] if e.contains("too long")));

        let most = "\u{1}".repeat(MAX_TEXT_LEN / 6);
        h.node.handle_command(Command::Send { to: Target::Room(name), text: most });
        let sent = h.sent(BOB);
        assert!(matches!(sent.as_slice(), [Frame::Room { .. }]));
        assert!(serde_json::to_vec(&sent[0]).unwrap().len() <= crate::protocol::MAX_FRAME_LEN);
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
    async fn joining_collects_members_that_answer_our_probe() {
        let mut h = with_peers();
        let keys = keys();
        h.node.handle_command(Command::JoinRoom { name: NAME.into(), key: KEY.into() });
        h.pump().await; // Derived
        let nonce = probe_nonce(&keys, &h.sent(BOB)[0]);
        h.sent(CAROL);
        // Both are members and answer, echoing our probe.
        deliver(&mut h, BOB, frame_from(BOB, &keys, here(NAME, Some(&nonce))));
        deliver(&mut h, CAROL, frame_from(CAROL, &keys, here(NAME, Some(&nonce))));
        h.pump().await; // JoinSettled (the JoinTimeout comes later and is ignored)
        assert_eq!(
            h.events(),
            vec![Event::RoomJoined { name: NAME.into(), members: vec!["bobRoger".into(), "carolFox".into()] }]
        );
        assert!(matches!(open_body(&keys, &h.sent(BOB)[0]), RoomBody::Here { echo: None, .. }));
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
    async fn replayed_presence_cannot_fake_a_member_for_a_joiner() {
        let mut h = with_peers();
        let keys = keys();
        h.node.handle_command(Command::JoinRoom { name: NAME.into(), key: KEY.into() });
        h.pump().await; // Derived
        // Captured Here frames: a heartbeat (no echo) and an answer to someone else's probe.
        deliver(&mut h, BOB, frame_from(BOB, &keys, here(NAME, None)));
        deliver(&mut h, BOB, frame_from(BOB, &keys, here(NAME, Some("00112233"))));
        h.pump().await; // JoinTimeout
        assert_eq!(h.events(), vec![Event::Error(format!("no room #{NAME} with that key"))]);
    }

    #[tokio::test(start_paused = true)]
    async fn joining_with_a_wrong_key_times_out() {
        let mut h = with_peers();
        h.node.handle_command(Command::JoinRoom { name: NAME.into(), key: "999-k7m9xq".into() });
        h.pump().await; // Derived
        // bob is in the real room; his Here is under the real key, so we can't read it.
        deliver(&mut h, BOB, frame_from(BOB, &keys(), here(NAME, None)));
        h.pump().await; // JoinTimeout
        assert_eq!(h.events(), vec![Event::Error(format!("no room #{NAME} with that key"))]);
    }

    #[tokio::test(start_paused = true)]
    async fn an_earlier_attempts_timer_does_not_cancel_a_rejoin() {
        let mut h = with_peers();
        let keys = keys();
        h.node.handle_command(Command::JoinRoom { name: NAME.into(), key: KEY.into() });
        h.pump().await; // Derived
        let nonce = probe_nonce(&keys, &h.sent(BOB)[0]);
        deliver(&mut h, BOB, frame_from(BOB, &keys, here(NAME, Some(&nonce))));
        h.pump().await; // JoinSettled
        h.node.handle_command(Command::LeaveRoom { name: NAME.into() });
        h.node.handle_command(Command::JoinRoom { name: NAME.into(), key: KEY.into() });
        h.pump().await; // Derived
        h.events();
        h.pump().await; // the first attempt's JoinTimeout: not ours any more
        assert!(h.events().is_empty());

        let nonce = probe_nonce(&keys, h.sent(BOB).last().unwrap());
        deliver(&mut h, BOB, frame_from(BOB, &keys, here(NAME, Some(&nonce))));
        h.pump().await; // JoinSettled and this attempt's JoinTimeout, due about together
        h.pump().await;
        assert_eq!(h.events(), vec![Event::RoomJoined { name: NAME.into(), members: vec!["bobRoger".into()] }]);
    }

    #[tokio::test(start_paused = true)]
    async fn leaving_cancels_a_join_still_deriving() {
        let mut h = with_peers();
        h.node.handle_command(Command::JoinRoom { name: NAME.into(), key: KEY.into() });
        h.node.handle_command(Command::LeaveRoom { name: NAME.into() });
        assert_eq!(h.events(), vec![Event::RoomLeft { name: NAME.into() }]);
        h.pump().await; // Derived: dropped
        assert!(h.sent(BOB).is_empty(), "no probe for a cancelled join");
        assert!(h.events().is_empty());
        assert_eq!(h.node.rooms.count(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn leaving_cancels_a_join_waiting_for_answers() {
        let mut h = with_peers();
        let keys = keys();
        h.node.handle_command(Command::JoinRoom { name: NAME.into(), key: KEY.into() });
        h.pump().await; // Derived
        let nonce = probe_nonce(&keys, &h.sent(BOB)[0]);
        h.node.handle_command(Command::LeaveRoom { name: NAME.into() });
        assert_eq!(h.events(), vec![Event::RoomLeft { name: NAME.into() }]);
        deliver(&mut h, BOB, frame_from(BOB, &keys, here(NAME, Some(&nonce))));
        h.pump().await; // JoinTimeout: nothing to time out
        assert!(h.events().is_empty());
        assert_eq!(h.node.rooms.count(), 0);
    }

    #[tokio::test]
    async fn members_answer_probes_privately_and_newcomers_must_prove_themselves() {
        let mut h = with_peers();
        let (name, keys) = create(&mut h).await;

        deliver(&mut h, BOB, frame_from(BOB, &keys, RoomBody::Probe { nonce: "00".into() }));
        assert_eq!(open_body(&keys, &h.sent(BOB)[0]), here(&name, Some("00")));
        assert!(h.sent(CAROL).is_empty(), "the reply goes only to the prober");
        assert!(h.events().is_empty(), "probing doesn't make you a member");

        // A heartbeat from someone we don't know yet gets a private challenge, not membership.
        deliver(&mut h, BOB, frame_from(BOB, &keys, here(&name, None)));
        let challenge = probe_nonce(&keys, &h.sent(BOB)[0]);
        assert!(h.events().is_empty());
        deliver(&mut h, BOB, frame_from(BOB, &keys, here(&name, Some("wrong"))));
        assert!(h.events().is_empty(), "a wrong echo proves nothing");
        deliver(&mut h, BOB, frame_from(BOB, &keys, here(&name, Some(&challenge))));
        assert_eq!(h.events(), vec![Event::MemberJoined { room: name.clone(), nick: "bobRoger".into() }]);
        deliver(&mut h, BOB, frame_from(BOB, &keys, here(&name, None)));
        assert!(h.events().is_empty(), "already a member");

        deliver(&mut h, BOB, frame_from(BOB, &keys, RoomBody::Leave));
        assert_eq!(h.events(), vec![Event::MemberLeft { room: name, nick: "bobRoger".into() }]);
    }

    #[tokio::test]
    async fn a_newcomers_first_messages_wait_for_its_proof() {
        let mut h = with_peers();
        let (name, keys) = create(&mut h).await;
        deliver(&mut h, BOB, frame_from(BOB, &keys, RoomBody::Chat { text: "early".into() }));
        assert!(h.events().is_empty());
        let challenge = probe_nonce(&keys, &h.sent(BOB)[0]);
        deliver(&mut h, BOB, frame_from(BOB, &keys, here(&name, Some(&challenge))));
        let events = h.events();
        assert_eq!(events[0], Event::MemberJoined { room: name.clone(), nick: "bobRoger".into() });
        assert!(matches!(&events[1], Event::Message { text, .. } if text == "early"), "{events:?}");
        assert_eq!(events.len(), 2);
    }

    #[tokio::test]
    async fn replayed_old_messages_are_dropped_even_with_fresh_nonces() {
        let mut h = with_peers();
        let (name, keys) = create(&mut h).await;
        let now = now_millis();
        // Captured a minute ago and replayed before bob is verified: buffered...
        deliver(&mut h, BOB, frame_at(BOB, &keys, now - 60_000, RoomBody::Chat { text: "old".into() }));
        let challenge = probe_nonce(&keys, &h.sent(BOB)[0]);
        deliver(&mut h, BOB, frame_from(BOB, &keys, here(&name, Some(&challenge))));
        // ...but older than his proof, so it's dropped once he's verified.
        assert_eq!(h.events(), vec![Event::MemberJoined { room: name.clone(), nick: "bobRoger".into() }]);
        // After verification: old frames are below his floor, new ones go through.
        deliver(&mut h, BOB, frame_at(BOB, &keys, now - 30_000, RoomBody::Chat { text: "older".into() }));
        deliver(&mut h, BOB, frame_from(BOB, &keys, RoomBody::Chat { text: "fresh".into() }));
        deliver(&mut h, BOB, frame_at(BOB, &keys, now - 10_000, RoomBody::Chat { text: "stale again".into() }));
        let texts: Vec<String> = h
            .events()
            .into_iter()
            .filter_map(|e| if let Event::Message { text, .. } = e { Some(text) } else { None })
            .collect();
        assert_eq!(texts, vec!["fresh".to_string()]);
    }

    #[tokio::test]
    async fn unanswered_challenges_expire_with_their_buffer() {
        let mut h = with_peers();
        let (name, keys) = create(&mut h).await;
        deliver(&mut h, BOB, frame_from(BOB, &keys, RoomBody::Chat { text: "spoofed".into() }));
        let challenge = probe_nonce(&keys, &h.sent(BOB)[0]);
        h.node.on_tick(Instant::now() + CHALLENGE_TIMEOUT);
        deliver(&mut h, BOB, frame_from(BOB, &keys, here(&name, Some(&challenge))));
        assert!(!h.events().iter().any(|e| matches!(e, Event::MemberJoined { .. } | Event::Message { .. })));
    }

    #[tokio::test]
    async fn silent_members_expire_and_presence_is_announced() {
        let mut h = with_peers();
        let (name, keys) = create(&mut h).await;
        let id = h.node.rooms.id_of(&name).unwrap();
        h.node.on_room_frame(BOB, &room::to_hex(&id), "", "", Instant::now()); // junk: ignored
        verify(&mut h, BOB, &name, &keys);
        let t0 = Instant::now();
        h.events();
        h.sent(CAROL);

        h.node.on_tick(t0 + Duration::from_secs(1)); // first tick announces
        assert!(matches!(open_body(&keys, &h.sent(CAROL)[0]), RoomBody::Here { echo: None, .. }));
        h.node.on_tick(t0 + Duration::from_secs(2)); // too soon to announce again
        assert!(h.sent(CAROL).is_empty());
        assert!(h.events().is_empty());
        h.node.on_tick(t0 + MEMBER_TIMEOUT);
        assert_eq!(h.events(), vec![Event::MemberLeft { room: name, nick: "bobRoger".into() }]);
    }

    #[tokio::test]
    async fn replays_unknown_rooms_and_wrong_senders_are_dropped() {
        let mut h = with_peers();
        let (name, keys) = create(&mut h).await;
        verify(&mut h, BOB, &name, &keys);
        h.events();

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
    async fn a_peer_whose_clock_is_off_is_named_once() {
        let mut h = with_peers();
        let (_, keys) = create(&mut h).await;
        let ten_min_ago = now_millis() - 10 * 60 * 1000;
        for text in ["one", "two"] {
            deliver(&mut h, BOB, frame_at(BOB, &keys, ten_min_ago, RoomBody::Chat { text: text.into() }));
        }
        assert_eq!(
            h.events(),
            vec![Event::Info("@bobRoger's clock is 10 min off; their room messages are ignored".into())]
        );
    }

    #[tokio::test]
    async fn leaving_tells_members_and_disconnects_drop_members() {
        let mut h = with_peers();
        let (name, keys) = create(&mut h).await;
        verify(&mut h, BOB, &name, &keys);
        h.events();

        let conn = h.node.peers[&BOB].conn;
        h.node.handle_msg(NodeMsg::PeerGone { conn, id: BOB, reason: crate::api::LeaveReason::Lost });
        assert!(h.events().contains(&Event::MemberLeft { room: name.clone(), nick: "bobRoger".into() }));

        h.sent(CAROL);
        h.node.handle_command(Command::LeaveRoom { name: name.clone() });
        assert!(matches!(open_body(&keys, &h.sent(CAROL)[0]), RoomBody::Leave));
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
