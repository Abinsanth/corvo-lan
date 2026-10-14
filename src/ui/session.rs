//! What the user typed and what happened, turned into commands and text. Shared by both frontends; knows nothing about terminals.

use super::input::{Input, parse_line};
use super::render::{self, Style};
use super::updates::UpdateAction;
use crate::api::{Command, Conv, Event, Target};
use crate::convo::Store;

pub const HELP_LINE: &str = "\
commands:
  <text>                 send to the current conversation (shown in the prompt)
  /msg @nick <text>      one-off private message (also /m)
  /chat @nick            switch to a private chat (also /c; the @ is optional)
  /chat #room <key>      join a room (just /chat #room once you're in)
  /create <name> <key>   create a private room; share its name and key to invite
  /leave [#room]         leave the current or named room
  /close [@nick]         close the current or named private chat
  /rooms                 your rooms and who's in them
  /chats                 your conversations, with unread counts
  /his [n]               show the last n messages here (default 20)
  /update [yes]          check for a newer version; yes installs it
  /who                   connected people
  /tui                   switch to the full-screen UI
  /help                  this list (also /h, /?)
  /quit                  leave (also /q, Ctrl-C / Ctrl-D)
  //text                 send a message starting with '/'";

pub const MAX_MESSAGE_LINES: usize = 50;

pub const HELP_TUI: &str = "\
commands:
  <text>                 send to the current tab
  /msg @nick <text>      one-off private message (also /m)
  /chat @nick            open a private chat (also /c; the @ is optional)
  /chat #room <key>      join a room (just /chat #room once you're in)
  /create <name> <key>   create a private room; share its name and key to invite
  /leave [#room]         leave the current or named room
  /close [@nick]         close the current or named private chat
  /rooms  /chats  /who   list rooms, chats, connected people
  /update [yes]          check for a newer version; yes installs it
  /line                  switch to line mode
  /help                  this list (also /h, /?)
  /quit                  leave (also /q, Ctrl-C)
  //text                 send a message starting with '/'
keys:
  Enter send · Alt+Enter or Shift+Enter new line · Tab/Shift-Tab or Alt-1..9 switch tabs
  PgUp/PgDn or mouse wheel scroll · Esc clear input / back to latest · click a name to chat";

/// Which frontend is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Line,
    Tui,
}

/// What the UI loop should do after an input line.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Reaction {
    pub commands: Vec<Command>,
    pub print: Vec<String>,
    pub quit: bool,
    pub update: Option<UpdateAction>,
    pub switch: Option<Mode>,
}

impl Reaction {
    pub(crate) fn say(s: impl Into<String>) -> Self {
        Reaction { print: vec![s.into()], ..Default::default() }
    }

    pub(crate) fn send(cmd: Command) -> Self {
        Reaction { commands: vec![cmd], ..Default::default() }
    }
}

pub struct Session {
    pub store: Store,
    mode: Mode,
    /// What line mode prints with; the TUI renders plain text and colours it itself.
    line_style: Style,
    style: Style,
}

impl Session {
    pub fn new(style: Style) -> Self {
        Session { store: Store::new(), mode: Mode::Line, line_style: style, style }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
        self.style = if mode == Mode::Tui { Style::plain() } else { self.line_style };
    }

    /// Whether colour is on (`NO_COLOR` and pipes turn it off).
    pub fn color(&self) -> bool {
        self.line_style.color
    }

    /// Printed when line mode takes over from the TUI.
    pub fn line_banner(&self) -> Vec<String> {
        let mut out = vec!["* line mode — /tui to switch back".to_string()];
        if self.store.conversations().iter().any(|c| c.unread > 0) {
            out.push(render::chats(&self.store, &self.style));
        }
        out
    }

    pub fn prompt(&self) -> String {
        render::prompt(self.store.current())
    }

    /// Folds an event into the store and returns what to print.
    pub fn on_event(&mut self, event: &Event, now: u64) -> Option<String> {
        self.store.apply(event, now);
        // Creating or joining a room takes you there.
        if let Event::RoomCreated { name, .. } | Event::RoomJoined { name, .. } = event {
            self.store.switch(Conv::Room(name.clone()));
        }
        render::event(event, now, &self.style)
    }

    /// What the prompt shows: its text and the conversation plain text goes to.
    pub fn prompt_state(&self) -> (String, Option<Conv>) {
        (self.prompt(), self.store.current().cloned())
    }

