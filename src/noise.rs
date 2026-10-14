//! The encrypted link: a version preface, a Noise XX handshake, then one
//! Noise transport message per frame. `peer.rs` decides what frames mean.

use std::sync::Arc;

use snow::{Builder, StatelessTransportState};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::identity::Keys;
use crate::protocol::{DecodeError, Frame, MAX_FRAME_LEN, PROTOCOL_VERSION, PeerId};

/// Sent before anything else, followed by `PROTOCOL_VERSION`.
pub const MAGIC: [u8; 4] = *b"CRVO";
const PARAMS: &str = "Noise_XX_25519_ChaChaPoly_BLAKE2s";
const PROLOGUE: &[u8] = b"corvo-lan v3";
const TAG_LEN: usize = 16;
const MAX_MSG: usize = 65_535;

#[derive(Debug, thiserror::Error)]
pub enum HandshakeError {
    #[error("peer runs an older corvo-lan")]
    OldPeer,
    #[error("unsupported protocol version {0}")]
    Version(u8),
    #[error("key mismatch")]
    KeyMismatch,
    #[error("peer key is invalid")]
    WeakKey,
    #[error("connected to myself")]
    Myself,
    #[error("noise handshake failed: {0}")]
    Noise(#[from] snow::Error),
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
}

/// Encrypts frames for one direction. Each side keeps its own nonce counter,
/// so the read and write halves can live in different tasks.
pub struct Sealer {
    state: Arc<StatelessTransportState>,
    nonce: u64,
}

pub struct Opener {
    state: Arc<StatelessTransportState>,
    nonce: u64,
}

/// Exchanges prefaces, runs Noise XX and checks who answered. `expected` is
/// the key discovery advertised, if we dialed because of it.
pub async fn handshake<S: AsyncRead + AsyncWrite + Unpin>(
    s: &mut S,
    keys: &Keys,
    initiator: bool,
    expected: Option<PeerId>,
) -> Result<(PeerId, Sealer, Opener), HandshakeError> {
    let mut preface = MAGIC.to_vec();
    preface.push(PROTOCOL_VERSION);
    s.write_all(&preface).await?;
    s.flush().await?;
    let mut theirs = [0u8; 5];
    s.read_exact(&mut theirs).await?;
    if theirs[..4] != MAGIC {
        return Err(HandshakeError::OldPeer);
    }
    if theirs[4] != PROTOCOL_VERSION {
        return Err(HandshakeError::Version(theirs[4]));
    }

    let builder = Builder::new(PARAMS.parse().expect("valid noise params"))
        .local_private_key(keys.secret())?
        .prologue(PROLOGUE)?;
    let mut hs = if initiator { builder.build_initiator()? } else { builder.build_responder()? };
    let mut buf = vec![0u8; MAX_MSG];
    while !hs.is_handshake_finished() {
        if hs.is_my_turn() {
            let n = hs.write_message(&[], &mut buf)?;
            write_msg(s, &buf[..n]).await?;
        } else {
            let msg = read_msg(s).await?;
            hs.read_message(&msg, &mut buf)?;
        }
    }

    let remote = hs.get_remote_static().and_then(|k| <[u8; 32]>::try_from(k).ok()).map(PeerId);
    let remote = remote.ok_or(HandshakeError::Noise(snow::Error::Input))?;
    if is_weak(&remote, keys) {
        return Err(HandshakeError::WeakKey);
    }
    if remote == keys.public {
        return Err(HandshakeError::Myself);
    }
    if expected.is_some_and(|e| e != remote) {
        return Err(HandshakeError::KeyMismatch);
    }
    let state = Arc::new(hs.into_stateless_transport_mode()?);
    Ok((remote, Sealer { state: state.clone(), nonce: 0 }, Opener { state, nonce: 0 }))
}

impl Sealer {
    /// Refuses (`InvalidInput`, nothing written) a frame over `MAX_FRAME_LEN`.
    pub async fn write_frame<W: AsyncWrite + Unpin>(&mut self, w: &mut W, frame: &Frame) -> std::io::Result<()> {
        let body = serde_json::to_vec(frame).expect("Frame always serializes");
        if body.len() > MAX_FRAME_LEN {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, DecodeError::TooLarge(body.len())));
        }
        let mut msg = vec![0u8; body.len() + TAG_LEN];
        let n = self.state.write_message(self.nonce, &body, &mut msg).map_err(std::io::Error::other)?;
        self.nonce += 1;
        write_msg(w, &msg[..n]).await
    }
}

