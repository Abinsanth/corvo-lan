//! Nick validation, display names and `/msg` target resolution. Pure functions.

use crate::protocol::PeerId;

pub const MAX_NICK_LEN: usize = 32;

/// A nick must be 1-32 chars with no whitespace, `#` or control characters.
pub fn validate_nick(nick: &str) -> Result<(), String> {
    if nick.is_empty() {
        return Err("nick cannot be empty".into());
    }
    if nick.chars().count() > MAX_NICK_LEN {
        return Err(format!("nick must be at most {MAX_NICK_LEN} characters"));
    }
    if nick.chars().any(|c| c.is_whitespace() || c.is_control() || is_invisible(c) || c == '#') {
        return Err("nick cannot contain spaces, '#', control or invisible characters".into());
    }
    Ok(())
}

/// Unicode default-ignorable characters (zero-width, bidi controls, tags, ...),
/// which would let `bob\u{200B}` pass as a second, identical-looking `bob`.
pub fn is_invisible(c: char) -> bool {
    matches!(c,
        '\u{00AD}' | '\u{034F}' | '\u{061C}' | '\u{115F}' | '\u{1160}' | '\u{17B4}' | '\u{17B5}'
        | '\u{180B}'..='\u{180F}' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}'
        | '\u{2060}'..='\u{206F}' | '\u{3164}' | '\u{FE00}'..='\u{FE0F}' | '\u{FEFF}' | '\u{FFA0}'
        | '\u{FFF0}'..='\u{FFF8}' | '\u{1BCA0}'..='\u{1BCA3}' | '\u{1D173}'..='\u{1D17A}'
        | '\u{E0000}'..='\u{E0FFF}')
}

