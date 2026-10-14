//! Parses a typed line. Pure: knows nothing about the current conversation.

#[derive(Debug, PartialEq, Eq)]
pub enum Input {
    /// Plain text for the current conversation.
    Text(String),
    Msg {
        to: String,
        text: String,
    },
    ChatDm(String),
    ChatRoom {
        name: String,
        key: Option<String>,
    },
    /// `/chat bob`: a name without `@` or `#`, resolved against who's around.
    ChatGuess(String),
    /// `/msg bob text`: a name without `@`, resolved against who's online.
    MsgGuess {
        to: String,
        text: String,
    },
    /// `max` is the head count as typed, if given.
    Create {
        base: String,
        max: Option<String>,
    },
    /// Let someone into a room; `room` if typed, else worked out from who's waiting where.
    Approve {
        nick: String,
        room: Option<String>,
    },
    Deny {
        nick: String,
        room: Option<String>,
    },
    /// Copy the join line for the current or named room.
    Invite(Option<String>),
    Leave(Option<String>),
    Rooms,
    Chats,
    /// `/update` checks; `/update yes` installs the newest release.
    Update {
        confirm: bool,
    },
    /// Close the current or named DM.
    Close(Option<String>),
    Who,
    Help,
    Quit,
    Empty,
    Error(String),
}

const SIGIL_HINT: &str = "use @name for people or #name for rooms";

/// Every command name `parse_line` accepts, aliases included. Completion and
/// typo hints read it, so keep it in step with the match below.
pub const COMMANDS: &[&str] = &[
    "msg", "m", "chat", "c", "create", "invite", "approve", "deny", "leave", "close", "rooms", "chats", "who",
    "update", "help", "h", "?", "quit", "q",
];

/// Names that were dropped, and the command that replaced each.
const RENAMED: &[(&str, &str)] = &[("exit", "quit")];

pub fn parse_line(line: &str) -> Input {
    if line.trim().is_empty() {
        return Input::Empty;
    }
    if let Some(escaped) = line.strip_prefix("//") {
        return Input::Text(format!("/{escaped}"));
    }
    let Some(rest) = line.strip_prefix('/') else {
        return Input::Text(line.to_string());
    };
    let (name, args) = match rest.split_once(char::is_whitespace) {
        Some((name, args)) => (name, args.trim()),
        None => (rest, ""),
    };
    let words: Vec<&str> = args.split_whitespace().collect();
    match name {
        "msg" | "m" => match args.split_once(char::is_whitespace) {
            Some((to, text)) if !text.trim().is_empty() => {
                let text = text.trim_start().to_string();
                match to.strip_prefix('@') {
                    Some(nick) => Input::Msg { to: nick.to_string(), text },
                    None if to.starts_with('#') => Input::Error(SIGIL_HINT.into()),
                    None => Input::MsgGuess { to: to.to_string(), text },
                }
            }
            _ => Input::Error("usage: /msg @nick <text>".into()),
        },
        "chat" | "c" => match words.as_slice() {
            [target] if target.starts_with('@') => Input::ChatDm(target[1..].to_string()),
            [target] if target.starts_with('#') => Input::ChatRoom { name: target[1..].to_string(), key: None },
            [target, key] if target.starts_with('#') => {
                Input::ChatRoom { name: target[1..].to_string(), key: Some(key.to_string()) }
            }
            [target] => Input::ChatGuess(target.to_string()),
            [_, _] => Input::Error(SIGIL_HINT.into()),
            _ => Input::Error("usage: /chat @nick | /chat #room [key]".into()),
        },
        "create" => match words.as_slice() {
            [base] => Input::Create { base: base.to_string(), max: None },
            [base, max] => Input::Create { base: base.to_string(), max: Some(max.to_string()) },
            _ => Input::Error("usage: /create <name> [people]".into()),
        },
        "approve" | "deny" => {
            let (nick, room) = match words.as_slice() {
                [nick] => (nick.strip_prefix('@'), None),
                [nick, room] => match room.strip_prefix('#') {
                    Some(room) => (nick.strip_prefix('@'), Some(room.to_string())),
                    None => return Input::Error(SIGIL_HINT.into()),
                },
                _ => return Input::Error(format!("usage: /{name} @nick [#room]")),
            };
            let Some(nick) = nick else {
                return Input::Error(SIGIL_HINT.into());
            };
            let nick = nick.to_string();
            if name == "approve" { Input::Approve { nick, room } } else { Input::Deny { nick, room } }
        }
        "invite" => match words.as_slice() {
            [] => Input::Invite(None),
            [room] if room.starts_with('#') => Input::Invite(Some(room[1..].to_string())),
            _ => Input::Error("usage: /invite [#room]".into()),
        },
        "leave" => match words.as_slice() {
            [] => Input::Leave(None),
            [room] if room.starts_with('#') => Input::Leave(Some(room[1..].to_string())),
            _ => Input::Error("usage: /leave [#room]".into()),
        },
        "update" => match words.as_slice() {
            [] => Input::Update { confirm: false },
            ["yes"] => Input::Update { confirm: true },
            _ => Input::Error("usage: /update [yes]".into()),
        },
        "close" => match words.as_slice() {
            [] => Input::Close(None),
            [nick] if nick.starts_with('@') => Input::Close(Some(nick[1..].to_string())),
            [room] if room.starts_with('#') => Input::Error("use /leave for rooms".into()),
            _ => Input::Error("usage: /close [@nick]".into()),
        },
        "rooms" => Input::Rooms,
        "chats" => Input::Chats,
        "who" => Input::Who,
        "help" | "h" | "?" => Input::Help,
        "quit" | "q" => Input::Quit,
        "line" | "tui" => Input::Error("/line is gone — corvo-lan always uses the full-screen view".into()),
        "his" | "history" => Input::Error("/his is gone — scroll with PgUp/PgDn or the mouse wheel".into()),
        "nick" => Input::Error("names are assigned by the system and can't be changed".into()),
        other => Input::Error(unknown(other)),
    }
}

