//! Wire types and framing. Pure encode/decode; no networking decisions.

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const PROTOCOL_VERSION: u8 = 1;
/// Largest JSON body accepted in a single TCP frame.
pub const MAX_FRAME_LEN: usize = 64 * 1024;

/// Random per-run identity. Serialized as 16 lowercase hex characters.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub struct PeerId(pub u64);

impl PeerId {
    pub fn random() -> Self {
        PeerId(rand::random())
    }

    /// First 4 hex characters, used to disambiguate clashing nicks (`bob#7f3a`).
    pub fn short(&self) -> String {
        self.to_string()[..4].to_string()
    }
}

impl fmt::Display for PeerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:016x}", self.0)
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
        if s.len() != 16 {
            return Err(format!("peer id must be 16 hex chars, got {:?}", s));
        }
        u64::from_str_radix(&s, 16)
            .map(PeerId)
            .map_err(|e| format!("invalid peer id {s:?}: {e}"))
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
    Hello { v: u8, id: PeerId, nick: String },
    Chat { text: String, ts: u64 },
    Direct { text: String, ts: u64 },
    Nick { nick: String },
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
        if e.kind() == std::io::ErrorKind::UnexpectedEof {
            DecodeError::Eof
        } else {
            DecodeError::Io(e)
        }
    }
}

/// Writes `frame` as a 4-byte big-endian length followed by its JSON body.
pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, frame: &Frame) -> std::io::Result<()> {
    let body = serde_json::to_vec(frame).expect("Frame always serializes");
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

/// Current time as unix milliseconds, for `Frame::Chat`/`Frame::Direct` timestamps.
pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
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
        let id = PeerId(0x7f3a_9c0d_12e4_5b68);
        let frames = vec![
            Frame::Hello { v: PROTOCOL_VERSION, id, nick: "alice".into() },
            Frame::Chat { text: "hello".into(), ts: 1 },
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
        let a = Announce { v: 1, id: PeerId(42), nick: "alice".into(), tcp_port: 54321 };
        let json = String::from_utf8(a.to_bytes()).unwrap();
        assert_eq!(json, r#"{"v":1,"id":"000000000000002a","nick":"alice","tcp_port":54321}"#);
        assert_eq!(Announce::from_bytes(json.as_bytes()).unwrap(), a);
    }

    #[test]
    fn peer_id_rejects_bad_hex() {
        assert!(PeerId::try_from("xyz".to_string()).is_err());
        assert!(PeerId::try_from("zzzzzzzzzzzzzzzz".to_string()).is_err());
        assert_eq!(PeerId(0xabcd_0000_0000_0000).short(), "abcd");
    }
}
