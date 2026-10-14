//! Re-keying. When anyone stops being a member, the members left move to a
//! new secret, so whoever left can neither read on nor answer a probe to get
//! back in. Removals within `REKEY_SETTLE` share one re-key; then the lowest
//! id among those left makes the secret and hands it to each member with
//! `send_to`, never broadcast, since the leaver still holds the old key. When
//! two re-keys cross, the higher epoch wins, then the lower maker.

use std::time::Duration;

use tokio::time::Instant;

use super::{HERE_INTERVAL, RoomId, skewed};
use crate::api::Event;
use crate::node::{Node, NodeMsg};
use crate::protocol::{PeerId, now_millis};
use crate::room::{self, RoomBody, RoomKeys, Secret};

/// Removals this close together share one re-key.
pub const REKEY_SETTLE: Duration = Duration::from_secs(1);
/// If no new key has come from the lowest id by then, make one ourselves.
pub const REKEY_BACKUP: Duration = Duration::from_secs(5);
/// How long a replaced key still opens frames: messages already on their way,
/// and members (or a newcomer) who missed the switch.
pub const OLD_KEY_FOR: Duration = Duration::from_secs(30);

/// A key the room used before a re-key.
pub(super) struct OldKey {
    pub(super) keys: RoomKeys,
    pub(super) until: Instant,
}

impl Node {
    /// Someone stopped being a member of room `id`: re-key once the wait is over.
    pub(super) fn schedule_rekey(&mut self, id: RoomId) {
        let Some(room) = self.rooms.joined.get_mut(&id) else {
            return;
        };
        if room.rekey_pending {
            return;
        }
        room.rekey_pending = true;
        let name = room.name.clone();
        self.after(REKEY_SETTLE, NodeMsg::RekeyDue(name));
    }

    /// The wait for room `name` is over. The lowest id re-keys; the rest wait
    /// for its key and make one themselves after `REKEY_BACKUP`.
    pub(in crate::node) fn on_rekey_due(&mut self, name: &str, now: Instant) {
        let me = self.id;
        let Some(id) = self.rooms.id_of(name) else {
            return;
        };
        let room = self.rooms.joined.get_mut(&id).expect("found above");
        if !room.rekey_pending {
            return;
        }
        room.rekey_pending = false;
        if room.members.keys().all(|m| me < *m) {
            self.rekey(id);
        } else {
            room.rekey_backup = Some((now + REKEY_BACKUP, room.epoch));
        }
    }

    /// Makes a new secret, hands it to each member directly, and moves to it.
    pub(super) fn rekey(&mut self, id: RoomId) {
        let me = self.id;
        let Some(room) = self.rooms.joined.get(&id) else {
            return;
        };
        let secret = room::new_secret();
        let epoch = room.epoch + 1;
        let body = RoomBody::Rekey { epoch, by: me, secret: room::to_hex(&secret), kicks: room.kick_list() };
        let frame = self.seal(&room.keys, body);
        for member in room.members.keys() {
            self.send_to(*member, frame.clone());
        }
        self.switch_key(id, secret, epoch, me);
    }

    /// A member's `Rekey`: taken if its epoch is newer, or the same epoch made
    /// by a lower id than our current key's maker.
    pub(super) fn on_rekey(&mut self, id: RoomId, epoch: u32, by: PeerId, secret: &str, kicks: &[(PeerId, u8)]) {
        let Some(secret) = room::from_hex(secret).and_then(|s| Secret::try_from(s).ok()) else {
            return;
        };
        let Some(room) = self.rooms.joined.get_mut(&id) else {
            return;
        };
        if epoch < room.epoch || (epoch == room.epoch && by >= room.keyed_by) {
            return;
        }
        room.merge_kicks(kicks);
        self.switch_key(id, secret, epoch, by);
    }

