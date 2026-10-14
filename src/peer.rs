//! One task per TCP connection: Hello handshake, then a read loop and a write loop.
//! The Node only ever sees decoded frames and an outbound `mpsc::Sender<Frame>`.
//! The connection is encrypted with Noise (see noise.rs) before `Hello`.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, bail};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{Instant, timeout, timeout_at};
use tracing::{debug, warn};

use crate::api::LeaveReason;
use crate::identity::Keys;
use crate::names;
use crate::node::{ConnKey, NodeMsg, Verdict};
use crate::noise::{self, Opener, Sealer};
use crate::protocol::{DecodeError, Frame, PROTOCOL_VERSION, PeerId};

pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
pub const PING_INTERVAL: Duration = Duration::from_secs(3);
/// A peer that sends nothing (not even `Ping`) for this long is considered lost.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(10);
/// A peer that accepts no data for this long is treated as lost.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// Outbound frames buffered per peer before the Node starts dropping them.
pub const OUTBOX_SIZE: usize = 256;

/// Identifies one TCP connection; a peer can briefly have two during a dial race.
pub type ConnId = u64;
static NEXT_CONN: AtomicU64 = AtomicU64::new(1);

/// Our identity for handshakes. `nick` follows re-rolls after a name clash.
#[derive(Clone)]
pub struct Me {
    pub keys: Keys,
    pub nick: watch::Receiver<String>,
}

/// Connects to `addr` in the background. `expected` is the id from discovery, if any.
pub fn dial(addr: SocketAddr, expected: Option<PeerId>, me: Me, node_tx: mpsc::Sender<NodeMsg>) {
    tokio::spawn(async move {
        let stream = match timeout(HANDSHAKE_TIMEOUT, TcpStream::connect(addr)).await {
            Ok(Ok(stream)) => stream,
            Ok(Err(e)) => return connect_failed(&node_tx, addr, expected, e.to_string()).await,
            Err(_) => return connect_failed(&node_tx, addr, expected, "timed out".into()).await,
        };
        handle(stream, addr, true, expected, me, node_tx).await;
    });
}

/// Handles an accepted inbound connection in the background.
pub fn accept(stream: TcpStream, addr: SocketAddr, me: Me, node_tx: mpsc::Sender<NodeMsg>) {
    tokio::spawn(handle(stream, addr, false, None, me, node_tx));
}

async fn connect_failed(node_tx: &mpsc::Sender<NodeMsg>, addr: SocketAddr, expected: Option<PeerId>, error: String) {
    let _ = node_tx.send(NodeMsg::ConnectFailed { addr, expected, error }).await;
}

async fn handle(
    mut stream: TcpStream,
    addr: SocketAddr,
    outbound: bool,
    expected: Option<PeerId>,
    me: Me,
    node_tx: mpsc::Sender<NodeMsg>,
) {
    let (id, nick, sealer, mut opener) = match handshake(&mut stream, &me, outbound, expected).await {
        Ok(peer) => peer,
        Err(e) => {
            debug!(%addr, "handshake failed: {e:#}");
            if outbound {
                connect_failed(&node_tx, addr, expected, format!("{e:#}")).await;
            }
            return;
        }
    };

    let dialer = if outbound { stream.local_addr() } else { stream.peer_addr() };
    let key = ConnKey { outbound, dialer_port: dialer.map(|a| a.port()).unwrap_or(0) };
    let conn = NEXT_CONN.fetch_add(1, Ordering::Relaxed);
    let (out_tx, out_rx) = mpsc::channel(OUTBOX_SIZE);
    let (accept_tx, accept_rx) = oneshot::channel();
    let connected =
        NodeMsg::PeerConnected { conn, id, nick, addr, key, dialed_as: expected, out: out_tx, accept: accept_tx };
    if node_tx.send(connected).await.is_err() {
        return;
    }
    match accept_rx.await {
        Ok(Verdict::Keep) => {}
        Ok(Verdict::Retire) => {
            // Keep the write half open (no FIN) so the other side doesn't
            // read this as us leaving while it still uses this connection.
            let (mut rd, _wr) = stream.into_split();
            read_loop(&mut rd, &mut opener, conn, id, &node_tx).await;
            return;
        }
        Ok(Verdict::Close) | Err(_) => return,
    }

    let reason = run(stream, conn, id, sealer, opener, out_rx, &node_tx).await;
    let _ = node_tx.send(NodeMsg::PeerGone { conn, id, reason }).await;
}