    /// Handles a typed line. `shown` is the conversation its prompt showed:
    /// plain text goes there even if a join switched us while the user typed.
    /// If the prompt showed none, the user means where they've just arrived.
    pub fn on_line(&mut self, line: &str, shown: Option<&Conv>) -> Reaction {
        self.on_input(parse_line(line), shown)
    }

    fn on_input(&mut self, input: Input, shown: Option<&Conv>) -> Reaction {
        match input {
            Input::Empty => Reaction::default(),
            Input::Error(e) => Reaction::say(format!("! {e}")),
            Input::Help => Reaction::say(if self.mode == Mode::Tui { HELP_TUI } else { HELP_LINE }),
            Input::Quit => Reaction { quit: true, ..Default::default() },
            Input::Who => Reaction::send(Command::ListPeers),
            Input::Text(text) => match tidy_message(&text) {
                Ok(text) => match shown.or(self.store.current()) {
                    Some(Conv::Room(n)) => Reaction::send(Command::Send { to: Target::Room(n.clone()), text }),
                    Some(Conv::Dm(n)) => Reaction::send(Command::Send { to: Target::Dm(n.clone()), text }),
                    None => Reaction::say("! no conversation — /chat @nick or /create <name> <key>"),
                },
                Err(e) => Reaction::say(format!("! {e}")),
            },
            Input::Msg { to, text } => match tidy_message(&text) {
                Ok(text) => Reaction::send(Command::Send { to: Target::Dm(to), text }),
                Err(e) => Reaction::say(format!("! {e}")),
            },
            Input::ChatDm(nick) => {
                if self.store.me() == Some(nick.as_str()) {
                    return Reaction::say("! that's you");
                }
                let conv = Conv::Dm(nick.clone());
                if !self.store.is_online(&nick) && self.store.get(&conv).is_none() {
                    return Reaction::say(format!("! @{nick} is offline"));
                }
                self.switch(conv)
            }
            Input::ChatRoom { name, key } => {
                let conv = Conv::Room(name.clone());
                if self.store.get(&conv).is_some() {
                    return self.switch(conv);
                }
                match key {
                    Some(key) => Reaction::send(Command::JoinRoom { name, key }),
                    None => Reaction::say(format!("! join with /chat #{name} <key>")),
                }
            }
            Input::ChatGuess(word) => match self.guess(&word, true) {
                Ok(Conv::Dm(nick)) => self.on_input(Input::ChatDm(nick), shown),
                Ok(Conv::Room(name)) => self.on_input(Input::ChatRoom { name, key: None }, shown),
                Err(e) => Reaction::say(format!("! {e}")),
            },
            Input::MsgGuess { to, text } => match self.guess(&to, false) {
                Ok(Conv::Dm(nick)) => self.on_input(Input::Msg { to: nick, text }, shown),
                // Only people were searched.
                Ok(Conv::Room(_)) => Reaction::default(),
                Err(e) => Reaction::say(format!("! {e}")),
            },
            Input::Create { base, part } => Reaction::send(Command::CreateRoom { base, key_part: part }),
            Input::Leave(Some(name)) => Reaction::send(Command::LeaveRoom { name }),
            Input::Leave(None) => match self.store.current() {
                Some(Conv::Room(n)) => Reaction::send(Command::LeaveRoom { name: n.clone() }),
                _ => Reaction::say("! you're not in a room here — /leave #room"),
            },
            Input::Update { confirm } => Reaction {
                update: Some(if confirm { UpdateAction::Install } else { UpdateAction::Check }),
                ..Default::default()
            },
            Input::Line if self.mode == Mode::Line => Reaction::say("! already in line mode"),
            Input::Tui if self.mode == Mode::Tui => Reaction::say("! already in the TUI"),
            Input::Line => Reaction { switch: Some(Mode::Line), ..Default::default() },
            Input::Tui => Reaction { switch: Some(Mode::Tui), ..Default::default() },
            Input::Close(target) => {
                let conv = match target {
                    Some(nick) => Conv::Dm(nick),
                    None => match self.store.current() {
                        Some(conv) => conv.clone(),
                        None => return Reaction::say("! no chat to close here"),
                    },
                };
                match &conv {
                    Conv::Room(_) => Reaction::say("! use /leave for rooms"),
                    Conv::Dm(nick) if self.store.get(&conv).is_none() => {
                        Reaction::say(format!("! no chat with @{nick}"))
                    }
                    Conv::Dm(nick) => {
                        let text = format!("* closed @{nick}");
                        self.store.close(&conv);
                        Reaction::say(text)
                    }
                }
            }
            Input::Rooms => Reaction::say(render::rooms(&self.store, &self.style)),
            Input::Chats => Reaction::say(render::chats(&self.store, &self.style)),
            Input::History(n) => {
                if self.mode == Mode::Tui {
                    return Reaction::say("* scroll with PgUp/PgDn");
                }
                let Some(conv) = self.store.current().cloned() else {
                    return Reaction::say("! no conversation — /chat @nick or /chat #room");
                };
                let lines = self.store.history(n);
                if lines.is_empty() {
                    return Reaction::say("* nothing here yet");
                }
                Reaction {
                    print: lines.iter().map(|l| render::line(&conv, l, &self.style)).collect(),
                    ..Default::default()
                }
            }
        }
    }