impl Opener {
    /// A message that doesn't decrypt is fatal: nonces are sequential, so a
    /// lost or altered message can't be skipped.
    pub async fn read_frame<R: AsyncRead + Unpin>(&mut self, r: &mut R) -> Result<Frame, DecodeError> {
        let msg = read_msg(r).await?;
        let mut body = vec![0u8; msg.len()];
        let n = self.state.read_message(self.nonce, &msg, &mut body).map_err(|_| DecodeError::Decrypt)?;
        self.nonce += 1;
        Ok(serde_json::from_slice(&body[..n])?)
    }
}

/// A 2-byte big-endian length, then the message.
async fn write_msg<W: AsyncWrite + Unpin>(w: &mut W, msg: &[u8]) -> std::io::Result<()> {
    w.write_u16(msg.len() as u16).await?;
    w.write_all(msg).await?;
    w.flush().await
}

async fn read_msg<R: AsyncRead + Unpin>(r: &mut R) -> std::io::Result<Vec<u8>> {
    let len = r.read_u16().await? as usize;
    let mut msg = vec![0u8; len];
    r.read_exact(&mut msg).await?;
    Ok(msg)
}

/// Two linked ends: `.0` is the initiator's, `.1` the responder's.
#[cfg(test)]
pub(crate) async fn test_pair() -> ((Sealer, Opener), (Sealer, Opener)) {
    let (mut a, mut b) = tokio::io::duplex(1 << 16);
    let (ka, kb) = (Keys::generate(), Keys::generate());
    let (ra, rb) = tokio::join!(handshake(&mut a, &ka, true, None), handshake(&mut b, &kb, false, None));
    let ((_, sa, oa), (_, sb, ob)) = (ra.unwrap(), rb.unwrap());
    ((sa, oa), (sb, ob))
}

