//! The core actor. Owns the peer table and room state; everything else talks
//! to it via channels. Room logic lives in `node/rooms.rs`.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tracing::{debug, warn};

use crate::api::{Command, Conv, Event, LeaveReason, PeerInfo, Target};
use crate::discovery::{self, Sighting};
use crate::identity::Keys;
use crate::known::{self, Judgement};
use crate::names;
use crate::peer::{self, ConnId, Me};
use crate::protocol::{Frame, MAX_FRAME_LEN, PeerId, now_millis};
use crate::room::{RoomId, RoomKeys};

mod rooms;

pub use rooms::{
    ADMIT_SETTLE, HERE_INTERVAL, JOIN_WAIT, KNOCK_EVERY, MAX_MEMBERS, MAX_ROOMS, MEMBER_TIMEOUT, MIN_MEMBERS,
    PENDING_FOR, REQUEST_STALE, needed,
};

/// Longest chat text accepted, measured JSON-escaped (a control character
/// becomes 6 bytes), leaving headroom under `MAX_FRAME_LEN` for hex and JSON.
pub const MAX_TEXT_LEN: usize = MAX_FRAME_LEN / 8;
/// Connected peers beyond this are refused, so a hostile host can't exhaust file descriptors.
pub const MAX_PEERS: usize = 64;
/// Name changes accepted from one peer. A real one follows a same-moment
/// clash and happens once; more is a peer churning names.
pub const MAX_RENAMES: u8 = 3;
/// Messages a peer may send in a burst before being throttled to `RATE_PER_SEC`.
pub const RATE_BURST: u32 = 30;
const RATE_PER_SEC: f64 = 10.0;
/// How long to listen to discovery before picking our name.
pub const NAME_WAIT: Duration = Duration::from_secs(2);
const TICK: Duration = Duration::from_secs(1);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(1);
const BACKOFF_START: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

pub struct NodeConfig {
    /// This machine's keypair; `keys.public` is our `PeerId`.
    pub keys: Keys,
    /// Where known peers are saved; `None` keeps them in memory (tests, throwaway identities).
    pub known_peers: Option<PathBuf>,
    /// Shown once at startup, e.g. that this run uses a throwaway identity.
    pub notice: Option<String>,
    /// What the user typed for `--nick`; the system adds a word.
    pub base: String,
    /// TCP listen address; port 0 lets the OS choose.
    pub listen: SocketAddr,
    /// Enable UDP multicast discovery (tests turn it off).
    pub discovery: bool,
    /// Addresses to dial once our name is assigned.
    pub manual_peers: Vec<SocketAddr>,
    /// Listen-only discovery time before naming (`NAME_WAIT`; tests use zero).
    pub name_wait: Duration,
    /// Pin the Word part of our first name instead of drawing one; tests use
    /// it to force a same-moment clash. `None` (the app) picks at random.
    pub fixed_word: Option<String>,
}

pub struct NodeHandle {
    pub id: PeerId,
    /// The bound TCP listen address (useful for `--peer` and tests).
    pub local_addr: SocketAddr,
    pub commands: mpsc::Sender<Command>,
    pub events: mpsc::UnboundedReceiver<Event>,
    /// Finishes after `Command::Quit` once `Bye` has been flushed.
    pub task: JoinHandle<()>,
}

/// Internal messages from listener, discovery, peer, timer and worker tasks.
pub enum NodeMsg {
    Discovered(Sighting),
    PeerConnected {
        conn: ConnId,
        id: PeerId,
        nick: String,
        addr: SocketAddr,
        key: ConnKey,
        dialed_as: Option<PeerId>,
        out: mpsc::Sender<Frame>,
        accept: oneshot::Sender<Verdict>,
    },
    PeerFrame {
        conn: ConnId,
        id: PeerId,
        frame: Frame,
    },
    PeerGone {
        conn: ConnId,
        id: PeerId,
        reason: LeaveReason,
    },
    ConnectFailed {
        addr: SocketAddr,
        expected: Option<PeerId>,
        error: String,
    },
    /// The listen-only naming window is over.
    NameWaitOver,
    /// A room's door keys finished deriving on a worker thread.
    Derived {
        job: rooms::Job,
        attempt: rooms::Attempt,
        keys: RoomKeys,
    },
    /// Nobody answered our knock (keyed by door id).
    JoinTimeout(RoomId, rooms::Attempt),
    /// Nobody let us in (keyed by door id).
    JoinExpired(RoomId, rooms::Attempt),
    /// Let in whoever has enough approvals (keyed by room id).
    Settle(RoomId),
    Tick,
}

/// Discovery dials only when our id is lower, so each pair opens one connection.
pub fn should_dial(me: PeerId, them: PeerId) -> bool {
    me < them
}

/// What both ends of a TCP connection know about it, used to break duplicate ties.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnKey {
    /// We dialed this connection.
    pub outbound: bool,
    /// Source port of the dialing side; the same number on both ends.
    pub dialer_port: u16,
}

/// The Node's answer to a finished handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Use this connection.
    Keep,
    /// A duplicate that lost the tie-break. The other side may not have
    /// noticed yet, so stop writing but keep reading until it drops it,
    /// instead of losing frames it already sent.
    Retire,
    /// Refused (ourselves, a bad or taken name, or the peer limit): close now.
    Close,
}