    /// Resolves a name typed without a sigil: online people (not me), plus
    /// joined rooms when `rooms`. An exact name (ignoring case) wins; else a
    /// unique prefix.
    fn guess(&self, word: &str, rooms: bool) -> Result<Conv, String> {
        let me = self.store.me();
        let mut all: Vec<Conv> =
            self.store.online().filter(|n| Some(n.as_str()) != me).map(|n| Conv::Dm(n.clone())).collect();
        if rooms {
            all.extend(
                self.store.conversations().iter().filter(|c| matches!(c.conv, Conv::Room(_))).map(|c| c.conv.clone()),
            );
        }
        let lower = |c: &Conv| match c {
            Conv::Dm(n) | Conv::Room(n) => n.to_lowercase(),
        };
        let typed = word.to_lowercase();
        let exact: Vec<&Conv> = all.iter().filter(|c| lower(c) == typed).collect();
        let found =
            if exact.is_empty() { all.iter().filter(|c| lower(c).starts_with(&typed)).collect() } else { exact };
        match found.as_slice() {
            [one] => Ok((*one).clone()),
            [] if rooms => {
                Err(format!("no one or no room called \"{word}\" — use @name for people or #name for rooms"))
            }
            [] => Err(format!("no one called \"{word}\" is online")),
            many => {
                let mut names: Vec<String> =
                    many.iter().take(5).map(|c| render::conv_name(c, &Style::plain())).collect();
                if many.len() > 5 {
                    names.push("…".into());
                }
                Err(format!("\"{word}\" matches {} — be more specific", names.join(", ")))
            }
        }
    }

    fn switch(&mut self, conv: Conv) -> Reaction {
        if self.mode == Mode::Tui {
            // The tab already shows the unread messages.
            self.store.switch(conv);
            return Reaction::default();
        }
        let unread = self.store.get(&conv).map_or(0, |c| c.unread);
        self.store.switch(conv.clone());
        if unread > 0 {
            let name = render::conv_name(&conv, &self.style);
            Reaction::say(format!("* {name}: {unread} unread — /his to see them"))
        } else {
            Reaction::default()
        }
    }
}