/// Runs the Noise handshake, then exchanges `Hello` frames over it, all within
/// one `HANDSHAKE_TIMEOUT`. Returns the remote key, its nick and the two
/// halves of the encrypted link.
async fn handshake<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    me: &Me,
    outbound: bool,
    expected: Option<PeerId>,
) -> anyhow::Result<(PeerId, String, Sealer, Opener)> {
    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    let (id, mut sealer, mut opener) = timeout_at(deadline, noise::handshake(stream, &me.keys, outbound, expected))
        .await
        .ok()
        .context("no handshake within 5s")??;
    let nick = me.nick.borrow().clone();
    let hello = async {
        sealer.write_frame(stream, &Frame::Hello { v: PROTOCOL_VERSION, nick }).await.context("sending hello")?;
        opener.read_frame(stream).await.context("reading hello")
    };
    let reply = timeout_at(deadline, hello).await.ok().context("no hello within 5s")??;
    match reply {
        Frame::Hello { v, .. } if v != PROTOCOL_VERSION => bail!("unsupported protocol version {v}"),
        Frame::Hello { nick, .. } if !names::is_valid_nick(&nick) => bail!("invalid nick {nick:?}"),
        Frame::Hello { nick, .. } => Ok((id, nick, sealer, opener)),
        other => bail!("expected hello, got {other:?}"),
    }
}

/// Pumps frames until the connection ends; returns why it ended.
async fn run(
    stream: TcpStream,
    conn: ConnId,
    id: PeerId,
    mut sealer: Sealer,
    mut opener: Opener,
    out_rx: mpsc::Receiver<Frame>,
    node_tx: &mpsc::Sender<NodeMsg>,
) -> LeaveReason {
    let (mut rd, mut wr) = stream.into_split();
    // The writer hands the write half back so a retired connection can stay half-open.
    let mut writer = tokio::spawn(async move {
        let exit = write_loop(&mut wr, &mut sealer, out_rx).await;
        (exit, wr)
    });
    let read = read_loop(&mut rd, &mut opener, conn, id, node_tx);
    tokio::pin!(read);

    let reason = tokio::select! {
        reason = &mut read => reason,
        done = &mut writer => match done {
            // We said goodbye; keep reading until the peer closes its side.
            Ok((WriterExit::Bye, _wr)) => read.await,
            // The Node swapped us out for a duplicate: retire like a rejected one.
            Ok((WriterExit::Closed, wr)) if !node_tx.is_closed() => {
                let reason = read.await;
                drop(wr);
                reason
            }
            other => {
                debug!(%id, "writer stopped: {:?}", other.map(|(exit, _)| exit));
                LeaveReason::Lost
            }
        },
    };
    writer.abort();
    reason
}

