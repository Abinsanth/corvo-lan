//! Kicking by vote, and bans. `/kick @nick` is one vote. At `needed(n)` live
//! votes from members the person is out, which starts a re-key. Each room
//! counts kicks per peer, and at `BAN_AFTER` their knocks are refused.

use std::time::Duration;

use tokio::time::Instant;

use super::{Room, RoomId, needed, small};
use crate::api::Event;
use crate::node::Node;
use crate::protocol::PeerId;
use crate::room::RoomBody;

/// A kick vote counts for this long.
pub const KICK_VOTE_FOR: Duration = Duration::from_secs(120);
/// Kicks after which someone can't get back into the room.
pub const BAN_AFTER: u8 = 2;

impl Room {
    pub(super) fn banned(&self, peer: PeerId) -> bool {
        self.kicks.get(&peer).is_some_and(|n| *n >= BAN_AFTER)
    }

    /// Live votes to kick `target`, from us or members still here, never from `target`.
    fn kick_votes_for(&self, target: PeerId, me: PeerId, now: Instant) -> usize {
        self.kick_votes.get(&target).map_or(0, |votes| {
            votes
                .iter()
                .filter(|(voter, _)| **voter != target && (**voter == me || self.members.contains_key(voter)))
                .filter(|(_, at)| now.saturating_duration_since(**at) < KICK_VOTE_FOR)
                .count()
        })
    }
}

impl Node {
    /// `/kick @nick` in room `name`: our vote to remove them.
    pub(in crate::node) fn kick(&mut self, name: &str, nick: &str) {
        let Some(id) = self.rooms.id_of(name) else {
            return self.emit(Event::Error(format!("you're not in #{name}")));
        };
        if nick == self.my_nick() {
            return self.emit(Event::Error("you can't kick yourself — /leave instead".into()));
        }
        let room = &self.rooms.joined[&id];
        let Some(target) = self.peer_by_nick(nick).filter(|p| room.members.contains_key(p)) else {
            return self.emit(Event::Error(format!("@{nick} isn't in #{name}")));
        };
        if room.kick_votes.get(&target).is_some_and(|votes| votes.contains_key(&self.id)) {
            return self.emit(Event::Error(format!("you already voted to kick @{nick}")));
        }
        self.broadcast(self.seal(&room.keys, RoomBody::Kick { peer: target }));
        self.on_kick_vote(id, self.id, target, Instant::now());
    }

    /// `voter` (us, or a proven member) wants `target` out of room `id`. A
    /// vote can't be repeated or taken back. Votes about us are tallied too,
    /// so we can tell the user once they're out.
    pub(super) fn on_kick_vote(&mut self, id: RoomId, voter: PeerId, target: PeerId, now: Instant) {
        let me = self.id;
        let Some(room) = self.rooms.joined.get_mut(&id) else {
            return;
        };
        let in_room = target == me || room.members.contains_key(&target);
        if voter == target || !in_room {
            return;
        }
        let votes = room.kick_votes.entry(target).or_default();
        if votes.contains_key(&voter) {
            return;
        }
        votes.insert(voter, now);
        let have = room.kick_votes_for(target, me, now);
        let need = needed(room.size());
        let name = room.name.clone();
        if have < need {
            let by = if voter == me { Some(self.my_nick()) } else { self.nick_of(voter) };
            if target != me
                && let (Some(nick), Some(by)) = (self.nick_of(target), by)
            {
                self.emit(Event::KickVote { room: name, nick, by, have: small(have), need: small(need) });
            }
            return;
        }
        room.kick_votes.remove(&target);
        let kicks = room.kicks.entry(target).or_default();
        *kicks = kicks.saturating_add(1);
        if target == me {
            self.removed(id);
        } else if self.forget_member(id, target)
            && let Some(nick) = self.nick_of(target)
        {
            self.emit(Event::MemberRemoved { room: name, nick });
        }
    }