    /// A frame under one of room `current`'s old keys (old room id `old`). A
    /// member still on it is sent the new key; a newcomer we let in with it is
    /// answered and sent the new key; anyone else, whoever left or was kicked
    /// included, is ignored.
    pub(super) fn on_old_key_frame(
        &mut self,
        current: RoomId,
        old: RoomId,
        from: PeerId,
        nonce: &str,
        ct: &str,
        now: Instant,
    ) {
        let Some(room) = self.rooms.joined.get_mut(&current) else {
            return;
        };
        let Some(old_keys) = room.previous.iter().find(|k| k.keys.id == old).map(|k| k.keys.clone()) else {
            return;
        };
        let Some((env, nonce)) = room::open(&old_keys, from, nonce, ct) else {
            return;
        };
        if skewed(env.ts) || !room.replay.accept(nonce, env.ts, now_millis()) {
            return;
        }
        if room.members.contains_key(&from) {
            if !matches!(env.body, RoomBody::Leave) {
                self.resend_key(current, from, &old_keys, now);
            }
            self.on_member_envelope(current, from, env, now);
            return;
        }
        let admitted = room.requests.get(&from).is_some_and(|r| r.admitted.is_some());
        if let (true, RoomBody::Probe { nonce }) = (admitted, env.body) {
            let reply = RoomBody::Here { name: room.name.clone(), echo: Some(nonce) };
            self.send_to(from, self.seal(&old_keys, reply));
            self.resend_key(current, from, &old_keys, now);
        }
    }

    /// Sends `to` the room's current secret, sealed with the `old` keys they
    /// still use; at most once per `HERE_INTERVAL` each.
    fn resend_key(&mut self, id: RoomId, to: PeerId, old: &RoomKeys, now: Instant) {
        let Some(room) = self.rooms.joined.get_mut(&id) else {
            return;
        };
        if room.resent.get(&to).is_some_and(|at| now.saturating_duration_since(*at) < HERE_INTERVAL) {
            return;
        }
        room.resent.insert(to, now);
        let body = RoomBody::Rekey {
            epoch: room.epoch,
            by: room.keyed_by,
            secret: room::to_hex(&room.secret),
            kicks: room.kick_list(),
        };
        self.send_to(to, self.seal(old, body));
    }

