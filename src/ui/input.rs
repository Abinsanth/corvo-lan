//! Parses a typed line. Pure: knows nothing about the current conversation.

/// Default and maximum `/history` length.
pub const HISTORY_DEFAULT: usize = 20;
pub const HISTORY_MAX: usize = 1000;

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
    Create {
        base: String,
        part: String,
    },
    Leave(Option<String>),
    Rooms,
    Chats,
    History(usize),
    Who,
    Help,
    Quit,
    Empty,
    Error(String),
}

const SIGIL_HINT: &str = "use @name for people or #name for rooms";

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
        "msg" => match args.split_once(char::is_whitespace) {
            Some((to, text)) if !text.trim().is_empty() => match to.strip_prefix('@') {
                Some(nick) => Input::Msg { to: nick.to_string(), text: text.trim_start().to_string() },
                None => Input::Error(SIGIL_HINT.into()),
            },
            _ => Input::Error("usage: /msg @nick <text>".into()),
        },
        "chat" => match words.as_slice() {
            [target] if target.starts_with('@') => Input::ChatDm(target[1..].to_string()),
            [target] if target.starts_with('#') => Input::ChatRoom { name: target[1..].to_string(), key: None },
            [target, key] if target.starts_with('#') => {
                Input::ChatRoom { name: target[1..].to_string(), key: Some(key.to_string()) }
            }
            [_] | [_, _] => Input::Error(SIGIL_HINT.into()),
            _ => Input::Error("usage: /chat @nick | /chat #room [key]".into()),
        },
        "create" => match words.as_slice() {
            [base, part] => Input::Create { base: base.to_string(), part: part.to_string() },
            _ => Input::Error("usage: /create <name> <key>".into()),
        },
        "leave" => match words.as_slice() {
            [] => Input::Leave(None),
            [room] if room.starts_with('#') => Input::Leave(Some(room[1..].to_string())),
            _ => Input::Error("usage: /leave [#room]".into()),
        },
        "history" => match words.as_slice() {
            [] => Input::History(HISTORY_DEFAULT),
            [n] => match n.parse::<usize>() {
                Ok(n) if n > 0 => Input::History(n.min(HISTORY_MAX)),
                _ => Input::Error("usage: /history [n]".into()),
            },
            _ => Input::Error("usage: /history [n]".into()),
        },
        "rooms" => Input::Rooms,
        "chats" => Input::Chats,
        "who" => Input::Who,
        "help" => Input::Help,
        "quit" | "exit" => Input::Quit,
        "nick" => Input::Error("names are assigned by the system and can't be changed".into()),
        other => Input::Error(format!("unknown command /{other}, try /help")),
    }
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
    fn msg_needs_a_sigil_and_text() {
        assert_eq!(
            parse_line("/msg @bobRoger hi  there"),
            Input::Msg { to: "bobRoger".into(), text: "hi  there".into() }
        );
        assert_eq!(parse_line("/msg bobRoger hi"), err(SIGIL_HINT));
        assert_eq!(parse_line("/msg @bobRoger"), err("usage: /msg @nick <text>"));
        assert_eq!(parse_line("/msg"), err("usage: /msg @nick <text>"));
    }

    #[test]
    fn chat_switches_or_joins() {
        assert_eq!(parse_line("/chat @bobRoger"), Input::ChatDm("bobRoger".into()));
        assert_eq!(parse_line("/chat #catPieR"), Input::ChatRoom { name: "catPieR".into(), key: None });
        assert_eq!(
            parse_line("/chat #catPieR 123-k7m9xq"),
            Input::ChatRoom { name: "catPieR".into(), key: Some("123-k7m9xq".into()) }
        );
        assert_eq!(parse_line("/chat bobRoger"), err(SIGIL_HINT));
        assert_eq!(parse_line("/chat @bob key"), err(SIGIL_HINT));
        assert_eq!(parse_line("/chat"), err("usage: /chat @nick | /chat #room [key]"));
    }

    #[test]
    fn room_commands() {
        assert_eq!(parse_line("/create cat 123"), Input::Create { base: "cat".into(), part: "123".into() });
        assert_eq!(parse_line("/create cat"), err("usage: /create <name> <key>"));
        assert_eq!(parse_line("/leave"), Input::Leave(None));
        assert_eq!(parse_line("/leave #catPieR"), Input::Leave(Some("catPieR".into())));
        assert_eq!(parse_line("/leave catPieR"), err("usage: /leave [#room]"));
        assert_eq!(parse_line("/rooms"), Input::Rooms);
    }

    #[test]
    fn history_and_the_rest() {
        assert_eq!(parse_line("/history"), Input::History(HISTORY_DEFAULT));
        assert_eq!(parse_line("/history 5"), Input::History(5));
        assert_eq!(parse_line("/history 99999"), Input::History(HISTORY_MAX));
        assert_eq!(parse_line("/history 0"), err("usage: /history [n]"));
        assert_eq!(parse_line("/history x"), err("usage: /history [n]"));
        assert_eq!(parse_line("/chats"), Input::Chats);
        assert_eq!(parse_line("/who"), Input::Who);
        assert_eq!(parse_line("/help"), Input::Help);
        assert_eq!(parse_line("/quit"), Input::Quit);
        assert_eq!(parse_line("/exit"), Input::Quit);
        assert_eq!(parse_line("/nick bob"), err("names are assigned by the system and can't be changed"));
        assert_eq!(parse_line("/dance"), err("unknown command /dance, try /help"));
    }
}