    /// We were voted out of room `id`: close it as if we'd left.
    fn removed(&mut self, id: RoomId) {
        if let Some(room) = self.rooms.joined.remove(&id) {
            self.rooms.doors.remove(&room.door.id);
            self.rooms.old_ids.retain(|_, current| *current != id);
            self.emit(Event::Removed { room: room.name });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::*;
    use super::*;
    use crate::api::{Command, Event, JoinEnd};
    use crate::room::RoomBody;

    fn kick(h: &mut Harness, made: &Made, nick: &str) {
        h.node.handle_command(Command::Kick { room: made.name.clone(), nick: nick.into() });
    }

    fn kick_vote(h: &mut Harness, from: PeerId, made: &Made, peer: PeerId) {
        let keys = keys_now(h, &made.name);
        deliver(h, from, frame_from(from, &keys, RoomBody::Kick { peer }));
    }

    fn vote_event(made: &Made, by: &str, have: u8, need: u8) -> Event {
        Event::KickVote { room: made.name.clone(), nick: "bobroger".into(), by: by.into(), have, need }
    }

    #[tokio::test(start_paused = true)]
    async fn one_vote_is_enough_in_a_small_room() {
        let mut h = with_peers();
        let made = room_with(&mut h, &[BOB]).await;
        kick(&mut h, &made, "bobroger");
        assert!(bodies(&made.keys, &h.sent(BOB)).contains(&RoomBody::Kick { peer: BOB }), "everyone sees the vote");
        assert_eq!(h.events(), vec![Event::MemberRemoved { room: made.name.clone(), nick: "bobroger".into() }]);
        assert_eq!(room_of(&h, &made.name).kicks.get(&BOB), Some(&1));
        h.pump().await; // RekeyDue
        assert_eq!(h.events(), vec![Event::Rekeyed { room: made.name.clone() }]);
    }

    #[tokio::test]
    async fn four_members_need_two_votes() {
        let mut h = with_peers();
        let made = room_with(&mut h, &[BOB, CAROL, DAVE]).await;
        kick(&mut h, &made, "bobroger");
        assert_eq!(h.events(), vec![vote_event(&made, "annmoon", 1, 2)]);
        kick_vote(&mut h, CAROL, &made, BOB);
        assert_eq!(h.events(), vec![Event::MemberRemoved { room: made.name.clone(), nick: "bobroger".into() }]);
    }

    #[tokio::test]
    async fn seven_members_need_three_votes() {
        let mut h = with_peers();
        h.connect(FRED, "fredelk", true);
        h.connect(GWEN, "gwenfox", true);
        h.events(); // their PeerJoineds
        let made = room_with(&mut h, &[BOB, CAROL, DAVE, ERIN, FRED, GWEN]).await;
        kick(&mut h, &made, "bobroger");
        assert_eq!(h.events(), vec![vote_event(&made, "annmoon", 1, 3)]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_vote_expires_after_two_minutes() {
        let mut h = with_peers();
        let made = room_with(&mut h, &[BOB, CAROL, DAVE]).await;
        kick(&mut h, &made, "bobroger");
        h.events();
        tokio::time::advance(KICK_VOTE_FOR).await;
        kick_vote(&mut h, CAROL, &made, BOB);
        assert_eq!(h.events(), vec![vote_event(&made, "carolwolf", 1, 2)], "ours had run out");
    }

    #[tokio::test]
    async fn repeat_votes_and_votes_about_yourself_dont_count() {
        let mut h = with_peers();
        let made = room_with(&mut h, &[BOB, CAROL, DAVE]).await;
        kick_vote(&mut h, CAROL, &made, BOB);
        assert_eq!(h.events(), vec![vote_event(&made, "carolwolf", 1, 2)]);
        kick_vote(&mut h, CAROL, &made, BOB);
        kick_vote(&mut h, BOB, &made, BOB);
        assert!(h.events().is_empty());
        kick(&mut h, &made, "bobroger");
        assert_eq!(h.events(), vec![Event::MemberRemoved { room: made.name.clone(), nick: "bobroger".into() }]);
    }

    #[tokio::test]
    async fn kick_input_is_checked() {
        let mut h = with_peers();
        let made = room_with(&mut h, &[BOB, CAROL, DAVE]).await;
        kick(&mut h, &made, "bobroger");
        h.events();
        kick(&mut h, &made, "bobroger");
        kick(&mut h, &made, "annmoon");
        kick(&mut h, &made, "erinowl");
        h.node.handle_command(Command::Kick { room: "nopie".into(), nick: "bobroger".into() });
        assert_eq!(
            h.events(),
            vec![
                Event::Error("you already voted to kick @bobroger".into()),
                Event::Error("you can't kick yourself — /leave instead".into()),
                Event::Error(format!("@erinowl isn't in #{}", made.name)),
                Event::Error("you're not in #nopie".into()),
            ]
        );
    }

    #[tokio::test]
    async fn votes_from_non_members_are_ignored() {
        let mut h = with_peers();
        let made = room_with(&mut h, &[BOB]).await;
        kick_vote(&mut h, DAVE, &made, BOB); // dave holds the key but isn't a proven member
        assert!(!h.events().iter().any(|e| matches!(e, Event::MemberRemoved { .. })));
        assert!(room_of(&h, &made.name).members.contains_key(&BOB));
    }

    #[tokio::test(start_paused = true)]
    async fn votes_are_forgotten_when_their_target_leaves() {
        let mut h = with_peers();
        let made = room_with(&mut h, &[BOB, CAROL, DAVE]).await;
        kick(&mut h, &made, "bobroger");
        deliver(&mut h, BOB, frame_from(BOB, &made.keys, RoomBody::Leave));
        assert!(!room_of(&h, &made.name).kick_votes.contains_key(&BOB));
    }

    #[tokio::test]
    async fn the_kicked_person_is_told_and_the_room_closes() {
        let mut h = with_peers();
        let made = room_with(&mut h, &[CAROL]).await;
        kick_vote(&mut h, CAROL, &made, ME);
        assert_eq!(h.events(), vec![Event::Removed { room: made.name.clone() }]);
        assert!(h.node.rooms.id_of(&made.name).is_none());
    }

    #[tokio::test]
    async fn kick_counts_merge_from_rekeys() {
        let mut h = with_peers();
        let made = room_with(&mut h, &[BOB, CAROL]).await;
        let rekey =
            RoomBody::Rekey { epoch: 1, by: CAROL, secret: crate::room::to_hex(&[6; 32]), kicks: vec![(DAVE, 1)] };
        deliver(&mut h, CAROL, frame_from(CAROL, &made.keys, rekey));
        assert_eq!(room_of(&h, &made.name).kicks.get(&DAVE), Some(&1));
        let keys = keys_now(&h, &made.name);
        let rekey = RoomBody::Rekey { epoch: 2, by: CAROL, secret: crate::room::to_hex(&[7; 32]), kicks: vec![] };
        deliver(&mut h, CAROL, frame_from(CAROL, &keys, rekey));
        assert_eq!(room_of(&h, &made.name).kicks.get(&DAVE), Some(&1), "the higher count stays");
    }

    #[tokio::test(start_paused = true)]
    async fn a_second_kick_bans() {
        let mut h = with_peers();
        let made = room_with(&mut h, &[BOB]).await;
        kick(&mut h, &made, "bobroger");
        h.pump().await; // RekeyDue
        h.events();
        // Kicked once: he may ask again, and is let in as usual.
        knock(&mut h, BOB, &made, 100);
        assert!(matches!(h.events().as_slice(), [Event::JoinRequest { .. }]));
        approve(&mut h, &made, "bobroger");
        h.pump().await; // Settle
        let keys = keys_now(&h, &made.name);
        verify(&mut h, BOB, &made.name, &keys);
        h.events();
        kick(&mut h, &made, "bobroger");
        h.pump().await; // RekeyDue
        assert_eq!(room_of(&h, &made.name).kicks.get(&BOB), Some(&2));
        h.events();
        h.sent(BOB);
        // Kicked twice: every knock is refused, with no request shown.
        for asked in [200, 300] {
            knock(&mut h, BOB, &made, asked);
            assert_eq!(bodies(&made.door, &h.sent(BOB)), vec![RoomBody::Refuse { full: false, banned: true }]);
        }
        assert!(h.events().is_empty());
        h.node.handle_command(Command::Invite { nick: "bobroger".into(), room: made.name.clone() });
        assert_eq!(h.events(), vec![Event::Error(format!("@bobroger is banned from #{}", made.name))]);
        assert!(h.sent(BOB).is_empty(), "no invite sent");
    }

    #[tokio::test]
    async fn a_banned_peer_is_never_proven_or_voted_on() {
        let mut h = with_peers();
        let made = room_with(&mut h, &[CAROL]).await;
        h.node.rooms.joined.get_mut(&made.id).unwrap().kicks.insert(BOB, BAN_AFTER);
        vote(&mut h, CAROL, &made, BOB, true);
        assert!(h.events().is_empty(), "no request opened for him");
        verify(&mut h, BOB, &made.name, &made.keys); // he still holds the key
        assert!(!room_of(&h, &made.name).members.contains_key(&BOB));
        assert!(h.events().is_empty());
    }

    #[tokio::test]
    async fn a_kick_count_at_its_limit_does_not_overflow() {
        let mut h = with_peers();
        let made = room_with(&mut h, &[BOB]).await;
        h.node.rooms.joined.get_mut(&made.id).unwrap().kicks.insert(BOB, u8::MAX); // as a peer's counts could claim
        kick(&mut h, &made, "bobroger");
        assert_eq!(room_of(&h, &made.name).kicks.get(&BOB), Some(&u8::MAX));
    }

    #[tokio::test(start_paused = true)]
    async fn a_banned_newcomer_is_told() {
        let mut h = with_peers();
        let (nonce, _) = start_join(&mut h).await;
        door_says(&mut h, BOB, &nonce, 2, 13);
        h.events();
        deliver(&mut h, BOB, frame_from(BOB, &door(), RoomBody::Refuse { full: false, banned: true }));
        assert_eq!(h.events(), vec![Event::JoinEnded { name: NAME.into(), why: JoinEnd::Banned }]);
    }
}