/// "unknown command", with a "did you mean" when a known name is close.
fn unknown(name: &str) -> String {
    match suggest(name) {
        Some(known) => format!("unknown command /{name} — did you mean /{known}?"),
        None => format!("unknown command /{name}, try /help"),
    }
}

/// A dropped name's replacement, or the closest command within 2 edits (and
/// closer than the name is long, so one letter doesn't guess). Ties go to the
/// earlier entry in `COMMANDS`.
fn suggest(name: &str) -> Option<&'static str> {
    if let Some((_, new)) = RENAMED.iter().find(|(old, _)| *old == name) {
        return Some(new);
    }
    let len = name.chars().count();
    COMMANDS
        .iter()
        .map(|c| (distance(name, c), *c))
        .filter(|(d, _)| *d <= 2 && *d < len)
        .min_by_key(|(d, _)| *d)
        .map(|(_, c)| c)
}

/// Levenshtein distance in chars.
fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut diag = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let next = (diag + usize::from(ca != *cb)).min(row[j] + 1).min(row[j + 1] + 1);
            diag = row[j + 1];
            row[j + 1] = next;
        }
    }
    row[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(s: &str) -> Input {
        Input::Error(s.into())
    }

    #[test]
    fn plain_text_and_escapes() {
        assert_eq!(parse_line("hello there"), Input::Text("hello there".into()));
        assert_eq!(parse_line("   "), Input::Empty);
        assert_eq!(parse_line("//shrug"), Input::Text("/shrug".into()));
    }

    #[test]
    fn msg_needs_text_and_guesses_a_bare_name() {
        assert_eq!(
            parse_line("/msg @bobRoger hi  there"),
            Input::Msg { to: "bobRoger".into(), text: "hi  there".into() }
        );
        assert_eq!(parse_line("/m @bobRoger hi"), Input::Msg { to: "bobRoger".into(), text: "hi".into() });
        assert_eq!(parse_line("/msg bob hi"), Input::MsgGuess { to: "bob".into(), text: "hi".into() });
        assert_eq!(parse_line("/msg #catPieR hi"), err(SIGIL_HINT));
        assert_eq!(parse_line("/msg @bobRoger"), err("usage: /msg @nick <text>"));
        assert_eq!(parse_line("/m"), err("usage: /msg @nick <text>"));
    }

    #[test]
    fn chat_switches_joins_or_guesses() {
        assert_eq!(parse_line("/chat @bobRoger"), Input::ChatDm("bobRoger".into()));
        assert_eq!(parse_line("/c @bobRoger"), Input::ChatDm("bobRoger".into()));
        assert_eq!(parse_line("/chat #catPieR"), Input::ChatRoom { name: "catPieR".into(), key: None });
        assert_eq!(
            parse_line("/chat #catPieR 123-k7m9xq"),
            Input::ChatRoom { name: "catPieR".into(), key: Some("123-k7m9xq".into()) }
        );
        assert_eq!(parse_line("/chat bob"), Input::ChatGuess("bob".into()));
        assert_eq!(parse_line("/chat bob key"), err(SIGIL_HINT));
        assert_eq!(parse_line("/chat @bob key"), err(SIGIL_HINT));
        assert_eq!(parse_line("/c"), err("usage: /chat @nick | /chat #room [key]"));
    }

    #[test]
    fn room_commands() {
        assert_eq!(parse_line("/create cat"), Input::Create { base: "cat".into(), max: None });
        assert_eq!(parse_line("/create cat 5"), Input::Create { base: "cat".into(), max: Some("5".into()) });
        assert_eq!(parse_line("/create"), err("usage: /create <name> [people]"));
        assert_eq!(parse_line("/create cat 5 6"), err("usage: /create <name> [people]"));
        assert_eq!(parse_line("/leave"), Input::Leave(None));
        assert_eq!(parse_line("/leave #catPieR"), Input::Leave(Some("catPieR".into())));
        assert_eq!(parse_line("/leave catPieR"), err("usage: /leave [#room]"));
        assert_eq!(parse_line("/rooms"), Input::Rooms);
    }

    #[test]
    fn invite_takes_an_optional_room() {
        assert_eq!(parse_line("/invite"), Input::Invite(None));
        assert_eq!(parse_line("/invite #catPieR"), Input::Invite(Some("catPieR".into())));
        assert_eq!(parse_line("/invite catPieR"), err("usage: /invite [#room]"));
    }

    #[test]
    fn approval_commands() {
        let bob = || "bobRoger".to_string();
        assert_eq!(parse_line("/approve @bobRoger"), Input::Approve { nick: bob(), room: None });
        assert_eq!(
            parse_line("/approve @bobRoger #catPieR"),
            Input::Approve { nick: bob(), room: Some("catPieR".into()) }
        );
        assert_eq!(parse_line("/deny @bobRoger"), Input::Deny { nick: bob(), room: None });
        assert_eq!(parse_line("/deny @bobRoger #catPieR"), Input::Deny { nick: bob(), room: Some("catPieR".into()) });
        assert_eq!(parse_line("/deny bob"), err(SIGIL_HINT));
        assert_eq!(parse_line("/approve @bob catPieR"), err(SIGIL_HINT));
        assert_eq!(parse_line("/approve"), err("usage: /approve @nick [#room]"));
        assert_eq!(parse_line("/deny"), err("usage: /deny @nick [#room]"));
    }

    #[test]
    fn history_and_the_rest() {
        assert_eq!(parse_line("/his"), err("/his is gone — scroll with PgUp/PgDn or the mouse wheel"));
        assert_eq!(parse_line("/history 5"), err("/his is gone — scroll with PgUp/PgDn or the mouse wheel"));
        assert_eq!(parse_line("/chats"), Input::Chats);
        assert_eq!(parse_line("/who"), Input::Who);
        for help in ["/help", "/h", "/?"] {
            assert_eq!(parse_line(help), Input::Help, "{help}");
        }
        assert_eq!(parse_line("/quit"), Input::Quit);
        assert_eq!(parse_line("/q"), Input::Quit);
        assert_eq!(parse_line("/nick bob"), err("names are assigned by the system and can't be changed"));
        assert_eq!(parse_line("/dance"), err("unknown command /dance, try /help"));
    }

    #[test]
    fn removed_and_mistyped_commands_get_a_hint() {
        assert_eq!(parse_line("/exit"), err("unknown command /exit — did you mean /quit?"));
        assert_eq!(parse_line("/qiut"), err("unknown command /qiut — did you mean /quit?"));
        assert_eq!(parse_line("/hlep"), err("unknown command /hlep — did you mean /help?"));
        assert_eq!(parse_line("/ms hi"), err("unknown command /ms — did you mean /msg?"));
        assert_eq!(parse_line("/z"), err("unknown command /z, try /help"), "one letter is too short to guess");
    }

    #[test]
    fn edit_distance_counts_chars() {
        assert_eq!(distance("quit", "quit"), 0);
        assert_eq!(distance("qiut", "quit"), 2);
        assert_eq!(distance("", "abc"), 3);
        assert_eq!(distance("héllo", "hello"), 1);
    }

    #[test]
    fn every_listed_command_parses() {
        for name in COMMANDS {
            let parsed = parse_line(&format!("/{name}"));
            assert!(!matches!(&parsed, Input::Error(e) if e.starts_with("unknown")), "/{name} -> {parsed:?}");
        }
    }

    #[test]
    fn update_checks_or_installs() {
        assert_eq!(parse_line("/update"), Input::Update { confirm: false });
        assert_eq!(parse_line("/update yes"), Input::Update { confirm: true });
        assert_eq!(parse_line("/update now"), err("usage: /update [yes]"));
        assert_eq!(parse_line("/update yes please"), err("usage: /update [yes]"));
    }

    #[test]
    fn mode_and_close_commands() {
        for gone in ["/line", "/tui", "/line now"] {
            assert_eq!(parse_line(gone), err("/line is gone — corvo-lan always uses the full-screen view"), "{gone}");
        }
        assert_eq!(parse_line("/close"), Input::Close(None));
        assert_eq!(parse_line("/close @bobRoger"), Input::Close(Some("bobRoger".into())));
        assert_eq!(parse_line("/close #catPieR"), err("use /leave for rooms"));
        assert_eq!(parse_line("/close bob"), err("usage: /close [@nick]"));
    }
}