/// When two connections to the same peer exist, both sides keep the one
/// dialed by the lower id. If both were dialed the same way, both keep the
/// lower dialer port, so the choice never depends on arrival order.
pub fn keep_new_connection(me: PeerId, them: PeerId, existing: ConnKey, new: ConnKey) -> bool {
    if existing.outbound == new.outbound {
        return new.dialer_port < existing.dialer_port;
    }
    let preferred_outbound = me < them;
    new.outbound == preferred_outbound
}

/// A unix-millis time as a local `YYYY-MM-DD`.
fn day(ms: u64) -> String {
    chrono::DateTime::from_timestamp_millis(ms as i64)
        .map(|t| t.with_timezone(&chrono::Local).format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "a while ago".into())
}

struct PeerEntry {
    conn: ConnId,
    nick: String,
    addr: SocketAddr,
    key: ConnKey,
    out: mpsc::Sender<Frame>,
    limit: RateLimit,
    renames: u8,
}

/// Token bucket for frames from one peer, so a flood can't outrun the terminal
/// and pile up in memory.
struct RateLimit {
    tokens: f64,
    last: Instant,
    /// Already told the user about the current flood.
    warned: bool,
}

impl RateLimit {
    fn new(now: Instant) -> Self {
        RateLimit { tokens: RATE_BURST as f64, last: now, warned: false }
    }

    fn allow(&mut self, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = (self.tokens + elapsed * RATE_PER_SEC).min(RATE_BURST as f64);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

struct Backoff {
    delay: Duration,
    next_try: Instant,
}

struct Node {
    id: PeerId,
    keys: Keys,
    /// What the user typed; kept to re-roll after a clash.
    base: String,
    /// Empty until assigned.
    nick: watch::Sender<String>,
    /// `Some(nicks seen so far)` during the listen-only naming window.
    naming: Option<HashSet<String>>,
    fixed_word: Option<String>,
    events: mpsc::UnboundedSender<Event>,
    msg_tx: mpsc::Sender<NodeMsg>,
    peers: HashMap<PeerId, PeerEntry>,
    dialing: HashSet<PeerId>,
    backoff: HashMap<PeerId, Backoff>,
    /// Bound at start, accepted from only once we have a name.
    listener: Option<TcpListener>,
    manual_peers: Vec<SocketAddr>,
    rooms: rooms::Rooms,
    known: known::Store,
    background: Vec<JoinHandle<()>>,
}

/// Binds the listener, starts discovery and the actor. Returns immediately;
/// `Event::Assigned` follows once the naming window is over.
pub async fn start(config: NodeConfig) -> anyhow::Result<NodeHandle> {
    let id = config.keys.public;
    let listener = TcpListener::bind(config.listen).await?;
    let local_addr = listener.local_addr()?;

    let (cmd_tx, cmd_rx) = mpsc::channel(32);
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (msg_tx, msg_rx) = mpsc::channel(256);
    let mut node = Node::new(config.keys, config.base, event_tx, msg_tx.clone());
    node.naming = Some(HashSet::new());
    node.fixed_word = config.fixed_word;
    node.listener = Some(listener);
    node.manual_peers = config.manual_peers;
    let (store, store_notice) = match config.known_peers {
        Some(path) => known::Store::load(path),
        None => (known::Store::in_memory(), None),
    };
    node.known = store;
    for notice in config.notice.into_iter().chain(store_notice) {
        node.emit(Event::Info(notice));
    }

    if config.discovery {
        let nick_rx = node.nick.subscribe();
        let events = node.events.clone();
        let port = local_addr.port();
        let tx = msg_tx.clone();
        node.background.push(tokio::spawn(async move {
            if let Err(e) = discovery::run(id, nick_rx, port, tx).await {
                let _ = events.send(Event::Info(format!(
                    "LAN discovery unavailable ({e}); use --peer <ip:port> to connect manually"
                )));
            }
        }));
    }

    let tick_tx = msg_tx.clone();
    node.background.push(tokio::spawn(async move {
        let mut tick = tokio::time::interval(TICK);
        loop {
            tick.tick().await;
            if tick_tx.send(NodeMsg::Tick).await.is_err() {
                return;
            }
        }
    }));

    node.emit(Event::Info("looking for peers…".into()));
    node.after(config.name_wait, NodeMsg::NameWaitOver);
    let task = tokio::spawn(node.run(cmd_rx, msg_rx));
    Ok(NodeHandle { id, local_addr, commands: cmd_tx, events: event_rx, task })
}

impl Node {
    fn new(keys: Keys, base: String, events: mpsc::UnboundedSender<Event>, msg_tx: mpsc::Sender<NodeMsg>) -> Self {
        Node {
            id: keys.public,
            keys,
            base,
            nick: watch::Sender::new(String::new()),
            naming: None,
            fixed_word: None,
            events,
            msg_tx,
            peers: HashMap::new(),
            dialing: HashSet::new(),
            backoff: HashMap::new(),
            listener: None,
            manual_peers: Vec::new(),
            rooms: rooms::Rooms::default(),
            known: known::Store::in_memory(),
            background: Vec::new(),
        }
    }

    fn me(&self) -> Me {
        Me { keys: self.keys.clone(), nick: self.nick.subscribe() }
    }

    fn my_nick(&self) -> String {
        self.nick.borrow().clone()
    }

    async fn run(mut self, mut cmd_rx: mpsc::Receiver<Command>, mut msg_rx: mpsc::Receiver<NodeMsg>) {
        loop {
            tokio::select! {
                cmd = cmd_rx.recv() => match cmd {
                    None | Some(Command::Quit) => break,
                    Some(cmd) => self.handle_command(cmd),
                },
                Some(msg) = msg_rx.recv() => self.handle_msg(msg),
            }
        }
        self.shutdown().await;
    }

    fn emit(&self, event: Event) {
        let _ = self.events.send(event);
    }

    /// Sends `msg` back to ourselves after `delay`.
    fn after(&self, delay: Duration, msg: NodeMsg) {
        let tx = self.msg_tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let _ = tx.send(msg).await;
        });
    }

