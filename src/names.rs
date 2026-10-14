//! System-assigned names: `base + word`, all lowercase. People (`bobroger`)
//! and rooms (`catpie`) share the shape; `@`/`#` tell them apart. They're
//! shown as `bobRoger` and `catPie` so you can see where the word starts.

use rand::seq::SliceRandom;

use crate::words::WORDS;

pub const MAX_BASE_LEN: usize = 9;

/// Command names and a few words that would read as commands or as "everyone".
pub const RESERVED: [&str; 21] = [
    "msg", "chat", "chats", "create", "approve", "deny", "invite", "leave", "rooms", "history", "who", "tui", "line",
    "help", "quit", "exit", "me", "all", "system", "accept", "decline",
];

/// A base is what the user types, lowercased: 1–9 ASCII letters or digits, not reserved.
pub fn validate_base(base: &str) -> Result<(), String> {
    if base.is_empty()
        || base.len() > MAX_BASE_LEN
        || !base.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    {
        return Err(format!("name must be 1–{MAX_BASE_LEN} letters or digits"));
    }
    if RESERVED.contains(&base) {
        return Err(format!("'{base}' is reserved"));
    }
    Ok(())
}

/// `bobroger` or `catpie`: a valid base followed by a listed word. No word
/// ends another, so there's only one way to split it.
pub fn is_valid_name(name: &str) -> bool {
    base_of(name).is_some()
}

/// `bobroger` → `bob`: the part the user chose. `None` if `name` isn't a valid name.
pub fn base_of(name: &str) -> Option<&str> {
    WORDS.iter().find_map(|w| name.strip_suffix(w).filter(|base| validate_base(base).is_ok()))
}

/// `bobroger` → `bobRoger`: how a name is shown. Only the screen sees this;
/// the wire, room keys and lookups keep the lowercase name. Anything that
/// isn't a valid name comes back as it is.
pub fn display(name: &str) -> String {
    match base_of(name) {
        Some(base) => {
            let word = &name[base.len()..];
            format!("{base}{}{}", word[..1].to_ascii_uppercase(), &word[1..])
        }
        None => name.to_string(),
    }
}

/// `text` with every `@name` and `#name` in it shown as [`display`] does.
/// Only whole words count: not `a@bobroger` or `@bobrogers`.
pub fn display_in(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find(['@', '#']) {
        out.push_str(&rest[..i]);
        let after_word = out.chars().next_back().is_some_and(char::is_alphanumeric);
        out.push_str(&rest[i..=i]);
        rest = &rest[i + 1..];
        let len = rest.find(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit())).unwrap_or(rest.len());
        let (name, tail) = rest.split_at(len);
        if !after_word && !tail.starts_with(char::is_alphanumeric) && is_valid_name(name) {
            out.push_str(&display(name));
        } else {
            out.push_str(name);
        }
        rest = tail;
    }
    out.push_str(rest);
    out
}

/// `base + word` for the first shuffled word whose name isn't taken. The
/// word is never the base itself: `ace` gets `aceluffy`, not `aceace`.
pub fn pick_name(base: &str, taken: impl Fn(&str) -> bool) -> Option<String> {
    let mut words = WORDS;
    words.shuffle(&mut rand::rng());
    words.iter().filter(|w| **w != base).map(|w| format!("{base}{w}")).find(|name| !taken(name))
}

/// The base used when `--nick` isn't given: the login name's letters and
/// digits, lowercased and cut to `MAX_BASE_LEN`, or `anon` if nothing usable is left.
pub fn default_base(login: Option<&str>) -> String {
    let base: String = login
        .unwrap_or_default()
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .take(MAX_BASE_LEN)
        .collect();
    if validate_base(&base).is_ok() { base } else { "anon".into() }
}

#[cfg(test)]
mod tests {
    #[test]
    fn default_base_makes_any_login_usable() {
        assert_eq!(default_base(Some("abinsanth")), "abinsanth");
        assert_eq!(default_base(Some("John Smith")), "johnsmith");
        assert_eq!(default_base(Some("j.doe")), "jdoe");
        assert_eq!(default_base(Some("christopher")), "christoph");
        assert_eq!(default_base(Some("José")), "jos");
        assert_eq!(default_base(Some("HELP")), "anon", "reserved");
        assert_eq!(default_base(Some("...")), "anon");
        assert_eq!(default_base(None), "anon");
    }

