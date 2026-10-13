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

    /// `s` in its tone's colour: green, red, yellow or dim.
    pub fn tone(&self, tone: Tone, s: &str) -> String {
        match tone {
            Tone::Good => self.paint("32", s),
            Tone::Bad => self.paint("31", s),
            Tone::Warn => self.paint("33", s),
            Tone::Dim => self.paint("2", s),
            Tone::Plain => s.to_string(),
        }
    }

    /// A whole line of our own text, in the tone its wording implies.
    pub fn toned(&self, text: &str) -> String {
        self.tone(tone_of_text(text), text)
    }
}

/// What a line means, which decides its colour in both frontends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Good,
    Bad,
    Warn,
    Dim,
    Plain,
}

/// Info texts that are warnings. The node doesn't label them, so match wording.
const WARN_MARKERS: &[&str] = &["sending too fast", "clock is", "already running"];

pub fn tone_of(event: &Event) -> Tone {
    match event {
        Event::PeerJoined { .. }
        | Event::MemberJoined { .. }
        | Event::RoomCreated { .. }
        | Event::RoomJoined { .. } => Tone::Good,
        Event::PeerLeft { .. } | Event::MemberLeft { .. } | Event::RoomLeft { .. } | Event::Error(_) => Tone::Bad,
        Event::Info(text) if WARN_MARKERS.iter().any(|m| text.contains(m)) => Tone::Warn,
        _ => Tone::Plain,
    }
}

/// For lines we print ourselves ("! …" errors, "* …" notes).
pub fn tone_of_text(text: &str) -> Tone {
    if text.starts_with("! ") {
        Tone::Bad
    } else if text.starts_with("* installed v") {
        Tone::Good
    } else if WARN_MARKERS.iter().any(|m| text.contains(m)) {
        Tone::Warn
    } else {
        Tone::Plain
    }
}

/// For notices the store keeps inside a conversation.
pub fn tone_of_notice(text: &str) -> Tone {
    if text.ends_with(" joined") || text.starts_with("created ") || text.starts_with("joined ") {
        Tone::Good
    } else if text.ends_with(" left") {
        Tone::Bad
    } else {
        Tone::Dim
    }
}

/// A message body for line mode: each line cleaned, continuations indented so
/// a peer can't make one look like a new message.
pub fn body(text: &str) -> String {
    text.split('\n').map(clean).collect::<Vec<_>>().join("\n  │ ")
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
    format!("{} * {text}", style.dim(&stamp(ts)))
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
        Event::PeerJoined { nick, .. } => {
            notice(now, &format!("{} {}", s.nick(nick), s.tone(Tone::Good, "is online")), s)
        }
        Event::PeerLeft { nick, reason: LeaveReason::Quit, .. } => {
            notice(now, &format!("{} {}", s.nick(nick), s.tone(Tone::Bad, "went offline")), s)
        }
        Event::PeerLeft { nick, reason: LeaveReason::Lost, .. } => {
            notice(now, &format!("{} {}", s.nick(nick), s.tone(Tone::Bad, "went offline (connection lost)")), s)
        }
        Event::Renamed { old, new } => notice(now, &format!("{} is now {}", s.nick(old), s.nick(new)), s),
        Event::Message { conv: Conv::Room(room), from, text, ts } => {
            format!("{} {} {}: {}", s.dim(&stamp(*ts)), s.room(room), s.nick(from), body(text))
        }
        Event::Message { conv: Conv::Dm(_), from, text, ts } => {
            format!("{} {} → you: {}", s.dim(&stamp(*ts)), s.nick(from), body(text))
        }
        Event::Sent { conv: Conv::Dm(to), text, ts } => {
            format!("{} you → {}: {}", s.dim(&stamp(*ts)), s.nick(to), body(text))
        }
        Event::Sent { conv: Conv::Room(_), .. } => return None,
        Event::RoomCreated { name, key } => {
            format!("{} {} — key {} — share both to invite", s.tone(Tone::Good, "* created"), s.room(name), clean(key))
        }
        Event::RoomJoined { name, members } if members.is_empty() => {
            format!("{} {}", s.tone(Tone::Good, "* joined"), s.room(name))
        }
        Event::RoomJoined { name, members } => {
            let members: Vec<String> = members.iter().map(|m| s.nick(m)).collect();
            format!("{} {} with {}", s.tone(Tone::Good, "* joined"), s.room(name), members.join(", "))
        }
        Event::RoomLeft { name } => format!("{} {}", s.tone(Tone::Bad, "* left"), s.room(name)),
        Event::MemberJoined { room, nick } => {
            notice(now, &format!("{} {} {}", s.nick(nick), s.tone(Tone::Good, "joined"), s.room(room)), s)
        }
        Event::MemberLeft { room, nick } => {
            notice(now, &format!("{} {} {}", s.nick(nick), s.tone(Tone::Bad, "left"), s.room(room)), s)
        }
        Event::Peers(peers) if peers.is_empty() => "* no peers connected".into(),
        Event::Peers(peers) => {
            let mut out = format!("* {} peer(s) online:", peers.len());
            for p in peers {
                out.push_str(&format!("\n  {:<16} {}  {}", s.nick(&p.nick), p.id, p.addr));
            }
            out
        }
        Event::Error(msg) => s.tone(Tone::Bad, &format!("! {}", clean(msg))),
        Event::Info(msg) => s.tone(tone_of(event), &format!("* {}", clean(msg))),
    })
}