    fn nick_of(&self, id: PeerId) -> Option<String> {
        self.peers.get(&id).map(|p| p.nick.clone())
    }

    fn peer_by_nick(&self, nick: &str) -> Option<PeerId> {
        self.peers.iter().find(|(_, p)| p.nick == nick).map(|(id, _)| *id)
    }

    /// Queues `frame` without waiting; a peer whose outbox is full misses it.
    fn send_to(&self, id: PeerId, frame: Frame) {
        if let Some(p) = self.peers.get(&id)
            && let Err(e) = p.out.try_send(frame)
        {
            warn!(%id, "dropping frame for slow or closed peer: {e}");
        }
    }

    fn broadcast(&self, frame: Frame) {
        for id in self.peers.keys() {
            self.send_to(*id, frame.clone());
        }
    }

    fn check_text(&self, text: &str) -> bool {
        let escaped = serde_json::to_string(text).expect("strings always serialize");
        if escaped.len() - 2 > MAX_TEXT_LEN {
            self.emit(Event::Error(format!("message too long (max {MAX_TEXT_LEN} bytes)")));
            return false;
        }
        true
    }

    fn handle_command(&mut self, cmd: Command) {
        if self.naming.is_some() && !matches!(cmd, Command::ListPeers | Command::Quit) {
            self.emit(Event::Error("still picking your name, try again in a moment".into()));
            return;
        }
        match cmd {
            Command::Send { to, text } => {
                if text.trim().is_empty() || !self.check_text(&text) {
                    return;
                }
                match to {
                    Target::Dm(nick) => self.send_dm(nick, text),
                    Target::Room(name) => self.send_room(&name, text),
                }
            }
            Command::CreateRoom { base, max } => self.create_room(&base, max),
            Command::JoinRoom { name, key } => self.join_room(&name, &key),
            Command::LeaveRoom { name } => self.leave_room(&name),
            Command::Approve { room, nick } => self.approve(&room, &nick),
            Command::Deny { room, nick } => self.deny(&room, &nick),
            Command::ListPeers => {
                let mut peers: Vec<PeerInfo> =
                    self.peers.iter().map(|(id, p)| PeerInfo { id: *id, nick: p.nick.clone(), addr: p.addr }).collect();
                peers.sort_by(|a, b| a.nick.cmp(&b.nick));
                self.emit(Event::Peers { you: self.id, nick: self.my_nick(), peers });
            }
            Command::Quit => {} // handled in `run`
        }
    }

    fn send_dm(&mut self, nick: String, text: String) {
        let Some(id) = self.peer_by_nick(&nick) else {
            self.emit(Event::Error(format!("@{nick} is offline")));
            return;
        };
        let ts = now_millis();
        self.send_to(id, Frame::Direct { text: text.clone(), ts });
        self.emit(Event::Sent { conv: Conv::Dm(nick), text, ts });
    }

    fn handle_msg(&mut self, msg: NodeMsg) {
        match msg {
            NodeMsg::Discovered(s) => self.on_discovered(s),
            NodeMsg::PeerConnected { conn, id, nick, addr, key, dialed_as, out, accept } => {
                if let Some(expected) = dialed_as {
                    self.dialing.remove(&expected);
                    self.backoff.remove(&expected);
                }
                let entry = PeerEntry { conn, nick, addr, key, out, limit: RateLimit::new(Instant::now()), renames: 0 };
                let verdict = self.on_connected(entry, id);
                let _ = accept.send(verdict);
            }
            NodeMsg::PeerFrame { conn, id, frame } => self.on_frame(conn, id, frame),
            NodeMsg::PeerGone { conn, id, reason } => {
                if self.peers.get(&id).is_some_and(|p| p.conn == conn) {
                    self.forget_member_everywhere(id);
                    if let Some(p) = self.peers.remove(&id) {
                        self.emit(Event::PeerLeft { id, nick: p.nick, reason });
                    }
                }
            }
            NodeMsg::ConnectFailed { addr, expected, error } => match expected {
                Some(id) => {
                    debug!(%addr, %id, "dial failed: {error}");
                    self.dialing.remove(&id);
                    let now = Instant::now();
                    let b = self.backoff.entry(id).or_insert(Backoff { delay: BACKOFF_START, next_try: now });
                    b.next_try = now + b.delay;
                    b.delay = (b.delay * 2).min(BACKOFF_MAX);
                }
                None => self.emit(Event::Error(format!("could not connect to {addr}: {error}"))),
            },
            NodeMsg::NameWaitOver => self.assign_name(),
            NodeMsg::Derived { job, attempt, keys } => self.on_derived(job, attempt, keys),
            NodeMsg::JoinTimeout(id, attempt) => self.on_join_timeout(id, attempt),
            NodeMsg::JoinExpired(id, attempt) => self.on_join_expired(id, attempt),
            NodeMsg::Settle(id) => self.settle(id),
            NodeMsg::Tick => self.on_tick(Instant::now()),
        }
    }

