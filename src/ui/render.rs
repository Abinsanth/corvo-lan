//! Turns events and stored lines into terminal text: timestamps, colours,
//! sigils, and sanitising of anything a peer sent.

use std::io::IsTerminal;

use crate::api::{Conv, Event, JoinEnd, LeaveReason};
use crate::convo::Store;
use crate::names::{display, display_in};

/// ANSI colour on or off.
#[derive(Debug, Clone, Copy)]
pub struct Style {
    pub color: bool,
}

/// The legacy Windows console prints escape codes raw unless virtual terminal
/// processing is on; this tries to turn it on. Elsewhere it's always true.
fn console_understands_ansi() -> bool {
    #[cfg(windows)]
    {
        ratatui::crossterm::ansi_support::supports_ansi()
    }
    #[cfg(not(windows))]
    {
        true
    }
}

impl Style {
    /// Colour only on a real terminal, and never when `NO_COLOR` is set.
    pub fn detect() -> Style {
        Style {
            color: std::io::stdout().is_terminal()
                && std::env::var_os("NO_COLOR").is_none()
                && console_understands_ansi(),
        }
    }

    pub fn plain() -> Style {
        Style { color: false }
    }

    fn paint(&self, code: &str, s: &str) -> String {
        if self.color { format!("\x1b[{code}m{s}\x1b[0m") } else { s.to_string() }
    }

    /// `@nickName` in its stable colour (cleaned first).
    pub fn nick(&self, nick: &str) -> String {
        self.paint(&nick_color(nick).to_string(), &format!("@{}", clean(&display(nick))))
    }

