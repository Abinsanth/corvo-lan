//! Wire types and framing. Pure encode/decode; no networking decisions.

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const PROTOCOL_VERSION: u8 = 3;
/// Largest JSON body accepted in a single TCP frame.
pub const MAX_FRAME_LEN: usize = 64 * 1024;

/// A peer's X25519 public key. Serialized as 64 lowercase hex characters.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub struct PeerId(pub [u8; 32]);

impl PeerId {
    pub fn random() -> Self {
        PeerId(rand::random())
    }

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
        id: PeerId,
        nick: String,
    },
    /// DM. Plaintext until v0.4.
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

/// Writes `frame` as a 4-byte big-endian length followed by its JSON body.
/// Refuses (`InvalidInput`, nothing written) a frame the peer would reject as too large.
pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, frame: &Frame) -> std::io::Result<()> {
    let body = serde_json::to_vec(frame).expect("Frame always serializes");
    if body.len() > MAX_FRAME_LEN {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, DecodeError::TooLarge(body.len())));
    }
    w.write_u32(body.len() as u32).await?;
    w.write_all(&body).await?;
    w.flush().await
}

/// Reads one length-prefixed frame. Rejects bodies over `MAX_FRAME_LEN` without reading them.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<Frame, DecodeError> {
    let len = r.read_u32().await? as usize;
    if len > MAX_FRAME_LEN {
        return Err(DecodeError::TooLarge(len));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    Ok(serde_json::from_slice(&body)?)
}

/// Current time as unix milliseconds, for message timestamps.
pub fn now_millis() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn round_trip(frame: Frame) -> Frame {
        let mut buf: Vec<u8> = Vec::new();
        write_frame(&mut buf, &frame).await.unwrap();
        let mut reader: &[u8] = &buf;
        read_frame(&mut reader).await.unwrap()
    }

    #[tokio::test]
    async fn every_frame_variant_round_trips() {
        let id = PeerId::test(0x7f3a_9c0d_12e4_5b68);
        let frames = vec![
            Frame::Hello { v: PROTOCOL_VERSION, id, nick: "aliceMoon".into() },
            Frame::Room { room_id: "ab".repeat(16), nonce: "cd".repeat(24), ct: "ef".repeat(20) },
            Frame::Direct { text: "psst".into(), ts: 2 },
            Frame::Nick { nick: "bob".into() },
            Frame::Ping,
            Frame::Bye,
        ];
        for frame in frames {
            assert_eq!(round_trip(frame.clone()).await, frame);
        }
    }

    #[tokio::test]
    async fn frame_wire_format_is_length_prefixed_tagged_json() {
        let mut buf: Vec<u8> = Vec::new();
        write_frame(&mut buf, &Frame::Ping).await.unwrap();
        let body = br#"{"type":"ping"}"#;
        assert_eq!(&buf[..4], &(body.len() as u32).to_be_bytes());
        assert_eq!(&buf[4..], body);
    }

    #[tokio::test]
    async fn oversized_length_is_rejected() {
        let mut buf = ((MAX_FRAME_LEN + 1) as u32).to_be_bytes().to_vec();
        buf.extend_from_slice(b"{}");
        let mut reader: &[u8] = &buf;
        assert!(matches!(read_frame(&mut reader).await, Err(DecodeError::TooLarge(_))));
    }

    #[tokio::test]
    async fn oversized_frame_is_refused_without_writing() {
        let mut buf: Vec<u8> = Vec::new();
        let big = Frame::Direct { text: "x".repeat(MAX_FRAME_LEN), ts: 0 };
        let err = write_frame(&mut buf, &big).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(buf.is_empty());
    }

    #[tokio::test]
    async fn truncated_frame_is_eof() {
        let mut buf = 10u32.to_be_bytes().to_vec();
        buf.extend_from_slice(b"{\"ty");
        let mut reader: &[u8] = &buf;
        assert!(matches!(read_frame(&mut reader).await, Err(DecodeError::Eof)));

        let mut empty: &[u8] = &[];
        assert!(matches!(read_frame(&mut empty).await, Err(DecodeError::Eof)));
    }

    #[tokio::test]
    async fn invalid_json_is_bad_json() {
        let body = b"not json";
        let mut buf = (body.len() as u32).to_be_bytes().to_vec();
        buf.extend_from_slice(body);
        let mut reader: &[u8] = &buf;
        assert!(matches!(read_frame(&mut reader).await, Err(DecodeError::BadJson(_))));
    }

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
}