    /// Ends the naming window: pick a name nobody announced, then go live.
    fn assign_name(&mut self) {
        let Some(seen) = self.naming.take() else {
            return;
        };
        let picked = match &self.fixed_word {
            Some(word) => Some(format!("{}{word}", self.base)),
            None => names::pick_nick(&self.base, |n| seen.contains(n)),
        };
        let Some(nick) = picked else {
            self.emit(Event::Error(format!("every name for '{}' is taken; restart with another --nick", self.base)));
            return;
        };
        self.nick.send_replace(nick.clone());
        self.emit(Event::Assigned { nick, renamed: false });

        if let Some(listener) = self.listener.take() {
            if let Ok(addr) = listener.local_addr() {
                self.emit(Event::Info(format!("listening on TCP port {}", addr.port())));
            }
            let me = self.me();
            let tx = self.msg_tx.clone();
            self.background.push(tokio::spawn(async move {
                loop {
                    match listener.accept().await {
                        Ok((stream, addr)) => peer::accept(stream, addr, me.clone(), tx.clone()),
                        Err(e) => {
                            warn!("accept failed: {e}");
                            tokio::time::sleep(Duration::from_millis(100)).await;
                        }
                    }
                }
            }));
        }
        for addr in std::mem::take(&mut self.manual_peers) {
            peer::dial(addr, None, self.me(), self.msg_tx.clone());
        }
    }

    fn on_discovered(&mut self, s: Sighting) {
        if let Some(seen) = self.naming.as_mut() {
            seen.insert(s.nick);
            return;
        }
        let id = s.id;
        if id == self.id
            || self.peers.contains_key(&id)
            || self.dialing.contains(&id)
            || self.peers.len() >= MAX_PEERS
            || !should_dial(self.id, id)
            || self.backoff.get(&id).is_some_and(|b| Instant::now() < b.next_try)
        {
            return;
        }
        self.dialing.insert(id);
        peer::dial(s.tcp_addr, Some(id), self.me(), self.msg_tx.clone());
    }

    fn on_connected(&mut self, mut entry: PeerEntry, id: PeerId) -> Verdict {
        if id == self.id {
            return Verdict::Close;
        }
        if let Some(existing) = self.peers.get(&id) {
            return if keep_new_connection(self.id, id, existing.key, entry.key) {
                // Dropping the old sender retires the old connection.
                entry.renames = existing.renames;
                entry.nick = existing.nick.clone();
                self.peers.insert(id, entry);
                Verdict::Keep
            } else {
                Verdict::Retire
            };
        }
        if self.peers.len() >= MAX_PEERS {
            warn!(%id, "refusing peer: already at {MAX_PEERS} peers");
            return Verdict::Close;
        }
        if self.peer_by_nick(&entry.nick).is_some() {
            debug!(%id, "refusing peer: @{} is already connected", entry.nick);
            return Verdict::Close;
        }
        let nick = entry.nick.clone();
        let clash = nick == self.my_nick();
        self.peers.insert(id, entry);
        self.emit(Event::PeerJoined { id, nick: nick.clone() });
        self.meet(id, &nick);
        if clash && self.id > id {
            self.reroll_name();
        }
        Verdict::Keep
    }

    /// Checks a newly joined peer's key against the ones we've met, then remembers it.
    fn meet(&mut self, id: PeerId, nick: &str) {
        let online: Vec<PeerId> = self.peers.keys().copied().filter(|k| *k != id).collect();
        match self.known.judge(id, nick, &online) {
            Judgement::Known => {}
            Judgement::FirstMeeting => {
                self.emit(Event::Info(format!("first time meeting @{nick} ({})", id.fingerprint())));
            }
            Judgement::KeyChanged { last_seen } => {
                let base = names::base_of(nick).unwrap_or(nick);
                self.emit(Event::Info(format!(
                    "@{nick} has a different key from the {base} you met before (last seen {}). Compare fingerprints in /who.",
                    day(last_seen)
                )));
            }
        }
        self.known.record(id, nick, now_millis());
        self.known.save();
    }

    /// We and a peer drew the same name at the same moment and we have the
    /// higher id, so we take a fresh one and tell everyone.
    fn reroll_name(&mut self) {
        let mine = self.my_nick();
        let taken: HashSet<String> = self.peers.values().map(|p| p.nick.clone()).collect();
        let Some(nick) = names::pick_nick(&self.base, |n| n == mine || taken.contains(n)) else {
            self.emit(Event::Error(format!("every name for '{}' is taken", self.base)));
            return;
        };
        self.nick.send_replace(nick.clone());
        self.broadcast(Frame::Nick { nick: nick.clone() });
        self.emit(Event::Assigned { nick, renamed: true });
    }

