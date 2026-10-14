//! End-to-end tests: real Nodes on 127.0.0.1, connected with manual peers
//! (multicast is disabled because it is unreliable on CI and shared machines).

use std::net::SocketAddr;
use std::time::Duration;

use corvo_lan::api::{Command, Conv, Event, JoinEnd, LeaveReason, Target};
use corvo_lan::identity::Keys;
use corvo_lan::node::{self, NodeConfig, NodeHandle};
use tokio::time::timeout;

struct Peer {
    node: NodeHandle,
    nick: String,
}

async fn spawn_node(base: &str, peers: &[SocketAddr]) -> Peer {
    spawn_node_with_word(base, peers, None).await
}

async fn spawn_node_with_word(base: &str, peers: &[SocketAddr], word: Option<&str>) -> Peer {
    let mut node = node::start(NodeConfig {
        keys: Keys::generate(),
        known_peers: None,
        notice: None,
        base: base.into(),
        listen: "127.0.0.1:0".parse().unwrap(),
        discovery: false,
        manual_peers: peers.to_vec(),
        name_wait: Duration::ZERO,
        fixed_word: word.map(String::from),
        ask_name: false,
    })
    .await
    .unwrap();
    let Event::Assigned { nick, .. } = wait_for(&mut node, "a name", |e| matches!(e, Event::Assigned { .. })).await
    else {
        unreachable!()
    };
    Peer { node, nick }
}

