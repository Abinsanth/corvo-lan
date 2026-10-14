//! Private rooms: keys, key derivation, sealing/opening room frames, replay checks.
//!
//! A room has two sets of keys. The *door* keys come from its name and the
//! 6-character join key: anyone who has those can knock, nothing more. The
//! *room* keys come from a random secret the creator made, which members hand
//! to a newcomer once enough of them approve. Every room frame goes to every
//! peer; only key holders can recognise (`room_id`) and read it.

use std::collections::{HashSet, VecDeque};

use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use hkdf::Hkdf;
use rand::seq::IndexedRandom;
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::protocol::{Frame, PeerId};

/// Join key characters: no look-alikes (`0 o 1 l i`).
pub const SYS_ALPHABET: &[u8] = b"23456789abcdefghjkmnpqrstuvwxyz";
pub const SYS_LEN: usize = 6;
/// Room messages further than this from our clock are dropped.
pub const MAX_SKEW_MS: u64 = 5 * 60 * 1000;
/// Nonces remembered per room for replay detection.
pub const REPLAY_WINDOW: usize = 1024;

pub type RoomId = [u8; 16];
/// The random value a room's creator makes; its room keys come from it.
pub type Secret = [u8; 32];

/// An id and encryption key: a room's door keys or its room keys.
#[derive(Clone)]
pub struct RoomKeys {
    pub id: RoomId,
    enc: [u8; 32],
}

/// The decrypted contents of a `Frame::Room`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoomEnvelope {
    /// Unix millis when sent; used for the replay window.
    pub ts: u64,
    pub body: RoomBody,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RoomBody {
    Chat {
        text: String,
    },
    /// "Is anyone here?" from someone joining.
    Probe {
        nonce: String,
    },
    /// Presence. Answers to a probe echo its nonce, which is what proves the
    /// sender is live rather than a replay; periodic heartbeats carry `None`.
    Here {
        name: String,
        #[serde(default)]
        echo: Option<String>,
    },
    Leave,
    /// Door channel, newcomer to everyone: let me in. `asked` is when the
    /// newcomer first knocked (unix millis), the same on every re-knock.
    Knock {
        nonce: String,
        asked: u64,
    },
    /// Door channel, member to a newcomer: someone's in, echoing its knock.
    /// `members` counts the sender.
    Door {
        echo: String,
        members: u8,
        max: u8,
    },
    /// Door channel, approver to a newcomer: the room secret (hex).
    Grant {
        secret: String,
        max: u8,
    },
    /// Door channel, member to a newcomer: not let in, because the room is
    /// `full` or a member said no.
    Refuse {
        full: bool,
    },
    /// Room channel: a member approves (`yes`) or denies `peer`'s request.
    Vote {
        peer: PeerId,
        asked: u64,
        yes: bool,
    },
}

/// A join key: `SYS_LEN` characters drawn from the OS-seeded CSPRNG.
pub fn make_key() -> String {
    let mut rng = rand::rng();
    (0..SYS_LEN).map(|_| *SYS_ALPHABET.choose(&mut rng).expect("alphabet is not empty") as char).collect()
}

/// A join key as shared by a room's members.
pub fn validate_key(key: &str) -> Result<(), String> {
    if key.len() != SYS_LEN || !key.bytes().all(|b| SYS_ALPHABET.contains(&b)) {
        return Err(format!("a room key is {SYS_LEN} letters and digits, like k7m9xq"));
    }
    Ok(())
}

pub fn new_secret() -> Secret {
    rand::random()
}

/// The door keys for a room name and join key. Slow on purpose (Argon2id,
/// ~19 MiB): call it off the async actor.
pub fn derive(name: &str, key: &str) -> RoomKeys {
    let params = Params::new(19_456, 2, 1, Some(32)).expect("valid Argon2 params");
    let salt = format!("corvo-lan room v4:{name}");
    let mut stretched = [0u8; 32];
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(key.as_bytes(), salt.as_bytes(), &mut stretched)
        .expect("salt and output lengths are valid");
    expand(None, &stretched, b"corvo-lan v4 door-id", b"corvo-lan v4 door-enc")
}

/// The room keys for a room name and its secret. The secret is random, so
/// no stretching is needed.
pub fn from_secret(name: &str, secret: &Secret) -> RoomKeys {
    expand(Some(name.as_bytes()), secret, b"corvo-lan v4 room-id", b"corvo-lan v4 room-enc")
}

