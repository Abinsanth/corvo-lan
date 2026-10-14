//! One task per TCP connection: Hello handshake, then a read loop and a write loop.
//! The Node only ever sees decoded frames and an outbound `mpsc::Sender<Frame>`.
//! (v0.3: the Noise handshake wraps the stream here.)

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, bail};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::timeout;
use tracing::{debug, warn};

use crate::api::LeaveReason;
use crate::names;
use crate::node::{ConnKey, NodeMsg, Verdict};
use crate::protocol::{Frame, PROTOCOL_VERSION, PeerId, read_frame, write_frame};

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

/// Our identity as sent in `Hello`. `nick` follows re-rolls after a name clash.
#[derive(Clone)]
pub struct Me {
    pub id: PeerId,
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
    let (id, nick) = match handshake(&mut stream, &me).await {
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
            read_loop(&mut rd, conn, id, &node_tx).await;
            return;
        }
        Ok(Verdict::Close) | Err(_) => return,
    }

    let reason = run(stream, conn, id, out_rx, &node_tx).await;
    let _ = node_tx.send(NodeMsg::PeerGone { conn, id, reason }).await;
}

/// Exchanges `Hello` frames. Returns the remote peer's id and nick.
async fn handshake(stream: &mut TcpStream, me: &Me) -> anyhow::Result<(PeerId, String)> {
    let nick = me.nick.borrow().clone();
    write_frame(stream, &Frame::Hello { v: PROTOCOL_VERSION, id: me.id, nick }).await.context("sending hello")?;
    let reply =
        timeout(HANDSHAKE_TIMEOUT, read_frame(stream)).await.context("no hello within 5s")?.context("reading hello")?;
    match reply {
        Frame::Hello { v, .. } if v != PROTOCOL_VERSION => bail!("unsupported protocol version {v}"),
        Frame::Hello { id, .. } if id == me.id => bail!("connected to myself"),
        Frame::Hello { nick, .. } if !names::is_valid_nick(&nick) => bail!("invalid nick {nick:?}"),
        Frame::Hello { id, nick, .. } => Ok((id, nick)),
        other => bail!("expected hello, got {other:?}"),
    }
}

/// Pumps frames until the connection ends; returns why it ended.
async fn run(
    stream: TcpStream,
    conn: ConnId,
    id: PeerId,
    out_rx: mpsc::Receiver<Frame>,
    node_tx: &mpsc::Sender<NodeMsg>,
) -> LeaveReason {
    let (mut rd, mut wr) = stream.into_split();
    // The writer hands the write half back so a retired connection can stay half-open.
    let mut writer = tokio::spawn(async move {
        let exit = write_loop(&mut wr, out_rx).await;
        (exit, wr)
    });
    let read = read_loop(&mut rd, conn, id, node_tx);
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
    conn: ConnId,
    id: PeerId,
    node_tx: &mpsc::Sender<NodeMsg>,
) -> LeaveReason {
    loop {
        match timeout(IDLE_TIMEOUT, read_frame(rd)).await {
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
async fn write_loop<W: AsyncWrite + Unpin>(wr: &mut W, mut out_rx: mpsc::Receiver<Frame>) -> WriterExit {
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
        match timeout(WRITE_TIMEOUT, write_frame(wr, &frame)).await {
            Err(_) => return WriterExit::Stalled,
            // Sending it would make the peer drop us; lose just this frame.
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::InvalidInput => warn!("not sending frame: {e}"),
            Ok(Err(_)) => return WriterExit::Failed,
            Ok(Ok(())) if frame == Frame::Bye => return WriterExit::Bye,
            Ok(Ok(())) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::MAX_FRAME_LEN;

    #[tokio::test(start_paused = true)]
    async fn writer_gives_up_on_a_peer_that_stops_reading() {
        let (mut ours, _theirs) = tokio::io::duplex(64); // the other end is never read
        let (tx, rx) = mpsc::channel(4);
        tx.send(Frame::Direct { text: "x".repeat(1000), ts: 0 }).await.unwrap();
        assert_eq!(write_loop(&mut ours, rx).await, WriterExit::Stalled);
    }

    #[tokio::test]
    async fn writer_stops_right_after_bye() {
        let (mut ours, mut theirs) = tokio::io::duplex(4096);
        let (tx, rx) = mpsc::channel(4);
        tx.send(Frame::Direct { text: "hi".into(), ts: 0 }).await.unwrap();
        tx.send(Frame::Bye).await.unwrap();
        tx.send(Frame::Direct { text: "after bye".into(), ts: 0 }).await.unwrap();
        assert_eq!(write_loop(&mut ours, rx).await, WriterExit::Bye);
        drop(ours);
        assert_eq!(read_frame(&mut theirs).await.unwrap(), Frame::Direct { text: "hi".into(), ts: 0 });
        assert_eq!(read_frame(&mut theirs).await.unwrap(), Frame::Bye);
        assert!(read_frame(&mut theirs).await.is_err()); // closed, nothing after Bye
    }

    #[tokio::test]
    async fn writer_skips_an_oversized_frame_and_keeps_going() {
        let (mut ours, mut theirs) = tokio::io::duplex(4096);
        let (tx, rx) = mpsc::channel(4);
        tx.send(Frame::Direct { text: "x".repeat(MAX_FRAME_LEN), ts: 0 }).await.unwrap();
        tx.send(Frame::Bye).await.unwrap();
        assert_eq!(write_loop(&mut ours, rx).await, WriterExit::Bye);
        assert_eq!(read_frame(&mut theirs).await.unwrap(), Frame::Bye);
    }

    #[tokio::test]
    async fn writer_stops_when_the_node_drops_its_sender() {
        let (mut ours, _theirs) = tokio::io::duplex(4096);
        let (tx, rx) = mpsc::channel::<Frame>(4);
        drop(tx);
        assert_eq!(write_loop(&mut ours, rx).await, WriterExit::Closed);
    }

    #[tokio::test]
    async fn read_loop_forwards_frames_until_the_peer_closes() {
        let (mut ours, mut theirs) = tokio::io::duplex(4096);
        let (node_tx, mut node_rx) = mpsc::channel(4);
        let chat = Frame::Direct { text: "late".into(), ts: 0 };
        write_frame(&mut theirs, &Frame::Ping).await.unwrap();
        write_frame(&mut theirs, &chat).await.unwrap();
        drop(theirs);
        assert_eq!(read_loop(&mut ours, 7, PeerId::test(9), &node_tx).await, LeaveReason::Lost);
        match node_rx.try_recv() {
            Ok(NodeMsg::PeerFrame { conn: 7, id, frame }) => {
                assert_eq!(id, PeerId::test(9));
                assert_eq!(frame, chat);
            }
            _ => panic!("expected the chat frame to be forwarded"),
        }
        assert!(node_rx.try_recv().is_err()); // Ping is swallowed
    }
}
