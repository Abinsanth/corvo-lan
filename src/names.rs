//! System-assigned names. People are `base + Word` (`bobRoger`), rooms are
//! `base + Word + "R"` (`catPieR`). `@`/`#` are display and command syntax only.

use rand::seq::SliceRandom;

use crate::words::WORDS;

pub const MAX_BASE_LEN: usize = 9;

/// Command names and a few words that would read as commands or as "everyone".
pub const RESERVED: [&str; 19] = [
    "msg", "chat", "chats", "create", "approve", "deny", "invite", "leave", "rooms", "history", "who", "tui", "line",
    "help", "quit", "exit", "me", "all", "system",
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
    WORDS.iter().any(|w| name.strip_suffix(w).is_some_and(|base| validate_base(base).is_ok()))
}

/// `catPieR`: a valid nick shape followed by `R`.
pub fn is_valid_room(name: &str) -> bool {
    name.strip_suffix('R').is_some_and(is_valid_nick)
}

/// `bobRoger` → `bob`: the part the user chose. `None` if `nick` isn't a valid nick.
pub fn base_of(nick: &str) -> Option<&str> {
    WORDS.iter().find_map(|w| nick.strip_suffix(w).filter(|base| validate_base(base).is_ok()))
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

/// The base used when `--nick` isn't given: the login name's letters and
/// digits, cut to `MAX_BASE_LEN`, or `anon` if nothing usable is left.
pub fn default_base(login: Option<&str>) -> String {
    let base: String =
        login.unwrap_or_default().chars().filter(char::is_ascii_alphanumeric).take(MAX_BASE_LEN).collect();
    if validate_base(&base).is_ok() { base } else { "anon".into() }
}

#[cfg(test)]
mod tests {
    #[test]
    fn default_base_makes_any_login_usable() {
        assert_eq!(default_base(Some("abinsanth")), "abinsanth");
        assert_eq!(default_base(Some("John Smith")), "JohnSmith");
        assert_eq!(default_base(Some("j.doe")), "jdoe");
        assert_eq!(default_base(Some("christopher")), "christoph");
        assert_eq!(default_base(Some("José")), "Jos");
        assert_eq!(default_base(Some("help")), "anon", "reserved");
        assert_eq!(default_base(Some("...")), "anon");
        assert_eq!(default_base(None), "anon");
    }

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
        for cmd in ["approve", "Deny", "invite"] {
            assert!(validate_base(cmd).is_err(), "{cmd}");
        }
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
    fn base_is_the_nick_without_its_word() {
        assert_eq!(base_of("bobRoger"), Some("bob"));
        assert_eq!(base_of("Team42Moon"), Some("Team42"));
        assert_eq!(base_of("bob"), None);
        assert_eq!(base_of("Roger"), None, "no base left");
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