fn expand(salt: Option<&[u8]>, ikm: &[u8], id_label: &[u8], enc_label: &[u8]) -> RoomKeys {
    let hk = Hkdf::<Sha256>::new(salt, ikm);
    let mut id = [0u8; 16];
    let mut enc = [0u8; 32];
    hk.expand(id_label, &mut id).expect("16 bytes is a valid length");
    hk.expand(enc_label, &mut enc).expect("32 bytes is a valid length");
    RoomKeys { id, enc }
}

/// Binds the frame to its room and to the peer it travels from, so a frame
/// relabelled to another sender fails to open.
fn aad(id: &RoomId, sender: PeerId) -> [u8; 48] {
    let mut aad = [0u8; 48];
    aad[..16].copy_from_slice(id);
    aad[16..].copy_from_slice(&sender.0);
    aad
}

/// Encrypts `env` as sent by `sender` (that's us).
pub fn seal(keys: &RoomKeys, sender: PeerId, env: &RoomEnvelope) -> Frame {
    let nonce: [u8; 24] = rand::random();
    let plain = serde_json::to_vec(env).expect("envelope always serializes");
    let aad = aad(&keys.id, sender);
    let ct = XChaCha20Poly1305::new(&keys.enc.into())
        .encrypt(&XNonce::from(nonce), Payload { msg: &plain, aad: &aad })
        .expect("encryption cannot fail for in-memory buffers");
    Frame::Room { room_id: to_hex(&keys.id), nonce: to_hex(&nonce), ct: to_hex(&ct) }
}

/// Decrypts a room frame that arrived on `sender`'s connection. Returns the
/// envelope and its nonce (for replay checks), or `None` if it doesn't open.
pub fn open(keys: &RoomKeys, sender: PeerId, nonce: &str, ct: &str) -> Option<(RoomEnvelope, [u8; 24])> {
    let nonce: [u8; 24] = from_hex(nonce)?.try_into().ok()?;
    let ct = from_hex(ct)?;
    let aad = aad(&keys.id, sender);
    let plain =
        XChaCha20Poly1305::new(&keys.enc.into()).decrypt(&XNonce::from(nonce), Payload { msg: &ct, aad: &aad }).ok()?;
    Some((serde_json::from_slice(&plain).ok()?, nonce))
}

/// Remembers recent nonces for one room and rejects repeats and stale messages.
#[derive(Default)]
pub struct ReplayGuard {
    seen: HashSet<[u8; 24]>,
    order: VecDeque<[u8; 24]>,
}

impl ReplayGuard {
    /// True if this message is fresh; records its nonce.
    pub fn accept(&mut self, nonce: [u8; 24], ts: u64, now: u64) -> bool {
        if ts.abs_diff(now) > MAX_SKEW_MS || self.seen.contains(&nonce) {
            return false;
        }
        if self.order.len() == REPLAY_WINDOW
            && let Some(old) = self.order.pop_front()
        {
            self.seen.remove(&old);
        }
        self.order.push_back(nonce);
        self.seen.insert(nonce);
        true
    }
}

pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn from_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| s.get(i..i + 2).and_then(|pair| u8::from_str_radix(pair, 16).ok())).collect()
}

