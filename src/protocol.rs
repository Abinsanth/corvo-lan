//! Wire types. Pure encode/decode; the encrypted framing is in noise.rs.

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u8 = 3;
/// Largest JSON body in one frame: a Noise message (65,535 bytes) minus its 16-byte tag.
pub const MAX_FRAME_LEN: usize = 65_535 - 16;

/// A peer's X25519 public key. Serialized as 64 lowercase hex characters.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub struct PeerId(pub [u8; 32]);

impl PeerId {
    /// First 4 hex characters, used to disambiguate clashing nicks (`bob#7f3a`).
    pub fn short(&self) -> String {
        self.to_string()[..4].to_string()
    }

    /// First 8 bytes as four groups of four hex digits, for comparing by eye.
    pub fn fingerprint(&self) -> String {
        self.0[..8].chunks(2).map(|c| format!("{:02x}{:02x}", c[0], c[1])).collect::<Vec<_>>().join(" ")
    }

    /// `n` big-endian in the first 8 bytes, so test ids order like their numbers.
    #[cfg(test)]
    pub const fn test(n: u64) -> PeerId {
        let be = n.to_be_bytes();
        let mut bytes = [0u8; 32];
        let mut i = 0;
        while i < 8 {
            bytes[i] = be[i];
            i += 1;
        }
        PeerId(bytes)
    }
}

impl fmt::Display for PeerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|b| write!(f, "{b:02x}"))
    }
}

impl From<PeerId> for String {
    fn from(id: PeerId) -> String {
        id.to_string()
    }
}

impl TryFrom<String> for PeerId {
    type Error = String;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        if s.len() != 64 {
            return Err(format!("peer id must be 64 hex chars, got {} chars", s.len()));
        }
        if !s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return Err(format!("invalid peer id {s:?}"));
        }
        crate::room::from_hex(&s)
            .and_then(|b| <[u8; 32]>::try_from(b).ok())
            .map(PeerId)
            .ok_or_else(|| format!("invalid peer id {s:?}"))
    }
}

/// UDP multicast discovery datagram.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Announce {
    pub v: u8,
    pub id: PeerId,
    pub nick: String,
    pub tcp_port: u16,
}

impl Announce {
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("Announce always serializes")
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, DecodeError> {
        Ok(serde_json::from_slice(bytes)?)
    }
}

/// One message on a peer-to-peer TCP connection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Frame {
    Hello {
        v: u8,
        nick: String,
    },
    /// DM, protected by the Noise transport.
    Direct {
        text: String,
        ts: u64,
    },
    /// Only sent to settle a same-moment name clash (see node).
    Nick {
        nick: String,
    },
    /// Encrypted room traffic, sent to every peer; see `room`. All fields are lowercase hex.
    Room {
        room_id: String,
        nonce: String,
        ct: String,
    },
    Ping,
    Bye,
}

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("frame of {0} bytes exceeds the {MAX_FRAME_LEN} byte limit")]
    TooLarge(usize),
    #[error("invalid JSON: {0}")]
    BadJson(#[from] serde_json::Error),
    #[error("message failed to decrypt")]
    Decrypt,
    #[error("connection closed")]
    Eof,
    #[error("i/o error: {0}")]
    Io(std::io::Error),
}

impl From<std::io::Error> for DecodeError {
    fn from(e: std::io::Error) -> Self {
        if e.kind() == std::io::ErrorKind::UnexpectedEof { DecodeError::Eof } else { DecodeError::Io(e) }
    }
}

/// Current time as unix milliseconds, for message timestamps.
pub fn now_millis() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn announce_round_trips() {
        let a = Announce { v: 3, id: PeerId::test(42), nick: "aliceMoon".into(), tcp_port: 54321 };
        let json = String::from_utf8(a.to_bytes()).unwrap();
        let hex = format!("000000000000002a{}", "0".repeat(48));
        assert_eq!(json, format!(r#"{{"v":3,"id":"{hex}","nick":"aliceMoon","tcp_port":54321}}"#));
        assert_eq!(Announce::from_bytes(json.as_bytes()).unwrap(), a);
    }

    #[test]
    fn peer_id_is_64_hex_chars() {
        assert!(PeerId::try_from("xyz".to_string()).is_err());
        assert!(PeerId::try_from("z".repeat(64)).is_err());
        assert!(PeerId::try_from("ab".repeat(31)).is_err(), "62 chars");
        let id = PeerId::try_from("ab".repeat(32)).unwrap();
        assert_eq!(id, PeerId([0xab; 32]));
        assert_eq!(id.to_string(), "ab".repeat(32));
        assert_eq!(id.short(), "abab");
    }

    #[test]
    fn fingerprint_is_the_first_eight_bytes_in_groups() {
        let mut bytes = [0u8; 32];
        bytes[..8].copy_from_slice(&[0x7f, 0x3a, 0x9c, 0x0d, 0x12, 0xe4, 0x5b, 0x68]);
        bytes[8] = 0xff; // not shown
        assert_eq!(PeerId(bytes).fingerprint(), "7f3a 9c0d 12e4 5b68");
    }

    #[test]
    fn test_ids_order_by_their_number() {
        assert!(PeerId::test(1) < PeerId::test(2));
        assert!(PeerId::test(0xff) < PeerId::test(0x100));
    }

    #[test]
    fn peer_ids_parse_only_as_lowercase_hex() {
        let good = "ab".repeat(32);
        assert_eq!(PeerId::try_from(good.clone()).unwrap().to_string(), good);
        assert!(PeerId::try_from("AB".repeat(32)).is_err(), "uppercase");
        assert!(PeerId::try_from(format!("+a{}", "ab".repeat(31))).is_err(), "a sign");
        assert!(PeerId::try_from(format!("zz{}", "ab".repeat(31))).is_err());
        assert!(PeerId::try_from("ab".repeat(31)).is_err(), "too short");
    }
}
