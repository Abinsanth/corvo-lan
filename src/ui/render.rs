//! Turns events and stored lines into terminal text: timestamps, colours,
//! sigils, and sanitising of anything a peer sent.

use std::io::IsTerminal;

use crate::api::{Conv, Event, LeaveReason};
use crate::convo::{Line, LineKind, Store};

/// ANSI colour on or off.
#[derive(Debug, Clone, Copy)]
pub struct Style {
    pub color: bool,
}

impl Style {
    /// Colour only on a real terminal, and never when `NO_COLOR` is set.
    pub fn detect() -> Style {
        Style { color: std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none() }
    }

    pub fn plain() -> Style {
        Style { color: false }
    }

    fn paint(&self, code: &str, s: &str) -> String {
        if self.color { format!("\x1b[{code}m{s}\x1b[0m") } else { s.to_string() }
    }

    /// `@nick` in its stable colour (cleaned first).
    pub fn nick(&self, nick: &str) -> String {
        self.paint(&nick_color(nick).to_string(), &format!("@{}", clean(nick)))
    }

    pub fn room(&self, room: &str) -> String {
        self.paint("1", &format!("#{}", clean(room)))
    }

    pub fn dim(&self, s: &str) -> String {
        self.paint("2", s)
    }
}

/// One of ANSI 31–36, the same for a name on every run (FNV-1a).
pub fn nick_color(nick: &str) -> u8 {
    let mut hash: u32 = 0x811c9dc5;
    for b in nick.bytes() {
        hash ^= b as u32;
        hash = hash.wrapping_mul(0x01000193);
    }
    31 + (hash % 6) as u8
}

/// Replaces control characters (including newlines and ANSI escapes) and bidi
/// overrides so a peer cannot rewrite our terminal or reverse displayed text.
/// Zero-width joiners and variation selectors stay, since emoji need them.
pub fn clean(s: &str) -> String {
    s.chars().map(|c| if c.is_control() || is_bidi_control(c) { '\u{fffd}' } else { c }).collect()
}

fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// Local `HH:MM` for unix millis.
pub fn hhmm(ts: u64) -> String {
    chrono::DateTime::from_timestamp_millis(ts as i64)
        .map(|t| t.with_timezone(&chrono::Local).format("%H:%M").to_string())
        .unwrap_or_else(|| "--:--".into())
}

fn stamp(ts: u64) -> String {
    format!("[{}]", hhmm(ts))
}

fn notice(ts: u64, text: &str, style: &Style) -> String {
    style.dim(&format!("{} * {text}", stamp(ts)))
}

/// What to print for an event, if anything. Our own room messages aren't
/// echoed: the typed line is already on screen.
pub fn event(event: &Event, now: u64, style: &Style) -> Option<String> {
    let s = style;
    Some(match event {
        Event::Assigned { nick, renamed: false } => {
            format!("* the system assigned you {} — this name can't be changed", s.nick(nick))
        }
        Event::Assigned { nick, renamed: true } => {
            format!("* your name was taken at the same moment — you are now {} (can't be changed)", s.nick(nick))
        }
        Event::PeerJoined { nick, .. } => notice(now, &format!("{} is online", s.nick(nick)), s),
        Event::PeerLeft { nick, reason: LeaveReason::Quit, .. } => {
            notice(now, &format!("{} went offline", s.nick(nick)), s)
        }
        Event::PeerLeft { nick, reason: LeaveReason::Lost, .. } => {
            notice(now, &format!("{} went offline (connection lost)", s.nick(nick)), s)
        }
        Event::Renamed { old, new } => notice(now, &format!("{} is now {}", s.nick(old), s.nick(new)), s),
        Event::Message { conv: Conv::Room(room), from, text, ts } => {
            format!("{} {} {}: {}", stamp(*ts), s.room(room), s.nick(from), clean(text))
        }
        Event::Message { conv: Conv::Dm(_), from, text, ts } => {
            format!("{} {} → you: {}", stamp(*ts), s.nick(from), clean(text))
        }
        Event::Sent { conv: Conv::Dm(to), text, ts } => format!("{} you → {}: {}", stamp(*ts), s.nick(to), clean(text)),
        Event::Sent { conv: Conv::Room(_), .. } => return None,
        Event::RoomCreated { name, key } => {
            format!("* created {} — key {} — share both to invite", s.room(name), clean(key))
        }
        Event::RoomJoined { name, members } if members.is_empty() => format!("* joined {}", s.room(name)),
        Event::RoomJoined { name, members } => {
            let members: Vec<String> = members.iter().map(|m| s.nick(m)).collect();
            format!("* joined {} with {}", s.room(name), members.join(", "))
        }
        Event::RoomLeft { name } => format!("* left {}", s.room(name)),
        Event::MemberJoined { room, nick } => notice(now, &format!("{} joined {}", s.nick(nick), s.room(room)), s),
        Event::MemberLeft { room, nick } => notice(now, &format!("{} left {}", s.nick(nick), s.room(room)), s),
        Event::Peers(peers) if peers.is_empty() => "* no peers connected".into(),
        Event::Peers(peers) => {
            let mut out = format!("* {} peer(s) online:", peers.len());
            for p in peers {
                out.push_str(&format!("\n  {:<16} {}  {}", s.nick(&p.nick), p.id, p.addr));
            }
            out
        }
        Event::Error(msg) => format!("! {}", clean(msg)),
        Event::Info(msg) => format!("* {}", clean(msg)),
    })
}