    /// Frames count from any connection to a known peer, including one being
    /// retired after a duplicate race; it may carry messages sent before the swap.
    fn on_frame(&mut self, _conn: ConnId, id: PeerId, frame: Frame) {
        let Some(peer) = self.peers.get_mut(&id) else {
            return;
        };
        if !peer.limit.allow(Instant::now()) {
            if !std::mem::replace(&mut peer.limit.warned, true) {
                let nick = peer.nick.clone();
                self.emit(Event::Info(format!("@{nick} is sending too fast; dropping messages")));
            }
            return;
        }
        peer.limit.warned = false;
        let nick = peer.nick.clone();
        match frame {
            Frame::Direct { text, .. } if text.len() > MAX_TEXT_LEN => {
                debug!(%id, "dropping oversized DM ({} bytes)", text.len());
            }
            Frame::Direct { text, .. } => {
                self.emit(Event::Message { conv: Conv::Dm(nick.clone()), from: nick, text, ts: now_millis() })
            }
            Frame::Room { room_id, nonce, ct } => self.on_room_frame(id, &room_id, &nonce, &ct, Instant::now()),
            Frame::Nick { nick: new } => {
                let taken = new == self.my_nick() || self.peer_by_nick(&new).is_some_and(|other| other != id);
                let used_up = self.peers.get(&id).is_some_and(|p| p.renames >= MAX_RENAMES);
                if !names::is_valid_nick(&new) || taken || used_up || names::base_of(&new) != names::base_of(&nick) {
                    debug!(%id, "ignoring rename to {new:?}");
                    return;
                }
                if let Some(p) = self.peers.get_mut(&id) {
                    p.nick = new.clone();
                    p.renames += 1;
                }
                self.emit(Event::Renamed { old: nick, new });
            }
            Frame::Hello { .. } | Frame::Ping | Frame::Bye => {} // consumed by the peer task
        }
    }

