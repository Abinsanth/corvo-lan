//! Tab completion for `@names`, `#rooms` and `/commands`. Pure; both
//! frontends use it.

use std::collections::BTreeSet;

use crate::api::Conv;
use crate::convo::Store;
use crate::ui::input::COMMANDS;

/// What can be completed right now.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Candidates {
    /// Online people except me: the current room's members first, then the rest.
    pub nicks: Vec<String>,
    /// Joined rooms.
    pub rooms: Vec<String>,
    /// People asking to join any of my rooms, for `/approve` and `/deny`.
    pub waiting: Vec<String>,
}

impl Candidates {
    pub fn from_store(store: &Store) -> Self {
        let by_name = |v: &mut Vec<String>| v.sort_by_key(|n| n.to_lowercase());
        let mut members: Vec<String> = match store.current() {
            Some(conv @ Conv::Room(_)) => {
                store.get(conv).map(|c| c.members.iter().cloned().collect()).unwrap_or_default()
            }
            _ => Vec::new(),
        };
        by_name(&mut members);
        let mut rest: Vec<String> = store.online().filter(|n| !members.contains(n)).cloned().collect();
        by_name(&mut rest);
        let me = store.me();
        let nicks = members.into_iter().chain(rest).filter(|n| Some(n.as_str()) != me).collect();
        let mut rooms: Vec<String> = store
            .conversations()
            .iter()
            .filter_map(|c| match &c.conv {
                Conv::Room(n) => Some(n.clone()),
                Conv::Dm(_) => None,
            })
            .collect();
        by_name(&mut rooms);
        let waiting: BTreeSet<&String> = store.conversations().iter().flat_map(|c| c.waiting.keys()).collect();
        let mut waiting: Vec<String> = waiting.into_iter().cloned().collect();
        by_name(&mut waiting);
        Candidates { nicks, rooms, waiting }
    }
}

/// The word being completed and what it could become.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    /// Byte offset in the text where the word starts.
    pub start: usize,
    /// Full replacements for the word, sigil included.
    pub matches: Vec<String>,
}

/// Where the word that ends `before` starts (a byte offset).
pub fn token_start(before: &str) -> usize {
    before.char_indices().rev().find(|(_, c)| c.is_whitespace()).map_or(0, |(i, c)| i + c.len_utf8())
}

/// Completes the word just before the cursor; `before` is all the input up
/// to the cursor. `/` completes only as the first word, and `//` never.
pub fn complete(before: &str, cands: &Candidates) -> Option<Completion> {
    let start = token_start(before);
    let token = &before[start..];
    let sigil = token.chars().next()?;
    let part = token[sigil.len_utf8()..].to_lowercase();
    let pool: Vec<&str> = match sigil {
        '@' if before.starts_with("/approve ") || before.starts_with("/deny ") => {
            cands.waiting.iter().map(String::as_str).collect()
        }
        '@' => cands.nicks.iter().map(String::as_str).collect(),
        '#' => cands.rooms.iter().map(String::as_str).collect(),
        '/' if before[..start].trim().is_empty() && !part.starts_with('/') => {
            let mut names = COMMANDS.to_vec();
            names.sort_unstable();
            names
        }
        _ => return None,
    };
    let matches: Vec<String> =
        pool.into_iter().filter(|n| n.to_lowercase().starts_with(&part)).map(|n| format!("{sigil}{n}")).collect();
    (!matches.is_empty()).then_some(Completion { start, matches })
}