/// Skips events until one matches `pred`; panics after 5s.
async fn wait_for(node: &mut NodeHandle, what: &str, pred: impl Fn(&Event) -> bool) -> Event {
    timeout(Duration::from_secs(5), async {
        loop {
            let event = node.events.recv().await.expect("node stopped");
            if pred(&event) {
                return event;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

/// Waits until every nick in `nicks` is online, in any order.
async fn wait_online(p: &mut Peer, nicks: &[&str]) {
    let mut missing: Vec<String> = nicks.iter().map(|n| n.to_string()).collect();
    while !missing.is_empty() {
        let event =
            wait_for(&mut p.node, &format!("{missing:?} online"), |e| matches!(e, Event::PeerJoined { .. })).await;
        if let Event::PeerJoined { nick, .. } = event {
            missing.retain(|n| *n != nick);
        }
    }
}

async fn send(p: &Peer, cmd: Command) {
    p.node.commands.send(cmd).await.unwrap();
}

async fn say(p: &Peer, to: Target, text: &str) {
    send(p, Command::Send { to, text: text.into() }).await;
}

/// alice and bob, connected and aware of each other.
async fn pair() -> (Peer, Peer) {
    let mut alice = spawn_node("alice", &[]).await;
    let mut bob = spawn_node("bob", &[alice.node.local_addr]).await;
    let (a, b) = (alice.nick.clone(), bob.nick.clone());
    wait_online(&mut alice, &[&b]).await;
    wait_online(&mut bob, &[&a]).await;
    (alice, bob)
}

/// Three fully connected peers.
async fn trio() -> (Peer, Peer, Peer) {
    let mut alice = spawn_node("alice", &[]).await;
    let mut bob = spawn_node("bob", &[alice.node.local_addr]).await;
    let mut carol = spawn_node("carol", &[alice.node.local_addr, bob.node.local_addr]).await;
    let (a, b, c) = (alice.nick.clone(), bob.nick.clone(), carol.nick.clone());
    wait_online(&mut alice, &[&b, &c]).await;
    wait_online(&mut bob, &[&a, &c]).await;
    wait_online(&mut carol, &[&a, &b]).await;
    (alice, bob, carol)
}

/// `p` creates `{base}pie`; returns its (name, key).
async fn create_room(p: &mut Peer, base: &str) -> (String, String) {
    send(p, Command::CreateRoom { name: format!("{base}pie"), max: 13 }).await;
    match wait_for(&mut p.node, "room created", |e| matches!(e, Event::RoomCreated { .. })).await {
        Event::RoomCreated { name, key, .. } => (name, key),
        _ => unreachable!(),
    }
}

/// `p` asks to join `name`.
async fn knock(p: &mut Peer, name: &str, key: &str) {
    send(p, Command::JoinRoom { name: name.into(), key: key.into() }).await;
}

/// `member` sees `nick`'s request to join `room` and approves it.
async fn approve(member: &mut Peer, room: &str, nick: &str) {
    wait_for(&mut member.node, "join request", |e| matches!(e, Event::JoinRequest { nick: n, .. } if n == nick)).await;
    send(member, Command::Approve { room: room.into(), nick: nick.into() }).await;
}

fn join_outcome(e: &Event) -> bool {
    matches!(e, Event::RoomJoined { .. } | Event::JoinEnded { .. } | Event::Error(_))
}

/// `p` joins `name`, let in by `member`; returns the members `p` met on the way in.
async fn join_room(p: &mut Peer, member: &mut Peer, name: &str, key: &str) -> Vec<String> {
    knock(p, name, key).await;
    let nick = p.nick.clone();
    approve(member, name, &nick).await;
    match wait_for(&mut p.node, "room joined", join_outcome).await {
        Event::RoomJoined { members, .. } => members,
        other => panic!("join failed: {other:?}"),
    }
}

#[tokio::test]
async fn names_are_assigned_from_the_base() {
    let (alice, bob) = pair().await;
    assert!(alice.nick.starts_with("alice") && corvo_lan::names::is_valid_name(&alice.nick));
    assert!(bob.nick.starts_with("bob") && corvo_lan::names::is_valid_name(&bob.nick));
}

#[tokio::test]
async fn a_same_moment_name_clash_is_settled_by_the_higher_id() {
    // Both draw "bobroger": exactly one of them, the higher id, must re-roll.
    let mut first = spawn_node_with_word("bob", &[], Some("roger")).await;
    let mut second = spawn_node_with_word("bob", &[first.node.local_addr], Some("roger")).await;
    assert_eq!((first.nick.as_str(), second.nick.as_str()), ("bobroger", "bobroger"));
    let (loser, keeper) =
        if first.node.id > second.node.id { (&mut first, &mut second) } else { (&mut second, &mut first) };

    let renamed = wait_for(&mut loser.node, "re-roll", |e| matches!(e, Event::Assigned { renamed: true, .. })).await;
    let Event::Assigned { nick: new_name, .. } = renamed else { unreachable!() };
    assert!(new_name.starts_with("bob") && new_name != "bobroger", "{new_name}");
    // The lower id keeps its name and learns the other's new one.
    let seen = wait_for(&mut keeper.node, "rename", |e| matches!(e, Event::Renamed { .. })).await;
    assert_eq!(seen, Event::Renamed { old: "bobroger".into(), new: new_name.clone() });
    assert_eq!(keeper.nick, "bobroger");

    // And the new name is addressable.
    say(keeper, Target::Dm(new_name.clone()), "which bob are you?").await;
    wait_for(&mut loser.node, "dm", |e| matches!(e, Event::Message { text, .. } if text == "which bob are you?")).await;
}

#[tokio::test]
async fn dm_reaches_its_target() {
    let (mut alice, bob) = pair().await;
    say(&bob, Target::Dm(alice.nick.clone()), "hello").await;
    let event = wait_for(&mut alice.node, "dm", |e| matches!(e, Event::Message { .. })).await;
    assert!(matches!(event, Event::Message { conv: Conv::Dm(c), from, text, .. }
        if c == bob.nick && from == bob.nick && text == "hello"));
}

#[tokio::test]
async fn room_messages_reach_members_only() {
    let (mut alice, mut bob, mut carol) = trio().await;
    let (room, key) = create_room(&mut alice, "cat").await;
    assert!(room.starts_with("cat") && corvo_lan::names::is_valid_name(&room));

    let members = join_room(&mut bob, &mut alice, &room, &key).await;
    assert_eq!(members, vec![alice.nick.clone()]);
    let joined = wait_for(&mut alice.node, "member joined", |e| matches!(e, Event::MemberJoined { .. })).await;
    assert_eq!(joined, Event::MemberJoined { room: room.clone(), nick: bob.nick.clone() });

    say(&bob, Target::Room(room.clone()), "meow").await;
    let got = wait_for(&mut alice.node, "room message", |e| matches!(e, Event::Message { .. })).await;
    assert!(matches!(got, Event::Message { conv: Conv::Room(r), from, text, .. }
        if r == room && from == bob.nick && text == "meow"));

    // carol received the encrypted frame too but can't read it: her first
    // message is the DM bob sends afterwards on the same connection.
    say(&bob, Target::Dm(carol.nick.clone()), "marker").await;
    let first = wait_for(&mut carol.node, "message", |e| matches!(e, Event::Message { .. })).await;
    assert!(matches!(first, Event::Message { conv: Conv::Dm(_), text, .. } if text == "marker"));
}

#[tokio::test]
async fn joining_with_the_wrong_key_fails() {
    let (mut alice, mut bob) = pair().await;
    let (room, key) = create_room(&mut alice, "cat").await;
    let wrong = format!("{}x", &key[..key.len() - 1]);
    let wrong = if wrong == key { format!("{}y", &key[..key.len() - 1]) } else { wrong };
    knock(&mut bob, &room, &wrong).await;
    let event = wait_for(&mut bob.node, "join outcome", join_outcome).await;
    assert_eq!(event, Event::JoinEnded { name: room, why: JoinEnd::NoRoom });
}

#[tokio::test]
async fn a_member_can_turn_a_newcomer_away() {
    let (mut alice, mut bob) = pair().await;
    let (room, key) = create_room(&mut alice, "cat").await;
    knock(&mut bob, &room, &key).await;
    let waiting = wait_for(&mut bob.node, "waiting", |e| matches!(e, Event::JoinWaiting { .. })).await;
    assert_eq!(waiting, Event::JoinWaiting { name: room.clone(), members: 1, needs: 1 });
    wait_for(&mut alice.node, "join request", |e| matches!(e, Event::JoinRequest { .. })).await;
    send(&alice, Command::Deny { room: room.clone(), nick: bob.nick.clone() }).await;
    let event = wait_for(&mut bob.node, "join outcome", join_outcome).await;
    assert_eq!(event, Event::JoinEnded { name: room, why: JoinEnd::Denied { by: alice.nick.clone() } });
}

#[tokio::test]
async fn a_room_is_gone_once_its_last_member_leaves() {
    let (mut alice, mut bob, mut carol) = trio().await;
    let (room, key) = create_room(&mut alice, "cat").await;
    join_room(&mut bob, &mut alice, &room, &key).await;
    // alice counts bob as a member once he has answered her challenge.
    wait_for(&mut alice.node, "member joined", |e| matches!(e, Event::MemberJoined { .. })).await;

    send(&bob, Command::LeaveRoom { name: room.clone() }).await;
    wait_for(&mut bob.node, "left", |e| matches!(e, Event::RoomLeft { .. })).await;
    let left = wait_for(&mut alice.node, "member left", |e| matches!(e, Event::MemberLeft { .. })).await;
    assert_eq!(left, Event::MemberLeft { room: room.clone(), nick: bob.nick.clone() });

    send(&alice, Command::LeaveRoom { name: room.clone() }).await;
    wait_for(&mut alice.node, "left", |e| matches!(e, Event::RoomLeft { .. })).await;

    // Nobody holds the room any more, so even the right key finds nothing.
    knock(&mut carol, &room, &key).await;
    let event = wait_for(&mut carol.node, "join outcome", join_outcome).await;
    assert_eq!(event, Event::JoinEnded { name: room, why: JoinEnd::NoRoom });
}

#[tokio::test]
async fn quitting_leaves_rooms_and_goes_offline() {
    let (mut alice, mut bob) = pair().await;
    let (room, key) = create_room(&mut alice, "cat").await;
    join_room(&mut bob, &mut alice, &room, &key).await;
    wait_for(&mut alice.node, "member joined", |e| matches!(e, Event::MemberJoined { .. })).await;

    send(&bob, Command::Quit).await;
    timeout(Duration::from_secs(3), bob.node.task).await.expect("bob did not shut down").unwrap();
    let left = wait_for(&mut alice.node, "member left", |e| matches!(e, Event::MemberLeft { .. })).await;
    assert_eq!(left, Event::MemberLeft { room, nick: bob.nick.clone() });
    let gone = wait_for(&mut alice.node, "offline", |e| matches!(e, Event::PeerLeft { .. })).await;
    assert!(matches!(gone, Event::PeerLeft { nick, reason: LeaveReason::Quit, .. } if nick == bob.nick));
}

#[tokio::test]
async fn crash_is_reported_as_connection_lost() {
    let (mut alice, bob) = pair().await;
    bob.node.task.abort(); // drops bob's Node without sending Bye
    let event = wait_for(&mut alice.node, "leave", |e| matches!(e, Event::PeerLeft { .. })).await;
    assert!(matches!(event, Event::PeerLeft { reason: LeaveReason::Lost, .. }));
}

#[tokio::test]
async fn who_lists_connected_peers_and_our_fingerprint() {
    let (mut alice, bob) = pair().await;
    send(&alice, Command::ListPeers).await;
    let Event::Peers { you, nick, peers } =
        wait_for(&mut alice.node, "peer list", |e| matches!(e, Event::Peers { .. })).await
    else {
        unreachable!()
    };
    assert_eq!(you, alice.node.id);
    assert_eq!(nick, alice.nick);
    assert_eq!(peers.len(), 1);
    assert_eq!(peers[0].nick, bob.nick);
    assert_eq!(peers[0].id, bob.node.id);
}

#[tokio::test]
async fn meeting_someone_new_shows_their_fingerprint() {
    let mut alice = spawn_node("alice", &[]).await;
    let bob = spawn_node("bob", &[alice.node.local_addr]).await;
    let expected = format!("first time meeting @{} ({})", bob.nick, bob.node.id.fingerprint());
    wait_for(&mut alice.node, "first meeting", |e| matches!(e, Event::Info(t) if *t == expected)).await;
}

#[tokio::test]
async fn unreachable_manual_peer_is_reported() {
    // Bind then drop a listener to get a port that refuses connections.
    let addr = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
    let mut alice = spawn_node("alice", &[addr]).await;
    wait_for(&mut alice.node, "connect error", |e| matches!(e, Event::Error(msg) if msg.contains("could not connect")))
        .await;
}

#[tokio::test]
async fn garbage_connection_does_not_disturb_the_node() {
    use tokio::io::AsyncWriteExt;
    let (mut alice, bob) = pair().await;
    let mut junk = tokio::net::TcpStream::connect(alice.node.local_addr).await.unwrap();
    junk.write_all(&u32::MAX.to_be_bytes()).await.unwrap();
    drop(junk);
    say(&bob, Target::Dm(alice.nick.clone()), "still here").await;
    wait_for(&mut alice.node, "message", |e| matches!(e, Event::Message { text, .. } if text == "still here")).await;
}

#[tokio::test]
async fn racing_duplicate_dials_settle_on_one_connection() {
    let mut alice = spawn_node("alice", &[]).await;
    // Two dials from bob to alice race; both sides must keep the same one.
    // (Loopback rarely reorders them; the unit tests in node.rs pin the tie-break.)
    let mut bob = spawn_node("bob", &[alice.node.local_addr, alice.node.local_addr]).await;
    let (a, b) = (alice.nick.clone(), bob.nick.clone());
    wait_online(&mut alice, &[&b]).await;
    wait_online(&mut bob, &[&a]).await;
    say(&bob, Target::Dm(a.clone()), "one").await;
    say(&alice, Target::Dm(b.clone()), "two").await;
    wait_for(&mut alice.node, "message", |e| matches!(e, Event::Message { text, .. } if text == "one")).await;
    wait_for(&mut bob.node, "message", |e| matches!(e, Event::Message { text, .. } if text == "two")).await;
    // Ping keeps a healthy link alive; a torn-down one would show up as a leave.
    tokio::time::sleep(Duration::from_millis(300)).await;
    while let Ok(event) = alice.node.events.try_recv() {
        assert!(!matches!(event, Event::PeerLeft { .. } | Event::PeerJoined { .. }), "unexpected {event:?}");
    }
}

#[tokio::test]
async fn an_asking_node_goes_live_on_confirm() {
    let mut node = node::start(NodeConfig {
        keys: Keys::generate(),
        known_peers: None,
        notice: None,
        base: "luffy".into(),
        listen: "127.0.0.1:0".parse().unwrap(),
        discovery: false,
        manual_peers: vec![],
        name_wait: Duration::ZERO,
        fixed_word: None,
        ask_name: true,
    })
    .await
    .unwrap();
    let Event::NamePreview { nick: first } =
        wait_for(&mut node, "a preview", |e| matches!(e, Event::NamePreview { .. })).await
    else {
        unreachable!()
    };
    assert!(first.starts_with("luffy"), "{first}");
    node.commands.send(Command::PreviewName { base: "zoro".into() }).await.unwrap();
    let Event::NamePreview { nick: second } =
        wait_for(&mut node, "a second preview", |e| matches!(e, Event::NamePreview { .. })).await
    else {
        unreachable!()
    };
    assert!(second.starts_with("zoro"), "{second}");
    node.commands.send(Command::ConfirmName).await.unwrap();
    let assigned = wait_for(&mut node, "a name", |e| matches!(e, Event::Assigned { .. })).await;
    assert_eq!(assigned, Event::Assigned { nick: second, renamed: false });
}