    async fn shutdown(self) {
        for task in &self.background {
            task.abort();
        }
        self.leave_all_rooms();
        // Queue Bye everywhere first, then wait (within the grace period) only
        // for peers whose outbox was full, so one slow peer can't starve the rest.
        let deadline = Instant::now() + SHUTDOWN_GRACE;
        let full: Vec<&PeerEntry> = self
            .peers
            .values()
            .filter(|p| matches!(p.out.try_send(Frame::Bye), Err(mpsc::error::TrySendError::Full(_))))
            .collect();
        for p in full {
            let _ = tokio::time::timeout_at(deadline, p.out.send(Frame::Bye)).await;
        }
        for p in self.peers.values() {
            let _ = tokio::time::timeout_at(deadline, p.out.closed()).await;
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    pub(in crate::node) const ME: PeerId = PeerId::test(0x1000_0000_0000_0000);
    pub(in crate::node) const BOB: PeerId = PeerId::test(0x2000_0000_0000_0000);
    pub(in crate::node) const CAROL: PeerId = PeerId::test(0x3000_0000_0000_0000);

    pub(in crate::node) struct Harness {
        pub(in crate::node) node: Node,
        pub(in crate::node) events: mpsc::UnboundedReceiver<Event>,
        pub(in crate::node) msgs: mpsc::Receiver<NodeMsg>,
        pub(in crate::node) outboxes: HashMap<PeerId, mpsc::Receiver<Frame>>,
        next_conn: ConnId,
    }

    impl Harness {
        /// A node already named `annMoon`, with no sockets.
        pub(in crate::node) fn new() -> Self {
            let (event_tx, events) = mpsc::unbounded_channel();
            let (msg_tx, msgs) = mpsc::channel(64);
            let node = Node::new(Keys::fake(ME), "ann".into(), event_tx, msg_tx);
            node.nick.send_replace("annMoon".into());
            Harness { node, events, msgs, outboxes: HashMap::new(), next_conn: 1 }
        }

        /// Simulates a peer task completing its handshake; returns whether it was kept.
        pub(in crate::node) fn connect(&mut self, id: PeerId, nick: &str, outbound: bool) -> bool {
            let port = 40000 + self.next_conn as u16;
            self.connect_key(id, nick, ConnKey { outbound, dialer_port: port })
        }

        pub(in crate::node) fn connect_key(&mut self, id: PeerId, nick: &str, key: ConnKey) -> bool {
            self.connect_verdict(id, nick, key) == Verdict::Keep
        }

        pub(in crate::node) fn connect_verdict(&mut self, id: PeerId, nick: &str, key: ConnKey) -> Verdict {
            // Harness peers are old friends, so joins stay free of key notices.
            self.node.known.record(id, nick, 0);
            self.connect_unknown(id, nick, key)
        }

        /// Connects a peer this node has never met.
        pub(in crate::node) fn connect_stranger(&mut self, id: PeerId, nick: &str) -> bool {
            self.connect_unknown(id, nick, ConnKey { outbound: false, dialer_port: 40000 + self.next_conn as u16 })
                == Verdict::Keep
        }

        fn connect_unknown(&mut self, id: PeerId, nick: &str, key: ConnKey) -> Verdict {
            let (out, rx) = mpsc::channel(64);
            let conn = self.next_conn;
            self.next_conn += 1;
            let entry = PeerEntry {
                conn,
                nick: nick.into(),
                addr: "127.0.0.1:1".parse().unwrap(),
                key,
                out,
                limit: RateLimit::new(Instant::now()),
                renames: 0,
            };
            let verdict = self.node.on_connected(entry, id);
            if verdict == Verdict::Keep {
                self.outboxes.insert(id, rx);
            }
            verdict
        }

        pub(in crate::node) fn sent(&mut self, id: PeerId) -> Vec<Frame> {
            let rx = self.outboxes.get_mut(&id).unwrap();
            std::iter::from_fn(|| rx.try_recv().ok()).collect()
        }

        pub(in crate::node) fn events(&mut self) -> Vec<Event> {
            std::iter::from_fn(|| self.events.try_recv().ok()).collect()
        }

        /// Handles the next internal message (worker result or timer).
        pub(in crate::node) async fn pump(&mut self) {
            let msg = self.msgs.recv().await.expect("node sender alive");
            self.node.handle_msg(msg);
        }
    }

    fn out(dialer_port: u16) -> ConnKey {
        ConnKey { outbound: true, dialer_port }
    }

    fn inb(dialer_port: u16) -> ConnKey {
        ConnKey { outbound: false, dialer_port }
    }

    fn dm(to: &str, text: &str) -> Command {
        Command::Send { to: Target::Dm(to.into()), text: text.into() }
    }

    #[test]
    fn dial_rule_picks_exactly_one_side() {
        assert!(should_dial(ME, BOB));
        assert!(!should_dial(BOB, ME));
    }

    #[test]
    fn opposite_direction_duplicates_keep_the_lower_ids_dial() {
        // ME < BOB, so the connection ME dialed (outbound for ME, inbound for BOB) wins.
        assert!(keep_new_connection(ME, BOB, inb(1), out(2)));
        assert!(!keep_new_connection(ME, BOB, out(2), inb(1)));
        assert!(keep_new_connection(BOB, ME, out(1), inb(2)));
        assert!(!keep_new_connection(BOB, ME, inb(2), out(1)));
    }

    #[test]
    fn same_direction_duplicates_agree_even_when_seen_in_opposite_order() {
        // ME dialed BOB twice (e.g. --peer racing discovery) from ports 5000 and 6000.
        // ME finished 5000 first; BOB finished 6000 first. Both must settle on 5000.
        assert!(!keep_new_connection(ME, BOB, out(5000), out(6000)));
        assert!(keep_new_connection(BOB, ME, inb(6000), inb(5000)));
    }

    #[test]
    fn duplicate_connection_does_not_rejoin() {
        let mut h = Harness::new();
        assert!(h.connect_key(BOB, "bobRoger", inb(7000)));
        assert!(h.connect_key(BOB, "bobRoger", out(9000))); // preferred direction replaces
        assert!(h.connect_key(BOB, "bobRoger", out(8000))); // same direction, lower port replaces
        assert!(!h.connect_key(BOB, "bobRoger", out(8500))); // same direction, higher port rejected
        let joins = h.events().into_iter().filter(|e| matches!(e, Event::PeerJoined { .. })).count();
        assert_eq!(joins, 1);
    }

    #[test]
    fn duplicate_connection_keeps_the_existing_nick() {
        let mut h = Harness::new();
        assert!(h.connect_key(BOB, "bobRoger", inb(7000)));
        h.events();
        assert!(h.connect_key(BOB, "carolWolf", out(9000)));
        assert_eq!(h.node.nick_of(BOB), Some("bobRoger".to_string()));
        assert!(h.events().is_empty());
    }

    #[test]
    fn a_rename_to_another_base_is_refused() {
        let mut h = Harness::new();
        h.connect(BOB, "bobRoger", true);
        h.events();
        let conn = h.node.peers[&BOB].conn;
        h.node.on_frame(conn, BOB, Frame::Nick { nick: "eveWolf".into() });
        assert!(h.events().is_empty());
        assert_eq!(h.node.nick_of(BOB), Some("bobRoger".to_string()));
        h.node.on_frame(conn, BOB, Frame::Nick { nick: "bobOtter".into() });
        assert_eq!(h.events(), vec![Event::Renamed { old: "bobRoger".into(), new: "bobOtter".into() }]);
    }

    #[test]
    fn peer_count_is_capped() {
        let mut h = Harness::new();
        for i in 0..MAX_PEERS as u64 {
            assert!(h.connect(PeerId::test(0x4000_0000_0000_0000 + i), &format!("p{i}Wolf"), true));
        }
        assert_eq!(h.connect_verdict(PeerId::test(0x5000_0000_0000_0000), "lateWolf", out(1)), Verdict::Close);
        assert_eq!(h.node.peers.len(), MAX_PEERS);
    }

    #[test]
    fn a_second_peer_with_a_taken_name_is_refused() {
        let mut h = Harness::new();
        assert!(h.connect(BOB, "bobRoger", true));
        assert_eq!(h.connect_verdict(CAROL, "bobRoger", out(1)), Verdict::Close);
    }

    #[test]
    fn same_moment_clash_rerolls_only_the_higher_id() {
        // BOB (higher id than ME) shows up with our name: we keep ours.
        let mut h = Harness::new();
        assert!(h.connect(BOB, "annMoon", true));
        assert_eq!(h.node.my_nick(), "annMoon");
        assert!(!h.events().iter().any(|e| matches!(e, Event::Assigned { .. })));

        // A lower id than ours shows up with our name: we re-roll and tell everyone.
        let mut h = Harness::new();
        let low = PeerId::test(1);
        assert!(h.connect(low, "annMoon", true));
        let mine = h.node.my_nick();
        assert_ne!(mine, "annMoon");
        assert!(mine.starts_with("ann") && names::is_valid_nick(&mine));
        assert_eq!(h.sent(low), vec![Frame::Nick { nick: mine.clone() }]);
        assert!(h.events().contains(&Event::Assigned { nick: mine, renamed: true }));
    }

    #[test]
    fn peer_rename_is_accepted_only_when_valid_and_free() {
        let mut h = Harness::new();
        h.connect(BOB, "bobRoger", true);
        h.connect(CAROL, "carolWolf", true);
        h.events();
        let conn = h.node.peers[&BOB].conn;
        h.node.on_frame(conn, BOB, Frame::Nick { nick: "carolWolf".into() }); // taken
        h.node.on_frame(conn, BOB, Frame::Nick { nick: "annMoon".into() }); // ours
        h.node.on_frame(conn, BOB, Frame::Nick { nick: "\u{1b}[2J".into() }); // invalid
        h.node.on_frame(conn, BOB, Frame::Nick { nick: "bobPie".into() });
        assert_eq!(h.events(), vec![Event::Renamed { old: "bobRoger".into(), new: "bobPie".into() }]);
        assert_eq!(h.node.peer_by_nick("bobPie"), Some(BOB));
    }

    #[test]
    fn a_peer_can_only_rename_a_few_times() {
        let mut h = Harness::new();
        h.connect(BOB, "bobRoger", false);
        h.events();
        let words = ["Pie", "Wolf", "Owl", "Elk", "Bee"];
        for w in &words[..2] {
            let conn = h.node.peers[&BOB].conn;
            h.node.on_frame(conn, BOB, Frame::Nick { nick: format!("bob{w}") });
        }
        // A duplicate connection replacing the first doesn't reset the count.
        h.connect(BOB, "bobWolf", true);
        for w in &words[2..] {
            let conn = h.node.peers[&BOB].conn;
            h.node.on_frame(conn, BOB, Frame::Nick { nick: format!("bob{w}") });
        }
        let renames = h.events().into_iter().filter(|e| matches!(e, Event::Renamed { .. })).count();
        assert_eq!(renames, MAX_RENAMES as usize);
        assert_eq!(h.node.peers[&BOB].nick, "bobOwl");
    }

    #[test]
    fn commands_wait_for_a_name() {
        let mut h = Harness::new();
        h.node.naming = Some(HashSet::new());
        h.node.handle_command(dm("bobRoger", "hi"));
        assert!(matches!(h.events().as_slice(), [Event::Error(e)] if e.contains("still picking")));
    }

    #[tokio::test]
    async fn naming_avoids_names_announced_during_the_window() {
        let mut h = Harness::new();
        h.node.nick.send_replace(String::new());
        h.node.naming = Some(HashSet::new());
        let taken: Vec<String> = crate::words::WORDS.iter().skip(1).map(|w| format!("ann{w}")).collect();
        for (i, nick) in taken.iter().enumerate() {
            let s = Sighting {
                id: PeerId::test(100 + i as u64),
                nick: nick.clone(),
                tcp_addr: "127.0.0.1:1".parse().unwrap(),
            };
            h.node.on_discovered(s);
        }
        assert!(h.node.dialing.is_empty(), "no dialing before we have a name");
        h.node.assign_name();
        let only_free = format!("ann{}", crate::words::WORDS[0]);
        assert_eq!(h.node.my_nick(), only_free);
        assert_eq!(h.events(), vec![Event::Assigned { nick: only_free, renamed: false }]);
    }

    #[tokio::test]
    async fn bye_waits_for_room_in_a_full_outbox() {
        let mut h = Harness::new();
        let (out, mut rx) = mpsc::channel(8);
        let entry = PeerEntry {
            conn: 1,
            nick: "bobRoger".into(),
            addr: "127.0.0.1:1".parse().unwrap(),
            key: out_key(),
            out,
            limit: RateLimit::new(Instant::now()),
            renames: 0,
        };
        assert_eq!(h.node.on_connected(entry, BOB), Verdict::Keep);
        for i in 0..8 {
            h.node.handle_command(dm("bobRoger", &format!("msg {i}"))); // fills the 8-slot outbox
        }
        let drain = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let mut got = Vec::new();
            while let Some(frame) = rx.recv().await {
                let bye = frame == Frame::Bye;
                got.push(frame);
                if bye {
                    break; // like the writer: stop after Bye, dropping the receiver
                }
            }
            got
        });
        h.node.shutdown().await;
        let got = drain.await.unwrap();
        assert_eq!(got.len(), 9);
        assert_eq!(got.last(), Some(&Frame::Bye));
    }