pub fn parse_room_id(s: &str) -> Option<RoomId> {
    from_hex(s)?.try_into().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;

    const ALICE: PeerId = PeerId::test(1);
    const BOB: PeerId = PeerId::test(2);

    /// Argon2 is slow, so derive the shared test room once.
    fn keys() -> &'static RoomKeys {
        static KEYS: OnceLock<RoomKeys> = OnceLock::new();
        KEYS.get_or_init(|| derive("catpie", "k7m9xq"))
    }

    fn chat(text: &str, ts: u64) -> RoomEnvelope {
        RoomEnvelope { ts, body: RoomBody::Chat { text: text.into() } }
    }

    fn parts(frame: &Frame) -> (&str, &str, &str) {
        match frame {
            Frame::Room { room_id, nonce, ct } => (room_id, nonce, ct),
            other => panic!("expected a room frame, got {other:?}"),
        }
    }

    #[test]
    fn keys_are_six_system_chars() {
        let key = make_key();
        assert_eq!(key.len(), SYS_LEN);
        assert!(key.bytes().all(|b| SYS_ALPHABET.contains(&b)));
        assert!(validate_key(&key).is_ok());
        assert_ne!(make_key(), make_key());
    }

    #[test]
    fn key_validation() {
        assert!(validate_key("k7m9xq").is_ok());
        assert!(validate_key("123-k7m9xq").is_err(), "the old two-part form");
        assert!(validate_key("k7m9x").is_err(), "too short");
        assert!(validate_key("k7m9xqq").is_err(), "too long");
        assert!(validate_key("k7m9x0").is_err(), "'0' isn't in the alphabet");
        assert!(validate_key("").is_err());
    }

    #[test]
    fn door_and_room_keys_differ_and_secret_derivation_is_deterministic() {
        let secret = [7u8; 32];
        let room = from_secret("catpie", &secret);
        assert_eq!(room.id, from_secret("catpie", &secret).id);
        assert_ne!(room.id, from_secret("dogpie", &secret).id);
        assert_ne!(room.id, from_secret("catpie", &[8; 32]).id);
        assert_ne!(room.id, keys().id, "door and room ids never coincide");
        assert_ne!(new_secret(), new_secret());
    }

    #[test]
    fn gate_bodies_round_trip() {
        let bodies = [
            RoomBody::Knock { nonce: "ab".into(), asked: 5 },
            RoomBody::Door { echo: "ab".into(), members: 3, max: 13 },
            RoomBody::Grant { secret: to_hex(&[1; 32]), max: 4 },
            RoomBody::Refuse { full: true },
            RoomBody::Vote { peer: BOB, asked: 5, yes: false },
        ];
        for body in bodies {
            let env = RoomEnvelope { ts: 1, body };
            let json = serde_json::to_string(&env).unwrap();
            assert_eq!(serde_json::from_str::<RoomEnvelope>(&json).unwrap(), env);
        }
    }

    #[test]
    fn derivation_is_deterministic_and_name_and_key_both_matter() {
        assert_eq!(to_hex(&keys().id), to_hex(&derive("catpie", "k7m9xq").id));
        assert_ne!(keys().id, derive("dogpie", "k7m9xq").id);
        assert_ne!(keys().id, derive("catpie", "k7m9xr").id);
    }

    #[test]
    fn sealed_frames_open_for_members() {
        let env = chat("hello room", 1000);
        let frame = seal(keys(), ALICE, &env);
        let (room_id, nonce, ct) = parts(&frame);
        assert_eq!(parse_room_id(room_id), Some(keys().id));
        assert!(!ct.contains(&to_hex(b"hello room")));
        let (opened, _) = open(keys(), ALICE, nonce, ct).unwrap();
        assert_eq!(opened, env);
    }

    #[test]
    fn tampering_relabelling_or_the_wrong_key_fail_to_open() {
        let frame = seal(keys(), ALICE, &chat("hi", 1000));
        let (_, nonce, ct) = parts(&frame);
        assert!(open(keys(), BOB, nonce, ct).is_none(), "relabelled sender");
        let other = derive("catpie", "k7m9xr");
        assert!(open(&other, ALICE, nonce, ct).is_none(), "wrong key");
        let mut flipped = ct.to_string();
        let last = if flipped.ends_with('0') { '1' } else { '0' };
        flipped.pop();
        flipped.push(last);
        assert!(open(keys(), ALICE, nonce, &flipped).is_none(), "tampered ciphertext");
        assert!(open(keys(), ALICE, "zz", ct).is_none(), "bad nonce hex");
    }

    #[test]
    fn replay_guard_rejects_repeats_and_stale_messages() {
        let mut guard = ReplayGuard::default();
        let now = 10 * MAX_SKEW_MS;
        assert!(guard.accept([1; 24], now, now));
        assert!(!guard.accept([1; 24], now, now), "same nonce twice");
        assert!(!guard.accept([2; 24], now - MAX_SKEW_MS - 1, now), "too old");
        assert!(!guard.accept([3; 24], now + MAX_SKEW_MS + 1, now), "too far ahead");
        for i in 0..REPLAY_WINDOW as u32 {
            let mut nonce = [0u8; 24];
            nonce[..4].copy_from_slice(&i.to_be_bytes());
            nonce[23] = 9;
            assert!(guard.accept(nonce, now, now));
        }
        assert!(guard.accept([1; 24], now, now), "forgotten once it leaves the window");
    }

    #[test]
    fn hex_round_trips_and_rejects_junk() {
        assert_eq!(to_hex(&[0x00, 0xab, 0xff]), "00abff");
        assert_eq!(from_hex("00abff"), Some(vec![0x00, 0xab, 0xff]));
        assert_eq!(from_hex("abc"), None);
        assert_eq!(from_hex("zz"), None);
        assert_eq!(parse_room_id("00"), None);
    }
}