/// A stored line, as `/history` shows it.
pub fn line(conv: &Conv, l: &Line, style: &Style) -> String {
    let s = style;
    match (&l.kind, conv) {
        (LineKind::Msg { from, text }, Conv::Room(room)) => {
            format!("{} {} {}: {}", stamp(l.ts), s.room(room), s.nick(from), clean(text))
        }
        (LineKind::Msg { from, text }, Conv::Dm(_)) => {
            format!("{} {} → you: {}", stamp(l.ts), s.nick(from), clean(text))
        }
        (LineKind::Mine { text }, Conv::Room(room)) => format!("{} {} you: {}", stamp(l.ts), s.room(room), clean(text)),
        (LineKind::Mine { text }, Conv::Dm(to)) => format!("{} you → {}: {}", stamp(l.ts), s.nick(to), clean(text)),
        (LineKind::Notice(text), _) => notice(l.ts, &clean(text), s),
    }
}

pub fn conv_name(conv: &Conv, style: &Style) -> String {
    match conv {
        Conv::Room(n) => style.room(n),
        Conv::Dm(n) => style.nick(n),
    }
}

/// `/chats`.
pub fn chats(store: &Store, style: &Style) -> String {
    if store.conversations().is_empty() {
        return "* no conversations yet — /chat @nick or /create <name> <key>".into();
    }
    let mut out = String::from("* conversations:");
    for c in store.conversations() {
        let mut tags = Vec::new();
        if c.unread > 0 {
            tags.push(format!("{} new", c.unread));
        }
        if c.offline {
            tags.push("offline".into());
        }
        if store.current() == Some(&c.conv) {
            tags.push("current".into());
        }
        out.push_str(&format!("\n  {:<18} {}", conv_name(&c.conv, style), tags.join(", ")));
    }
    out
}

/// `/rooms`.
pub fn rooms(store: &Store, style: &Style) -> String {
    let rooms: Vec<_> = store.conversations().iter().filter(|c| matches!(c.conv, Conv::Room(_))).collect();
    if rooms.is_empty() {
        return "* you're not in any rooms — /create <name> <key> or /chat #room <key>".into();
    }
    let mut out = String::from("* your rooms:");
    for c in rooms {
        let members = if c.members.is_empty() {
            "(just you)".to_string()
        } else {
            c.members.iter().map(|m| style.nick(m)).collect::<Vec<_>>().join(", ")
        };
        out.push_str(&format!("\n  {:<18} {members}", conv_name(&c.conv, style)));
    }
    out
}