    fn out_key() -> ConnKey {
        out(1)
    }

    #[test]
    fn rate_limit_allows_a_burst_then_refills() {
        let t0 = Instant::now();
        let mut limit = RateLimit::new(t0);
        let allowed = (0..100).filter(|_| limit.allow(t0)).count();
        assert_eq!(allowed, RATE_BURST as usize);
        assert!(!limit.allow(t0));
        assert!(limit.allow(t0 + Duration::from_millis(150))); // refills 10/s
    }

    #[test]
    fn flooding_peer_is_throttled_with_one_warning() {
        let mut h = Harness::new();
        h.connect(BOB, "bobRoger", true);
        h.events();
        let conn = h.node.peers[&BOB].conn;
        for i in 0..100 {
            h.node.on_frame(conn, BOB, Frame::Direct { text: format!("spam {i}"), ts: 0 });
        }
        let events = h.events();
        let messages = events.iter().filter(|e| matches!(e, Event::Message { .. })).count();
        let warnings = events.iter().filter(|e| matches!(e, Event::Info(m) if m.contains("too fast"))).count();
        assert_eq!(messages, RATE_BURST as usize);
        assert_eq!(warnings, 1);
    }

    #[test]
    fn dm_goes_only_to_its_target_and_is_recorded() {
        let mut h = Harness::new();
        h.connect(BOB, "bobRoger", true);
        h.connect(CAROL, "carolWolf", true);
        h.events();
        h.node.handle_command(dm("bobRoger", "psst"));
        assert!(matches!(h.sent(BOB).as_slice(), [Frame::Direct { text, .. }] if text == "psst"));
        assert!(h.sent(CAROL).is_empty());
        assert!(matches!(
            h.events().as_slice(),
            [Event::Sent { conv: Conv::Dm(n), text, .. }] if n == "bobRoger" && text == "psst"
        ));
    }