/// A stored line, as `/history` shows it.
pub fn line(conv: &Conv, l: &Line, style: &Style) -> String {
    let s = style;
    match (&l.kind, conv) {
        (LineKind::Msg { from, text }, Conv::Room(room)) => {
            format!("{} {} {}: {}", s.dim(&stamp(l.ts)), s.room(room), s.nick(from), body(text))
        }
        (LineKind::Msg { from, text }, Conv::Dm(_)) => {
            format!("{} {} → you: {}", s.dim(&stamp(l.ts)), s.nick(from), body(text))
        }
        (LineKind::Mine { text }, Conv::Room(room)) => {
            format!("{} {} you: {}", s.dim(&stamp(l.ts)), s.room(room), body(text))
        }
        (LineKind::Mine { text }, Conv::Dm(to)) => {
            format!("{} you → {}: {}", s.dim(&stamp(l.ts)), s.nick(to), body(text))
        }
        (LineKind::Notice(text), _) => notice(l.ts, &s.tone(tone_of_notice(text), &clean(text)), s),
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
        assert!(out.ends_with("a\u{fffd}[2J\n  │ b\u{fffd}c"), "{out:?}");
        // Our own colour codes are present; the peer's escape sequence is not.
        assert!(out.contains("\x1b[3") && !out.contains("\x1b[2J"));
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

    #[test]
    fn events_have_tones() {
        let nick = || "bobRoger".to_string();
        assert_eq!(tone_of(&Event::PeerJoined { id: PeerId(1), nick: nick() }), Tone::Good);
        assert_eq!(tone_of(&Event::MemberJoined { room: "r".into(), nick: nick() }), Tone::Good);
        assert_eq!(tone_of(&Event::RoomCreated { name: "r".into(), key: "k".into() }), Tone::Good);
        assert_eq!(tone_of(&Event::RoomJoined { name: "r".into(), members: vec![] }), Tone::Good);
        assert_eq!(tone_of(&Event::PeerLeft { id: PeerId(1), nick: nick(), reason: LeaveReason::Quit }), Tone::Bad);
        assert_eq!(tone_of(&Event::MemberLeft { room: "r".into(), nick: nick() }), Tone::Bad);
        assert_eq!(tone_of(&Event::RoomLeft { name: "r".into() }), Tone::Bad);
        assert_eq!(tone_of(&Event::Error("x".into())), Tone::Bad);
        assert_eq!(tone_of(&Event::Info("@bob is sending too fast; dropping messages".into())), Tone::Warn);
        assert_eq!(
            tone_of(&Event::Info("@bob's clock is 9 min off; their room messages are ignored".into())),
            Tone::Warn
        );
        assert_eq!(tone_of(&Event::Info("looking for peers…".into())), Tone::Plain);
        assert_eq!(tone_of(&Event::Peers(vec![])), Tone::Plain);
    }

    #[test]
    fn texts_and_notices_have_tones() {
        assert_eq!(tone_of_text("! @bob is offline"), Tone::Bad);
        assert_eq!(tone_of_text("! an update check is already running"), Tone::Bad);
        assert_eq!(tone_of_text("* an update check is already running"), Tone::Warn);
        assert_eq!(tone_of_text("* installed v0.3.0. Restart corvo-lan to use it."), Tone::Good);
        assert_eq!(tone_of_text("* you have 0.2.0. Newer:"), Tone::Plain);
        assert_eq!(tone_of_notice("@carolFox joined"), Tone::Good);
        assert_eq!(tone_of_notice("created #catPieR — key 1-x — share both to invite"), Tone::Good);
        assert_eq!(tone_of_notice("joined #catPieR"), Tone::Good);
        assert_eq!(tone_of_notice("@bobRoger left"), Tone::Bad);
        assert_eq!(tone_of_notice("something else"), Tone::Dim);
    }

    #[test]
    fn line_mode_colours_by_tone() {
        let c = Style { color: true };
        assert_eq!(c.tone(Tone::Good, "x"), "\x1b[32mx\x1b[0m");
        assert_eq!(c.tone(Tone::Bad, "x"), "\x1b[31mx\x1b[0m");
        assert_eq!(c.tone(Tone::Warn, "x"), "\x1b[33mx\x1b[0m");
        assert_eq!(c.tone(Tone::Dim, "x"), "\x1b[2mx\x1b[0m");
        assert_eq!(c.tone(Tone::Plain, "x"), "x");
        assert_eq!(c.toned("! nope"), "\x1b[31m! nope\x1b[0m");
        assert_eq!(P.tone(Tone::Bad, "x"), "x");
        let online = event(&Event::PeerJoined { id: PeerId(1), nick: "bobRoger".into() }, 0, &c).unwrap();
        assert!(online.contains("\x1b[32mis online\x1b[0m"), "{online:?}");
        let gone = event(&Event::PeerLeft { id: PeerId(1), nick: "bobRoger".into(), reason: LeaveReason::Quit }, 0, &c)
            .unwrap();
        assert!(gone.contains("\x1b[31mwent offline\x1b[0m"), "{gone:?}");
        let error = event(&Event::Error("no room".into()), 0, &c).unwrap();
        assert_eq!(error, "\x1b[31m! no room\x1b[0m");
    }

    #[test]
    fn newlines_become_indented_continuations() {
        assert_eq!(body("one\ntwo"), "one\n  │ two");
        let forged = Event::Message {
            conv: Conv::Room("catPieR".into()),
            from: "bobRoger".into(),
            text: "hi\n[14:02] #catPieR @alice: fake\r!".into(),
            ts: 0,
        };
        let out = event(&forged, 0, &P).unwrap();
        assert_eq!(out, format!("{} #catPieR @bobRoger: hi\n  │ [14:02] #catPieR @alice: fake\u{fffd}!", t(0)));
        let stored = Line { ts: 0, kind: LineKind::Msg { from: "bobRoger".into(), text: "a\nb".into() } };
        assert_eq!(line(&Conv::Dm("bobRoger".into()), &stored, &P), format!("{} @bobRoger → you: a\n  │ b", t(0)));
    }
}