/// The input prompt: where plain typing goes. Uncoloured so rustyline measures it right.
pub fn prompt(current: Option<&Conv>) -> String {
    match current {
        Some(Conv::Room(n)) => format!("#{}> ", clean(n)),
        Some(Conv::Dm(n)) => format!("@{}> ", clean(n)),
        None => "> ".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{PeerId, PeerInfo};

    const P: Style = Style { color: false };

    fn t(ts: u64) -> String {
        format!("[{}]", hhmm(ts))
    }

    #[test]
    fn messages_show_time_sigils_and_direction() {
        let room =
            Event::Message { conv: Conv::Room("catPieR".into()), from: "bobRoger".into(), text: "hi".into(), ts: 0 };
        assert_eq!(event(&room, 0, &P).unwrap(), format!("{} #catPieR @bobRoger: hi", t(0)));
        let dm_in =
            Event::Message { conv: Conv::Dm("bobRoger".into()), from: "bobRoger".into(), text: "psst".into(), ts: 0 };
        assert_eq!(event(&dm_in, 0, &P).unwrap(), format!("{} @bobRoger → you: psst", t(0)));
        let dm_out = Event::Sent { conv: Conv::Dm("bobRoger".into()), text: "yo".into(), ts: 0 };
        assert_eq!(event(&dm_out, 0, &P).unwrap(), format!("{} you → @bobRoger: yo", t(0)));
        let room_out = Event::Sent { conv: Conv::Room("catPieR".into()), text: "mine".into(), ts: 0 };
        assert_eq!(event(&room_out, 0, &P), None);
    }

    #[test]
    fn room_and_presence_notices() {
        let created = Event::RoomCreated { name: "catPieR".into(), key: "123-k7m9xq".into() };
        assert_eq!(event(&created, 0, &P).unwrap(), "* created #catPieR — key 123-k7m9xq — share both to invite");
        let joined = Event::RoomJoined { name: "catPieR".into(), members: vec!["bobRoger".into(), "carolFox".into()] };
        assert_eq!(event(&joined, 0, &P).unwrap(), "* joined #catPieR with @bobRoger, @carolFox");
        let member = Event::MemberJoined { room: "catPieR".into(), nick: "carolFox".into() };
        assert_eq!(event(&member, 0, &P).unwrap(), format!("{} * @carolFox joined #catPieR", t(0)));
        let lost = Event::PeerLeft { id: PeerId(1), nick: "bobRoger".into(), reason: LeaveReason::Lost };
        assert_eq!(event(&lost, 0, &P).unwrap(), format!("{} * @bobRoger went offline (connection lost)", t(0)));
        let assigned = Event::Assigned { nick: "annMoon".into(), renamed: false };
        assert_eq!(event(&assigned, 0, &P).unwrap(), "* the system assigned you @annMoon — this name can't be changed");
        let peers = Event::Peers(vec![PeerInfo {
            id: PeerId(1),
            nick: "bobRoger".into(),
            addr: "10.0.0.2:5000".parse().unwrap(),
        }]);
        let text = event(&peers, 0, &P).unwrap();
        assert!(text.contains("@bobRoger") && text.contains("10.0.0.2:5000"));
    }

    #[test]
    fn peer_text_is_cleaned_before_colouring() {
        let color = Style { color: true };
        let evil = Event::Message {
            conv: Conv::Dm("bobRoger".into()),
            from: "bobRoger".into(),
            text: "a\u{1b}[2J\nb\u{202E}c".into(),
            ts: 0,
        };
        let out = event(&evil, 0, &color).unwrap();
        assert!(out.ends_with("a\u{fffd}[2J\u{fffd}b\u{fffd}c"), "{out:?}");
        // Our own colour codes are present, the peer's escape is not.
        assert!(out.contains("\x1b[3"));
        assert_eq!(out.matches('\x1b').count(), 2);
        assert_eq!(clean("\u{2764}\u{FE0F} \u{1F468}\u{200D}\u{1F469}"), "\u{2764}\u{FE0F} \u{1F468}\u{200D}\u{1F469}");
    }

    #[test]
    fn plain_style_has_no_escape_codes() {
        let e = Event::MemberJoined { room: "catPieR".into(), nick: "carolFox".into() };
        assert!(!event(&e, 0, &P).unwrap().contains('\x1b'));
    }

    #[test]
    fn nick_colours_are_stable_and_in_range() {
        assert_eq!(nick_color("bobRoger"), nick_color("bobRoger"));
        for n in ["a", "bobRoger", "carolFox", "x9Moon"] {
            assert!((31..=36).contains(&nick_color(n)));
        }
    }

    #[test]
    fn prompt_follows_the_current_conversation() {
        assert_eq!(prompt(None), "> ");
        assert_eq!(prompt(Some(&Conv::Room("catPieR".into()))), "#catPieR> ");
        assert_eq!(prompt(Some(&Conv::Dm("bobRoger".into()))), "@bobRoger> ");
    }

    #[test]
    fn chats_and_rooms_listings() {
        let mut store = Store::new();
        assert!(chats(&store, &P).starts_with("* no conversations yet"));
        assert!(rooms(&store, &P).starts_with("* you're not in any rooms"));
        store.apply(&Event::RoomJoined { name: "catPieR".into(), members: vec!["bobRoger".into()] }, 0);
        store.switch(Conv::Room("catPieR".into()));
        store.apply(
            &Event::Message { conv: Conv::Dm("bobRoger".into()), from: "bobRoger".into(), text: "x".into(), ts: 0 },
            0,
        );
        let c = chats(&store, &P);
        assert!(c.contains("#catPieR") && c.contains("current"), "{c}");
        assert!(c.contains("@bobRoger") && c.contains("1 new") && c.contains("offline"), "{c}");
        assert!(rooms(&store, &P).contains("#catPieR           @bobRoger"));
    }

    #[test]
    fn history_lines_cover_every_kind() {
        let room = Conv::Room("catPieR".into());
        let dm = Conv::Dm("bobRoger".into());
        let mine = Line { ts: 0, kind: LineKind::Mine { text: "hey".into() } };
        assert_eq!(line(&room, &mine, &P), format!("{} #catPieR you: hey", t(0)));
        assert_eq!(line(&dm, &mine, &P), format!("{} you → @bobRoger: hey", t(0)));
        let note = Line { ts: 0, kind: LineKind::Notice("@carolFox joined".into()) };
        assert_eq!(line(&room, &note, &P), format!("{} * @carolFox joined", t(0)));
    }
}
