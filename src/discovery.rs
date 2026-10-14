//! UDP multicast discovery: announce ourselves every 2s, report peers we hear.

use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::Duration;

use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, watch};
use tracing::{debug, warn};

use crate::names;
use crate::node::NodeMsg;
use crate::protocol::{Announce, PROTOCOL_VERSION, PeerId};

pub const GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 42, 99);
pub const PORT: u16 = 4299;
pub const ANNOUNCE_INTERVAL: Duration = Duration::from_secs(2);

/// A peer heard on the multicast group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sighting {
    pub id: PeerId,
    pub nick: String,
    pub tcp_addr: SocketAddr,
}

/// Validates a datagram. The TCP address uses the UDP source IP, never the payload.
pub fn parse_announce(bytes: &[u8], src: SocketAddr, me: PeerId) -> Option<Sighting> {
    let a = Announce::from_bytes(bytes).ok()?;
    if a.v != PROTOCOL_VERSION || a.id == me || a.tcp_port == 0 || !names::is_valid_name(&a.nick) {
        return None;
    }
    Some(Sighting { id: a.id, nick: a.nick, tcp_addr: SocketAddr::new(src.ip(), a.tcp_port) })
}

fn bind_socket() -> io::Result<std::net::UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    // Lets several instances on one machine share the discovery port. On
    // Windows SO_REUSEADDR alone does that; SO_REUSEPORT is Unix-only.
    #[cfg(unix)]
    socket.set_reuse_port(true)?;
    socket.bind(&SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, PORT).into())?;
    socket.join_multicast_v4(&GROUP, &Ipv4Addr::UNSPECIFIED)?;
    socket.set_multicast_loop_v4(true)?;
    socket.set_multicast_ttl_v4(1)?;
    socket.set_nonblocking(true)?;
    Ok(socket.into())
}

/// Runs until the socket fails to set up (returns the error) or `node_tx` closes.
/// Listens from the start but only announces once `nick` is non-empty (assigned).
pub async fn run(
    me: PeerId,
    nick: watch::Receiver<String>,
    tcp_port: u16,
    node_tx: mpsc::Sender<NodeMsg>,
) -> io::Result<()> {
    let socket = UdpSocket::from_std(bind_socket()?)?;
    let group = SocketAddr::V4(SocketAddrV4::new(GROUP, PORT));
    let mut tick = tokio::time::interval(ANNOUNCE_INTERVAL);
    let mut buf = [0u8; 2048];
    loop {
        tokio::select! {
            _ = tick.tick() => {
                let nick = nick.borrow().clone();
                if nick.is_empty() {
                    continue;
                }
                let announce = Announce { v: PROTOCOL_VERSION, id: me, nick, tcp_port };
                if let Err(e) = socket.send_to(&announce.to_bytes(), group).await {
                    warn!("discovery announce failed: {e}");
                }
            }
            received = socket.recv_from(&mut buf) => match received {
                Ok((n, src)) => {
                    if let Some(sighting) = parse_announce(&buf[..n], src, me) {
                        debug!(?sighting, "discovered");
                        if node_tx.send(NodeMsg::Discovered(sighting)).await.is_err() {
                            return Ok(());
                        }
                    }
                }
                Err(e) => warn!("discovery receive failed: {e}"),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ME: PeerId = PeerId::test(1);
    const OTHER: PeerId = PeerId::test(2);

    fn src() -> SocketAddr {
        "192.168.1.20:4299".parse().unwrap()
    }

    fn bytes(v: u8, id: PeerId, tcp_port: u16) -> Vec<u8> {
        Announce { v, id, nick: "bobroger".into(), tcp_port }.to_bytes()
    }

    #[test]
    fn valid_announce_uses_source_ip_and_payload_port() {
        let s = parse_announce(&bytes(PROTOCOL_VERSION, OTHER, 5000), src(), ME).unwrap();
        assert_eq!(s.id, OTHER);
        assert_eq!(s.nick, "bobroger");
        assert_eq!(s.tcp_addr, "192.168.1.20:5000".parse().unwrap());
    }

    #[test]
    fn own_announce_is_ignored() {
        assert_eq!(parse_announce(&bytes(PROTOCOL_VERSION, ME, 5000), src(), ME), None);
    }

    #[test]
    fn wrong_version_is_ignored() {
        assert_eq!(parse_announce(&bytes(1, OTHER, 5000), src(), ME), None);
    }

    #[test]
    fn zero_port_is_ignored() {
        assert_eq!(parse_announce(&bytes(PROTOCOL_VERSION, OTHER, 0), src(), ME), None);
    }

    #[test]
    fn invalid_nick_is_ignored() {
        let a = Announce { v: PROTOCOL_VERSION, id: OTHER, nick: "bob".into(), tcp_port: 5000 };
        assert_eq!(parse_announce(&a.to_bytes(), src(), ME), None);
    }

    #[test]
    fn garbage_is_ignored() {
        assert_eq!(parse_announce(b"\xff\x00garbage", src(), ME), None);
    }
}
