//! End-to-end tests: real Nodes on 127.0.0.1, connected with manual peers
//! (multicast is disabled because it is unreliable on CI and shared machines).

use std::net::SocketAddr;
use std::time::Duration;

use lan_chat::api::{Command, Conv, Event, LeaveReason, Target};
use lan_chat::node::{self, NodeConfig, NodeHandle};
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
        base: base.into(),
        listen: "127.0.0.1:0".parse().unwrap(),
        discovery: false,
        manual_peers: peers.to_vec(),
        name_wait: Duration::ZERO,
        fixed_word: word.map(String::from),
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

/// `p` creates a room from `base`/`part`; returns its (name, key).
async fn create_room(p: &mut Peer, base: &str, part: &str) -> (String, String) {
    send(p, Command::CreateRoom { base: base.into(), key_part: part.into() }).await;
    match wait_for(&mut p.node, "room created", |e| matches!(e, Event::RoomCreated { .. })).await {
        Event::RoomCreated { name, key } => (name, key),
        _ => unreachable!(),
    }
}

async fn join_room(p: &mut Peer, name: &str, key: &str) -> Vec<String> {
    send(p, Command::JoinRoom { name: name.into(), key: key.into() }).await;
    match wait_for(&mut p.node, "room joined", |e| matches!(e, Event::RoomJoined { .. } | Event::Error(_))).await {
        Event::RoomJoined { members, .. } => members,
        other => panic!("join failed: {other:?}"),
    }
}

#[tokio::test]
async fn names_are_assigned_from_the_base() {
    let (alice, bob) = pair().await;
    assert!(alice.nick.starts_with("alice") && lan_chat::names::is_valid_nick(&alice.nick));
    assert!(bob.nick.starts_with("bob") && lan_chat::names::is_valid_nick(&bob.nick));
}

#[tokio::test]
async fn a_same_moment_name_clash_is_settled_by_the_higher_id() {
    // Both draw "bobRoger": exactly one of them, the higher id, must re-roll.
    let mut first = spawn_node_with_word("bob", &[], Some("Roger")).await;
    let mut second = spawn_node_with_word("bob", &[first.node.local_addr], Some("Roger")).await;
    assert_eq!((first.nick.as_str(), second.nick.as_str()), ("bobRoger", "bobRoger"));
    let (loser, keeper) =
        if first.node.id > second.node.id { (&mut first, &mut second) } else { (&mut second, &mut first) };

    let renamed = wait_for(&mut loser.node, "re-roll", |e| matches!(e, Event::Assigned { renamed: true, .. })).await;
    let Event::Assigned { nick: new_name, .. } = renamed else { unreachable!() };
    assert!(new_name.starts_with("bob") && new_name != "bobRoger", "{new_name}");
    // The lower id keeps its name and learns the other's new one.
    let seen = wait_for(&mut keeper.node, "rename", |e| matches!(e, Event::Renamed { .. })).await;
    assert_eq!(seen, Event::Renamed { old: "bobRoger".into(), new: new_name.clone() });
    assert_eq!(keeper.nick, "bobRoger");

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
    let (room, key) = create_room(&mut alice, "cat", "123").await;
    assert!(room.starts_with("cat") && room.ends_with('R'));

    let members = join_room(&mut bob, &room, &key).await;
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
    let (room, key) = create_room(&mut alice, "cat", "123").await;
    let wrong = format!("{}x", &key[..key.len() - 1]);
    let wrong = if wrong == key { format!("{}y", &key[..key.len() - 1]) } else { wrong };
    send(&bob, Command::JoinRoom { name: room.clone(), key: wrong }).await;
    let event = wait_for(&mut bob.node, "error", |e| matches!(e, Event::Error(_) | Event::RoomJoined { .. })).await;
    assert_eq!(event, Event::Error(format!("no room #{room} with that key")));
}

#[tokio::test]
async fn a_room_is_gone_once_its_last_member_leaves() {
    let (mut alice, mut bob, mut carol) = trio().await;
    let (room, key) = create_room(&mut alice, "cat", "123").await;
    join_room(&mut bob, &room, &key).await;
    // alice counts bob as a member once he has answered her challenge.
    wait_for(&mut alice.node, "member joined", |e| matches!(e, Event::MemberJoined { .. })).await;

    send(&bob, Command::LeaveRoom { name: room.clone() }).await;
    wait_for(&mut bob.node, "left", |e| matches!(e, Event::RoomLeft { .. })).await;
    let left = wait_for(&mut alice.node, "member left", |e| matches!(e, Event::MemberLeft { .. })).await;
    assert_eq!(left, Event::MemberLeft { room: room.clone(), nick: bob.nick.clone() });

    send(&alice, Command::LeaveRoom { name: room.clone() }).await;
    wait_for(&mut alice.node, "left", |e| matches!(e, Event::RoomLeft { .. })).await;

    // Nobody holds the room any more, so even the right key finds nothing.
    send(&carol, Command::JoinRoom { name: room.clone(), key }).await;
    let event = wait_for(&mut carol.node, "error", |e| matches!(e, Event::Error(_) | Event::RoomJoined { .. })).await;
    assert_eq!(event, Event::Error(format!("no room #{room} with that key")));
}

#[tokio::test]
async fn quitting_leaves_rooms_and_goes_offline() {
    let (mut alice, mut bob) = pair().await;
    let (room, key) = create_room(&mut alice, "cat", "123").await;
    join_room(&mut bob, &room, &key).await;
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
async fn who_lists_connected_peers() {
    let (mut alice, bob) = pair().await;
    send(&alice, Command::ListPeers).await;
    let Event::Peers(peers) = wait_for(&mut alice.node, "peer list", |e| matches!(e, Event::Peers(_))).await else {
        unreachable!()
    };
    assert_eq!(peers.len(), 1);
    assert_eq!(peers[0].nick, bob.nick);
    assert_eq!(peers[0].id, bob.node.id);
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