    #[test]
    fn dm_to_someone_not_connected_is_an_error() {
        let mut h = Harness::new();
        h.connect(BOB, "bobRoger", true);
        h.events();
        h.node.handle_command(dm("bobroger", "case matters"));
        assert_eq!(h.events(), vec![Event::Error("@bobroger is offline".into())]);
    }

    #[test]
    fn incoming_dm_becomes_a_dm_message() {
        let mut h = Harness::new();
        h.connect(BOB, "bobRoger", true);
        h.events();
        let conn = h.node.peers[&BOB].conn;
        h.node.on_frame(conn, BOB, Frame::Direct { text: "hi".into(), ts: 0 });
        assert!(matches!(
            h.events().as_slice(),
            [Event::Message { conv: Conv::Dm(c), from, text, .. }] if c == "bobRoger" && from == "bobRoger" && text == "hi"
        ));
    }

    #[test]
    fn oversized_text_is_rejected_before_sending() {
        let mut h = Harness::new();
        h.connect(BOB, "bobRoger", true);
        h.events();
        h.node.handle_command(dm("bobRoger", &"x".repeat(MAX_TEXT_LEN + 1)));
        assert!(h.sent(BOB).is_empty());
        assert!(matches!(h.events().as_slice(), [Event::Error(e)] if e.contains("too long")));
    }

    #[test]
    fn frames_from_a_replaced_connection_still_arrive_but_its_close_is_silent() {
        let mut h = Harness::new();
        h.connect(BOB, "bobRoger", false); // conn 1
        h.connect(BOB, "bobRoger", true); // conn 2 replaces it
        h.events();
        // bob may have sent this before he switched over too.
        h.node.on_frame(1, BOB, Frame::Direct { text: "in flight".into(), ts: 0 });
        h.node.handle_msg(NodeMsg::PeerGone { conn: 1, id: BOB, reason: LeaveReason::Lost });
        assert!(matches!(h.events().as_slice(), [Event::Message { text, .. }] if text == "in flight"));
        assert!(h.node.peers.contains_key(&BOB));
    }

    #[test]
    fn losing_duplicates_are_retired_and_other_refusals_closed() {
        let mut h = Harness::new();
        assert_eq!(h.connect_verdict(BOB, "bobRoger", out(5000)), Verdict::Keep);
        assert_eq!(h.connect_verdict(BOB, "bobRoger", out(6000)), Verdict::Retire);
        assert_eq!(h.connect_verdict(ME, "annMoon", out(7000)), Verdict::Close);
    }

    #[test]
    fn meeting_a_new_key_says_so_once() {
        let mut h = Harness::new();
        assert!(h.connect_stranger(BOB, "bobRoger"));
        let fp = BOB.fingerprint();
        assert_eq!(
            h.events(),
            vec![
                Event::PeerJoined { id: BOB, nick: "bobRoger".into() },
                Event::Info(format!("first time meeting @bobRoger ({fp})")),
            ]
        );
        h.node.handle_msg(NodeMsg::PeerGone { conn: 1, id: BOB, reason: LeaveReason::Quit });
        h.events();
        assert!(h.connect_stranger(BOB, "bobOtter"));
        assert_eq!(h.events(), vec![Event::PeerJoined { id: BOB, nick: "bobOtter".into() }]);
    }

    #[test]
    fn a_familiar_name_with_a_new_key_warns() {
        let mut h = Harness::new();
        h.node.known.record(BOB, "bobRoger", 0);
        assert!(h.connect_stranger(CAROL, "bobMoon"));
        let events = h.events();
        let Some(Event::Info(text)) = events.get(1) else { panic!("{events:?}") };
        assert!(text.starts_with("@bobMoon has a different key from the bob you met before (last seen "), "{text}");
        assert!(text.ends_with("). Compare fingerprints in /who."), "{text}");
    }

    #[test]
    fn who_includes_our_own_id() {
        let mut h = Harness::new();
        h.connect(BOB, "bobRoger", true);
        h.events();
        h.node.handle_command(Command::ListPeers);
        match h.events().as_slice() {
            [Event::Peers { you, nick, peers }] => {
                assert_eq!(*you, ME);
                assert_eq!(nick, "annMoon");
                assert_eq!(peers.len(), 1);
                assert_eq!(peers[0].id, BOB);
            }
            other => panic!("{other:?}"),
        }
    }
}
