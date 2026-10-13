//! UDP multicast discovery: announce ourselves every 2s, report peers we hear.

use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use crate::protocol::{Announce, PeerId, PROTOCOL_VERSION};

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
    if a.v != PROTOCOL_VERSION || a.id == me || a.tcp_port == 0 {
        return None;
    }
    Some(Sighting { id: a.id, nick: a.nick, tcp_addr: SocketAddr::new(src.ip(), a.tcp_port) })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ME: PeerId = PeerId(1);
    const OTHER: PeerId = PeerId(2);

    fn src() -> SocketAddr {
        "192.168.1.20:4299".parse().unwrap()
    }

    fn bytes(v: u8, id: PeerId, tcp_port: u16) -> Vec<u8> {
        Announce { v, id, nick: "bob".into(), tcp_port }.to_bytes()
    }

    #[test]
    fn valid_announce_uses_source_ip_and_payload_port() {
        let s = parse_announce(&bytes(1, OTHER, 5000), src(), ME).unwrap();
        assert_eq!(s.id, OTHER);
        assert_eq!(s.nick, "bob");
        assert_eq!(s.tcp_addr, "192.168.1.20:5000".parse().unwrap());
    }

    #[test]
    fn own_announce_is_ignored() {
        assert_eq!(parse_announce(&bytes(1, ME, 5000), src(), ME), None);
    }

    #[test]
    fn wrong_version_is_ignored() {
        assert_eq!(parse_announce(&bytes(2, OTHER, 5000), src(), ME), None);
    }

    #[test]
    fn zero_port_is_ignored() {
        assert_eq!(parse_announce(&bytes(1, OTHER, 0), src(), ME), None);
    }

    #[test]
    fn garbage_is_ignored() {
        assert_eq!(parse_announce(b"\xff\x00garbage", src(), ME), None);
    }
}
