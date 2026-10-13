//! The core actor. Owns the peer table; everything else talks to it via channels.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tracing::{debug, warn};

use crate::api::{Command, Event, LeaveReason, PeerInfo};
use crate::discovery::{self, Sighting};
use crate::names::{self, Resolve};
use crate::peer::{self, ConnId, Me};
use crate::protocol::{now_millis, Frame, PeerId, MAX_FRAME_LEN};

/// Longest chat text accepted, leaving headroom under `MAX_FRAME_LEN` for JSON escaping.
pub const MAX_TEXT_LEN: usize = MAX_FRAME_LEN / 8;
const SHUTDOWN_GRACE: Duration = Duration::from_secs(1);
const BACKOFF_START: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

pub struct NodeConfig {
    pub nick: String,
    /// TCP listen address; port 0 lets the OS choose.
    pub listen: SocketAddr,
    /// Enable UDP multicast discovery (tests turn it off).
    pub discovery: bool,
    /// Addresses to dial at startup regardless of discovery.
    pub manual_peers: Vec<SocketAddr>,
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

/// Internal messages from listener, discovery and peer tasks to the Node.
pub enum NodeMsg {
    Discovered(Sighting),
    PeerConnected {
        conn: ConnId,
        id: PeerId,
        nick: String,
        addr: SocketAddr,
        outbound: bool,
        dialed_as: Option<PeerId>,
        out: mpsc::Sender<Frame>,
        /// Node replies `true` to keep the connection, `false` to close it.
        accept: oneshot::Sender<bool>,
    },
    PeerFrame { conn: ConnId, id: PeerId, frame: Frame },
    PeerGone { conn: ConnId, id: PeerId, reason: LeaveReason },
    ConnectFailed { addr: SocketAddr, expected: Option<PeerId>, error: String },
}

/// Discovery dials only when our id is lower, so each pair opens one connection.
pub fn should_dial(me: PeerId, them: PeerId) -> bool {
    me < them
}

/// When two connections to the same peer exist, both sides keep the one
/// dialed by the lower id. `outbound` = we dialed that connection.
pub fn keep_new_connection(me: PeerId, them: PeerId, existing_outbound: bool, new_outbound: bool) -> bool {
    let preferred_outbound = me < them;
    new_outbound == preferred_outbound && existing_outbound != preferred_outbound
}

struct PeerEntry {
    conn: ConnId,
    nick: String,
    addr: SocketAddr,
    outbound: bool,
    out: mpsc::Sender<Frame>,
}

struct Backoff {
    delay: Duration,
    next_try: Instant,
}

struct Node {
    id: PeerId,
    nick: watch::Sender<String>,
    events: mpsc::UnboundedSender<Event>,
    msg_tx: mpsc::Sender<NodeMsg>,
    peers: HashMap<PeerId, PeerEntry>,
    dialing: HashSet<PeerId>,
    backoff: HashMap<PeerId, Backoff>,
    background: Vec<JoinHandle<()>>,
}

/// Binds the listener, starts discovery and the actor. Returns immediately.
pub async fn start(config: NodeConfig) -> anyhow::Result<NodeHandle> {
    let id = PeerId::random();
    let listener = TcpListener::bind(config.listen).await?;
    let local_addr = listener.local_addr()?;

    let (cmd_tx, cmd_rx) = mpsc::channel(32);
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (msg_tx, msg_rx) = mpsc::channel(256);
    let mut node = Node::new(id, config.nick.clone(), event_tx, msg_tx.clone());

    let me = node.me();
    let listen_tx = msg_tx.clone();
    node.background.push(tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, addr)) => peer::accept(stream, addr, me.clone(), listen_tx.clone()),
                Err(e) => {
                    warn!("accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }));

    if config.discovery {
        let nick_rx = node.nick.subscribe();
        let events = node.events.clone();
        let port = local_addr.port();
        node.background.push(tokio::spawn(async move {
            if let Err(e) = discovery::run(id, nick_rx, port, msg_tx).await {
                let _ = events.send(Event::Info(format!(
                    "LAN discovery unavailable ({e}); use --peer <ip:port> to connect manually"
                )));
            }
        }));
    }

    for addr in config.manual_peers {
        peer::dial(addr, None, node.me(), node.msg_tx.clone());
    }

    node.emit(Event::Info(format!(
        "you are {} ({id}), listening on TCP port {}",
        config.nick,
        local_addr.port()
    )));
    let task = tokio::spawn(node.run(cmd_rx, msg_rx));
    Ok(NodeHandle { id, local_addr, commands: cmd_tx, events: event_rx, task })
}

impl Node {
    fn new(
        id: PeerId,
        nick: String,
        events: mpsc::UnboundedSender<Event>,
        msg_tx: mpsc::Sender<NodeMsg>,
    ) -> Self {
        Node {
            id,
            nick: watch::Sender::new(nick),
            events,
            msg_tx,
            peers: HashMap::new(),
            dialing: HashSet::new(),
            backoff: HashMap::new(),
            background: Vec::new(),
        }
    }

    fn me(&self) -> Me {
        Me { id: self.id, nick: self.nick.subscribe() }
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

    fn roster(&self) -> Vec<(PeerId, &str)> {
        self.peers.iter().map(|(id, p)| (*id, p.nick.as_str())).collect()
    }

    fn display(&self, id: PeerId) -> String {
        match self.peers.get(&id) {
            Some(p) => names::display_name(id, &p.nick, &self.roster()),
            None => id.short(),
        }
    }

    /// Queues `frame` without waiting; a peer whose outbox is full misses it.
    fn send_to(&self, id: PeerId, frame: Frame) {
        if let Some(p) = self.peers.get(&id) {
            if let Err(e) = p.out.try_send(frame) {
                warn!(%id, "dropping frame for slow or closed peer: {e}");
            }
        }
    }

    fn broadcast(&self, frame: Frame) {
        for id in self.peers.keys() {
            self.send_to(*id, frame.clone());
        }
    }

    fn check_text(&self, text: &str) -> bool {
        if text.len() > MAX_TEXT_LEN {
            self.emit(Event::Error(format!("message too long (max {MAX_TEXT_LEN} bytes)")));
            return false;
        }
        true
    }

    fn handle_command(&mut self, cmd: Command) {
        match cmd {
            Command::Say(text) => {
                if text.trim().is_empty() || !self.check_text(&text) {
                    return;
                }
                if self.peers.is_empty() {
                    self.emit(Event::Info("nobody else is here yet".into()));
                    return;
                }
                self.broadcast(Frame::Chat { text, ts: now_millis() });
            }
            Command::Direct { to, text } => {
                if text.trim().is_empty() || !self.check_text(&text) {
                    return;
                }
                match names::resolve(&to, &self.roster()) {
                    Resolve::Found(id) => {
                        self.send_to(id, Frame::Direct { text: text.clone(), ts: now_millis() });
                        self.emit(Event::DirectSent { to: self.display(id), text });
                    }
                    Resolve::NotFound => self.emit(Event::Error(format!("no peer named '{to}'"))),
                    Resolve::Ambiguous(candidates) => self.emit(Event::Error(format!(
                        "'{to}' is ambiguous: {}",
                        candidates.join(", ")
                    ))),
                }
            }
            Command::SetNick(nick) => {
                if let Err(e) = names::validate_nick(&nick) {
                    self.emit(Event::Error(e));
                    return;
                }
                self.nick.send_replace(nick.clone());
                self.broadcast(Frame::Nick { nick: nick.clone() });
                self.emit(Event::Info(format!("you are now {nick}")));
            }
            Command::ListPeers => {
                let mut peers: Vec<PeerInfo> = self
                    .peers
                    .iter()
                    .map(|(id, p)| PeerInfo { id: *id, nick: self.display(*id), addr: p.addr })
                    .collect();
                peers.sort_by(|a, b| a.nick.cmp(&b.nick));
                self.emit(Event::Peers(peers));
            }
            Command::Quit => {} // handled in `run`
        }
    }

    fn handle_msg(&mut self, msg: NodeMsg) {
        match msg {
            NodeMsg::Discovered(s) => self.on_discovered(s),
            NodeMsg::PeerConnected { conn, id, nick, addr, outbound, dialed_as, out, accept } => {
                if let Some(expected) = dialed_as {
                    self.dialing.remove(&expected);
                    self.backoff.remove(&expected);
                }
                let keep = self.on_connected(PeerEntry { conn, nick, addr, outbound, out }, id);
                let _ = accept.send(keep);
            }
            NodeMsg::PeerFrame { conn, id, frame } => self.on_frame(conn, id, frame),
            NodeMsg::PeerGone { conn, id, reason } => {
                if self.peers.get(&id).is_some_and(|p| p.conn == conn) {
                    let nick = self.display(id);
                    self.peers.remove(&id);
                    self.emit(Event::PeerLeft { id, nick, reason });
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
        }
    }

    fn on_discovered(&mut self, s: Sighting) {
        let id = s.id;
        if id == self.id
            || self.peers.contains_key(&id)
            || self.dialing.contains(&id)
            || !should_dial(self.id, id)
            || self.backoff.get(&id).is_some_and(|b| Instant::now() < b.next_try)
        {
            return;
        }
        self.dialing.insert(id);
        peer::dial(s.tcp_addr, Some(id), self.me(), self.msg_tx.clone());
    }

    /// Returns whether to keep the new connection.
    fn on_connected(&mut self, entry: PeerEntry, id: PeerId) -> bool {
        if id == self.id {
            return false;
        }
        match self.peers.get(&id) {
            None => {
                self.peers.insert(id, entry);
                self.emit(Event::PeerJoined { id, nick: self.display(id) });
                true
            }
            Some(existing) => {
                if keep_new_connection(self.id, id, existing.outbound, entry.outbound) {
                    // Dropping the old sender closes the old connection quietly.
                    self.peers.insert(id, entry);
                    true
                } else {
                    false
                }
            }
        }
    }

    fn on_frame(&mut self, conn: ConnId, id: PeerId, frame: Frame) {
        if !self.peers.get(&id).is_some_and(|p| p.conn == conn) {
            return; // stale duplicate connection
        }
        match frame {
            Frame::Chat { text, ts } | Frame::Direct { text, ts } if text.len() > MAX_TEXT_LEN => {
                debug!(%id, ts, "dropping oversized message ({} bytes)", text.len());
            }
            Frame::Chat { text, ts } => {
                self.emit(Event::Message { from_id: id, from_nick: self.display(id), text, direct: false, ts })
            }
            Frame::Direct { text, ts } => {
                self.emit(Event::Message { from_id: id, from_nick: self.display(id), text, direct: true, ts })
            }
            Frame::Nick { nick } => {
                if names::validate_nick(&nick).is_err() {
                    debug!(%id, "ignoring invalid nick {nick:?}");
                    return;
                }
                let old = self.display(id);
                if let Some(p) = self.peers.get_mut(&id) {
                    p.nick = nick;
                }
                let new = self.display(id);
                self.emit(Event::NickChanged { id, old, new });
            }
            Frame::Hello { .. } | Frame::Ping | Frame::Bye => {} // consumed by the peer task
        }
    }

    async fn shutdown(self) {
        for task in &self.background {
            task.abort();
        }
        self.broadcast(Frame::Bye);
        let deadline = Instant::now() + SHUTDOWN_GRACE;
        for p in self.peers.values() {
            let _ = tokio::time::timeout_at(deadline, p.out.closed()).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ME: PeerId = PeerId(0x1000_0000_0000_0000);
    const BOB: PeerId = PeerId(0x2000_0000_0000_0000);
    const CAROL: PeerId = PeerId(0x3000_0000_0000_0000);

    struct Harness {
        node: Node,
        events: mpsc::UnboundedReceiver<Event>,
        outboxes: HashMap<PeerId, mpsc::Receiver<Frame>>,
        next_conn: ConnId,
    }

    impl Harness {
        fn new() -> Self {
            let (event_tx, events) = mpsc::unbounded_channel();
            let (msg_tx, _msg_rx) = mpsc::channel(8);
            Harness { node: Node::new(ME, "me".into(), event_tx, msg_tx), events, outboxes: HashMap::new(), next_conn: 1 }
        }

        /// Simulates a peer task completing its handshake; returns the Node's verdict.
        fn connect(&mut self, id: PeerId, nick: &str, outbound: bool) -> bool {
            let (out, rx) = mpsc::channel(8);
            let conn = self.next_conn;
            self.next_conn += 1;
            let entry = PeerEntry { conn, nick: nick.into(), addr: "127.0.0.1:1".parse().unwrap(), outbound, out };
            let keep = self.node.on_connected(entry, id);
            if keep {
                self.outboxes.insert(id, rx);
            }
            keep
        }

        fn sent(&mut self, id: PeerId) -> Vec<Frame> {
            let rx = self.outboxes.get_mut(&id).unwrap();
            std::iter::from_fn(|| rx.try_recv().ok()).collect()
        }

        fn events(&mut self) -> Vec<Event> {
            std::iter::from_fn(|| self.events.try_recv().ok()).collect()
        }
    }

    #[test]
    fn dial_rule_picks_exactly_one_side() {
        assert!(should_dial(ME, BOB));
        assert!(!should_dial(BOB, ME));
    }

    #[test]
    fn both_sides_agree_on_which_duplicate_to_keep() {
        // ME < BOB, so the connection ME dialed (outbound for ME, inbound for BOB) wins.
        assert!(keep_new_connection(ME, BOB, false, true));
        assert!(!keep_new_connection(ME, BOB, true, false));
        assert!(keep_new_connection(BOB, ME, true, false));
        assert!(!keep_new_connection(BOB, ME, false, true));
        // Same direction twice: keep the existing one.
        assert!(!keep_new_connection(ME, BOB, true, true));
    }

    #[test]
    fn duplicate_connection_does_not_rejoin() {
        let mut h = Harness::new();
        assert!(h.connect(BOB, "bob", false));
        assert!(h.connect(BOB, "bob", true)); // preferred direction replaces
        assert!(!h.connect(BOB, "bob", true)); // same direction rejected
        let joins = h.events().into_iter().filter(|e| matches!(e, Event::PeerJoined { .. })).count();
        assert_eq!(joins, 1);
    }

    #[test]
    fn say_fans_out_to_every_peer() {
        let mut h = Harness::new();
        h.connect(BOB, "bob", true);
        h.connect(CAROL, "carol", true);
        h.node.handle_command(Command::Say("hi all".into()));
        for id in [BOB, CAROL] {
            assert!(matches!(h.sent(id).as_slice(), [Frame::Chat { text, .. }] if text == "hi all"));
        }
    }

    #[test]
    fn say_with_no_peers_tells_the_user() {
        let mut h = Harness::new();
        h.node.handle_command(Command::Say("anyone?".into()));
        assert_eq!(h.events(), vec![Event::Info("nobody else is here yet".into())]);
    }

    #[test]
    fn direct_goes_only_to_target_and_is_echoed() {
        let mut h = Harness::new();
        h.connect(BOB, "bob", true);
        h.connect(CAROL, "carol", true);
        h.events();
        h.node.handle_command(Command::Direct { to: "bob".into(), text: "psst".into() });
        assert!(matches!(h.sent(BOB).as_slice(), [Frame::Direct { text, .. }] if text == "psst"));
        assert!(h.sent(CAROL).is_empty());
        assert_eq!(h.events(), vec![Event::DirectSent { to: "bob".into(), text: "psst".into() }]);
    }

    #[test]
    fn direct_to_unknown_or_ambiguous_nick_is_an_error() {
        let mut h = Harness::new();
        h.connect(BOB, "bob", true);
        h.connect(CAROL, "bob", true);
        h.events();
        h.node.handle_command(Command::Direct { to: "dave".into(), text: "x".into() });
        h.node.handle_command(Command::Direct { to: "bob".into(), text: "x".into() });
        assert_eq!(
            h.events(),
            vec![
                Event::Error("no peer named 'dave'".into()),
                Event::Error("'bob' is ambiguous: bob#2000, bob#3000".into()),
            ]
        );
        assert!(h.sent(BOB).is_empty() && h.sent(CAROL).is_empty());
    }

    #[test]
    fn oversized_text_is_rejected_before_sending() {
        let mut h = Harness::new();
        h.connect(BOB, "bob", true);
        h.events();
        h.node.handle_command(Command::Say("x".repeat(MAX_TEXT_LEN + 1)));
        assert!(h.sent(BOB).is_empty());
        assert!(matches!(h.events().as_slice(), [Event::Error(e)] if e.contains("too long")));
    }

    #[test]
    fn invalid_nick_change_is_rejected() {
        let mut h = Harness::new();
        h.connect(BOB, "bob", true);
        h.events();
        h.node.handle_command(Command::SetNick("two words".into()));
        assert!(h.sent(BOB).is_empty());
        assert!(matches!(h.events().as_slice(), [Event::Error(_)]));
    }

    #[test]
    fn frames_from_stale_connection_are_ignored() {
        let mut h = Harness::new();
        h.connect(BOB, "bob", false); // conn 1
        h.connect(BOB, "bob", true); // conn 2 replaces it
        h.events();
        h.node.on_frame(1, BOB, Frame::Chat { text: "old".into(), ts: 0 });
        h.node.handle_msg(NodeMsg::PeerGone { conn: 1, id: BOB, reason: LeaveReason::Lost });
        assert!(h.events().is_empty());
        assert!(h.node.peers.contains_key(&BOB));
    }

    #[test]
    fn remote_nick_change_uses_display_names() {
        let mut h = Harness::new();
        h.connect(BOB, "bob", true);
        h.events();
        let conn = h.node.peers[&BOB].conn;
        h.node.on_frame(conn, BOB, Frame::Nick { nick: "robert".into() });
        h.node.on_frame(conn, BOB, Frame::Nick { nick: "\u{1b}[2J".into() });
        assert_eq!(
            h.events(),
            vec![Event::NickChanged { id: BOB, old: "bob".into(), new: "robert".into() }]
        );
    }
}