    use super::*;

    #[test]
    fn bases_are_short_lowercase_alphanumeric_and_not_reserved() {
        assert!(validate_base("bob").is_ok());
        assert!(validate_base("team42").is_ok());
        assert!(validate_base("abcdefghi").is_ok());
        assert_eq!(validate_base("abcdefghij"), Err("name must be 1–9 letters or digits".into()));
        assert!(validate_base("").is_err());
        assert!(validate_base("Bob").is_err(), "callers lowercase what's typed");
        assert!(validate_base("bob smith").is_err());
        assert!(validate_base("bob#").is_err());
        assert!(validate_base("élan").is_err());
        assert_eq!(validate_base("chat"), Err("'chat' is reserved".into()));
        assert!(validate_base("exit").is_err());
        for cmd in ["approve", "deny", "invite", "accept", "decline"] {
            assert!(validate_base(cmd).is_err(), "{cmd}");
        }
    }

    #[test]
    fn name_shapes() {
        assert!(is_valid_name("bobroger"));
        assert!(is_valid_name("catpie"));
        assert!(is_valid_name("b2wolf"));
        assert!(!is_valid_name("bobRoger"), "names are all lowercase");
        assert!(!is_valid_name("BOBroger"));
        assert!(!is_valid_name("bob")); // no word
        assert!(!is_valid_name("roger")); // empty base
        assert!(!is_valid_name("bobzzz")); // not a listed word
        assert!(!is_valid_name("chatroger")); // reserved base
        assert!(!is_valid_name("abcdefghijroger")); // base too long
        assert!(!is_valid_name("catpier"), "rooms have no R any more");
    }

    #[test]
    fn base_is_the_name_without_its_word() {
        assert_eq!(base_of("bobroger"), Some("bob"));
        assert_eq!(base_of("team42moon"), Some("team42"));
        assert_eq!(base_of("grapegrape"), Some("grape"));
        assert_eq!(base_of("bob"), None);
        assert_eq!(base_of("roger"), None, "no base left");
    }

    #[test]
    fn names_show_with_the_word_capitalised() {
        assert_eq!(display("bobroger"), "bobRoger");
        assert_eq!(display("catpie"), "catPie");
        assert_eq!(display("team42moon"), "team42Moon");
        assert_eq!(display("grapegrape"), "grapeGrape");
        assert_eq!(display("bob"), "bob", "not a name, left alone");
        assert_eq!(display("bobRoger"), "bobRoger");
    }

    #[test]
    fn names_inside_text_show_the_same_way() {
        assert_eq!(display_in("@bobroger joined #catpie"), "@bobRoger joined #catPie");
        assert_eq!(display_in("/approve @bobroger or /deny @bobroger"), "/approve @bobRoger or /deny @bobRoger");
        assert_eq!(display_in("(/leave #catpie to stop)"), "(/leave #catPie to stop)");
        assert_eq!(display_in("hi @bobroger, you there?"), "hi @bobRoger, you there?");
        assert_eq!(display_in("mail a@bobroger"), "mail a@bobroger", "not after a word");
        assert_eq!(display_in("@bobrogers"), "@bobrogers", "not a whole name");
        assert_eq!(display_in("@bobrogerX"), "@bobrogerX");
        assert_eq!(display_in("@bob and #1 and # and @"), "@bob and #1 and # and @");
        assert_eq!(display_in("@@bobroger é#catpie"), "@@bobRoger é#catpie");
        assert_eq!(display_in(&display_in("@bobroger")), "@bobRoger", "twice is the same as once");
    }

    #[test]
    fn picked_names_have_the_right_shape_and_avoid_taken_ones() {
        let name = pick_name("bob", |_| false).unwrap();
        assert!(name.starts_with("bob") && is_valid_name(&name), "{name}");

        // Everything but bobroger is taken, so that's the only pick.
        assert_eq!(pick_name("bob", |n| n != "bobroger"), Some("bobroger".into()));
        assert_eq!(pick_name("bob", |_| true), None);
    }

    #[test]
    fn a_base_never_gets_itself_as_its_word() {
        for _ in 0..50 {
            assert_ne!(pick_name("ace", |_| false).as_deref(), Some("aceace"));
        }
        assert_eq!(pick_name("ace", |n| n != "aceace"), None, "aceace is never offered");
    }
}
