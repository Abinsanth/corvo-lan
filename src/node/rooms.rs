//! Private-room state and handlers. See `crate::room` for the crypto.
//!
//! Each room has a door and a room channel. The door (keys from the name and
//! join key) carries knocks: a newcomer asks to join, members answer and
//! record the request, and members vote on it over the room channel (keys from
//! the room secret) so every member's tally matches. Once enough members
//! approve and there's a free slot, each approver hands the newcomer the
//! secret privately; the newcomer then joins the room channel like any member.
//!
//! On the room channel a sender only becomes a member after answering a fresh
//! probe (its `Here` echoes our nonce), and from then on frames older than
//! that sender's floor are dropped, so captured frames can't be replayed.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use tokio::time::Instant;
use tracing::debug;

use super::{MAX_TEXT_LEN, Node, NodeMsg};
use crate::api::{Conv, Event, JoinEnd, RequestEnd};
use crate::names;
use crate::protocol::{Frame, PeerId, now_millis};
use crate::room::{self, ReplayGuard, RoomBody, RoomEnvelope, RoomId, RoomKeys, Secret};

/// Rooms you can be in at once, counting ones you're creating or waiting to get into.
pub const MAX_ROOMS: usize = 3;
pub const MIN_MEMBERS: u8 = 2;
pub const MAX_MEMBERS: u8 = 13;
/// How long a join waits for any member to answer its knock.
pub const JOIN_WAIT: Duration = Duration::from_secs(2);
/// While waiting to be let in, knock again this often so members who come online see it.
pub const KNOCK_EVERY: Duration = Duration::from_secs(10);
/// How long a newcomer waits to be let in.
pub const PENDING_FOR: Duration = Duration::from_secs(120);
/// How long an invitation lasts, for the invitee and for the inviter's approval.
pub const INVITE_FOR: Duration = Duration::from_secs(600);
/// Invitations an invitee holds; a newer one drops the oldest.
pub const MAX_INVITES: usize = 5;
/// After a yes vote, wait this long for other votes before letting people in,
/// so every member sees the same votes and picks the same people.
pub const ADMIT_SETTLE: Duration = Duration::from_secs(1);
/// A request with no knock for this long (or let in but not arrived) is dropped.
pub const REQUEST_STALE: Duration = Duration::from_secs(30);
pub const HERE_INTERVAL: Duration = Duration::from_secs(5);
/// A member silent this long is considered gone.
pub const MEMBER_TIMEOUT: Duration = Duration::from_secs(15);
/// How long an unknown sender has to answer our challenge.
pub const CHALLENGE_TIMEOUT: Duration = Duration::from_secs(5);
/// Open requests remembered per room.
const MAX_REQUESTS: usize = 32;
/// Frames from an unproven sender held while we wait for its answer.
const MAX_BUFFERED: usize = 32;
/// At proof time, accept that sender's frames stamped up to this much earlier
/// (its first messages race its answer to our challenge).
const PROOF_GRACE_MS: u64 = 3_000;
/// After that, a sender's floor trails its newest frame by this much.
const REORDER_GRACE_MS: u64 = 2_000;

/// Approvals a request needs when `members` (us included) are in the room.
pub fn needed(members: usize) -> usize {
    members.div_ceil(3).max(1)
}

/// Numbers each create or join, so a timer or key derivation left over from
/// an earlier (left or failed) attempt at the same room is recognised and ignored.
pub type Attempt = u64;

/// What a worker thread was deriving door keys for.
pub enum Job {
    Create { name: String, key: String, max: u8 },
    Join { name: String, key: String },
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

/// Someone asking to join one of our rooms.
struct Request {
    /// When they first knocked; earlier asks get the last slots first.
    asked: u64,
    /// Their last knock, or when we first heard of them.
    seen: Instant,
    /// Members (maybe us) who said yes.
    approvals: HashSet<PeerId>,
    /// When they were let in; they hold a slot until they arrive.
    admitted: Option<Instant>,
}

struct Room {
    name: String,
    /// The 6-character join key, for invitations.
    key: String,
    max: u8,
    secret: Secret,
    keys: RoomKeys,
    door: RoomKeys,
    /// Other members, all proven.
    members: HashMap<PeerId, Member>,
    challenges: HashMap<PeerId, Challenge>,
    requests: HashMap<PeerId, Request>,
    /// Recently closed requests (their `asked`, and when), so a vote still on
    /// its way can't reopen one.
    closed: HashMap<PeerId, (u64, Instant)>,
    /// Our probe from when we were let in; members echoing it are proven.
    probe: Option<String>,
    replay: ReplayGuard,
    door_replay: ReplayGuard,
    told_alone: bool,
}

impl Room {
    /// Members including us.
    fn size(&self) -> usize {
        self.members.len() + 1
    }

    fn admitted(&self) -> usize {
        self.requests.values().filter(|r| r.admitted.is_some()).count()
    }

    fn full(&self) -> bool {
        self.size() + self.admitted() >= self.max as usize
    }

    /// Removes `peer`'s request and remembers it was closed.
    fn close(&mut self, peer: PeerId) -> Option<Request> {
        let request = self.requests.remove(&peer)?;
        self.closed.insert(peer, (request.asked, Instant::now()));
        Some(request)
    }

    /// Approvals from people still in the room.
    fn approvals(&self, request: &Request, me: PeerId) -> usize {
        request.approvals.iter().filter(|a| **a == me || self.members.contains_key(a)).count()
    }
}

/// A join waiting at the door.
/// An invitation someone sent us.
struct Invite {
    from: PeerId,
    room: String,
    key: String,
    at: Instant,
}

struct PendingJoin {
    attempt: Attempt,
    name: String,
    key: String,
    door: RoomKeys,
    /// Our knock's nonce; only answers echoing it count.
    nonce: String,
    asked: u64,
    /// Someone answered, so we're waiting to be let in.
    waiting: bool,
    last_knock: Instant,
    replay: ReplayGuard,
}

#[derive(Default)]
pub(super) struct Rooms {
    joined: HashMap<RoomId, Room>,
    /// Door id → room id, for the rooms we're in.
    doors: HashMap<RoomId, RoomId>,
    /// Keyed by door id.
    joining: HashMap<RoomId, PendingJoin>,
    /// Names whose door keys are being derived right now, and for which attempt.
    deriving: HashMap<String, Attempt>,
    last_attempt: Attempt,
    /// Peers we've already told the user have a skewed clock.
    skew_warned: HashSet<PeerId>,
    last_here: Option<Instant>,
    /// Who we invited to which room, and when; their knock gets our approval.
    invited: HashMap<(RoomId, PeerId), Instant>,
    /// Invitations to us, oldest first.
    invites: Vec<Invite>,
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

fn small(n: usize) -> u8 {
    n.min(u8::MAX as usize) as u8
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
            self.emit(Event::Error(format!("you're in {MAX_ROOMS} rooms (the max) — /leave one first")));
            return false;
        }
        true
    }

    pub(super) fn create_room(&mut self, name: &str, max: u8) {
        if !names::is_valid_name(name) {
            self.emit(Event::Error(format!("#{name} isn't a room name")));
            return;
        }
        if !(MIN_MEMBERS..=MAX_MEMBERS).contains(&max) {
            self.emit(Event::Error(format!("a room holds {MIN_MEMBERS}–{MAX_MEMBERS} people")));
            return;
        }
        if self.rooms.holds(name) {
            self.emit(Event::Error(format!("you're already in #{name}")));
            return;
        }
        if !self.room_slot_free() {
            return;
        }
        let key = room::make_key();
        self.spawn_derive(Job::Create { name: name.to_string(), key: key.clone(), max }, name.to_string(), key);
    }