/// A low-order remote key gives an all-zero shared secret, so anyone could claim it.
fn is_weak(remote: &PeerId, keys: &Keys) -> bool {
    curve25519_dalek::MontgomeryPoint(remote.0).mul_clamped(*keys.secret()).to_bytes() == [0u8; 32]
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    async fn both(a: Keys, b: Keys, expected: Option<PeerId>) -> (Result<PeerId, String>, Result<PeerId, String>) {
        let (mut x, mut y) = duplex(1 << 16);
        let (ra, rb) = tokio::join!(handshake(&mut x, &a, true, expected), handshake(&mut y, &b, false, None));
        (ra.map(|r| r.0).map_err(|e| e.to_string()), rb.map(|r| r.0).map_err(|e| e.to_string()))
    }

    #[test]
    fn low_order_keys_are_weak() {
        let k = Keys::generate();
        assert!(is_weak(&PeerId([0; 32]), &k));
        assert!(!is_weak(&Keys::generate().public, &k));
    }

    #[tokio::test]
    async fn handshake_yields_each_others_keys() {
        let (a, b) = (Keys::generate(), Keys::generate());
        let (ra, rb) = both(a.clone(), b.clone(), Some(b.public)).await;
        assert_eq!(ra.unwrap(), b.public);
        assert_eq!(rb.unwrap(), a.public);
    }

    #[tokio::test]
    async fn a_different_key_than_discovered_is_refused() {
        let (ra, _) = both(Keys::generate(), Keys::generate(), Some(Keys::generate().public)).await;
        assert_eq!(ra.unwrap_err(), "key mismatch");
    }

    #[tokio::test]
    async fn talking_to_myself_is_refused() {
        let me = Keys::generate();
        let (ra, rb) = both(me.clone(), me, None).await;
        assert_eq!(ra.unwrap_err(), "connected to myself");
        assert_eq!(rb.unwrap_err(), "connected to myself");
    }

    #[tokio::test]
    async fn an_old_plaintext_hello_is_refused_at_the_preface() {
        let (mut ours, mut theirs) = duplex(1 << 16);
        let body = br#"{"type":"hello","v":2,"id":"0000000000000001","nick":"bobroger"}"#;
        theirs.write_u32(body.len() as u32).await.unwrap();
        theirs.write_all(body).await.unwrap();
        let err = handshake(&mut ours, &Keys::generate(), false, None).await.err().unwrap();
        assert_eq!(err.to_string(), "peer runs an older corvo-lan");
    }

    #[tokio::test]
    async fn another_version_byte_is_refused() {
        let (mut ours, mut theirs) = duplex(1 << 16);
        theirs.write_all(b"CRVO\x06").await.unwrap();
        let err = handshake(&mut ours, &Keys::generate(), true, None).await.err().unwrap();
        assert_eq!(err.to_string(), "unsupported protocol version 6");
    }

    #[tokio::test]
    async fn garbage_after_the_preface_fails_cleanly() {
        let (mut ours, mut theirs) = duplex(1 << 16);
        theirs.write_all(b"CRVO\x05").await.unwrap();
        theirs.write_u16(10).await.unwrap();
        theirs.write_all(&[7u8; 10]).await.unwrap();
        let err = handshake(&mut ours, &Keys::generate(), false, None).await.err().unwrap();
        assert!(matches!(err, HandshakeError::Noise(_)), "{err}");
    }

    #[tokio::test]
    async fn closing_mid_handshake_is_an_error() {
        let (mut ours, mut theirs) = duplex(1 << 16);
        theirs.write_all(b"CRVO\x05").await.unwrap();
        drop(theirs);
        let err = handshake(&mut ours, &Keys::generate(), false, None).await.err().unwrap();
        assert!(matches!(err, HandshakeError::Io(_)), "{err}");
    }

    #[tokio::test]
    async fn every_frame_variant_round_trips() {
        let ((mut seal, _), (_, mut open)) = test_pair().await;
        let (mut wr, mut rd) = duplex(1 << 20);
        let frames = vec![
            Frame::Hello { v: PROTOCOL_VERSION, nick: "alicemoon".into() },
            Frame::Room { room_id: "ab".repeat(16), nonce: "cd".repeat(24), ct: "ef".repeat(20) },
            Frame::Direct { text: "psst".into(), ts: 2 },
            Frame::Nick { nick: "bob".into() },
            Frame::Ping,
            Frame::Bye,
        ];
        for frame in &frames {
            seal.write_frame(&mut wr, frame).await.unwrap();
        }
        for frame in frames {
            assert_eq!(open.read_frame(&mut rd).await.unwrap(), frame);
        }
    }

    #[tokio::test]
    async fn frames_are_not_readable_on_the_wire() {
        let ((mut seal, _), _) = test_pair().await;
        let mut wire: Vec<u8> = Vec::new();
        seal.write_frame(&mut wire, &Frame::Direct { text: "secret plans".into(), ts: 0 }).await.unwrap();
        assert!(!wire.windows(6).any(|w| w == b"secret"));
        let len = u16::from_be_bytes([wire[0], wire[1]]) as usize;
        assert_eq!(len, wire.len() - 2);
    }

    #[tokio::test]
    async fn a_tampered_message_fails_to_decrypt() {
        let ((mut seal, _), (_, mut open)) = test_pair().await;
        let mut wire: Vec<u8> = Vec::new();
        seal.write_frame(&mut wire, &Frame::Ping).await.unwrap();
        let last = wire.len() - 1;
        wire[last] ^= 1;
        let mut reader: &[u8] = &wire;
        assert!(matches!(open.read_frame(&mut reader).await, Err(DecodeError::Decrypt)));
    }

    #[tokio::test]
    async fn a_skipped_message_fails_to_decrypt() {
        let ((mut seal, _), (_, mut open)) = test_pair().await;
        let mut first: Vec<u8> = Vec::new();
        seal.write_frame(&mut first, &Frame::Ping).await.unwrap();
        let mut second: Vec<u8> = Vec::new();
        seal.write_frame(&mut second, &Frame::Ping).await.unwrap();
        let mut reader: &[u8] = &second; // first never arrives
        assert!(matches!(open.read_frame(&mut reader).await, Err(DecodeError::Decrypt)));
    }

    #[tokio::test]
    async fn an_oversized_frame_is_refused_without_writing() {
        let ((mut seal, _), _) = test_pair().await;
        let mut wire: Vec<u8> = Vec::new();
        let big = Frame::Direct { text: "x".repeat(MAX_FRAME_LEN), ts: 0 };
        let err = seal.write_frame(&mut wire, &big).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(wire.is_empty());
    }

    #[tokio::test]
    async fn a_frame_at_the_limit_goes_through() {
        let ((mut seal, _), (_, mut open)) = test_pair().await;
        let overhead = serde_json::to_vec(&Frame::Direct { text: String::new(), ts: 0 }).unwrap().len();
        let frame = Frame::Direct { text: "x".repeat(MAX_FRAME_LEN - overhead), ts: 0 };
        let mut wire: Vec<u8> = Vec::new();
        seal.write_frame(&mut wire, &frame).await.unwrap();
        let mut reader: &[u8] = &wire;
        assert_eq!(open.read_frame(&mut reader).await.unwrap(), frame);
    }

    #[tokio::test]
    async fn truncated_input_is_eof() {
        let ((_, _), (_, mut open)) = test_pair().await;
        let mut reader: &[u8] = &[0, 40, 1, 2];
        assert!(matches!(open.read_frame(&mut reader).await, Err(DecodeError::Eof)));
        let mut empty: &[u8] = &[];
        assert!(matches!(open.read_frame(&mut empty).await, Err(DecodeError::Eof)));
    }

    use snow::params::{CipherChoice, DHChoice, HashChoice};
    use snow::resolvers::{CryptoResolver, DefaultResolver};
    use snow::types::{Cipher, Dh, Hash, Random};

    /// The default primitives, except that a static key set on a DH claims the
    /// all-zero (low-order) point. Its DH output is then always zero, which is
    /// exactly what an attacker without a real key could compute.
    struct WeakResolver;

    impl CryptoResolver for WeakResolver {
        fn resolve_rng(&self) -> Option<Box<dyn Random>> {
            DefaultResolver.resolve_rng()
        }
        fn resolve_dh(&self, choice: &DHChoice) -> Option<Box<dyn Dh>> {
            Some(Box::new(WeakStatic { inner: DefaultResolver.resolve_dh(choice)?, weak: false }))
        }
        fn resolve_hash(&self, choice: &HashChoice) -> Option<Box<dyn Hash>> {
            DefaultResolver.resolve_hash(choice)
        }
        fn resolve_cipher(&self, choice: &CipherChoice) -> Option<Box<dyn Cipher>> {
            DefaultResolver.resolve_cipher(choice)
        }
    }

    struct WeakStatic {
        inner: Box<dyn Dh>,
        /// Set for the static key (`set`), not for ephemerals (`generate`).
        weak: bool,
    }

    impl Dh for WeakStatic {
        fn name(&self) -> &'static str {
            self.inner.name()
        }
        fn pub_len(&self) -> usize {
            self.inner.pub_len()
        }
        fn priv_len(&self) -> usize {
            self.inner.priv_len()
        }
        fn set(&mut self, privkey: &[u8]) {
            self.inner.set(privkey);
            self.weak = true;
        }
        fn generate(&mut self, rng: &mut dyn Random) -> Result<(), snow::Error> {
            self.inner.generate(rng)
        }
        fn pubkey(&self) -> &[u8] {
            if self.weak { &[0; 32] } else { self.inner.pubkey() }
        }
        fn privkey(&self) -> &[u8] {
            self.inner.privkey()
        }
        fn dh(&self, pubkey: &[u8], out: &mut [u8]) -> Result<(), snow::Error> {
            if self.weak {
                out[..32].fill(0);
                Ok(())
            } else {
                self.inner.dh(pubkey, out)
            }
        }
    }

    /// The responder side of a handshake from a peer claiming a low-order key.
    async fn weak_responder<S: AsyncRead + AsyncWrite + Unpin>(s: &mut S) -> Result<(), HandshakeError> {
        s.write_all(&[MAGIC.as_slice(), &[PROTOCOL_VERSION]].concat()).await?;
        let mut theirs = [0u8; 5];
        s.read_exact(&mut theirs).await?;
        let mut hs = Builder::with_resolver(PARAMS.parse().unwrap(), Box::new(WeakResolver))
            .local_private_key(&[7; 32])?
            .prologue(PROLOGUE)?
            .build_responder()?;
        let mut buf = vec![0u8; MAX_MSG];
        while !hs.is_handshake_finished() {
            if hs.is_my_turn() {
                let n = hs.write_message(&[], &mut buf)?;
                write_msg(s, &buf[..n]).await?;
            } else {
                let msg = read_msg(s).await?;
                hs.read_message(&msg, &mut buf)?;
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn a_peer_claiming_a_low_order_key_is_refused_end_to_end() {
        let (mut ours, mut theirs) = duplex(1 << 16);
        let keys = Keys::generate();
        let (ra, _) = tokio::join!(handshake(&mut ours, &keys, true, None), weak_responder(&mut theirs));
        assert_eq!(ra.err().unwrap().to_string(), "peer key is invalid");
    }
}