/// `nick` if no other peer in `roster` shares it, otherwise `nick#abcd`.
pub fn display_name(id: PeerId, nick: &str, roster: &[(PeerId, &str)]) -> String {
    let clash = roster.iter().any(|(other, n)| *other != id && *n == nick);
    if clash {
        format!("{nick}#{}", id.short())
    } else {
        nick.to_string()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Resolve {
    Found(PeerId),
    NotFound,
    /// Several peers matched; holds their display names.
    Ambiguous(Vec<String>),
}

/// Resolves a `/msg` target: an exact nick, or `nick#hexprefix`.
pub fn resolve(query: &str, roster: &[(PeerId, &str)]) -> Resolve {
    let (nick, prefix) = match query.split_once('#') {
        Some((n, p)) => (n, Some(p.to_ascii_lowercase())),
        None => (query, None),
    };
    let matches: Vec<PeerId> = roster
        .iter()
        .filter(|(id, n)| {
            *n == nick
                && prefix
                    .as_deref()
                    .is_none_or(|p| !p.is_empty() && id.to_string().starts_with(p))
        })
        .map(|(id, _)| *id)
        .collect();
    match matches.as_slice() {
        [] => Resolve::NotFound,
        [one] => Resolve::Found(*one),
        many => {
            let mut names: Vec<String> = many
                .iter()
                .map(|id| display_name(*id, nick, roster))
                .collect();
            names.sort();
            Resolve::Ambiguous(names)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: PeerId = PeerId(0xaaaa_0000_0000_0001);
    const B1: PeerId = PeerId(0xb111_0000_0000_0002);
    const B2: PeerId = PeerId(0xb222_0000_0000_0003);

    fn roster() -> Vec<(PeerId, &'static str)> {
        vec![(A, "alice"), (B1, "bob"), (B2, "bob")]
    }

    #[test]
    fn valid_and_invalid_nicks() {
        assert!(validate_nick("alice").is_ok());
        assert!(validate_nick("élan_42").is_ok());
        assert!(validate_nick("").is_err());
        assert!(validate_nick("bob smith").is_err());
        assert!(validate_nick("bob#1").is_err());
        assert!(validate_nick("bad\u{1b}[31m").is_err());
        assert!(validate_nick(&"x".repeat(33)).is_err());
        assert!(validate_nick(&"x".repeat(32)).is_ok());
    }

    #[test]
    fn invisible_characters_are_rejected_in_nicks() {
        for nick in ["bob\u{200B}", "\u{202E}evil", "\u{FEFF}x", "a\u{00AD}b", "a\u{2066}b", "ab\u{E0041}"] {
            assert!(validate_nick(nick).is_err(), "{nick:?} should be rejected");
        }
    }

    #[test]
    fn display_name_adds_suffix_only_on_clash() {
        let r = roster();
        assert_eq!(display_name(A, "alice", &r), "alice");
        assert_eq!(display_name(B1, "bob", &r), "bob#b111");
        assert_eq!(display_name(B2, "bob", &r), "bob#b222");
    }

    #[test]
    fn resolve_unique_nick() {
        assert_eq!(resolve("alice", &roster()), Resolve::Found(A));
    }

    #[test]
    fn resolve_unknown_nick() {
        assert_eq!(resolve("carol", &roster()), Resolve::NotFound);
        assert_eq!(resolve("Alice", &roster()), Resolve::NotFound);
    }

    #[test]
    fn resolve_ambiguous_lists_candidates() {
        assert_eq!(
            resolve("bob", &roster()),
            Resolve::Ambiguous(vec!["bob#b111".into(), "bob#b222".into()])
        );
    }

    #[test]
    fn resolve_with_id_prefix() {
        assert_eq!(resolve("bob#b222", &roster()), Resolve::Found(B2));
        assert_eq!(resolve("bob#B1", &roster()), Resolve::Found(B1));
        assert_eq!(resolve("bob#ffff", &roster()), Resolve::NotFound);
        assert_eq!(resolve("bob#", &roster()), Resolve::NotFound);
    }
}

// ---- v0.2: system-assigned names. The v0.1 helpers above are removed in Task 4. ----

use rand::seq::SliceRandom;

use crate::words::WORDS;

pub const MAX_BASE_LEN: usize = 9;

/// Command names and a few words that would read as commands or as "everyone".
pub const RESERVED: [&str; 16] = [
    "msg", "chat", "chats", "create", "leave", "rooms", "history", "who", "tui", "line", "help", "quit",
    "exit", "me", "all", "system",
];

/// A base is what the user types: 1–9 ASCII letters or digits, not reserved.
pub fn validate_base(base: &str) -> Result<(), String> {
    if base.is_empty() || base.len() > MAX_BASE_LEN || !base.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err(format!("name must be 1–{MAX_BASE_LEN} letters or digits"));
    }
    if RESERVED.iter().any(|r| r.eq_ignore_ascii_case(base)) {
        return Err(format!("'{base}' is reserved"));
    }
    Ok(())
}

/// `bobRoger`: a valid base followed by a listed word.
pub fn is_valid_nick(name: &str) -> bool {
    WORDS
        .iter()
        .any(|w| name.strip_suffix(w).is_some_and(|base| validate_base(base).is_ok()))
}

/// `catPieR`: a valid nick shape followed by `R`.
pub fn is_valid_room(name: &str) -> bool {
    name.strip_suffix('R').is_some_and(is_valid_nick)
}

/// `base + Word` for the first shuffled word whose name isn't taken.
pub fn pick_nick(base: &str, taken: impl Fn(&str) -> bool) -> Option<String> {
    pick(base, "", taken)
}

/// `base + Word + "R"` for the first shuffled word whose name isn't taken.
pub fn pick_room(base: &str, taken: impl Fn(&str) -> bool) -> Option<String> {
    pick(base, "R", taken)
}

fn pick(base: &str, tail: &str, taken: impl Fn(&str) -> bool) -> Option<String> {
    let mut words = WORDS;
    words.shuffle(&mut rand::rng());
    words.iter().map(|w| format!("{base}{w}{tail}")).find(|name| !taken(name))
}

#[cfg(test)]
mod v02_tests {
    use super::*;

    #[test]
    fn bases_are_short_alphanumeric_and_not_reserved() {
        assert!(validate_base("bob").is_ok());
        assert!(validate_base("Team42").is_ok());
        assert!(validate_base("abcdefghi").is_ok());
        assert_eq!(validate_base("abcdefghij"), Err("name must be 1–9 letters or digits".into()));
        assert!(validate_base("").is_err());
        assert!(validate_base("bob smith").is_err());
        assert!(validate_base("bob#").is_err());
        assert!(validate_base("élan").is_err());
        assert_eq!(validate_base("Chat"), Err("'Chat' is reserved".into()));
        assert!(validate_base("exit").is_err());
    }

    #[test]
    fn nick_and_room_shapes() {
        assert!(is_valid_nick("bobRoger"));
        assert!(is_valid_nick("b2Fox"));
        assert!(!is_valid_nick("bob")); // no word
        assert!(!is_valid_nick("Roger")); // empty base
        assert!(!is_valid_nick("bobZzz")); // not a listed word
        assert!(!is_valid_nick("chatRoger")); // reserved base
        assert!(!is_valid_nick("abcdefghijRoger")); // base too long
        assert!(!is_valid_nick("bobPieR")); // that's a room
        assert!(is_valid_room("catPieR"));
        assert!(!is_valid_room("catPie"));
        assert!(!is_valid_room("catR"));
    }

    #[test]
    fn picked_names_have_the_right_shape_and_avoid_taken_ones() {
        let nick = pick_nick("bob", |_| false).unwrap();
        assert!(nick.starts_with("bob") && is_valid_nick(&nick), "{nick}");
        let room = pick_room("cat", |_| false).unwrap();
        assert!(room.starts_with("cat") && is_valid_room(&room), "{room}");

        // Everything but bobRoger is taken, so that's the only pick.
        assert_eq!(pick_nick("bob", |n| n != "bobRoger"), Some("bobRoger".into()));
        assert_eq!(pick_nick("bob", |_| true), None);
    }
}