    pub(super) fn join_room(&mut self, name: &str, key: &str) {
        if !names::is_valid_name(name) {
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
        let job = Job::Join { name: name.to_string(), key: key.to_string() };
        self.spawn_derive(job, name.to_string(), key.to_string());
    }

    pub(super) fn on_derived(&mut self, job: Job, attempt: Attempt, door: RoomKeys) {
        let name = match &job {
            Job::Create { name, .. } | Job::Join { name, .. } => name.clone(),
        };
        // Otherwise the user left (or left and retried) while we were deriving.
        if self.rooms.deriving.get(&name) != Some(&attempt) {
            return;
        }
        self.rooms.deriving.remove(&name);
        if self.rooms.doors.contains_key(&door.id) || self.rooms.joining.contains_key(&door.id) {
            self.emit(Event::Error(format!("you're already in #{name}")));
            return;
        }
        match job {
            Job::Create { name, key, max } => {
                let secret = room::new_secret();
                let keys = room::from_secret(&name, &secret);
                self.hold_room(name.clone(), key.clone(), max, secret, keys, door, HashMap::new());
                self.emit(Event::RoomCreated { name, key, max });
            }
            Job::Join { name, key } => {
                let nonce = fresh_nonce();
                let asked = now_millis();
                self.broadcast(self.seal(&door, RoomBody::Knock { nonce: nonce.clone(), asked }));
                let id = door.id;
                let pending = PendingJoin {
                    attempt,
                    name,
                    key,
                    door,
                    nonce,
                    asked,
                    waiting: false,
                    last_knock: Instant::now(),
                    replay: ReplayGuard::default(),
                };
                self.rooms.joining.insert(id, pending);
                self.after(JOIN_WAIT, NodeMsg::JoinTimeout(id, attempt));
            }
        }
    }

    fn hold_room(
        &mut self,
        name: String,
        key: String,
        max: u8,
        secret: Secret,
        keys: RoomKeys,
        door: RoomKeys,
        members: HashMap<PeerId, Member>,
    ) -> RoomId {
        let id = keys.id;
        self.rooms.doors.insert(door.id, id);
        let room = Room {
            name,
            key,
            max,
            secret,
            keys,
            door,
            members,
            challenges: HashMap::new(),
            requests: HashMap::new(),
            closed: HashMap::new(),
            probe: None,
            replay: ReplayGuard::default(),
            door_replay: ReplayGuard::default(),
            told_alone: false,
        };
        self.rooms.joined.insert(id, room);
        id
    }

    /// Nobody answered our knock in time.
    pub(super) fn on_join_timeout(&mut self, door: RoomId, attempt: Attempt) {
        if self.rooms.joining.get(&door).is_some_and(|p| p.attempt == attempt && !p.waiting)
            && let Some(p) = self.rooms.joining.remove(&door)
        {
            self.emit(Event::JoinEnded { name: p.name, why: JoinEnd::NoRoom });
        }
    }

    /// Nobody let us in within `PENDING_FOR`.
    pub(super) fn on_join_expired(&mut self, door: RoomId, attempt: Attempt) {
        if self.rooms.joining.get(&door).is_some_and(|p| p.attempt == attempt)
            && let Some(p) = self.rooms.joining.remove(&door)
        {
            self.broadcast(self.seal(&p.door, RoomBody::Leave));
            self.emit(Event::JoinEnded { name: p.name, why: JoinEnd::TimedOut });
        }
    }

    /// Also cancels a join of `name` that hasn't finished yet.
    pub(super) fn leave_room(&mut self, name: &str) {
        let pending = self.rooms.joining.iter().find(|(_, p)| p.name == name).map(|(id, _)| *id);
        if let Some(p) = pending.and_then(|id| self.rooms.joining.remove(&id)) {
            self.broadcast(self.seal(&p.door, RoomBody::Leave));
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
            self.rooms.doors.remove(&room.door.id);
            self.broadcast(self.seal(&room.keys, RoomBody::Leave));
            self.emit(Event::RoomLeft { name: room.name });
        }
    }

    pub(super) fn leave_all_rooms(&self) {
        for room in self.rooms.joined.values() {
            self.broadcast(self.seal(&room.keys, RoomBody::Leave));
        }
        for p in self.rooms.joining.values() {
            self.broadcast(self.seal(&p.door, RoomBody::Leave));
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

    /// The room `name` and `nick`'s open request to join it, or an error for the user.
    fn request_of(&self, name: &str, nick: &str) -> Result<(RoomId, PeerId), String> {
        let id = self.rooms.id_of(name).ok_or_else(|| format!("you're not in #{name}"))?;
        let peer = self.peer_by_nick(nick).ok_or_else(|| format!("@{nick} is offline"))?;
        match self.rooms.joined[&id].requests.get(&peer) {
            Some(r) if r.admitted.is_none() => Ok((id, peer)),
            Some(_) => Err(format!("@{nick} is already being let in")),
            None => Err(format!("@{nick} isn't waiting to join #{name}")),
        }
    }

    pub(super) fn invite(&mut self, nick: &str, name: &str) {
        let Some(id) = self.rooms.id_of(name) else {
            return self.emit(Event::Error(format!("you're not in #{name}")));
        };
        if nick == self.my_nick() {
            return self.emit(Event::Error(format!("you're already in #{name}")));
        }
        let Some(peer) = self.peer_by_nick(nick) else {
            return self.emit(Event::Error(format!("@{nick} is offline")));
        };
        let room = &self.rooms.joined[&id];
        if room.members.contains_key(&peer) {
            return self.emit(Event::Error(format!("@{nick} is already in #{name}")));
        }
        self.send_to(peer, Frame::Invite { room: name.to_string(), key: room.key.clone() });
        self.rooms.invited.insert((id, peer), Instant::now());
        self.emit(Event::Info(format!("invited @{nick} to #{name}")));
    }

    /// `from` knocked on room `id`: if we invited them, our invitation is our yes.
    fn approve_invited(&mut self, id: RoomId, from: PeerId, now: Instant) {
        let Some(at) = self.rooms.invited.remove(&(id, from)) else {
            return;
        };
        if now.saturating_duration_since(at) >= INVITE_FOR {
            return;
        }
        let (Some(room), Some(nick)) = (self.rooms.joined.get(&id), self.nick_of(from)) else {
            return;
        };
        if room.requests.get(&from).is_some_and(|r| r.admitted.is_none() && !r.approvals.contains(&self.id)) {
            let name = room.name.clone();
            self.approve(&name, &nick);
        }
    }

    /// `from` turned down our invitation to `name`; only counts if we invited them.
    pub(super) fn on_decline(&mut self, from: PeerId, name: &str) {
        let Some(id) = self.rooms.id_of(name) else {
            return;
        };
        if self.rooms.invited.remove(&(id, from)).is_some()
            && let Some(by) = self.nick_of(from)
        {
            self.emit(Event::InviteDeclined { by, room: name.to_string() });
        }
    }

    /// `from` invites us to `name`. Bad shapes and rooms we're in are dropped.
    pub(super) fn on_invite(&mut self, from: PeerId, name: String, key: String, now: Instant) {
        if !names::is_valid_name(&name) || room::validate_key(&key).is_err() || self.rooms.holds(&name) {
            debug!(%from, "ignoring invite to {name:?}");
            return;
        }
        let Some(nick) = self.nick_of(from) else {
            return;
        };
        let invites = &mut self.rooms.invites;
        invites.retain(|i| i.room != name);
        invites.push(Invite { from, room: name.clone(), key, at: now });
        if invites.len() > MAX_INVITES {
            invites.remove(0);
        }
        self.emit(Event::Invited { from: nick, room: name });
    }

    /// Joins `name` with the key its invitation carried.
    pub(super) fn accept(&mut self, name: &str) {
        let Some(pos) = self.rooms.invites.iter().position(|i| i.room == name) else {
            return self.emit(Event::Error(format!("no invitation to #{name}")));
        };
        if self.rooms.holds(name) {
            self.rooms.invites.remove(pos);
            return self.emit(Event::Error(format!("you're already in #{name}")));
        }
        // At the room limit this says why, and the invitation stays for after a /leave.
        if !self.room_slot_free() {
            return;
        }
        let invite = self.rooms.invites.remove(pos);
        self.join_room(&invite.room, &invite.key);
    }

    pub(super) fn decline(&mut self, name: &str) {
        let Some(pos) = self.rooms.invites.iter().position(|i| i.room == name) else {
            return self.emit(Event::Error(format!("no invitation to #{name}")));
        };
        let invite = self.rooms.invites.remove(pos);
        self.send_to(invite.from, Frame::Decline { room: invite.room.clone() });
        self.emit(Event::Info(format!("declined #{}", invite.room)));
    }

    pub(super) fn approve(&mut self, name: &str, nick: &str) {
        let (id, peer) = match self.request_of(name, nick) {
            Ok(found) => found,
            Err(e) => return self.emit(Event::Error(e)),
        };
        let me = self.id;
        let room = self.rooms.joined.get_mut(&id).expect("found above");
        let request = room.requests.get_mut(&peer).expect("found above");
        request.approvals.insert(me);
        let asked = request.asked;
        let have = room.approvals(&room.requests[&peer], me);
        let need = needed(room.size());
        let frame = room::seal(
            &room.keys,
            me,
            &RoomEnvelope { ts: now_millis(), body: RoomBody::Vote { peer, asked, yes: true } },
        );
        self.broadcast(frame);
        let by = self.my_nick();
        self.emit(Event::Approved { room: name.into(), nick: nick.into(), by, have: small(have), need: small(need) });
        self.after(ADMIT_SETTLE, NodeMsg::Settle(id));
    }

    pub(super) fn deny(&mut self, name: &str, nick: &str) {
        let (id, peer) = match self.request_of(name, nick) {
            Ok(found) => found,
            Err(e) => return self.emit(Event::Error(e)),
        };
        let room = self.rooms.joined.get_mut(&id).expect("found above");
        let Some(request) = room.close(peer) else {
            return;
        };
        let vote = RoomBody::Vote { peer, asked: request.asked, yes: false };
        let (keys, door) = (room.keys.clone(), room.door.clone());
        self.broadcast(self.seal(&keys, vote));
        self.send_to(peer, self.seal(&door, RoomBody::Refuse { full: false }));
        let by = self.my_nick();
        self.emit(Event::RequestEnded { room: name.into(), nick: nick.into(), why: RequestEnd::Denied { by } });
    }

    /// Lets in, in order of asking, whoever has enough approvals while slots
    /// last; refuses the rest as full. Approvers send the secret or the refusal.
    pub(super) fn settle(&mut self, id: RoomId) {
        let me = self.id;
        let Some(room) = self.rooms.joined.get_mut(&id) else {
            return;
        };
        let need = needed(room.size());
        let mut free = (room.max as usize).saturating_sub(room.size() + room.admitted());
        let mut ready: Vec<(u64, PeerId)> = room
            .requests
            .iter()
            .filter(|(_, r)| r.admitted.is_none() && room.approvals(r, me) >= need)
            .map(|(p, r)| (r.asked, *p))
            .collect();
        ready.sort();
        let now = Instant::now();
        let mut grants = Vec::new();
        let mut refusals = Vec::new();
        for (_, peer) in ready {
            if free > 0 {
                free -= 1;
                let request = room.requests.get_mut(&peer).expect("listed above");
                request.admitted = Some(now);
                if request.approvals.contains(&me) {
                    grants.push(peer);
                }
            } else if let Some(request) = room.close(peer) {
                refusals.push((peer, request.approvals.contains(&me)));
            }
        }
        let (name, door, max) = (room.name.clone(), room.door.clone(), room.max);
        let secret = room::to_hex(&room.secret);
        for peer in grants {
            self.send_to(peer, self.seal(&door, RoomBody::Grant { secret: secret.clone(), max }));
        }
        for (peer, approved) in refusals {
            if approved {
                self.send_to(peer, self.seal(&door, RoomBody::Refuse { full: true }));
            }
            if let Some(nick) = self.nick_of(peer) {
                self.emit(Event::RequestEnded { room: name.clone(), nick, why: RequestEnd::Full });
            }
        }
    }

    /// A `Frame::Room` from peer `from`. Silently drops anything that isn't for
    /// one of our rooms or doors, doesn't decrypt, is stale or is a replay.
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
        } else if let Some(&room_id) = self.rooms.doors.get(&id) {
            let Some(room) = self.rooms.joined.get_mut(&room_id) else {
                return;
            };
            let Some((env, nonce)) = room::open(&room.door, from, nonce, ct) else {
                return;
            };
            if skewed(env.ts) {
                self.warn_skew(from, env.ts);
                return;
            }
            if !room.door_replay.accept(nonce, env.ts, now_millis()) {
                return;
            }
            self.on_door_envelope(room_id, from, env, now);
        } else if let Some(p) = self.rooms.joining.get_mut(&id) {
            let Some((env, nonce)) = room::open(&p.door, from, nonce, ct) else {
                return;
            };
            if skewed(env.ts) {
                self.warn_skew(from, env.ts);
                return;
            }
            if !p.replay.accept(nonce, env.ts, now_millis()) {
                return;
            }
            self.on_answer(id, from, env, now);
        }
    }

    /// Door traffic for a room we're in: knocks and given-up requests.
    fn on_door_envelope(&mut self, id: RoomId, from: PeerId, env: RoomEnvelope, now: Instant) {
        let Some(room) = self.rooms.joined.get_mut(&id) else {
            return;
        };
        if room.members.contains_key(&from) {
            return;
        }
        match env.body {
            RoomBody::Knock { nonce, asked } => {
                let door = room.door.clone();
                if let Some(request) = room.requests.get_mut(&from) {
                    request.seen = now;
                } else if room.full() {
                    self.send_to(from, self.seal(&door, RoomBody::Refuse { full: true }));
                    return;
                } else if room.requests.len() < MAX_REQUESTS {
                    let request = Request { asked, seen: now, approvals: HashSet::new(), admitted: None };
                    room.requests.insert(from, request);
                    room.closed.remove(&from);
                    let (name, need) = (room.name.clone(), needed(room.size()));
                    if let Some(nick) = self.nick_of(from) {
                        self.emit(Event::JoinRequest { room: name, nick, have: 0, need: small(need) });
                    }
                } else {
                    return;
                }
                let Some(room) = self.rooms.joined.get(&id) else {
                    return;
                };
                let reply = RoomBody::Door { echo: nonce, members: small(room.size()), max: room.max };
                self.send_to(from, self.seal(&door, reply));
                self.approve_invited(id, from, now);
            }
            RoomBody::Leave => self.close_request(id, from, RequestEnd::Gone),
            _ => {}
        }
    }

    fn close_request(&mut self, id: RoomId, peer: PeerId, why: RequestEnd) {
        let Some(room) = self.rooms.joined.get_mut(&id) else {
            return;
        };
        if room.close(peer).is_some() {
            let name = room.name.clone();
            if let Some(nick) = self.nick_of(peer) {
                self.emit(Event::RequestEnded { room: name, nick, why });
            }
        }
    }

    /// A vote from a proven member of room `id`.
    fn on_vote(&mut self, id: RoomId, from: PeerId, peer: PeerId, asked: u64, yes: bool, now: Instant) {
        let me = self.id;
        let (Some(nick), Some(by)) = (self.nick_of(peer), self.nick_of(from)) else {
            return;
        };
        let Some(room) = self.rooms.joined.get_mut(&id) else {
            return;
        };
        if peer == me || room.members.contains_key(&peer) {
            return;
        }
        let name = room.name.clone();
        if !yes {
            // Once someone's let in, a no that crossed with the yeses doesn't undo it.
            if room.requests.get(&peer).is_some_and(|r| r.admitted.is_none()) {
                room.close(peer);
                self.emit(Event::RequestEnded { room: name, nick, why: RequestEnd::Denied { by } });
            }
            return;
        }
        let need = small(needed(room.size()));
        if !room.requests.contains_key(&peer) {
            let reopens = room.closed.get(&peer).is_some_and(|(closed, _)| *closed == asked);
            if reopens || room.requests.len() >= MAX_REQUESTS {
                return;
            }
            let request = Request { asked, seen: now, approvals: HashSet::new(), admitted: None };
            room.requests.insert(peer, request);
            self.emit(Event::JoinRequest { room: name.clone(), nick: nick.clone(), have: 0, need });
        }
        let Some(room) = self.rooms.joined.get_mut(&id) else {
            return;
        };
        let request = room.requests.get_mut(&peer).expect("inserted above");
        if request.admitted.is_some() || !request.approvals.insert(from) {
            return;
        }
        let have = small(room.approvals(&room.requests[&peer], me));
        self.emit(Event::Approved { room: name, nick, by, have, need });
        self.after(ADMIT_SETTLE, NodeMsg::Settle(id));
    }

    /// Door traffic for a join we're waiting on: answers, the secret, or a no.
    fn on_answer(&mut self, door: RoomId, from: PeerId, env: RoomEnvelope, now: Instant) {
        let Some(p) = self.rooms.joining.get_mut(&door) else {
            return;
        };
        match env.body {
            RoomBody::Door { echo, members, max } if echo == p.nonce => {
                if p.waiting {
                    return;
                }
                if members >= max {
                    let name = p.name.clone();
                    self.rooms.joining.remove(&door);
                    self.emit(Event::JoinEnded { name, why: JoinEnd::Full });
                    return;
                }
                p.waiting = true;
                let (name, attempt) = (p.name.clone(), p.attempt);
                let needs = small(needed(members as usize));
                self.emit(Event::JoinWaiting { name, members, needs });
                self.after(PENDING_FOR, NodeMsg::JoinExpired(door, attempt));
            }
            RoomBody::Grant { secret, max } => {
                let Some(secret) = room::from_hex(&secret).and_then(|s| Secret::try_from(s).ok()) else {
                    return;
                };
                if !(MIN_MEMBERS..=MAX_MEMBERS).contains(&max) {
                    return;
                }
                let Some(p) = self.rooms.joining.remove(&door) else {
                    return;
                };
                let keys = room::from_secret(&p.name, &secret);
                let granter = Member { seen: now, floor: env.ts.saturating_sub(PROOF_GRACE_MS) };
                let members = HashMap::from([(from, granter)]);
                let (name, key) = (p.name.clone(), p.key.clone());
                let id = self.hold_room(p.name, p.key, max, secret, keys.clone(), p.door, members);
                let probe = fresh_nonce();
                if let Some(room) = self.rooms.joined.get_mut(&id) {
                    room.probe = Some(probe.clone());
                }
                self.broadcast(self.seal(&keys, RoomBody::Probe { nonce: probe }));
                self.broadcast(self.seal(&keys, RoomBody::Here { name: name.clone(), echo: None }));
                let members = self.nick_of(from).into_iter().collect();
                self.emit(Event::RoomJoined { name, key, max, members });
            }
            RoomBody::Refuse { full } => {
                let Some(p) = self.rooms.joining.remove(&door) else {
                    return;
                };
                let why = match (full, self.nick_of(from)) {
                    (false, Some(by)) => JoinEnd::Denied { by },
                    _ => JoinEnd::Full,
                };
                self.emit(Event::JoinEnded { name: p.name, why });
            }
            _ => {}
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
        // Unknown sender: is this the answer to our challenge, or to the probe we sent on arrival?
        let proven = match (&env.body, room.challenges.get(&from)) {
            (RoomBody::Here { name, echo: Some(e) }, c) if *name == room.name => {
                c.is_some_and(|c| c.nonce == *e) || room.probe.as_ref() == Some(e)
            }
            _ => false,
        };
        if proven {
            let mut buffered = room.challenges.remove(&from).map(|c| c.buffered).unwrap_or_default();
            room.members.insert(from, Member { seen: now, floor: env.ts.saturating_sub(PROOF_GRACE_MS) });
            let name = room.name.clone();
            let was_asking = room.requests.remove(&from).is_some();
            if let Some(nick) = self.nick_of(from) {
                if was_asking {
                    self.emit(Event::RequestEnded {
                        room: name.clone(),
                        nick: nick.clone(),
                        why: RequestEnd::Admitted,
                    });
                }
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
        let me = self.id;
        let mut probe = None;
        let challenge = room.challenges.entry(from).or_insert_with(|| {
            let nonce = fresh_nonce();
            let body = RoomBody::Probe { nonce: nonce.clone() };
            probe = Some(room::seal(&room.keys, me, &RoomEnvelope { ts: now_millis(), body }));
            Challenge { nonce, started: now, buffered: Vec::new() }
        });
        if matches!(env.body, RoomBody::Chat { .. }) && challenge.buffered.len() < MAX_BUFFERED {
            challenge.buffered.push(env);
        }
        if let Some(frame) = probe {
            self.send_to(from, frame);
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
            RoomBody::Vote { peer, asked, yes } => self.on_vote(id, from, peer, asked, yes, now),
            _ => {}
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

    /// A peer disconnected: it leaves every room we share with it, and its requests close.
    pub(super) fn forget_member_everywhere(&mut self, member: PeerId) {
        let ids: Vec<RoomId> = self.rooms.joined.keys().copied().collect();
        for id in ids {
            self.drop_member(id, member);
            self.close_request(id, member, RequestEnd::Gone);
        }
    }

    /// Every second: expire silent members, unanswered challenges and stale
    /// requests, and knock again while waiting; every `HERE_INTERVAL`: announce presence.
    pub(super) fn on_tick(&mut self, now: Instant) {
        self.rooms.invites.retain(|i| now.saturating_duration_since(i.at) < INVITE_FOR);
        self.rooms.invited.retain(|_, at| now.saturating_duration_since(*at) < INVITE_FOR);
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
        let stale: Vec<(RoomId, PeerId)> = self
            .rooms
            .joined
            .iter()
            .flat_map(|(id, r)| {
                r.requests
                    .iter()
                    .filter(|(_, q)| now.saturating_duration_since(q.admitted.unwrap_or(q.seen)) >= REQUEST_STALE)
                    .map(|(p, _)| (*id, *p))
            })
            .collect();
        for (id, peer) in stale {
            self.close_request(id, peer, RequestEnd::Gone);
        }
        for room in self.rooms.joined.values_mut() {
            room.challenges.retain(|_, c| now.saturating_duration_since(c.started) < CHALLENGE_TIMEOUT);
            room.closed.retain(|_, (_, at)| now.saturating_duration_since(*at) < REQUEST_STALE);
        }
        let mut knocks = Vec::new();
        for p in self.rooms.joining.values_mut() {
            if p.waiting && now.saturating_duration_since(p.last_knock) >= KNOCK_EVERY {
                p.last_knock = now;
                knocks.push((p.door.clone(), RoomBody::Knock { nonce: p.nonce.clone(), asked: p.asked }));
            }
        }
        for (door, knock) in knocks {
            self.broadcast(self.seal(&door, knock));
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
    use crate::api::{Command, JoinEnd, RequestEnd, Target};
    use crate::protocol::Frame;

    const DAVE: PeerId = PeerId::test(0x4000_0000_0000_0000);
    const ERIN: PeerId = PeerId::test(0x5000_0000_0000_0000);
    const NAME: &str = "catpie";
    const KEY: &str = "k7m9xq";
    const SECRET: room::Secret = [9; 32];

    /// The door keys a newcomer derives for `NAME` and `KEY`.
    fn door() -> RoomKeys {
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
        try_open(keys, frame).unwrap_or_else(|| panic!("doesn't open: {frame:?}"))
    }

    fn try_open(keys: &RoomKeys, frame: &Frame) -> Option<RoomBody> {
        let Frame::Room { nonce, ct, .. } = frame else { return None };
        room::open(keys, ME, nonce, ct).map(|(env, _)| env.body)
    }

    /// The bodies among `frames` that open with `keys`.
    fn bodies(keys: &RoomKeys, frames: &[Frame]) -> Vec<RoomBody> {
        frames.iter().filter_map(|f| try_open(keys, f)).collect()
    }

    fn probe_nonce(keys: &RoomKeys, frame: &Frame) -> String {
        match open_body(keys, frame) {
            RoomBody::Probe { nonce } => nonce,
            other => panic!("expected a probe, got {other:?}"),
        }
    }

    fn with_peers() -> Harness {
        let mut h = Harness::new();
        h.connect(BOB, "bobroger", true);
        h.connect(CAROL, "carolwolf", true);
        h.connect(DAVE, "daveoak", true);
        h.connect(ERIN, "erinowl", true);
        h.events();
        h
    }

    struct Made {
        name: String,
        key: String,
        id: RoomId,
        keys: RoomKeys,
        door: RoomKeys,
    }

    async fn create(h: &mut Harness, max: u8) -> Made {
        let name = names::pick_name("cat", |n| h.node.rooms.holds(n)).unwrap();
        h.node.handle_command(Command::CreateRoom { name: name.clone(), max });
        h.pump().await;
        let (name, key) = match h.events().as_slice() {
            [Event::RoomCreated { name, key, max: m }] => {
                assert!(name.starts_with("cat") && names::is_valid_name(name), "{name}");
                assert!(room::validate_key(key).is_ok(), "{key}");
                assert_eq!(*m, max);
                (name.clone(), key.clone())
            }
            other => panic!("expected RoomCreated, got {other:?}"),
        };
        let id = h.node.rooms.id_of(&name).unwrap();
        let room = &h.node.rooms.joined[&id];
        Made { name, key, id, keys: room.keys.clone(), door: room.door.clone() }
    }

    /// `member` announces itself, answers our challenge, and becomes a member.
    fn verify(h: &mut Harness, member: PeerId, name: &str, keys: &RoomKeys) {
        deliver(h, member, frame_from(member, keys, here(name, None)));
        let challenge = probe_nonce(keys, h.sent(member).last().expect("challenge sent"));
        deliver(h, member, frame_from(member, keys, here(name, Some(&challenge))));
    }

    fn knock(h: &mut Harness, from: PeerId, made: &Made, asked: u64) {
        deliver(h, from, frame_from(from, &made.door, RoomBody::Knock { nonce: "n1".into(), asked }));
    }

    fn approve(h: &mut Harness, made: &Made, nick: &str) {
        h.node.handle_command(Command::Approve { room: made.name.clone(), nick: nick.into() });
    }

    fn vote(h: &mut Harness, from: PeerId, made: &Made, peer: PeerId, yes: bool) {
        deliver(h, from, frame_from(from, &made.keys, RoomBody::Vote { peer, asked: 100, yes }));
    }

    fn granted(made: &Made, frames: &[Frame]) -> bool {
        bodies(&made.door, frames).iter().any(|b| match b {
            RoomBody::Grant { secret, max } => {
                let secret: room::Secret = room::from_hex(secret).unwrap().try_into().unwrap();
                assert_eq!(room::from_secret(&made.name, &secret).id, made.keys.id, "the real secret");
                assert!(*max >= MIN_MEMBERS);
                true
            }
            _ => false,
        })
    }

    fn refused(made: &Made, frames: &[Frame], full: bool) -> bool {
        bodies(&made.door, frames).contains(&RoomBody::Refuse { full })
    }

    fn request_ended(room: &str, nick: &str, why: RequestEnd) -> Event {
        Event::RequestEnded { room: room.into(), nick: nick.into(), why }
    }

    // ---- creating ----

    #[tokio::test]
    async fn creating_a_room_holds_key_and_cap() {
        let mut h = with_peers();
        h.node.handle_command(Command::CreateRoom { name: "catpie".into(), max: 1 });
        h.node.handle_command(Command::CreateRoom { name: "catpie".into(), max: 14 });
        let e = Event::Error("a room holds 2–13 people".into());
        assert_eq!(h.events(), vec![e.clone(), e]);
        let made = create(&mut h, 5).await;
        assert_ne!(made.keys.id, made.door.id);
        assert_eq!(made.door.id, room::derive(&made.name, &made.key).id, "the key opens the door");
    }

    #[tokio::test]
    async fn room_count_is_capped_at_three_and_waiting_counts() {
        let mut h = with_peers();
        create(&mut h, 13).await;
        create(&mut h, 13).await;
        h.node.handle_command(Command::JoinRoom { name: "dogpie".into(), key: KEY.into() });
        // `create` only picks cat… names, so this one is free.
        h.node.handle_command(Command::CreateRoom { name: "owlpie".into(), max: 13 });
        assert_eq!(h.events(), vec![Event::Error(format!("you're in {MAX_ROOMS} rooms (the max) — /leave one first"))]);
    }

    #[tokio::test]
    async fn creating_a_room_you_are_in_is_refused() {
        let mut h = with_peers();
        let made = create(&mut h, 5).await;
        h.node.handle_command(Command::CreateRoom { name: made.name.clone(), max: 5 });
        assert_eq!(h.events(), vec![Event::Error(format!("you're already in #{}", made.name))]);
    }

    #[test]
    fn needed_approvals_follow_a_third_of_the_members() {
        for (members, need) in [(1, 1), (2, 1), (3, 1), (4, 2), (6, 2), (7, 3), (9, 3), (10, 4), (12, 4)] {
            assert_eq!(needed(members), need, "{members} members");
        }
    }

    #[test]
    fn bad_room_input_is_rejected_up_front() {
        let mut h = with_peers();
        h.node.handle_command(Command::CreateRoom { name: "chat".into(), max: 5 });
        h.node.handle_command(Command::JoinRoom { name: "catPie".into(), key: KEY.into() });
        h.node.handle_command(Command::JoinRoom { name: NAME.into(), key: "123-k7m9xq".into() });
        h.node.handle_command(Command::LeaveRoom { name: NAME.into() });
        h.node.handle_command(Command::Send { to: Target::Room(NAME.into()), text: "x".into() });
        h.node.handle_command(Command::Approve { room: NAME.into(), nick: "bobroger".into() });
        let errors: Vec<String> =
            h.events().into_iter().map(|e| if let Event::Error(m) = e { m } else { panic!("{e:?}") }).collect();
        assert_eq!(errors[0], "#chat isn't a room name");
        assert_eq!(errors[1], "#catPie isn't a room name");
        assert!(errors[2].starts_with("a room key is 6"));
        assert_eq!(errors[3..], ["you're not in #catpie"; 3]);
        assert!(h.sent(BOB).is_empty());
    }

    // ---- members: requests and votes ----

    #[tokio::test]
    async fn knock_gets_a_private_door_reply_and_announces_once() {
        let mut h = with_peers();
        let made = create(&mut h, 13).await;
        knock(&mut h, BOB, &made, 100);
        assert_eq!(bodies(&made.door, &h.sent(BOB)), vec![RoomBody::Door { echo: "n1".into(), members: 1, max: 13 }]);
        assert!(h.sent(CAROL).is_empty(), "the reply goes only to the knocker");
        assert_eq!(
            h.events(),
            vec![Event::JoinRequest { room: made.name.clone(), nick: "bobroger".into(), have: 0, need: 1 }]
        );
    }

    #[tokio::test]
    async fn reknocks_refresh_without_reannouncing() {
        let mut h = with_peers();
        let made = create(&mut h, 13).await;
        knock(&mut h, BOB, &made, 100);
        h.events();
        knock(&mut h, BOB, &made, 200);
        assert!(h.events().is_empty());
        assert_eq!(h.node.rooms.joined[&made.id].requests[&BOB].asked, 100, "the first ask keeps its place");
    }

    #[tokio::test]
    async fn full_room_refuses_a_knock() {
        let mut h = with_peers();
        let made = create(&mut h, 2).await;
        verify(&mut h, CAROL, &made.name, &made.keys);
        h.events();
        knock(&mut h, BOB, &made, 100);
        assert!(refused(&made, &h.sent(BOB), true));
        assert!(h.events().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn approving_alone_in_a_small_room_grants_the_secret() {
        let mut h = with_peers();
        let made = create(&mut h, 13).await;
        knock(&mut h, BOB, &made, 100);
        h.events();
        h.sent(BOB);
        approve(&mut h, &made, "bobroger");
        assert_eq!(
            h.events(),
            vec![Event::Approved {
                room: made.name.clone(),
                nick: "bobroger".into(),
                by: "annmoon".into(),
                have: 1,
                need: 1
            }]
        );
        assert!(
            bodies(&made.keys, &h.sent(CAROL))
                .iter()
                .any(|b| matches!(b, RoomBody::Vote { peer, yes: true, .. } if *peer == BOB)),
            "the vote goes to the room"
        );
        h.pump().await; // Settle
        assert!(granted(&made, &h.sent(BOB)));
    }

    #[tokio::test(start_paused = true)]
    async fn two_approvals_needed_with_four_members() {
        let mut h = with_peers();
        let made = create(&mut h, 13).await;
        for p in [CAROL, DAVE, ERIN] {
            verify(&mut h, p, &made.name, &made.keys);
        }
        knock(&mut h, BOB, &made, 100);
        h.events();
        approve(&mut h, &made, "bobroger");
        assert!(matches!(h.events().as_slice(), [Event::Approved { have: 1, need: 2, .. }]));
        h.pump().await; // Settle: not enough yet
        assert!(!granted(&made, &h.sent(BOB)));
        vote(&mut h, CAROL, &made, BOB, true);
        assert!(matches!(h.events().as_slice(), [Event::Approved { by, have: 2, need: 2, .. }] if by == "carolwolf"));
        h.pump().await; // Settle
        assert!(granted(&made, &h.sent(BOB)), "we approved, so we hand over the secret");
    }

    #[tokio::test(start_paused = true)]
    async fn only_approvers_hand_over_the_secret() {
        let mut h = with_peers();
        let made = create(&mut h, 13).await;
        verify(&mut h, CAROL, &made.name, &made.keys);
        knock(&mut h, BOB, &made, 100);
        h.sent(BOB);
        vote(&mut h, CAROL, &made, BOB, true);
        h.pump().await; // Settle
        assert!(!granted(&made, &h.sent(BOB)));
        assert!(h.node.rooms.joined[&made.id].requests[&BOB].admitted.is_some(), "but it holds a slot");
    }

    #[tokio::test]
    async fn denying_refuses_and_tells_members() {
        let mut h = with_peers();
        let made = create(&mut h, 13).await;
        knock(&mut h, BOB, &made, 100);
        h.events();
        h.sent(BOB);
        h.node.handle_command(Command::Deny { room: made.name.clone(), nick: "bobroger".into() });
        assert!(refused(&made, &h.sent(BOB), false));
        assert!(
            bodies(&made.keys, &h.sent(CAROL))
                .iter()
                .any(|b| matches!(b, RoomBody::Vote { peer, yes: false, .. } if *peer == BOB))
        );
        assert_eq!(
            h.events(),
            vec![request_ended(&made.name, "bobroger", RequestEnd::Denied { by: "annmoon".into() })]
        );
        approve(&mut h, &made, "bobroger");
        assert_eq!(h.events(), vec![Event::Error(format!("@bobroger isn't waiting to join #{}", made.name))]);
    }

    #[tokio::test]
    async fn a_members_deny_ends_the_request() {
        let mut h = with_peers();
        let made = create(&mut h, 13).await;
        verify(&mut h, CAROL, &made.name, &made.keys);
        knock(&mut h, BOB, &made, 100);
        h.events();
        vote(&mut h, CAROL, &made, BOB, false);
        assert_eq!(
            h.events(),
            vec![request_ended(&made.name, "bobroger", RequestEnd::Denied { by: "carolwolf".into() })]
        );
        assert!(h.node.rooms.joined[&made.id].requests.is_empty());
    }

    #[tokio::test]
    async fn a_late_yes_does_not_revive_a_closed_request() {
        let mut h = with_peers();
        let made = create(&mut h, 13).await;
        verify(&mut h, CAROL, &made.name, &made.keys);
        knock(&mut h, BOB, &made, 100);
        knock(&mut h, DAVE, &made, 100);
        h.node.handle_command(Command::Deny { room: made.name.clone(), nick: "bobroger".into() });
        deliver(&mut h, DAVE, frame_from(DAVE, &made.door, RoomBody::Leave));
        h.events();
        // carol approved each of them at the same moment.
        vote(&mut h, CAROL, &made, BOB, true);
        vote(&mut h, CAROL, &made, DAVE, true);
        assert!(h.events().is_empty());
        assert!(h.node.rooms.joined[&made.id].requests.is_empty());
        // A fresh request (a new ask) is welcome again.
        knock(&mut h, BOB, &made, 500);
        assert!(matches!(h.events().as_slice(), [Event::JoinRequest { nick, .. }] if nick == "bobroger"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_late_no_does_not_undo_letting_someone_in() {
        let mut h = with_peers();
        let made = create(&mut h, 13).await;
        verify(&mut h, CAROL, &made.name, &made.keys);
        knock(&mut h, BOB, &made, 100);
        approve(&mut h, &made, "bobroger");
        h.pump().await; // Settle: bob is let in
        h.events();
        vote(&mut h, CAROL, &made, BOB, false);
        assert!(h.events().is_empty());
        assert!(h.node.rooms.joined[&made.id].requests[&BOB].admitted.is_some(), "still holds his slot");
    }

    #[tokio::test]
    async fn votes_from_non_members_are_ignored() {
        let mut h = with_peers();
        let made = create(&mut h, 13).await;
        knock(&mut h, BOB, &made, 100);
        h.events();
        // DAVE holds the room keys (say, an old member) but hasn't proven himself.
        vote(&mut h, DAVE, &made, BOB, true);
        assert!(h.events().is_empty());
        assert!(h.node.rooms.joined[&made.id].requests[&BOB].approvals.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn last_slot_goes_to_whoever_asked_first() {
        let mut h = with_peers();
        let made = create(&mut h, 3).await;
        verify(&mut h, CAROL, &made.name, &made.keys); // 2 of 3: one slot left
        knock(&mut h, BOB, &made, 200);
        knock(&mut h, DAVE, &made, 100);
        h.events();
        h.sent(BOB);
        h.sent(DAVE);
        approve(&mut h, &made, "bobroger");
        approve(&mut h, &made, "daveoak");
        h.events();
        h.pump().await; // Settle
        h.pump().await; // Settle (nothing left to do)
        assert!(granted(&made, &h.sent(DAVE)), "dave asked first");
        assert!(refused(&made, &h.sent(BOB), true));
        assert_eq!(h.events(), vec![request_ended(&made.name, "bobroger", RequestEnd::Full)]);
    }

    #[tokio::test(start_paused = true)]
    async fn admitted_request_that_never_arrives_frees_its_slot() {
        let mut h = with_peers();
        let made = create(&mut h, 2).await;
        knock(&mut h, BOB, &made, 100);
        approve(&mut h, &made, "bobroger");
        h.pump().await; // Settle: bob is let in and holds the only slot
        h.events();
        knock(&mut h, DAVE, &made, 300);
        assert!(refused(&made, &h.sent(DAVE), true));
        h.node.on_tick(Instant::now() + REQUEST_STALE);
        assert_eq!(h.events(), vec![request_ended(&made.name, "bobroger", RequestEnd::Gone)]);
        knock(&mut h, DAVE, &made, 300);
        assert!(matches!(h.events().as_slice(), [Event::JoinRequest { nick, .. }] if nick == "daveoak"));
    }

    #[tokio::test(start_paused = true)]
    async fn the_newcomer_arriving_closes_its_request() {
        let mut h = with_peers();
        let made = create(&mut h, 13).await;
        knock(&mut h, BOB, &made, 100);
        approve(&mut h, &made, "bobroger");
        h.pump().await; // Settle
        h.events();
        verify(&mut h, BOB, &made.name, &made.keys);
        assert_eq!(
            h.events(),
            vec![
                request_ended(&made.name, "bobroger", RequestEnd::Admitted),
                Event::MemberJoined { room: made.name.clone(), nick: "bobroger".into() }
            ]
        );
        assert!(h.node.rooms.joined[&made.id].requests.is_empty());
    }

    #[tokio::test]
    async fn a_knocker_leaving_or_going_offline_closes_its_request() {
        let mut h = with_peers();
        let made = create(&mut h, 13).await;
        knock(&mut h, BOB, &made, 100);
        knock(&mut h, CAROL, &made, 100);
        h.events();
        deliver(&mut h, BOB, frame_from(BOB, &made.door, RoomBody::Leave));
        assert_eq!(h.events(), vec![request_ended(&made.name, "bobroger", RequestEnd::Gone)]);
        let conn = h.node.peers[&CAROL].conn;
        h.node.handle_msg(NodeMsg::PeerGone { conn, id: CAROL, reason: crate::api::LeaveReason::Lost });
        assert!(h.events().contains(&request_ended(&made.name, "carolwolf", RequestEnd::Gone)));
    }

    #[tokio::test]
    async fn quiet_requests_go_stale() {
        let mut h = with_peers();
        let made = create(&mut h, 13).await;
        knock(&mut h, BOB, &made, 100);
        h.events();
        h.node.on_tick(Instant::now() + REQUEST_STALE);
        assert_eq!(h.events(), vec![request_ended(&made.name, "bobroger", RequestEnd::Gone)]);
    }

    // ---- newcomers ----

    /// Starts joining `NAME`; returns the knock's nonce and ask time.
    async fn start_join(h: &mut Harness) -> (String, u64) {
        h.node.handle_command(Command::JoinRoom { name: NAME.into(), key: KEY.into() });
        h.pump().await; // Derived
        h.sent(CAROL);
        match open_body(&door(), &h.sent(BOB)[0]) {
            RoomBody::Knock { nonce, asked } => (nonce, asked),
            other => panic!("expected a knock, got {other:?}"),
        }
    }

    fn door_says(h: &mut Harness, from: PeerId, nonce: &str, members: u8, max: u8) {
        deliver(h, from, frame_from(from, &door(), RoomBody::Door { echo: nonce.into(), members, max }));
    }

    #[tokio::test(start_paused = true)]
    async fn newcomer_waits_then_joins_on_grant() {
        let mut h = with_peers();
        let (nonce, _) = start_join(&mut h).await;
        door_says(&mut h, BOB, &nonce, 4, 13);
        assert_eq!(h.events(), vec![Event::JoinWaiting { name: NAME.into(), members: 4, needs: 2 }]);
        door_says(&mut h, CAROL, &nonce, 4, 13);
        door_says(&mut h, DAVE, "stale", 4, 13);
        assert!(h.events().is_empty(), "announced once");

        let grant = RoomBody::Grant { secret: room::to_hex(&SECRET), max: 13 };
        deliver(&mut h, BOB, frame_from(BOB, &door(), grant.clone()));
        assert_eq!(
            h.events(),
            vec![Event::RoomJoined { name: NAME.into(), key: KEY.into(), max: 13, members: vec!["bobroger".into()] }]
        );
        deliver(&mut h, CAROL, frame_from(CAROL, &door(), grant));
        assert!(h.events().is_empty(), "a second grant changes nothing");

        // We ask who's in on the room channel and say we're here.
        let keys = room::from_secret(NAME, &SECRET);
        let sent = bodies(&keys, &h.sent(CAROL));
        let RoomBody::Probe { nonce: probe } = &sent[0] else { panic!("{sent:?}") };
        assert!(matches!(&sent[1], RoomBody::Here { echo: None, .. }));
        deliver(&mut h, CAROL, frame_from(CAROL, &keys, here(NAME, Some(probe))));
        assert_eq!(h.events(), vec![Event::MemberJoined { room: NAME.into(), nick: "carolwolf".into() }]);
        deliver(&mut h, BOB, frame_from(BOB, &keys, RoomBody::Chat { text: "welcome".into() }));
        assert!(matches!(h.events().as_slice(), [Event::Message { text, .. }] if text == "welcome"));
    }

    #[tokio::test(start_paused = true)]
    async fn newcomer_hears_nothing_and_gets_no_room() {
        let mut h = with_peers();
        start_join(&mut h).await;
        // bob is in a room of that name under another key: we can't read his frames.
        let other = room::derive(NAME, "k7m9xr");
        deliver(&mut h, BOB, frame_from(BOB, &other, RoomBody::Door { echo: "x".into(), members: 1, max: 13 }));
        h.pump().await; // JoinTimeout
        assert_eq!(h.events(), vec![Event::JoinEnded { name: NAME.into(), why: JoinEnd::NoRoom }]);
        assert_eq!(h.node.rooms.count(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn newcomer_gives_up_after_two_minutes_and_reknocks_meanwhile() {
        let mut h = with_peers();
        let (nonce, asked) = start_join(&mut h).await;
        door_says(&mut h, BOB, &nonce, 1, 13);
        h.events();
        let t0 = Instant::now();
        h.node.on_tick(t0 + KNOCK_EVERY / 2);
        assert!(bodies(&door(), &h.sent(BOB)).is_empty(), "too soon");
        h.node.on_tick(t0 + KNOCK_EVERY);
        assert_eq!(bodies(&door(), &h.sent(BOB)), vec![RoomBody::Knock { nonce: nonce.clone(), asked }]);
        h.pump().await; // JoinTimeout: we're waiting, so nothing
        assert!(h.events().is_empty());
        h.pump().await; // JoinExpired
        assert_eq!(h.events(), vec![Event::JoinEnded { name: NAME.into(), why: JoinEnd::TimedOut }]);
        assert_eq!(bodies(&door(), &h.sent(BOB)), vec![RoomBody::Leave], "members forget us");
        assert_eq!(h.node.rooms.count(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn newcomer_is_told_when_the_room_is_full() {
        let mut h = with_peers();
        let (nonce, _) = start_join(&mut h).await;
        door_says(&mut h, BOB, &nonce, 13, 13);
        assert_eq!(h.events(), vec![Event::JoinEnded { name: NAME.into(), why: JoinEnd::Full }]);
        assert_eq!(h.node.rooms.count(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn newcomer_is_told_when_refused() {
        let mut h = with_peers();
        let (nonce, _) = start_join(&mut h).await;
        door_says(&mut h, BOB, &nonce, 2, 13);
        h.events();
        deliver(&mut h, CAROL, frame_from(CAROL, &door(), RoomBody::Refuse { full: false }));
        assert_eq!(
            h.events(),
            vec![Event::JoinEnded { name: NAME.into(), why: JoinEnd::Denied { by: "carolwolf".into() } }]
        );
        let (nonce, _) = start_join(&mut h).await;
        door_says(&mut h, BOB, &nonce, 2, 13);
        h.events();
        deliver(&mut h, BOB, frame_from(BOB, &door(), RoomBody::Refuse { full: true }));
        assert_eq!(h.events(), vec![Event::JoinEnded { name: NAME.into(), why: JoinEnd::Full }]);
    }

    #[tokio::test(start_paused = true)]
    async fn leaving_while_waiting_tells_members_and_frees_the_slot() {
        let mut h = with_peers();
        let (nonce, _) = start_join(&mut h).await;
        door_says(&mut h, BOB, &nonce, 1, 13);
        h.events();
        h.node.handle_command(Command::LeaveRoom { name: NAME.into() });
        assert_eq!(h.events(), vec![Event::RoomLeft { name: NAME.into() }]);
        assert_eq!(bodies(&door(), &h.sent(BOB)), vec![RoomBody::Leave]);
        assert_eq!(h.node.rooms.count(), 0);
        let grant = RoomBody::Grant { secret: room::to_hex(&SECRET), max: 13 };
        deliver(&mut h, BOB, frame_from(BOB, &door(), grant));
        assert!(h.events().is_empty(), "a late grant is ignored");
    }

    #[tokio::test(start_paused = true)]
    async fn leaving_cancels_a_join_still_deriving() {
        let mut h = with_peers();
        h.node.handle_command(Command::JoinRoom { name: NAME.into(), key: KEY.into() });
        h.node.handle_command(Command::LeaveRoom { name: NAME.into() });
        assert_eq!(h.events(), vec![Event::RoomLeft { name: NAME.into() }]);
        h.pump().await; // Derived: dropped
        assert!(h.sent(BOB).is_empty(), "no knock for a cancelled join");
        assert!(h.events().is_empty());
        assert_eq!(h.node.rooms.count(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn an_earlier_attempts_timer_does_not_end_a_rejoin() {
        let mut h = with_peers();
        start_join(&mut h).await;
        h.node.handle_command(Command::LeaveRoom { name: NAME.into() });
        h.sent(BOB);
        let (nonce, _) = start_join(&mut h).await;
        h.events();
        h.pump().await; // the first attempt's JoinTimeout: not ours any more
        assert!(h.events().is_empty());
        door_says(&mut h, BOB, &nonce, 1, 13);
        assert!(matches!(h.events().as_slice(), [Event::JoinWaiting { .. }]));
    }

    // ---- members: proof, replay and presence (unchanged by the gate) ----

    #[tokio::test]
    async fn creating_a_room_makes_us_its_only_member() {
        let mut h = with_peers();
        let made = create(&mut h, 13).await;
        h.node.handle_command(Command::Send { to: Target::Room(made.name.clone()), text: "anyone?".into() });
        // Sent to everyone (only members can read it), recorded, and we're told we're alone.
        assert!(matches!(h.sent(BOB).as_slice(), [Frame::Room { .. }]));
        assert!(matches!(h.sent(CAROL).as_slice(), [Frame::Room { .. }]));
        let events = h.events();
        assert!(matches!(&events[0], Event::Sent { conv: Conv::Room(n), .. } if *n == made.name));
        assert_eq!(events[1], Event::Info(format!("nobody else is in #{} yet", made.name)));
    }

    #[tokio::test]
    async fn escaped_text_is_what_counts_against_the_limit() {
        let mut h = with_peers();
        let name = create(&mut h, 13).await.name;
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

    #[tokio::test]
    async fn members_answer_probes_privately_and_newcomers_must_prove_themselves() {
        let mut h = with_peers();
        let Made { name, keys, .. } = create(&mut h, 13).await;

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
        assert_eq!(h.events(), vec![Event::MemberJoined { room: name.clone(), nick: "bobroger".into() }]);
        deliver(&mut h, BOB, frame_from(BOB, &keys, here(&name, None)));
        assert!(h.events().is_empty(), "already a member");

        deliver(&mut h, BOB, frame_from(BOB, &keys, RoomBody::Leave));
        assert_eq!(h.events(), vec![Event::MemberLeft { room: name, nick: "bobroger".into() }]);
    }

    #[tokio::test]
    async fn a_newcomers_first_messages_wait_for_its_proof() {
        let mut h = with_peers();
        let Made { name, keys, .. } = create(&mut h, 13).await;
        deliver(&mut h, BOB, frame_from(BOB, &keys, RoomBody::Chat { text: "early".into() }));
        assert!(h.events().is_empty());
        let challenge = probe_nonce(&keys, &h.sent(BOB)[0]);
        deliver(&mut h, BOB, frame_from(BOB, &keys, here(&name, Some(&challenge))));
        let events = h.events();
        assert_eq!(events[0], Event::MemberJoined { room: name.clone(), nick: "bobroger".into() });
        assert!(matches!(&events[1], Event::Message { text, .. } if text == "early"), "{events:?}");
        assert_eq!(events.len(), 2);
    }

    #[tokio::test]
    async fn replayed_old_messages_are_dropped_even_with_fresh_nonces() {
        let mut h = with_peers();
        let Made { name, keys, .. } = create(&mut h, 13).await;
        let now = now_millis();
        // Captured a minute ago and replayed before bob is verified: buffered...
        deliver(&mut h, BOB, frame_at(BOB, &keys, now - 60_000, RoomBody::Chat { text: "old".into() }));
        let challenge = probe_nonce(&keys, &h.sent(BOB)[0]);
        deliver(&mut h, BOB, frame_from(BOB, &keys, here(&name, Some(&challenge))));
        // ...but older than his proof, so it's dropped once he's verified.
        assert_eq!(h.events(), vec![Event::MemberJoined { room: name.clone(), nick: "bobroger".into() }]);
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
        let Made { name, keys, .. } = create(&mut h, 13).await;
        deliver(&mut h, BOB, frame_from(BOB, &keys, RoomBody::Chat { text: "spoofed".into() }));
        let challenge = probe_nonce(&keys, &h.sent(BOB)[0]);
        h.node.on_tick(Instant::now() + CHALLENGE_TIMEOUT);
        deliver(&mut h, BOB, frame_from(BOB, &keys, here(&name, Some(&challenge))));
        assert!(!h.events().iter().any(|e| matches!(e, Event::MemberJoined { .. } | Event::Message { .. })));
    }

    #[tokio::test]
    async fn silent_members_expire_and_presence_is_announced() {
        let mut h = with_peers();
        let Made { name, keys, id, .. } = create(&mut h, 13).await;
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
        assert_eq!(h.events(), vec![Event::MemberLeft { room: name, nick: "bobroger".into() }]);
    }

    #[tokio::test]
    async fn replays_unknown_rooms_and_wrong_senders_are_dropped() {
        let mut h = with_peers();
        let Made { name, keys, .. } = create(&mut h, 13).await;
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
        deliver(
            &mut h,
            BOB,
            frame_from(BOB, &room::from_secret("dogpie", &SECRET), RoomBody::Chat { text: "x".into() }),
        );
        assert!(h.events().is_empty());
    }

    #[tokio::test]
    async fn a_peer_whose_clock_is_off_is_named_once() {
        let mut h = with_peers();
        let keys = create(&mut h, 13).await.keys;
        let ten_min_ago = now_millis() - 10 * 60 * 1000;
        for text in ["one", "two"] {
            deliver(&mut h, BOB, frame_at(BOB, &keys, ten_min_ago, RoomBody::Chat { text: text.into() }));
        }
        assert_eq!(
            h.events(),
            vec![Event::Info("@bobroger's clock is 10 min off; their room messages are ignored".into())]
        );
    }

    #[tokio::test]
    async fn leaving_tells_members_and_disconnects_drop_members() {
        let mut h = with_peers();
        let Made { name, keys, .. } = create(&mut h, 13).await;
        verify(&mut h, BOB, &name, &keys);
        h.events();

        let conn = h.node.peers[&BOB].conn;
        h.node.handle_msg(NodeMsg::PeerGone { conn, id: BOB, reason: crate::api::LeaveReason::Lost });
        assert!(h.events().contains(&Event::MemberLeft { room: name.clone(), nick: "bobroger".into() }));

        h.sent(CAROL);
        h.node.handle_command(Command::LeaveRoom { name: name.clone() });
        assert!(matches!(open_body(&keys, &h.sent(CAROL)[0]), RoomBody::Leave));
        assert_eq!(h.events(), vec![Event::RoomLeft { name: name.clone() }]);
        assert!(h.node.rooms.id_of(&name).is_none());
        assert!(h.node.rooms.doors.is_empty(), "the door closes too");
    }

    // ---- invitations: inviting ----

    fn invite(h: &mut Harness, nick: &str, room: &str) {
        h.node.handle_command(Command::Invite { nick: nick.into(), room: room.into() });
    }

    fn approved(events: &[Event], nick: &str) -> usize {
        events
            .iter()
            .filter(|e| matches!(e, Event::Approved { nick: n, by, .. } if n == nick && by == "annmoon"))
            .count()
    }

    #[tokio::test]
    async fn an_invite_goes_to_that_peer_only_with_the_key() {
        let mut h = with_peers();
        let made = create(&mut h, 13).await;
        h.sent(BOB);
        h.sent(CAROL);
        invite(&mut h, "bobroger", &made.name);
        assert_eq!(h.sent(BOB), vec![Frame::Invite { room: made.name.clone(), key: made.key.clone() }]);
        assert!(h.sent(CAROL).is_empty());
        assert_eq!(h.events(), vec![Event::Info(format!("invited @bobroger to #{}", made.name))]);
    }

    #[tokio::test]
    async fn inviting_checks_the_room_and_the_person() {
        let mut h = with_peers();
        let made = create(&mut h, 13).await;
        verify(&mut h, CAROL, &made.name, &made.keys);
        h.events();
        invite(&mut h, "bobroger", "dogpie");
        invite(&mut h, "zedmoon", &made.name);
        invite(&mut h, "annmoon", &made.name);
        invite(&mut h, "carolwolf", &made.name);
        let n = &made.name;
        assert_eq!(
            h.events(),
            vec![
                Event::Error("you're not in #dogpie".into()),
                Event::Error("@zedmoon is offline".into()),
                Event::Error(format!("you're already in #{n}")),
                Event::Error(format!("@carolwolf is already in #{n}")),
            ]
        );
        assert!(!h.sent(BOB).iter().any(|f| matches!(f, Frame::Invite { .. })));
        assert!(!h.sent(CAROL).iter().any(|f| matches!(f, Frame::Invite { .. })));
    }

    #[tokio::test]
    async fn an_invited_knock_is_approved_for_us() {
        let mut h = with_peers();
        let made = create(&mut h, 13).await;
        invite(&mut h, "bobroger", &made.name);
        h.events();
        knock(&mut h, CAROL, &made, 100);
        knock(&mut h, BOB, &made, 100);
        knock(&mut h, BOB, &made, 100); // the re-knock 10 s later
        let events = h.events();
        assert_eq!(approved(&events, "bobroger"), 1, "{events:?}");
        assert_eq!(approved(&events, "carolwolf"), 0, "only the one we invited");
    }

    #[tokio::test]
    async fn the_invite_counts_even_if_a_vote_opened_the_request() {
        let mut h = with_peers();
        let made = create(&mut h, 13).await;
        for member in [CAROL, DAVE, ERIN] {
            verify(&mut h, member, &made.name, &made.keys);
        }
        invite(&mut h, "bobroger", &made.name);
        h.events();
        vote(&mut h, CAROL, &made, BOB, true);
        knock(&mut h, BOB, &made, 100);
        let events = h.events();
        assert_eq!(approved(&events, "bobroger"), 1, "{events:?}");
    }

    #[tokio::test]
    async fn the_invite_follows_a_rename() {
        let mut h = with_peers();
        let made = create(&mut h, 13).await;
        invite(&mut h, "bobroger", &made.name);
        deliver(&mut h, BOB, Frame::Nick { nick: "bobpie".into() });
        h.events();
        knock(&mut h, BOB, &made, 100);
        assert_eq!(approved(&h.events(), "bobpie"), 1);
    }

    #[tokio::test]
    async fn only_an_invited_peer_can_decline() {
        let mut h = with_peers();
        let made = create(&mut h, 13).await;
        invite(&mut h, "bobroger", &made.name);
        h.events();
        deliver(&mut h, CAROL, Frame::Decline { room: made.name.clone() });
        assert!(h.events().is_empty(), "carol wasn't invited");
        deliver(&mut h, BOB, Frame::Decline { room: made.name.clone() });
        assert_eq!(h.events(), vec![Event::InviteDeclined { by: "bobroger".into(), room: made.name.clone() }]);
        deliver(&mut h, BOB, Frame::Decline { room: made.name.clone() });
        assert!(h.events().is_empty(), "only once");
        knock(&mut h, BOB, &made, 100);
        assert_eq!(approved(&h.events(), "bobroger"), 0, "a declined invite no longer approves");
    }

    #[tokio::test]
    async fn an_old_invite_no_longer_approves() {
        let mut h = with_peers();
        let made = create(&mut h, 13).await;
        invite(&mut h, "bobroger", &made.name);
        h.node.on_tick(Instant::now() + INVITE_FOR);
        h.events();
        knock(&mut h, BOB, &made, 100);
        assert_eq!(approved(&h.events(), "bobroger"), 0);
    }

    // ---- invitations: being invited ----

    fn invite_from(h: &mut Harness, from: PeerId, room: &str) {
        deliver(h, from, Frame::Invite { room: room.into(), key: KEY.into() });
    }

    fn invited_rooms(h: &Harness) -> Vec<&str> {
        h.node.rooms.invites.iter().map(|i| i.room.as_str()).collect()
    }

    #[tokio::test]
    async fn invites_arrive_and_bad_ones_are_dropped() {
        let mut h = with_peers();
        invite_from(&mut h, BOB, NAME);
        assert_eq!(h.events(), vec![Event::Invited { from: "bobroger".into(), room: NAME.into() }]);
        deliver(&mut h, BOB, Frame::Invite { room: "CatPie".into(), key: KEY.into() });
        deliver(&mut h, BOB, Frame::Invite { room: NAME.into(), key: "nope".into() });
        assert!(h.events().is_empty());
        assert_eq!(invited_rooms(&h), [NAME]);
    }

    #[tokio::test]
    async fn an_invite_to_a_room_we_are_in_is_ignored() {
        let mut h = with_peers();
        let made = create(&mut h, 13).await;
        deliver(&mut h, BOB, Frame::Invite { room: made.name.clone(), key: made.key.clone() });
        assert!(h.events().is_empty());
    }

    #[tokio::test]
    async fn a_repeat_replaces_and_the_sixth_drops_the_oldest() {
        let mut h = with_peers();
        for room in ["apie", "bpie", "cpie", "dpie", "epie"] {
            invite_from(&mut h, BOB, room);
        }
        invite_from(&mut h, CAROL, "cpie");
        invite_from(&mut h, BOB, "fpie");
        assert_eq!(invited_rooms(&h), ["bpie", "dpie", "epie", "cpie", "fpie"]);
        assert_eq!(h.node.rooms.invites[3].from, CAROL);
    }

    #[tokio::test]
    async fn accept_knocks_with_the_invited_key() {
        let mut h = with_peers();
        invite_from(&mut h, BOB, NAME);
        h.events();
        h.sent(CAROL);
        h.node.handle_command(Command::Accept { room: NAME.into() });
        h.pump().await; // the door keys finish deriving
        let knock = h.sent(CAROL).iter().find_map(|f| try_open(&door(), f));
        assert!(matches!(knock, Some(RoomBody::Knock { .. })), "{knock:?}");
        assert!(invited_rooms(&h).is_empty());
        h.node.handle_command(Command::Accept { room: NAME.into() });
        assert!(h.events().contains(&Event::Error(format!("no invitation to #{NAME}"))));
    }

    #[tokio::test]
    async fn accept_at_the_room_limit_keeps_the_invite() {
        let mut h = with_peers();
        for _ in 0..MAX_ROOMS {
            create(&mut h, 13).await;
        }
        invite_from(&mut h, BOB, "owlpie");
        h.events();
        h.node.handle_command(Command::Accept { room: "owlpie".into() });
        assert_eq!(h.events(), vec![Event::Error(format!("you're in {MAX_ROOMS} rooms (the max) — /leave one first"))]);
        assert_eq!(invited_rooms(&h), ["owlpie"]);
    }

    #[tokio::test]
    async fn accepting_a_room_you_joined_meanwhile_drops_the_invite() {
        let mut h = with_peers();
        invite_from(&mut h, BOB, "owlpie");
        h.node.handle_command(Command::CreateRoom { name: "owlpie".into(), max: 13 });
        h.pump().await;
        h.events();
        h.node.handle_command(Command::Accept { room: "owlpie".into() });
        assert_eq!(h.events(), vec![Event::Error("you're already in #owlpie".into())]);
        assert!(invited_rooms(&h).is_empty());
    }

    #[tokio::test]
    async fn decline_tells_the_inviter() {
        let mut h = with_peers();
        invite_from(&mut h, BOB, NAME);
        h.events();
        h.sent(BOB);
        h.node.handle_command(Command::Decline { room: NAME.into() });
        assert_eq!(h.sent(BOB), vec![Frame::Decline { room: NAME.into() }]);
        assert_eq!(h.events(), vec![Event::Info(format!("declined #{NAME}"))]);
        h.node.handle_command(Command::Decline { room: NAME.into() });
        assert_eq!(h.events(), vec![Event::Error(format!("no invitation to #{NAME}"))]);
    }

    #[tokio::test]
    async fn invites_expire() {
        let mut h = with_peers();
        invite_from(&mut h, BOB, NAME);
        h.node.on_tick(Instant::now() + INVITE_FOR);
        assert!(invited_rooms(&h).is_empty());
    }
}