/// Drops trailing blank lines; refuses a message that's empty or too long.
pub fn tidy_message(text: &str) -> Result<String, String> {
    let lines: Vec<&str> = text.split('\n').collect();
    let keep = lines.iter().rposition(|l| !l.trim().is_empty()).map_or(0, |i| i + 1);
    if keep == 0 {
        return Err("message is empty".into());
    }
    if keep > MAX_MESSAGE_LINES {
        return Err(format!("message has more than {MAX_MESSAGE_LINES} lines"));
    }
    Ok(lines[..keep].join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::PeerId;

    fn session() -> Session {
        let mut s = Session::new(Style::plain());
        s.on_event(&Event::Assigned { nick: "annMoon".into(), renamed: false }, 0);
        s.on_event(&Event::PeerJoined { id: PeerId::test(2), nick: "bobRoger".into() }, 0);
        s
    }

    /// Types `line` at whatever prompt is currently showing.
    fn typed(s: &mut Session, line: &str) -> Reaction {
        let shown = s.store.current().cloned();
        s.on_line(line, shown.as_ref())
    }

    fn send(to: Target, text: &str) -> Reaction {
        Reaction::send(Command::Send { to, text: text.into() })
    }

    #[test]
    fn plain_text_goes_to_the_current_conversation() {
        let mut s = session();
        assert_eq!(typed(&mut s, "hello").print, vec!["! no conversation — /chat @nick or /create <name> <key>"]);
        assert_eq!(typed(&mut s, "/chat @bobRoger"), Reaction::default());
        assert_eq!(s.prompt(), "@bobRoger> ");
        assert_eq!(typed(&mut s, "hello"), send(Target::Dm("bobRoger".into()), "hello"));
    }

    #[test]
    fn creating_or_joining_a_room_switches_to_it() {
        let mut s = session();
        assert_eq!(
            typed(&mut s, "/create cat 123"),
            Reaction::send(Command::CreateRoom { base: "cat".into(), key_part: "123".into() })
        );
        s.on_event(&Event::RoomCreated { name: "catPieR".into(), key: "123-k7m9xq".into() }, 0);
        assert_eq!(s.prompt(), "#catPieR> ");
        assert_eq!(typed(&mut s, "meow"), send(Target::Room("catPieR".into()), "meow"));

        assert_eq!(
            typed(&mut s, "/chat #dogFoxR 1-abcdef"),
            Reaction::send(Command::JoinRoom { name: "dogFoxR".into(), key: "1-abcdef".into() })
        );
        assert_eq!(s.prompt(), "#catPieR> ", "not until the join succeeds");
        s.on_event(&Event::RoomJoined { name: "dogFoxR".into(), members: vec!["bobRoger".into()] }, 0);
        assert_eq!(s.prompt(), "#dogFoxR> ");
        assert_eq!(typed(&mut s, "/chat #catPieR"), Reaction::default(), "already in: just switch");
        assert_eq!(s.prompt(), "#catPieR> ");
        assert_eq!(typed(&mut s, "/chat #nopeMoonR").print, vec!["! join with /chat #nopeMoonR <key>"]);
    }

    #[test]
    fn text_goes_where_the_prompt_pointed_even_if_a_join_switched_us() {
        let mut s = session();
        typed(&mut s, "/chat @bobRoger");
        typed(&mut s, "/chat #catPieR 1-abcdef");
        // The user starts typing at the "@bobRoger> " prompt...
        let shown = s.store.current().cloned();
        assert_eq!(shown, Some(Conv::Dm("bobRoger".into())));
        // ...and the join completes meanwhile, switching the current conversation.
        s.on_event(&Event::RoomJoined { name: "catPieR".into(), members: vec![] }, 0);
        assert_eq!(s.on_line("for bob only", shown.as_ref()), send(Target::Dm("bobRoger".into()), "for bob only"));
        assert_eq!(s.prompt(), "#catPieR> ", "the next prompt shows the room");
    }

    #[test]
    fn text_typed_right_after_creating_a_room_goes_to_it() {
        let mut s = session();
        // The prompt after /create still shows no conversation...
        typed(&mut s, "/create cat 123");
        let shown = s.store.current().cloned();
        assert_eq!(shown, None);
        // ...the room arrives while the user types, and that's where they mean.
        s.on_event(&Event::RoomCreated { name: "catPieR".into(), key: "123-k7m9xq".into() }, 0);
        assert_eq!(s.on_line("first!", shown.as_ref()), send(Target::Room("catPieR".into()), "first!"));
    }

    #[test]
    fn leave_uses_the_current_room_or_the_named_one() {
        let mut s = session();
        assert_eq!(typed(&mut s, "/leave").print, vec!["! you're not in a room here — /leave #room"]);
        s.on_event(&Event::RoomCreated { name: "catPieR".into(), key: "1-abcdef".into() }, 0);
        assert_eq!(typed(&mut s, "/leave"), Reaction::send(Command::LeaveRoom { name: "catPieR".into() }));
        assert_eq!(typed(&mut s, "/leave #dogFoxR"), Reaction::send(Command::LeaveRoom { name: "dogFoxR".into() }));
        s.on_event(&Event::RoomLeft { name: "catPieR".into() }, 0);
        assert_eq!(s.prompt(), "> ");
    }

    #[test]
    fn chatting_with_yourself_or_someone_offline_is_refused() {
        let mut s = session();
        assert_eq!(typed(&mut s, "/chat @annMoon").print, vec!["! that's you"]);
        assert_eq!(typed(&mut s, "/chat @ghostMoon").print, vec!["! @ghostMoon is offline"]);
    }

    #[test]
    fn switching_mentions_unread_and_history_shows_them() {
        let mut s = session();
        s.on_event(
            &Event::Message { conv: Conv::Dm("bobRoger".into()), from: "bobRoger".into(), text: "one".into(), ts: 0 },
            0,
        );
        s.on_event(
            &Event::Message { conv: Conv::Dm("bobRoger".into()), from: "bobRoger".into(), text: "two".into(), ts: 0 },
            0,
        );
        assert_eq!(typed(&mut s, "/chat @bobRoger").print, vec!["* @bobRoger: 2 unread — /his to see them"]);
        let shown = typed(&mut s, "/his 1").print;
        assert_eq!(shown.len(), 1);
        assert!(shown[0].ends_with("@bobRoger → you: two"), "{shown:?}");
    }

    #[test]
    fn msg_who_quit_and_errors() {
        let mut s = session();
        assert_eq!(typed(&mut s, "/msg @bobRoger hi"), send(Target::Dm("bobRoger".into()), "hi"));
        assert_eq!(typed(&mut s, "/who"), Reaction::send(Command::ListPeers));
        assert!(typed(&mut s, "/quit").quit);
        assert!(typed(&mut s, "/q").quit);
        assert_eq!(typed(&mut s, "/nick x").print, vec!["! names are assigned by the system and can't be changed"]);
        assert_eq!(typed(&mut s, "/his").print, vec!["! no conversation — /chat @nick or /chat #room"]);
    }

    #[test]
    fn names_without_a_sigil_are_guessed() {
        let mut s = session(); // me: annMoon; online: bobRoger
        s.on_event(&Event::PeerJoined { id: PeerId::test(3), nick: "bobKite".into() }, 0);
        s.on_event(&Event::PeerJoined { id: PeerId::test(4), nick: "carolFox".into() }, 0);
        s.on_event(&Event::RoomJoined { name: "catPieR".into(), members: vec![] }, 0);
        s.on_event(&Event::RoomJoined { name: "carolFoxR".into(), members: vec![] }, 0);

        assert_eq!(typed(&mut s, "/chat catp"), Reaction::default());
        assert_eq!(s.store.current(), Some(&Conv::Room("catPieR".into())));
        assert_eq!(typed(&mut s, "/c CAROLFOX"), Reaction::default(), "an exact name wins, any case");
        assert_eq!(s.store.current(), Some(&Conv::Dm("carolFox".into())));
        assert_eq!(
            typed(&mut s, "/chat carol").print,
            vec!["! \"carol\" matches @carolFox, #carolFoxR — be more specific"]
        );
        assert_eq!(typed(&mut s, "/chat bob").print, vec!["! \"bob\" matches @bobKite, @bobRoger — be more specific"]);
        assert_eq!(
            typed(&mut s, "/chat ann").print,
            vec!["! no one or no room called \"ann\" — use @name for people or #name for rooms"],
            "never me"
        );
        assert_eq!(typed(&mut s, "/msg bobk hi"), send(Target::Dm("bobKite".into()), "hi"));
        assert_eq!(typed(&mut s, "/m catp hi").print, vec!["! no one called \"catp\" is online"], "/msg skips rooms");
    }

    #[test]
    fn a_long_list_of_guesses_is_cut_short() {
        let mut s = session();
        for i in 0..7 {
            s.on_event(&Event::PeerJoined { id: PeerId::test(10 + i), nick: format!("zed{i}Owl") }, 0);
        }
        assert_eq!(
            typed(&mut s, "/chat zed").print,
            vec!["! \"zed\" matches @zed0Owl, @zed1Owl, @zed2Owl, @zed3Owl, @zed4Owl, … — be more specific"]
        );
    }

    #[test]
    fn update_hands_the_work_to_the_updater() {
        let mut s = session();
        let check = typed(&mut s, "/update");
        assert_eq!(check.update, Some(UpdateAction::Check));
        assert!(check.commands.is_empty() && check.print.is_empty());
        assert_eq!(typed(&mut s, "/update yes").update, Some(UpdateAction::Install));
        assert!(HELP_LINE.contains("/update [yes]"));
    }

    #[test]
    fn mode_switches_are_refused_in_the_mode_you_are_in() {
        let mut s = session();
        assert_eq!(typed(&mut s, "/line").print, vec!["! already in line mode"]);
        assert_eq!(typed(&mut s, "/tui").switch, Some(Mode::Tui));
        s.set_mode(Mode::Tui);
        assert_eq!(typed(&mut s, "/tui").print, vec!["! already in the TUI"]);
        assert_eq!(typed(&mut s, "/line").switch, Some(Mode::Line));
    }

    #[test]
    fn close_forgets_a_dm_but_not_a_room() {
        let mut s = session();
        typed(&mut s, "/chat @bobRoger");
        assert_eq!(typed(&mut s, "/close").print, vec!["* closed @bobRoger"]);
        assert!(s.store.get(&Conv::Dm("bobRoger".into())).is_none());
        assert_eq!(typed(&mut s, "/close").print, vec!["! no chat to close here"]);
        s.on_event(&Event::RoomJoined { name: "catPieR".into(), members: vec![] }, 0);
        assert_eq!(typed(&mut s, "/close").print, vec!["! use /leave for rooms"]);
        assert_eq!(typed(&mut s, "/close @ghostMoon").print, vec!["! no chat with @ghostMoon"]);
    }

    #[test]
    fn multi_line_messages_are_tidied_and_capped() {
        assert_eq!(tidy_message("hi\nthere\n\n  \n"), Ok("hi\nthere".into()));
        assert_eq!(tidy_message("  \n"), Err("message is empty".into()));
        let long = vec!["x"; MAX_MESSAGE_LINES + 1].join("\n");
        assert_eq!(tidy_message(&long), Err(format!("message has more than {MAX_MESSAGE_LINES} lines")));
        assert!(tidy_message(&vec!["x"; MAX_MESSAGE_LINES].join("\n")).is_ok());

        let mut s = session();
        typed(&mut s, "/chat @bobRoger");
        assert_eq!(typed(&mut s, "one\ntwo\n\n"), send(Target::Dm("bobRoger".into()), "one\ntwo"));
        assert_eq!(typed(&mut s, &long).print, vec![format!("! message has more than {MAX_MESSAGE_LINES} lines")]);
        assert_eq!(typed(&mut s, "/msg @bobRoger a\nb\n"), send(Target::Dm("bobRoger".into()), "a\nb"));
    }

    #[test]
    fn help_and_history_depend_on_the_mode() {
        let mut s = session();
        assert_eq!(typed(&mut s, "/help").print, vec![HELP_LINE]);
        assert!(HELP_LINE.contains("/tui") && !HELP_LINE.contains("/line"));
        assert!(HELP_LINE.contains("/close [@nick]"), "/close works in line mode too");
        assert!(HELP_LINE.contains("/quit") && HELP_LINE.contains("/q,") && HELP_LINE.contains("/his [n]"));
        s.set_mode(Mode::Tui);
        assert_eq!(typed(&mut s, "/help").print, vec![HELP_TUI]);
        assert!(HELP_TUI.contains("/line") && HELP_TUI.contains("/close") && HELP_TUI.contains("Alt+Enter"));
        assert_eq!(typed(&mut s, "/his").print, vec!["* scroll with PgUp/PgDn"]);
    }

    #[test]
    fn the_tui_renders_plain_and_switching_there_says_nothing() {
        let mut s = Session::new(Style { color: true });
        s.set_mode(Mode::Tui);
        assert!(s.color());
        let text = s.on_event(&Event::PeerJoined { id: PeerId::test(3), nick: "daveElk".into() }, 0).unwrap();
        assert!(!text.contains('\x1b'), "{text:?}");
        s.on_event(
            &Event::Message { conv: Conv::Dm("daveElk".into()), from: "daveElk".into(), text: "x".into(), ts: 0 },
            0,
        );
        assert_eq!(typed(&mut s, "/chat @daveElk"), Reaction::default());
        s.set_mode(Mode::Line);
        assert!(
            s.on_event(&Event::PeerJoined { id: PeerId::test(4), nick: "eveOwl".into() }, 0).unwrap().contains('\x1b')
        );
    }

    #[test]
    fn line_banner_mentions_unread_chats() {
        let mut s = session();
        assert_eq!(s.line_banner(), vec!["* line mode — /tui to switch back"]);
        s.on_event(
            &Event::Message { conv: Conv::Dm("bobRoger".into()), from: "bobRoger".into(), text: "x".into(), ts: 0 },
            0,
        );
        let banner = s.line_banner();
        assert_eq!(banner.len(), 2);
        assert!(banner[1].contains("1 new"), "{banner:?}");
    }
}
