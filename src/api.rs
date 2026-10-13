//! The boundary between the networking core (`node`) and any frontend.
//! A frontend sends `Command`s and renders `Event`s; it never touches sockets.

use std::net::SocketAddr;

pub use crate::protocol::PeerId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Room message to every connected peer.
    Say(String),
    /// Private message; `to` is a nick or `nick#abcd`.
    Direct { to: String, text: String },
    SetNick(String),
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
    /// Display name (`nick` or `nick#abcd`).
    pub nick: String,
    pub addr: SocketAddr,
}

/// All nick fields are display names (`nick`, or `nick#abcd` on a clash).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    PeerJoined { id: PeerId, nick: String },
    PeerLeft { id: PeerId, nick: String, reason: LeaveReason },
    NickChanged { id: PeerId, old: String, new: String },
    Message { from_id: PeerId, from_nick: String, text: String, direct: bool, ts: u64 },
    /// Confirms a DM we sent, so the UI can echo it.
    DirectSent { to: String, text: String },
    Peers(Vec<PeerInfo>),
    /// User-facing problem, e.g. unknown `/msg` target.
    Error(String),
    /// User-facing notice, e.g. startup banner.
    Info(String),
}