    pub fn room(&self, room: &str) -> String {
        self.paint("1", &format!("#{}", clean(&display(room))))
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
        self.tone(tone_of_text(text), &display_in(text))
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
const WARN_MARKERS: &[&str] = &["sending too fast", "clock is", "already running", "different key"];

pub fn tone_of(event: &Event) -> Tone {
    match event {
        Event::PeerJoined { .. }
        | Event::MemberJoined { .. }
        | Event::RoomCreated { .. }
        | Event::RoomJoined { .. } => Tone::Good,
        Event::PeerLeft { .. }
        | Event::MemberLeft { .. }
        | Event::RoomLeft { .. }
        | Event::JoinEnded { .. }
        | Event::Error(_) => Tone::Bad,
        Event::JoinRequest { .. } | Event::Invited { .. } => Tone::Warn,
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
    text.split('\n').map(|l| clean(&display_in(l))).collect::<Vec<_>>().join("\n  │ ")
}

/// "1 mention", "2 mentions".
pub fn mention_count(n: u32) -> String {
    format!("{n} mention{}", if n == 1 { "" } else { "s" })
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
            format!("* you're {} — restart to pick another name", s.nick(nick))
        }
        Event::Assigned { nick, renamed: true } => {
            format!("* your name was taken just now — you're {} (restart to pick another name)", s.nick(nick))
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
        Event::RoomCreated { name, key, max } => format!(
            "{} {} for up to {max} people — /invite @name, or share /chat #{} {}",
            s.tone(Tone::Good, "* created"),
            s.room(name),
            clean(&display(name)),
            clean(key)
        ),
        Event::RoomJoined { name, members, .. } if members.is_empty() => {
            format!("{} {}", s.tone(Tone::Good, "* joined"), s.room(name))
        }
        Event::RoomJoined { name, members, .. } => {
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
        Event::Peers { you, nick, peers } if peers.is_empty() => {
            format!("* no peers connected — you are {}, fingerprint {}", s.nick(nick), you.fingerprint())
        }
        Event::Peers { you, nick, peers } => {
            let mut out = format!("* {} peer(s) online:", peers.len());
            out.push_str(&format!("\n  {} {}", pad(&format!("you {}", s.nick(nick)), 16), you.fingerprint()));
            for p in peers {
                out.push_str(&format!("\n  {} {}  {}", pad(&s.nick(&p.nick), 16), p.id.fingerprint(), p.addr));
            }
            out
        }
        Event::Error(msg) => s.tone(Tone::Bad, &format!("! {}", clean(&display_in(msg)))),
        Event::Info(msg) => s.tone(tone_of(event), &format!("* {}", clean(&display_in(msg)))),
        Event::JoinWaiting { name, members, needs } => {
            let who = if members == needs {
                format!("its {members} member{}", if *members == 1 { "" } else { "s" })
            } else {
                format!("{needs} of its {members} members")
            };
            format!(
                "* waiting to be let into {} — {who} must approve (/leave #{} to stop)",
                s.room(name),
                clean(&display(name))
            )
        }
        Event::JoinEnded { name, why } => {
            let text = match why {
                JoinEnd::NoRoom => format!("! no room {} with that key", s.room(name)),
                JoinEnd::Full => format!("! {} is full", s.room(name)),
                JoinEnd::Denied { by } => format!("! {} didn't let you into {}", s.nick(by), s.room(name)),
                JoinEnd::TimedOut => format!("! nobody let you into {} — try again later", s.room(name)),
            };
            s.tone(Tone::Bad, &text)
        }
        Event::JoinRequest { room, nick, .. } => {
            let n = clean(&display(nick));
            s.tone(
                Tone::Warn,
                &format!("* {} wants to join {} — /approve @{n} or /deny @{n}", s.nick(nick), s.room(room)),
            )
        }
        Event::Invited { from, room } => {
            format!("* {} invites you to {} — /accept or /decline", s.nick(from), s.room(room))
        }
        Event::InviteDeclined { by, room } => {
            format!("* {} declined your invite to {}", s.nick(by), s.room(room))
        }
        // Shown inside the room's conversation.
        Event::Approved { .. } | Event::RequestEnded { .. } => return None,
        Event::NamePreview { .. } => return None,
    })
}

/// Pads `s` to `width` columns on screen; colour codes take no room.
fn pad(s: &str, width: usize) -> String {
    format!("{s}{}", " ".repeat(width.saturating_sub(visible_len(s))))
}

fn visible_len(s: &str) -> usize {
    let mut n = 0;
    let mut in_code = false;
    for c in s.chars() {
        match (in_code, c) {
            (false, '\x1b') => in_code = true,
            (true, 'm') => in_code = false,
            (false, _) => n += 1,
            (true, _) => {}
        }
    }
    n
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
        return "* no conversations yet — /chat @nick or /create <name>".into();
    }
    let mut out = String::from("* conversations:");
    for c in store.conversations() {
        let mut tags = Vec::new();
        if c.unread > 0 {
            tags.push(format!("{} new", c.unread));
        }
        if c.mentions > 0 {
            tags.push(mention_count(c.mentions));
        }
        if c.offline {
            tags.push("offline".into());
        }
        if store.current() == Some(&c.conv) {
            tags.push("current".into());
        }
        out.push_str(&format!("\n  {} {}", pad(&conv_name(&c.conv, style), 18), tags.join(", ")));
    }
    out
}

/// `/rooms`.
pub fn rooms(store: &Store, style: &Style) -> String {
    let rooms: Vec<_> = store.conversations().iter().filter(|c| matches!(c.conv, Conv::Room(_))).collect();
    if rooms.is_empty() && store.pending().next().is_none() {
        return "* you're not in any rooms — /create <name> or /chat #room <key>".into();
    }
    let mut out = String::from("* your rooms:");
    for c in rooms {
        let members = if c.members.is_empty() {
            "(just you)".to_string()
        } else {
            c.members.iter().map(|m| style.nick(m)).collect::<Vec<_>>().join(", ")
        };
        let count = c.max.map(|max| format!("{}/{max} · ", c.members.len() + 1)).unwrap_or_default();
        let key = c.key.as_ref().map(|k| format!("key {} · ", clean(k))).unwrap_or_default();
        out.push_str(&format!("\n  {} {count}{key}{members}", pad(&conv_name(&c.conv, style), 18)));
    }
    for name in store.pending() {
        out.push_str(&format!("\n  {} waiting to be let in", pad(&style.room(name), 18)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{PeerId, PeerInfo, RequestEnd};

    const P: Style = Style { color: false };

    fn t(ts: u64) -> String {
        format!("[{}]", hhmm(ts))
    }

    #[test]
    fn messages_show_time_sigils_and_direction() {
        let room =
            Event::Message { conv: Conv::Room("catpie".into()), from: "bobroger".into(), text: "hi".into(), ts: 0 };
        assert_eq!(event(&room, 0, &P).unwrap(), format!("{} #catPie @bobRoger: hi", t(0)));
        let dm_in =
            Event::Message { conv: Conv::Dm("bobroger".into()), from: "bobroger".into(), text: "psst".into(), ts: 0 };
        assert_eq!(event(&dm_in, 0, &P).unwrap(), format!("{} @bobRoger → you: psst", t(0)));
        let dm_out = Event::Sent { conv: Conv::Dm("bobroger".into()), text: "yo".into(), ts: 0 };
        assert_eq!(event(&dm_out, 0, &P).unwrap(), format!("{} you → @bobRoger: yo", t(0)));
        let room_out = Event::Sent { conv: Conv::Room("catpie".into()), text: "mine".into(), ts: 0 };
        assert_eq!(event(&room_out, 0, &P), None);
    }

    #[test]
    fn room_and_presence_notices() {
        let created = Event::RoomCreated { name: "catpie".into(), key: "k7m9xq".into(), max: 13 };
        assert_eq!(
            event(&created, 0, &P).unwrap(),
            "* created #catPie for up to 13 people — /invite @name, or share /chat #catPie k7m9xq"
        );
        let joined = Event::RoomJoined {
            name: "catpie".into(),
            key: "k7m9xq".into(),
            max: 13,
            members: vec!["bobroger".into(), "carolwolf".into()],
        };
        assert_eq!(event(&joined, 0, &P).unwrap(), "* joined #catPie with @bobRoger, @carolWolf");
        let member = Event::MemberJoined { room: "catpie".into(), nick: "carolwolf".into() };
        assert_eq!(event(&member, 0, &P).unwrap(), format!("{} * @carolWolf joined #catPie", t(0)));
        let lost = Event::PeerLeft { id: PeerId::test(1), nick: "bobroger".into(), reason: LeaveReason::Lost };
        assert_eq!(event(&lost, 0, &P).unwrap(), format!("{} * @bobRoger went offline (connection lost)", t(0)));
        let assigned = Event::Assigned { nick: "annmoon".into(), renamed: false };
        assert_eq!(event(&assigned, 0, &P).unwrap(), "* you're @annMoon — restart to pick another name");
        let peers = Event::Peers {
            you: PeerId::test(0x7f3a_9c0d_12e4_5b68),
            nick: "annmoon".into(),
            peers: vec![PeerInfo {
                id: PeerId::test(0xa1b2_c3d4_e5f6_0718),
                nick: "bobroger".into(),
                addr: "10.0.0.2:5000".parse().unwrap(),
            }],
        };
        let text = event(&peers, 0, &P).unwrap();
        let you = format!("\n  {:<16} 7f3a 9c0d 12e4 5b68", "you @annMoon");
        let bob = format!("\n  {:<16} a1b2 c3d4 e5f6 0718  10.0.0.2:5000", "@bobRoger");
        assert_eq!(text, format!("* 1 peer(s) online:{you}{bob}"));
        let alone = Event::Peers { you: PeerId::test(0x7f3a_9c0d_12e4_5b68), nick: "annmoon".into(), peers: vec![] };
        assert_eq!(
            event(&alone, 0, &P).unwrap(),
            "* no peers connected — you are @annMoon, fingerprint 7f3a 9c0d 12e4 5b68"
        );
    }

    #[test]
    fn peer_text_is_cleaned_before_colouring() {
        let color = Style { color: true };
        let evil = Event::Message {
            conv: Conv::Dm("bobroger".into()),
            from: "bobroger".into(),
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
        let e = Event::MemberJoined { room: "catpie".into(), nick: "carolwolf".into() };
        assert!(!event(&e, 0, &P).unwrap().contains('\x1b'));
    }

    #[test]
    fn nick_colours_are_stable_and_in_range() {
        assert_eq!(nick_color("bobroger"), nick_color("bobroger"));
        for n in ["a", "bobroger", "carolwolf", "x9moon"] {
            assert!((31..=36).contains(&nick_color(n)));
        }
    }

    /// The text as it shows on screen: colour codes removed.
    fn on_screen(s: &str) -> String {
        let mut out = String::new();
        let mut in_code = false;
        for c in s.chars() {
            match (in_code, c) {
                (false, '\x1b') => in_code = true,
                (true, 'm') => in_code = false,
                (false, c) => out.push(c),
                (true, _) => {}
            }
        }
        out
    }

    #[test]
    fn coloured_who_lines_up_like_plain() {
        let peers = Event::Peers {
            you: PeerId::test(0x7f3a_9c0d_12e4_5b68),
            nick: "annmoon".into(),
            peers: vec![PeerInfo {
                id: PeerId::test(0xa1b2_c3d4_e5f6_0718),
                nick: "bobroger".into(),
                addr: "10.0.0.2:5000".parse().unwrap(),
            }],
        };
        let coloured = event(&peers, 0, &Style { color: true }).unwrap();
        assert_ne!(coloured, event(&peers, 0, &P).unwrap(), "colour is really on");
        assert_eq!(on_screen(&coloured), event(&peers, 0, &P).unwrap());
    }

    #[test]
    fn chats_and_rooms_listings() {
        let mut store = Store::new();
        assert!(chats(&store, &P).starts_with("* no conversations yet"));
        assert!(rooms(&store, &P).starts_with("* you're not in any rooms"));
        store.apply(
            &Event::RoomJoined {
                name: "catpie".into(),
                key: "k7m9xq".into(),
                max: 13,
                members: vec!["bobroger".into()],
            },
            0,
        );
        store.switch(Conv::Room("catpie".into()));
        store.apply(
            &Event::Message { conv: Conv::Dm("bobroger".into()), from: "bobroger".into(), text: "x".into(), ts: 0 },
            0,
        );
        let c = chats(&store, &P);
        assert!(c.contains("#catPie") && c.contains("current"), "{c}");
        assert!(c.contains("@bobRoger") && c.contains("1 new") && c.contains("offline"), "{c}");
        assert!(rooms(&store, &P).contains("#catPie            2/13 · key k7m9xq · @bobRoger"));
        store.apply(&Event::JoinWaiting { name: "dogpie".into(), members: 3, needs: 1 }, 0);
        assert!(rooms(&store, &P).contains("#dogPie            waiting to be let in"), "{}", rooms(&store, &P));
    }

    #[test]
    fn join_gate_lines() {
        let name = || "catpie".to_string();
        let line = |e: Event| event(&e, 0, &P);
        assert_eq!(
            line(Event::JoinWaiting { name: name(), members: 4, needs: 2 }).unwrap(),
            "* waiting to be let into #catPie — 2 of its 4 members must approve (/leave #catPie to stop)"
        );
        assert_eq!(
            line(Event::JoinWaiting { name: name(), members: 1, needs: 1 }).unwrap(),
            "* waiting to be let into #catPie — its 1 member must approve (/leave #catPie to stop)"
        );
        let ended = |why| line(Event::JoinEnded { name: name(), why }).unwrap();
        assert_eq!(ended(JoinEnd::NoRoom), "! no room #catPie with that key");
        assert_eq!(ended(JoinEnd::Full), "! #catPie is full");
        assert_eq!(ended(JoinEnd::Denied { by: "bobroger".into() }), "! @bobRoger didn't let you into #catPie");
        assert_eq!(ended(JoinEnd::TimedOut), "! nobody let you into #catPie — try again later");
        let request = Event::JoinRequest { room: name(), nick: "bobroger".into(), have: 0, need: 1 };
        assert_eq!(
            line(request.clone()).unwrap(),
            "* @bobRoger wants to join #catPie — /approve @bobRoger or /deny @bobRoger"
        );
        assert_eq!(tone_of(&request), Tone::Warn, "flashes so it's seen from any tab");
        assert_eq!(tone_of(&Event::JoinEnded { name: name(), why: JoinEnd::Full }), Tone::Bad);
        let approved = Event::Approved { room: name(), nick: "b".into(), by: "c".into(), have: 1, need: 2 };
        assert_eq!(line(approved), None, "shown in the room only");
        assert_eq!(line(Event::RequestEnded { room: name(), nick: "b".into(), why: RequestEnd::Gone }), None);
    }

    #[test]
    fn events_have_tones() {
        let nick = || "bobroger".to_string();
        assert_eq!(tone_of(&Event::PeerJoined { id: PeerId::test(1), nick: nick() }), Tone::Good);
        assert_eq!(tone_of(&Event::MemberJoined { room: "r".into(), nick: nick() }), Tone::Good);
        assert_eq!(tone_of(&Event::RoomCreated { name: "r".into(), key: "k".into(), max: 13 }), Tone::Good);
        assert_eq!(
            tone_of(&Event::RoomJoined { name: "r".into(), key: "k7m9xq".into(), max: 13, members: vec![] }),
            Tone::Good
        );
        assert_eq!(
            tone_of(&Event::PeerLeft { id: PeerId::test(1), nick: nick(), reason: LeaveReason::Quit }),
            Tone::Bad
        );
        assert_eq!(tone_of(&Event::MemberLeft { room: "r".into(), nick: nick() }), Tone::Bad);
        assert_eq!(tone_of(&Event::RoomLeft { name: "r".into() }), Tone::Bad);
        assert_eq!(tone_of(&Event::Error("x".into())), Tone::Bad);
        assert_eq!(tone_of(&Event::Info("@bob is sending too fast; dropping messages".into())), Tone::Warn);
        assert_eq!(
            tone_of(&Event::Info("@bob's clock is 9 min off; their room messages are ignored".into())),
            Tone::Warn
        );
        assert_eq!(tone_of(&Event::Info("looking for peers…".into())), Tone::Plain);
        assert_eq!(tone_of(&Event::Peers { you: PeerId::test(1), nick: "annmoon".into(), peers: vec![] }), Tone::Plain);
        let changed = "@bobmoon has a different key from the bob you met before (last seen 2026-10-12). Compare fingerprints in /who.";
        assert_eq!(tone_of(&Event::Info(changed.into())), Tone::Warn);
        assert_eq!(tone_of(&Event::Info("first time meeting @bobroger (7f3a 9c0d 12e4 5b68)".into())), Tone::Plain);
    }

    #[test]
    fn texts_and_notices_have_tones() {
        assert_eq!(tone_of_text("! @bob is offline"), Tone::Bad);
        assert_eq!(tone_of_text("! an update check is already running"), Tone::Bad);
        assert_eq!(tone_of_text("* an update check is already running"), Tone::Warn);
        assert_eq!(tone_of_text("* installed v0.3.0. Restart corvo-lan to use it."), Tone::Good);
        assert_eq!(tone_of_text("* you have 0.2.0. Newer:"), Tone::Plain);
        assert_eq!(tone_of_notice("@carolWolf joined"), Tone::Good);
        assert_eq!(tone_of_notice("created #catPie — key 1-x — share both to invite"), Tone::Good);
        assert_eq!(tone_of_notice("joined #catPie"), Tone::Good);
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
        let online = event(&Event::PeerJoined { id: PeerId::test(1), nick: "bobroger".into() }, 0, &c).unwrap();
        assert!(online.contains("\x1b[32mis online\x1b[0m"), "{online:?}");
        let gone =
            event(&Event::PeerLeft { id: PeerId::test(1), nick: "bobroger".into(), reason: LeaveReason::Quit }, 0, &c)
                .unwrap();
        assert!(gone.contains("\x1b[31mwent offline\x1b[0m"), "{gone:?}");
        let error = event(&Event::Error("no room".into()), 0, &c).unwrap();
        assert_eq!(error, "\x1b[31m! no room\x1b[0m");
    }

    #[test]
    fn newlines_become_indented_continuations() {
        assert_eq!(body("one\ntwo"), "one\n  │ two");
        let forged = Event::Message {
            conv: Conv::Room("catpie".into()),
            from: "bobroger".into(),
            text: "hi\n[14:02] #catpie @alice: fake\r!".into(),
            ts: 0,
        };
        let out = event(&forged, 0, &P).unwrap();
        assert_eq!(out, format!("{} #catPie @bobRoger: hi\n  │ [14:02] #catPie @alice: fake\u{fffd}!", t(0)));
    }

    #[test]
    fn chats_list_mentions() {
        let mut s = Store::new();
        s.apply(&Event::Assigned { nick: "annmoon".into(), renamed: false }, 0);
        let dm = Conv::Dm("bobroger".into());
        for text in ["@annmoon a", "@annmoon b", "c"] {
            s.apply(&Event::Message { conv: dm.clone(), from: "bobroger".into(), text: text.into(), ts: 0 }, 0);
        }
        assert!(chats(&s, &P).contains("3 new, 2 mentions"), "{}", chats(&s, &P));
        assert_eq!(mention_count(1), "1 mention");
    }

    #[test]
    fn invitation_lines() {
        let invited = Event::Invited { from: "alicemoon".into(), room: "catpie".into() };
        assert_eq!(event(&invited, 0, &P).unwrap(), "* @aliceMoon invites you to #catPie — /accept or /decline");
        assert_eq!(tone_of(&invited), Tone::Warn, "it wants an answer");
        let declined = Event::InviteDeclined { by: "bobroger".into(), room: "catpie".into() };
        assert_eq!(event(&declined, 0, &P).unwrap(), "* @bobRoger declined your invite to #catPie");
    }

    #[test]
    fn a_name_taken_meanwhile_reads_like_the_other_name_lines() {
        let e = Event::Assigned { nick: "luffymoon".into(), renamed: true };
        assert_eq!(
            event(&e, 0, &P).unwrap(),
            "* your name was taken just now — you're @luffyMoon (restart to pick another name)"
        );
    }
}
