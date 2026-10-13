//! The boundary between the networking core (`node`) and any frontend.
//! A frontend sends `Command`s and renders `Event`s; it never touches sockets.
//! Names here never carry the `@`/`#` sigils.

use std::net::SocketAddr;

pub use crate::protocol::PeerId;

/// Where a message goes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Target {
    Dm(String),
    Room(String),
}

/// Which conversation a message belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Conv {
    Dm(String),
    Room(String),
}

impl From<Target> for Conv {
    fn from(t: Target) -> Conv {
        match t {
            Target::Dm(n) => Conv::Dm(n),
            Target::Room(n) => Conv::Room(n),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Send { to: Target, text: String },
    /// `base` and `key_part` are what the user typed; the node adds the rest.
    CreateRoom { base: String, key_part: String },
    JoinRoom { name: String, key: String },
    LeaveRoom { name: String },
    ListPeers,
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaveReason {
    /// The peer sent `Bye`.
    Quit,
    /// The connection dropped or went silent.
    Lost,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerInfo {
    pub id: PeerId,
    pub nick: String,
    pub addr: SocketAddr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// Our system-assigned name. `renamed` means a same-moment clash forced a new one.
    Assigned { nick: String, renamed: bool },
    PeerJoined { id: PeerId, nick: String },
    PeerLeft { id: PeerId, nick: String, reason: LeaveReason },
    /// A peer re-rolled its name after a same-moment clash.
    Renamed { old: String, new: String },
    /// `ts` is when we received it (unix millis).
    Message { conv: Conv, from: String, text: String, ts: u64 },
    /// Something we sent, so the conversation store can record it.
    Sent { conv: Conv, text: String, ts: u64 },
    RoomCreated { name: String, key: String },
    RoomJoined { name: String, members: Vec<String> },
    RoomLeft { name: String },
    MemberJoined { room: String, nick: String },
    MemberLeft { room: String, nick: String },
    Peers(Vec<PeerInfo>),
    /// User-facing problem, e.g. a wrong room key.
    Error(String),
    /// User-facing notice.
    Info(String),
}