    /// Moves room `id` to `secret`. Everything but the keys and the id carries
    /// over; the old keys still open frames for `OLD_KEY_FOR`.
    fn switch_key(&mut self, id: RoomId, secret: Secret, epoch: u32, by: PeerId) {
        let Some(mut room) = self.rooms.joined.remove(&id) else {
            return;
        };
        let keys = room::from_secret(&room.name, &secret);
        let new_id = keys.id;
        let old = std::mem::replace(&mut room.keys, keys);
        room.previous.push(OldKey { keys: old, until: Instant::now() + OLD_KEY_FOR });
        (room.secret, room.epoch, room.keyed_by) = (secret, epoch, by);
        // A running timer stays: it may be for a removal this key doesn't cover yet.
        room.rekey_backup = None;
        room.challenges.clear();
        // Lets a newcomer who just switched be challenged, and so proven, under the new key.
        let here = self.seal(&room.keys, RoomBody::Here { name: room.name.clone(), echo: None });
        self.rooms.doors.insert(room.door.id, new_id);
        for current in self.rooms.old_ids.values_mut() {
            if *current == id {
                *current = new_id;
            }
        }
        self.rooms.old_ids.insert(id, new_id);
        self.rooms.invited = std::mem::take(&mut self.rooms.invited)
            .into_iter()
            .map(|((r, peer), at)| ((if r == id { new_id } else { r }, peer), at))
            .collect();
        let name = room.name.clone();
        self.rooms.joined.insert(new_id, room);
        self.broadcast(here);
        self.emit(Event::Rekeyed { room: name });
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::*;
    use super::*;
    use crate::api::{Command, Event};
    use crate::room::{self, RoomBody};

    #[tokio::test(start_paused = true)]
    async fn a_leave_rekeys_and_only_the_members_left_get_the_key() {
        let mut h = with_peers();
        let made = room_with(&mut h, &[BOB, CAROL]).await;
        deliver(&mut h, BOB, frame_from(BOB, &made.keys, RoomBody::Leave));
        h.events();
        h.pump().await; // RekeyDue
        let to_carol = rekeys(&made.keys, &h.sent(CAROL));
        assert_eq!(to_carol.len(), 1);
        let (epoch, by, secret) = to_carol[0];
        assert_eq!((epoch, by), (1, ME));
        assert!(rekeys(&made.keys, &h.sent(BOB)).is_empty(), "the leaver gets nothing");
        let now = keys_now(&h, &made.name);
        assert_eq!(now.id, room::from_secret(&made.name, &secret).id);
        assert_ne!(now.id, made.keys.id);
        assert_eq!(h.events(), vec![Event::Rekeyed { room: made.name.clone() }]);
    }

    #[tokio::test(start_paused = true)]
    async fn removals_close_together_share_one_rekey() {
        let mut h = with_peers();
        let made = room_with(&mut h, &[BOB, CAROL, DAVE]).await;
        deliver(&mut h, BOB, frame_from(BOB, &made.keys, RoomBody::Leave));
        deliver(&mut h, CAROL, frame_from(CAROL, &made.keys, RoomBody::Leave));
        h.events();
        h.pump().await; // RekeyDue: the only one scheduled
        assert_eq!(rekeys(&made.keys, &h.sent(DAVE)).len(), 1);
        tokio::time::sleep(REKEY_SETTLE * 2).await;
        while let Ok(msg) = h.msgs.try_recv() {
            h.node.handle_msg(msg);
        }
        assert_eq!(room_of(&h, &made.name).epoch, 1, "one re-key for both");
    }

    #[tokio::test(start_paused = true)]
    async fn a_disconnect_or_silence_also_rekeys() {
        let mut h = with_peers();
        let made = room_with(&mut h, &[BOB, CAROL]).await;
        let conn = h.node.peers[&BOB].conn;
        h.node.handle_msg(crate::node::NodeMsg::PeerGone { conn, id: BOB, reason: crate::api::LeaveReason::Lost });
        h.pump().await; // RekeyDue
        assert_eq!(room_of(&h, &made.name).epoch, 1);
        h.node.on_tick(tokio::time::Instant::now() + super::super::MEMBER_TIMEOUT); // carol went quiet
        h.pump().await; // RekeyDue
        assert_eq!(room_of(&h, &made.name).epoch, 2);
    }

    #[tokio::test(start_paused = true)]
    async fn the_last_one_left_rekeys_alone() {
        let mut h = with_peers();
        let made = room_with(&mut h, &[BOB]).await;
        deliver(&mut h, BOB, frame_from(BOB, &made.keys, RoomBody::Leave));
        h.events();
        h.pump().await; // RekeyDue
        assert_eq!(h.events(), vec![Event::Rekeyed { room: made.name.clone() }]);
        assert_ne!(keys_now(&h, &made.name).id, made.keys.id);
    }

    #[tokio::test(start_paused = true)]
    async fn only_the_lowest_id_rekeys_and_the_rest_back_it_up() {
        let mut h = with_peers();
        h.connect(ZED, "zedfox", true);
        h.events(); // its PeerJoined
        let made = room_with(&mut h, &[ZED, BOB]).await;
        deliver(&mut h, BOB, frame_from(BOB, &made.keys, RoomBody::Leave));
        h.pump().await; // RekeyDue: zed is lower, so we wait for it
        assert!(rekeys(&made.keys, &h.sent(ZED)).is_empty());
        assert_eq!(room_of(&h, &made.name).epoch, 0);
        h.node.on_tick(tokio::time::Instant::now() + REKEY_BACKUP);
        assert_eq!(rekeys(&made.keys, &h.sent(ZED)).len(), 1, "zed never sent one, so we do");
        assert_eq!(room_of(&h, &made.name).epoch, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn the_lowest_ids_key_is_taken_and_no_backup_follows() {
        let mut h = with_peers();
        h.connect(ZED, "zedfox", true);
        h.events(); // its PeerJoined
        let made = room_with(&mut h, &[ZED, BOB]).await;
        deliver(&mut h, BOB, frame_from(BOB, &made.keys, RoomBody::Leave));
        h.pump().await; // RekeyDue: wait for zed
        let secret = [5; 32];
        deliver(&mut h, ZED, frame_from(ZED, &made.keys, rekey_body(1, ZED, secret)));
        assert_eq!(keys_now(&h, &made.name).id, room::from_secret(&made.name, &secret).id);
        h.node.on_tick(tokio::time::Instant::now() + REKEY_BACKUP);
        assert!(rekeys(&made.keys, &h.sent(ZED)).is_empty());
        assert_eq!(room_of(&h, &made.name).epoch, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn crossing_rekeys_settle_on_one_key() {
        let mut h = with_peers();
        h.connect(ZED, "zedfox", true);
        h.events(); // its PeerJoined
        let made = room_with(&mut h, &[ZED, BOB, CAROL]).await;
        let id_of = |s: [u8; 32]| room::from_secret(&made.name, &s).id;
        deliver(&mut h, BOB, frame_from(BOB, &made.keys, rekey_body(1, BOB, [1; 32])));
        assert_eq!(keys_now(&h, &made.name).id, id_of([1; 32]), "a newer epoch is taken");
        let k1 = keys_now(&h, &made.name);
        deliver(&mut h, CAROL, frame_from(CAROL, &k1, rekey_body(1, CAROL, [2; 32])));
        assert_eq!(keys_now(&h, &made.name).id, id_of([1; 32]), "same epoch, higher maker: ignored");
        deliver(&mut h, CAROL, frame_from(CAROL, &k1, rekey_body(1, ZED, [3; 32])));
        assert_eq!(keys_now(&h, &made.name).id, id_of([3; 32]), "same epoch, lower maker wins, whoever passes it on");
        let k2 = keys_now(&h, &made.name);
        deliver(&mut h, BOB, frame_from(BOB, &k2, rekey_body(1, ME, [4; 32])));
        assert_eq!(keys_now(&h, &made.name).id, id_of([3; 32]), "ZED is still lower than ME");
        deliver(&mut h, CAROL, frame_from(CAROL, &k2, rekey_body(2, CAROL, [2; 32])));
        assert_eq!(keys_now(&h, &made.name).id, id_of([2; 32]), "a higher epoch always wins");
    }

    #[tokio::test]
    async fn a_rekey_from_a_non_member_is_ignored() {
        let mut h = with_peers();
        let made = room_with(&mut h, &[BOB]).await;
        deliver(&mut h, DAVE, frame_from(DAVE, &made.keys, rekey_body(1, DAVE, [4; 32])));
        assert_eq!(keys_now(&h, &made.name).id, made.keys.id);
    }

    #[tokio::test(start_paused = true)]
    async fn a_join_settling_after_a_rekey_gets_the_new_key() {
        let mut h = with_peers();
        let made = room_with(&mut h, &[CAROL]).await;
        knock(&mut h, BOB, &made, 100);
        deliver(&mut h, CAROL, frame_from(CAROL, &made.keys, RoomBody::Leave)); // RekeyDue in 1 s
        tokio::time::advance(REKEY_SETTLE / 2).await;
        approve(&mut h, &made, "bobroger"); // Settle in 1 s, after the re-key
        h.pump().await; // RekeyDue
        h.pump().await; // Settle, still under the room's old id
        let grant = bodies(&made.door, &h.sent(BOB))
            .into_iter()
            .find_map(|b| match b {
                RoomBody::Grant { secret, epoch, .. } => Some((secret, epoch)),
                _ => None,
            })
            .expect("granted");
        assert_eq!(grant.1, 1);
        assert_eq!(room::from_hex(&grant.0).unwrap(), room_of(&h, &made.name).secret.to_vec());
    }

    #[tokio::test(start_paused = true)]
    async fn an_invite_still_approves_after_a_rekey() {
        let mut h = with_peers();
        let made = room_with(&mut h, &[CAROL]).await;
        h.node.handle_command(Command::Invite { nick: "bobroger".into(), room: made.name.clone() });
        deliver(&mut h, CAROL, frame_from(CAROL, &made.keys, RoomBody::Leave));
        h.pump().await; // RekeyDue
        h.events();
        knock(&mut h, BOB, &made, 100);
        assert!(h.events().iter().any(|e| matches!(e, Event::Approved { by, .. } if by == "annmoon")));
    }

    #[tokio::test(start_paused = true)]
    async fn whoever_left_cannot_read_or_get_back_in_with_the_old_key() {
        let mut h = with_peers();
        let made = room_with(&mut h, &[BOB, CAROL]).await;
        deliver(&mut h, BOB, frame_from(BOB, &made.keys, RoomBody::Leave));
        h.pump().await; // RekeyDue
        h.events();
        h.sent(BOB);
        deliver(&mut h, BOB, frame_from(BOB, &made.keys, RoomBody::Chat { text: "still here?".into() }));
        deliver(&mut h, BOB, frame_from(BOB, &made.keys, RoomBody::Probe { nonce: "p1".into() }));
        deliver(&mut h, BOB, frame_from(BOB, &made.keys, here(&made.name, None)));
        assert!(h.events().is_empty());
        assert!(h.sent(BOB).is_empty(), "no answer, no challenge, no key");
        assert!(!room_of(&h, &made.name).members.contains_key(&BOB));
        h.node.handle_command(Command::Send { to: crate::api::Target::Room(made.name.clone()), text: "hi".into() });
        assert!(bodies(&made.keys, &h.sent(BOB)).is_empty(), "the room's frames don't open with his key");
    }

    #[tokio::test(start_paused = true)]
    async fn a_member_who_missed_the_rekey_is_sent_the_key_again() {
        let mut h = with_peers();
        let made = room_with(&mut h, &[BOB, CAROL]).await;
        deliver(&mut h, BOB, frame_from(BOB, &made.keys, RoomBody::Leave));
        h.pump().await; // RekeyDue
        h.sent(CAROL);
        deliver(&mut h, CAROL, frame_from(CAROL, &made.keys, here(&made.name, None)));
        let again = rekeys(&made.keys, &h.sent(CAROL));
        assert_eq!(again.len(), 1);
        assert_eq!(room::from_secret(&made.name, &again[0].2).id, keys_now(&h, &made.name).id);
        deliver(&mut h, CAROL, frame_from(CAROL, &made.keys, RoomBody::Chat { text: "hello?".into() }));
        assert!(rekeys(&made.keys, &h.sent(CAROL)).is_empty(), "once per interval");
        assert!(h.events().iter().any(|e| matches!(e, Event::Message { text, .. } if text == "hello?")));
    }

    #[tokio::test(start_paused = true)]
    async fn a_newcomer_let_in_with_the_old_key_ends_up_on_the_new_one() {
        let mut h = with_peers();
        let made = room_with(&mut h, &[CAROL]).await;
        knock(&mut h, BOB, &made, 100);
        approve(&mut h, &made, "bobroger");
        h.pump().await; // Settle: bob gets the secret that's about to be replaced
        assert!(granted(&made, &h.sent(BOB)));
        deliver(&mut h, CAROL, frame_from(CAROL, &made.keys, RoomBody::Leave));
        h.pump().await; // RekeyDue
        h.sent(BOB);
        deliver(&mut h, BOB, frame_from(BOB, &made.keys, RoomBody::Probe { nonce: "p1".into() }));
        let sent = bodies(&made.keys, &h.sent(BOB));
        assert_eq!(sent[0], here(&made.name, Some("p1")), "proves us to him first");
        assert!(matches!(&sent[1], RoomBody::Rekey { epoch: 1, by, .. } if *by == ME));
    }

    #[tokio::test(start_paused = true)]
    async fn old_keys_are_forgotten_after_a_while() {
        let mut h = with_peers();
        let made = room_with(&mut h, &[BOB, CAROL]).await;
        deliver(&mut h, BOB, frame_from(BOB, &made.keys, RoomBody::Leave));
        h.pump().await; // RekeyDue
        h.node.on_tick(tokio::time::Instant::now() + OLD_KEY_FOR);
        assert!(h.node.rooms.old_ids.is_empty());
        h.sent(CAROL);
        deliver(&mut h, CAROL, frame_from(CAROL, &made.keys, here(&made.name, None)));
        assert!(rekeys(&made.keys, &h.sent(CAROL)).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_crossing_rekey_under_the_key_we_left_still_counts() {
        let mut h = with_peers();
        h.connect(ZED, "zedfox", true);
        h.events(); // its PeerJoined
        let made = room_with(&mut h, &[ZED, BOB, CAROL]).await;
        let id_of = |s: [u8; 32]| room::from_secret(&made.name, &s).id;
        deliver(&mut h, BOB, frame_from(BOB, &made.keys, rekey_body(1, BOB, [1; 32])));
        assert_eq!(keys_now(&h, &made.name).id, id_of([1; 32]));
        deliver(&mut h, ZED, frame_from(ZED, &made.keys, rekey_body(1, ZED, [3; 32])));
        assert_eq!(keys_now(&h, &made.name).id, id_of([3; 32]), "ZED is lower than BOB");
        deliver(&mut h, CAROL, frame_from(CAROL, &made.keys, rekey_body(1, CAROL, [2; 32])));
        assert_eq!(keys_now(&h, &made.name).id, id_of([3; 32]), "CAROL is higher: ignored");
    }

    #[test]
    fn zed_sorts_below_us() {
        // The tests above rely on this order.
        assert!(ZED < ME && ME < BOB && BOB < CAROL);
    }
}
