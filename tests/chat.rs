//! End-to-end tests: real Nodes on 127.0.0.1, connected with manual peers
//! (multicast is disabled because it is unreliable on CI and shared machines).

use std::net::SocketAddr;
use std::time::Duration;

use lan_chat::api::{Command, Event, LeaveReason};
use lan_chat::node::{self, NodeConfig, NodeHandle};
use tokio::time::timeout;

async fn spawn_node(nick: &str, peers: &[SocketAddr]) -> NodeHandle {
    node::start(NodeConfig {
        nick: nick.into(),
        listen: "127.0.0.1:0".parse().unwrap(),
        discovery: false,
        manual_peers: peers.to_vec(),
    })
    .await
    .unwrap()
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

/// Waits until every nick in `nicks` has joined, in any order.
async fn wait_joined(node: &mut NodeHandle, nicks: &[&str]) {
    let mut missing: Vec<&str> = nicks.to_vec();
    while !missing.is_empty() {
        let event = wait_for(node, &format!("{missing:?} to join"), |e| matches!(e, Event::PeerJoined { .. })).await;
        if let Event::PeerJoined { nick, .. } = event {
            missing.retain(|n| *n != nick);
        }
    }
}

async fn send(node: &NodeHandle, cmd: Command) {
    node.commands.send(cmd).await.unwrap();
}

/// alice and bob, connected and both aware of each other.
async fn pair() -> (NodeHandle, NodeHandle) {
    let mut alice = spawn_node("alice", &[]).await;
    let mut bob = spawn_node("bob", &[alice.local_addr]).await;
    wait_joined(&mut alice, &["bob"]).await;
    wait_joined(&mut bob, &["alice"]).await;
    (alice, bob)
}

#[tokio::test]
async fn room_message_reaches_peer() {
    let (mut alice, bob) = pair().await;
    send(&bob, Command::Say("hello".into())).await;
    let event = wait_for(&mut alice, "message", |e| matches!(e, Event::Message { .. })).await;
    assert!(matches!(event, Event::Message { from_nick, text, direct: false, .. } if from_nick == "bob" && text == "hello"));
}

#[tokio::test]
async fn direct_message_reaches_only_its_target() {
    let mut alice = spawn_node("alice", &[]).await;
    let mut bob = spawn_node("bob", &[alice.local_addr]).await;
    let mut carol = spawn_node("carol", &[alice.local_addr, bob.local_addr]).await;
    wait_joined(&mut alice, &["bob", "carol"]).await;
    wait_joined(&mut bob, &["alice", "carol"]).await;
    wait_joined(&mut carol, &["alice", "bob"]).await;

    send(&alice, Command::Direct { to: "bob".into(), text: "secret".into() }).await;
    let event = wait_for(&mut bob, "DM", |e| matches!(e, Event::Message { .. })).await;
    assert!(matches!(event, Event::Message { text, direct: true, .. } if text == "secret"));
    wait_for(&mut alice, "DM echo", |e| matches!(e, Event::DirectSent { to, .. } if to == "bob")).await;

    // Frames on one connection arrive in order, so if carol's first message
    // from alice is the marker, she never received the DM.
    send(&alice, Command::Say("marker".into())).await;
    let event = wait_for(&mut carol, "marker", |e| matches!(e, Event::Message { .. })).await;
    assert!(matches!(event, Event::Message { text, direct: false, .. } if text == "marker"));
}

#[tokio::test]
async fn nick_change_propagates_and_is_addressable() {
    let (mut alice, mut bob) = pair().await;
    send(&bob, Command::SetNick("robert".into())).await;
    wait_for(&mut alice, "nick change", |e| {
        matches!(e, Event::NickChanged { old, new, .. } if old == "bob" && new == "robert")
    })
    .await;
    send(&alice, Command::Direct { to: "robert".into(), text: "hi robert".into() }).await;
    wait_for(&mut bob, "DM", |e| matches!(e, Event::Message { text, .. } if text == "hi robert")).await;
}

#[tokio::test]
async fn unknown_dm_target_is_reported() {
    let (mut alice, _bob) = pair().await;
    send(&alice, Command::Direct { to: "nobody".into(), text: "hi".into() }).await;
    wait_for(&mut alice, "error", |e| matches!(e, Event::Error(msg) if msg.contains("nobody"))).await;
}

#[tokio::test]
async fn who_lists_connected_peers() {
    let (mut alice, bob) = pair().await;
    send(&alice, Command::ListPeers).await;
    let event = wait_for(&mut alice, "peer list", |e| matches!(e, Event::Peers(_))).await;
    let Event::Peers(peers) = event else { unreachable!() };
    assert_eq!(peers.len(), 1);
    assert_eq!(peers[0].nick, "bob");
    assert_eq!(peers[0].id, bob.id);
}

#[tokio::test]
async fn quit_is_reported_as_left() {
    let (mut alice, bob) = pair().await;
    send(&bob, Command::Quit).await;
    timeout(Duration::from_secs(3), bob.task).await.expect("bob did not shut down").unwrap();
    let event = wait_for(&mut alice, "leave", |e| matches!(e, Event::PeerLeft { .. })).await;
    assert!(matches!(event, Event::PeerLeft { nick, reason: LeaveReason::Quit, .. } if nick == "bob"));
}

#[tokio::test]
async fn crash_is_reported_as_connection_lost() {
    let (mut alice, bob) = pair().await;
    bob.task.abort(); // drops bob's Node without sending Bye
    let event = wait_for(&mut alice, "leave", |e| matches!(e, Event::PeerLeft { .. })).await;
    assert!(matches!(event, Event::PeerLeft { reason: LeaveReason::Lost, .. }));
}

#[tokio::test]
async fn unreachable_manual_peer_is_reported() {
    // Bind then drop a listener to get a port that refuses connections.
    let addr = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
    let mut alice = spawn_node("alice", &[addr]).await;
    wait_for(&mut alice, "connect error", |e| matches!(e, Event::Error(msg) if msg.contains("could not connect"))).await;
}

#[tokio::test]
async fn garbage_connection_does_not_disturb_the_node() {
    use tokio::io::AsyncWriteExt;
    let (mut alice, bob) = pair().await;
    let mut junk = tokio::net::TcpStream::connect(alice.local_addr).await.unwrap();
    junk.write_all(&u32::MAX.to_be_bytes()).await.unwrap();
    drop(junk);
    send(&bob, Command::Say("still here".into())).await;
    wait_for(&mut alice, "message", |e| matches!(e, Event::Message { text, .. } if text == "still here")).await;
}

#[tokio::test]
async fn racing_duplicate_dials_settle_on_one_connection() {
    let mut alice = spawn_node("alice", &[]).await;
    // Two dials from bob to alice race; both sides must keep the same one.
    // (Loopback rarely reorders them; the unit tests in node.rs pin the tie-break.)
    let mut bob = spawn_node("bob", &[alice.local_addr, alice.local_addr]).await;
    wait_joined(&mut alice, &["bob"]).await;
    wait_joined(&mut bob, &["alice"]).await;
    send(&bob, Command::Say("one".into())).await;
    send(&alice, Command::Say("two".into())).await;
    wait_for(&mut alice, "message", |e| matches!(e, Event::Message { text, .. } if text == "one")).await;
    wait_for(&mut bob, "message", |e| matches!(e, Event::Message { text, .. } if text == "two")).await;
    // Ping keeps a healthy link alive; a torn-down one would show up as a leave.
    tokio::time::sleep(Duration::from_millis(300)).await;
    while let Ok(event) = alice.events.try_recv() {
        assert!(!matches!(event, Event::PeerLeft { .. } | Event::PeerJoined { .. }), "unexpected {event:?}");
    }
}