/// Forwards frames from `rd` to the Node until EOF, an error, `Bye`, or
/// `IDLE_TIMEOUT` of silence.
async fn read_loop<R: AsyncRead + Unpin>(
    rd: &mut R,
    opener: &mut Opener,
    conn: ConnId,
    id: PeerId,
    node_tx: &mpsc::Sender<NodeMsg>,
) -> LeaveReason {
    loop {
        match timeout(IDLE_TIMEOUT, opener.read_frame(rd)).await {
            Err(_) => {
                debug!(%id, "idle timeout");
                return LeaveReason::Lost;
            }
            Ok(Err(e)) => {
                debug!(%id, "read failed: {e}");
                return LeaveReason::Lost;
            }
            Ok(Ok(Frame::Bye)) => return LeaveReason::Quit,
            Ok(Ok(Frame::Ping | Frame::Hello { .. })) => {}
            Ok(Ok(frame)) => {
                if node_tx.send(NodeMsg::PeerFrame { conn, id, frame }).await.is_err() {
                    return LeaveReason::Lost;
                }
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum WriterExit {
    /// Wrote `Bye`.
    Bye,
    /// The Node dropped our sender (shutdown or a replaced duplicate).
    Closed,
    Failed,
    /// A single write took longer than `WRITE_TIMEOUT`: the peer stopped reading.
    Stalled,
}

/// Writes queued frames plus a `Ping` every `PING_INTERVAL`. Dropping `out_rx`
/// on return is what `Sender::closed()` in Node shutdown waits for. The caller
/// keeps `wr`, so returning doesn't close the write half by itself.
async fn write_loop<W: AsyncWrite + Unpin>(
    wr: &mut W,
    sealer: &mut Sealer,
    mut out_rx: mpsc::Receiver<Frame>,
) -> WriterExit {
    let mut ping = tokio::time::interval(PING_INTERVAL);
    ping.tick().await; // first tick fires immediately
    loop {
        let frame = tokio::select! {
            frame = out_rx.recv() => match frame {
                Some(frame) => frame,
                None => return WriterExit::Closed,
            },
            _ = ping.tick() => Frame::Ping,
        };
        match timeout(WRITE_TIMEOUT, sealer.write_frame(wr, &frame)).await {
            Err(_) => return WriterExit::Stalled,
            // Sending it would make the peer drop us; lose just this frame.
            Ok(Err(e)) if is_too_large(&e) => warn!("not sending frame: {e}"),
            Ok(Err(_)) => return WriterExit::Failed,
            Ok(Ok(())) if frame == Frame::Bye => return WriterExit::Bye,
            Ok(Ok(())) => {}
        }
    }
}

/// `Sealer::write_frame`'s refusal of a frame over `MAX_FRAME_LEN`.
fn is_too_large(e: &std::io::Error) -> bool {
    e.get_ref()
        .and_then(|inner| inner.downcast_ref::<DecodeError>())
        .is_some_and(|d| matches!(d, DecodeError::TooLarge(_)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::noise::test_pair;
    use crate::protocol::MAX_FRAME_LEN;

    #[tokio::test(start_paused = true)]
    async fn writer_gives_up_on_a_peer_that_stops_reading() {
        let ((mut seal, _), _) = test_pair().await;
        let (mut ours, _theirs) = tokio::io::duplex(64); // the other end is never read
        let (tx, rx) = mpsc::channel(4);
        tx.send(Frame::Direct { text: "x".repeat(1000), ts: 0 }).await.unwrap();
        assert_eq!(write_loop(&mut ours, &mut seal, rx).await, WriterExit::Stalled);
    }

    #[tokio::test]
    async fn writer_stops_right_after_bye() {
        let ((mut seal, _), (_, mut open)) = test_pair().await;
        let (mut ours, mut theirs) = tokio::io::duplex(4096);
        let (tx, rx) = mpsc::channel(4);
        tx.send(Frame::Direct { text: "hi".into(), ts: 0 }).await.unwrap();
        tx.send(Frame::Bye).await.unwrap();
        tx.send(Frame::Direct { text: "after bye".into(), ts: 0 }).await.unwrap();
        assert_eq!(write_loop(&mut ours, &mut seal, rx).await, WriterExit::Bye);
        drop(ours);
        assert_eq!(open.read_frame(&mut theirs).await.unwrap(), Frame::Direct { text: "hi".into(), ts: 0 });
        assert_eq!(open.read_frame(&mut theirs).await.unwrap(), Frame::Bye);
        assert!(open.read_frame(&mut theirs).await.is_err()); // closed, nothing after Bye
    }

    #[tokio::test]
    async fn writer_skips_an_oversized_frame_and_keeps_going() {
        let ((mut seal, _), (_, mut open)) = test_pair().await;
        let (mut ours, mut theirs) = tokio::io::duplex(4096);
        let (tx, rx) = mpsc::channel(4);
        tx.send(Frame::Direct { text: "x".repeat(MAX_FRAME_LEN), ts: 0 }).await.unwrap();
        tx.send(Frame::Bye).await.unwrap();
        assert_eq!(write_loop(&mut ours, &mut seal, rx).await, WriterExit::Bye);
        assert_eq!(open.read_frame(&mut theirs).await.unwrap(), Frame::Bye);
    }

    #[tokio::test]
    async fn writer_stops_when_the_node_drops_its_sender() {
        let ((mut seal, _), _) = test_pair().await;
        let (mut ours, _theirs) = tokio::io::duplex(4096);
        let (tx, rx) = mpsc::channel::<Frame>(4);
        drop(tx);
        assert_eq!(write_loop(&mut ours, &mut seal, rx).await, WriterExit::Closed);
    }

    #[tokio::test]
    async fn read_loop_forwards_frames_until_the_peer_closes() {
        let ((mut seal, _), (_, mut open)) = test_pair().await;
        let (mut ours, mut theirs) = tokio::io::duplex(4096);
        let (node_tx, mut node_rx) = mpsc::channel(4);
        let chat = Frame::Direct { text: "late".into(), ts: 0 };
        seal.write_frame(&mut theirs, &Frame::Ping).await.unwrap();
        seal.write_frame(&mut theirs, &chat).await.unwrap();
        drop(theirs);
        assert_eq!(read_loop(&mut ours, &mut open, 7, PeerId::test(9), &node_tx).await, LeaveReason::Lost);
        match node_rx.try_recv() {
            Ok(NodeMsg::PeerFrame { conn: 7, id, frame }) => {
                assert_eq!(id, PeerId::test(9));
                assert_eq!(frame, chat);
            }
            _ => panic!("expected the chat frame to be forwarded"),
        }
        assert!(node_rx.try_recv().is_err()); // Ping is swallowed
    }

    use crate::identity::Keys;
    use crate::noise;
    use std::pin::Pin;
    use std::task::Poll;

    fn me(nick: &str) -> (watch::Sender<String>, Me) {
        let (tx, rx) = watch::channel(nick.to_string());
        (tx, Me { keys: Keys::generate(), nick: rx })
    }

    /// Our side of `handshake` against a peer that finishes Noise and then sends `hello`.
    async fn against(hello: Frame) -> anyhow::Result<(PeerId, String, Sealer, Opener)> {
        let (_tx, me) = me("annMoon");
        let (mut ours, mut theirs) = tokio::io::duplex(1 << 16);
        let them = Keys::generate();
        let peer = async move {
            let (_, mut seal, _open) = noise::handshake(&mut theirs, &them, false, None).await.unwrap();
            seal.write_frame(&mut theirs, &hello).await.unwrap();
            theirs // stays open until our side is done
        };
        let (result, _theirs) = tokio::join!(handshake(&mut ours, &me, true, None), peer);
        result
    }

    #[tokio::test]
    async fn a_good_hello_gives_the_peers_nick() {
        let (_, nick, _, _) = against(Frame::Hello { v: PROTOCOL_VERSION, nick: "bobRoger".into() }).await.unwrap();
        assert_eq!(nick, "bobRoger");
    }

    #[tokio::test]
    async fn a_hello_with_another_version_is_refused() {
        let err = against(Frame::Hello { v: 2, nick: "bobRoger".into() }).await.err().unwrap();
        assert_eq!(format!("{err:#}"), "unsupported protocol version 2");
    }

    #[tokio::test]
    async fn a_hello_with_an_invalid_nick_is_refused() {
        let err = against(Frame::Hello { v: PROTOCOL_VERSION, nick: "bob".into() }).await.err().unwrap();
        assert_eq!(format!("{err:#}"), "invalid nick \"bob\"");
    }

    #[tokio::test(start_paused = true)]
    async fn the_hello_shares_the_handshake_deadline() {
        let (_tx, me) = me("annMoon");
        let (mut ours, mut theirs) = tokio::io::duplex(1 << 16);
        let them = Keys::generate();
        let start = tokio::time::Instant::now();
        let peer = async move {
            tokio::time::sleep(Duration::from_secs(4)).await;
            let _link = noise::handshake(&mut theirs, &them, false, None).await.unwrap();
            tokio::time::sleep(Duration::from_secs(60)).await; // never says hello
        };
        let our_side = async {
            let result = handshake(&mut ours, &me, true, None).await;
            (result, start.elapsed())
        };
        let ((result, took), ()) = tokio::join!(our_side, peer);
        assert_eq!(format!("{:#}", result.err().unwrap()), "no hello within 5s");
        assert!(took <= HANDSHAKE_TIMEOUT, "took {took:?}");
    }

    /// Refuses every write with `InvalidInput`, like a socket that rejects the call.
    struct Refuses;

    impl AsyncWrite for Refuses {
        fn poll_write(self: Pin<&mut Self>, _: &mut std::task::Context<'_>, _: &[u8]) -> Poll<std::io::Result<usize>> {
            Poll::Ready(Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "refused")))
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut std::task::Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut std::task::Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn writer_stops_on_an_invalid_input_that_isnt_too_large() {
        let ((mut seal, _), _) = test_pair().await;
        let (tx, rx) = mpsc::channel(4);
        tx.send(Frame::Direct { text: "hi".into(), ts: 0 }).await.unwrap();
        drop(tx);
        assert_eq!(write_loop(&mut Refuses, &mut seal, rx).await, WriterExit::Failed);
    }

    #[test]
    fn only_too_large_counts_as_too_large() {
        let big = std::io::Error::new(std::io::ErrorKind::InvalidInput, DecodeError::TooLarge(70_000));
        assert!(is_too_large(&big));
        assert!(!is_too_large(&std::io::Error::new(std::io::ErrorKind::InvalidInput, "other")));
        assert!(!is_too_large(&std::io::Error::other(DecodeError::Decrypt)));
    }
}