/// The longest start all `matches` share, compared char by char.
pub fn common_prefix(matches: &[String]) -> String {
    let Some(first) = matches.first() else {
        return String::new();
    };
    let mut len = first.chars().count();
    for m in &matches[1..] {
        len = len.min(first.chars().zip(m.chars()).take_while(|(a, b)| a == b).count());
    }
    first.chars().take(len).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Event, PeerId};

    fn cands() -> Candidates {
        Candidates {
            nicks: vec!["bobroger".into(), "bobkite".into(), "carolwolf".into()],
            rooms: vec!["catpie".into()],
            waiting: vec!["daveoak".into()],
        }
    }

    fn matches(before: &str) -> Vec<String> {
        complete(before, &cands()).map(|c| c.matches).unwrap_or_default()
    }

    #[test]
    fn names_and_rooms_complete_by_prefix_ignoring_case() {
        assert_eq!(
            complete("hi @BO", &cands()),
            Some(Completion { start: 3, matches: vec!["@bobroger".into(), "@bobkite".into()] })
        );
        assert_eq!(matches("#c"), vec!["#catpie"]);
        assert_eq!(matches("@").len(), 3, "a bare sigil lists everyone");
        assert_eq!(complete("@zed", &cands()), None);
        assert_eq!(complete("hello", &cands()), None);
        assert_eq!(complete("hello ", &cands()), None);
        assert_eq!(complete("", &cands()), None);
        assert_eq!(complete("line one\n@car", &cands()).unwrap().start, 9);
    }

    #[test]
    fn approve_and_deny_complete_who_is_waiting() {
        assert_eq!(matches("/approve @"), vec!["@daveoak"]);
        assert_eq!(matches("/deny @d"), vec!["@daveoak"]);
        assert_eq!(matches("/approve @b"), Vec::<String>::new(), "only people who asked");
        assert_eq!(matches("/msg @d"), Vec::<String>::new(), "elsewhere it's everyone online");
        assert_eq!(matches("/invite #c"), vec!["#catpie"]);
        assert_eq!(matches("/inv"), vec!["/invite"]);
    }

    #[test]
    fn commands_complete_only_as_the_first_word() {
        assert_eq!(matches("/ch"), vec!["/chat", "/chats"]);
        assert_eq!(
            complete("  /q", &cands()),
            Some(Completion { start: 2, matches: vec!["/q".into(), "/quit".into()] })
        );
        assert_eq!(complete("hi /ch", &cands()), None);
        assert_eq!(complete("line one\n/ch", &cands()), None);
        assert_eq!(complete("//ch", &cands()), None);
    }

    #[test]
    fn the_word_starts_after_any_whitespace() {
        assert_eq!(token_start("@b"), 0);
        assert_eq!(token_start("a\n@b"), 2);
        assert_eq!(token_start("é @b"), 3, "é is two bytes");
        assert_eq!(token_start("a\u{3000}@b"), 4, "multi-byte whitespace");
    }

    #[test]
    fn common_prefix_is_char_wise() {
        assert_eq!(common_prefix(&["@bobroger".into(), "@bobkite".into()]), "@bob");
        assert_eq!(common_prefix(&["/q".into()]), "/q");
        assert_eq!(common_prefix(&[]), "");
    }

    #[test]
    fn candidates_put_room_members_first_and_leave_me_out() {
        let mut s = Store::new();
        s.apply(&Event::Assigned { nick: "annmoon".into(), renamed: false }, 0);
        for (i, n) in ["annmoon", "zoeowl", "carolwolf", "bobroger"].iter().enumerate() {
            s.apply(&Event::PeerJoined { id: PeerId::test(i as u64), nick: n.to_string() }, 0);
        }
        s.apply(
            &Event::RoomJoined { name: "dogR".into(), key: "k7m9xq".into(), max: 13, members: vec!["zoeowl".into()] },
            0,
        );
        s.apply(&Event::RoomJoined { name: "catR".into(), key: "k7m9xq".into(), max: 13, members: vec![] }, 0);
        s.switch(Conv::Room("dogR".into()));
        for (room, nick) in [("dogR", "eveowl"), ("catR", "daveoak"), ("dogR", "daveoak")] {
            s.apply(&Event::JoinRequest { room: room.into(), nick: nick.into(), have: 0, need: 1 }, 0);
        }
        let c = Candidates::from_store(&s);
        assert_eq!(c.nicks, vec!["zoeowl", "bobroger", "carolwolf"]);
        assert_eq!(c.waiting, vec!["daveoak", "eveowl"], "everyone waiting in any of my rooms, once");
        assert_eq!(c.rooms, vec!["catR", "dogR"]);
    }
}
