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
    Send {
        to: Target,
        text: String,
    },
    /// A room name from `names::pick_name` (the UI previews it); the node makes the key.
    /// `max` counts everyone, the creator included.
    CreateRoom {
        name: String,
        max: u8,
    },
    JoinRoom {
        name: String,
        key: String,
    },
    LeaveRoom {
        name: String,
    },
    /// Let `nick` into `room`, which they've asked to join.
    Approve {
        room: String,
        nick: String,
    },
    /// Refuse `nick`'s request to join `room`.
    Deny {
        room: String,
        nick: String,
    },
    /// Our vote to remove `nick` from `room`.
    Kick {
        room: String,
        nick: String,
    },
    /// While picking our name: show the name `base` would get.
    PreviewName {
        base: String,
    },
    /// While picking our name: keep the last preview.
    ConfirmName,
    /// Invite `nick`, who's online, to `room`, one of ours.
    Invite {
        nick: String,
        room: String,
    },
    /// Join `room` with the key its invitation carried.
    Accept {
        room: String,
    },
    /// Turn down the invitation to `room`.
    Decline {
        room: String,
    },
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

/// Why our request to join a room ended without getting in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JoinEnd {
    /// Nobody answered: a wrong key, or nobody in the room is online.
    NoRoom,
    Full,
    Denied {
        by: String,
    },
    /// Nobody let us in within `PENDING_FOR`.
    TimedOut,
    /// Kicked from this room twice.
    Banned,
}

/// Why someone's request to join one of our rooms is closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestEnd {
    /// Let in; a `MemberJoined` follows.
    Admitted,
    Denied {
        by: String,
    },
    Full,
    /// They gave up, went offline, or never arrived.
    Gone,
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
    Assigned {
        nick: String,
        renamed: bool,
    },
    /// The name we'd get if the user keeps it (only when the app asks).
    NamePreview {
        nick: String,
    },
    /// `from` invites us to `room`; `/accept` or `/decline` answers.
    Invited {
        from: String,
        room: String,
    },
    /// `by` turned down our invitation to `room`.
    InviteDeclined {
        by: String,
        room: String,
    },
    PeerJoined {
        id: PeerId,
        nick: String,
    },
    PeerLeft {
        id: PeerId,
        nick: String,
        reason: LeaveReason,
    },
    /// A peer re-rolled its name after a same-moment clash.
    Renamed {
        old: String,
        new: String,
    },
    /// `ts` is when we received it (unix millis).
    Message {
        conv: Conv,
        from: String,
        text: String,
        ts: u64,
    },
    /// Something we sent, so the conversation store can record it. A DM has an
    /// `id`, and a `Delivered` or `NotDelivered` with it follows; a room message has none.
    Sent {
        conv: Conv,
        text: String,
        ts: u64,
        id: Option<u64>,
    },
    /// The other app got our DM `id`.
    Delivered {
        id: u64,
    },
    /// These DMs got no ack within `ACK_WAIT`, or their peer went away first. Sorted.
    NotDelivered {
        ids: Vec<u64>,
    },
    RoomCreated {
        name: String,
        key: String,
        max: u8,
    },
    RoomJoined {
        name: String,
        key: String,
        max: u8,
        members: Vec<String>,
    },
    /// Someone in the room answered our knock; we wait for `needs` approvals.
    JoinWaiting {
        name: String,
        members: u8,
        needs: u8,
    },
    JoinEnded {
        name: String,
        why: JoinEnd,
    },
    /// `nick` asked to join `room`; `have` of `need` approvals so far.
    JoinRequest {
        room: String,
        nick: String,
        have: u8,
        need: u8,
    },
    /// `by` approved `nick`'s request.
    Approved {
        room: String,
        nick: String,
        by: String,
        have: u8,
        need: u8,
    },
    RequestEnded {
        room: String,
        nick: String,
        why: RequestEnd,
    },
    RoomLeft {
        name: String,
    },
    MemberJoined {
        room: String,
        nick: String,
    },
    MemberLeft {
        room: String,
        nick: String,
    },
    /// `room` moved to a new key because someone left or was removed.
    Rekeyed {
        room: String,
    },
    /// `by` voted to remove `nick` from `room`; `have` of `need` votes so far.
    KickVote {
        room: String,
        nick: String,
        by: String,
        have: u8,
        need: u8,
    },
    /// `nick` was voted out of `room`.
    MemberRemoved {
        room: String,
        nick: String,
    },
    /// We were voted out of `room`; it's closed as if we'd left.
    Removed {
        room: String,
    },
    /// `you`/`nick` are ours, so `/who` can show our fingerprint.
    Peers {
        you: PeerId,
        nick: String,
        peers: Vec<PeerInfo>,
    },
    /// User-facing problem, e.g. a wrong room key.
    Error(String),
    /// User-facing notice.
    Info(String),
}
