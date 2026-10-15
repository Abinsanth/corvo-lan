//! What the user typed and what happened, turned into commands and text. Used by the TUI; knows nothing about terminals.

use super::input::{Input, parse_line};
use super::render::{self, Style};
use super::updates::UpdateAction;
use crate::api::{Command, Conv, Event, Target};
use crate::convo::Store;
use crate::node::{MAX_MEMBERS, MIN_MEMBERS};
use crate::update::Version;

pub const MAX_MESSAGE_LINES: usize = 50;

/// Shown in the help box; lines without indent are headings.
pub const HELP: &str = "\
chat
  /chat @nick          private chat (/c)
  /chat #room <key>    join a room
  /msg @nick <text>    one-off message (/m)
  /create <name>       new private room
  /invite @nick        bring someone in
  /kick @nick          vote someone out
  /accept  /decline    answer an invite
  /approve /deny       answer a join request
  /leave   /close      leave a room / chat

info
  /rooms /chats /who   lists
  /clear               clear the screen
  /update              check for updates
  /quit                exit (/q, Ctrl-C)

keys
  Shift-Tab  switch tabs    Tab  complete
  Alt+Enter  new line       PgUp/PgDn  scroll";

/// What the UI loop should do after an input line.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Reaction {
    pub commands: Vec<Command>,
    pub print: Vec<String>,
    pub quit: bool,
    /// Open the help box.
    pub help: bool,
    pub update: Option<UpdateAction>,
    /// Clear the open tab's view.
    pub clear: bool,
    /// Quit and start the newly installed version.
    pub restart: bool,
    /// Text for the clipboard.
    pub copy: Option<String>,
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
    /// Whether the TUI may use colour (`NO_COLOR` turns it off).
    color: bool,
    /// Text is always plain; the TUI colours lines itself.
    style: Style,
    /// The question the next line answers, if any.
    asking: Option<Asking>,
    /// A newer version to ask about once the open question is answered.
    pending: Option<Version>,
}

/// A question the input is waiting on.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Asking {
    /// Startup: keep the previewed name or type another. Only a name ends it.
    /// `confirmed` once Enter kept it, until the node goes live.
    Name { preview: String, confirmed: bool },
    /// `/create`, step 1: keep the previewed room name or type another. `max` if typed with it.
    RoomName { preview: String, max: Option<u8> },
    /// `/create`, step 2: how many people.
    HeadCount { name: String },
    /// A newer version is out: install it? Only y/yes/n/no answer it.
    Install { version: Version },
    /// A newer version is installed: restart into it? Only y/yes/n/no answer it.
    Restart { version: Version },
}

/// `y`/`yes` → true, `n`/`no` → false, in any case; anything else isn't an answer.
fn yes_no(line: &str) -> Option<bool> {
    match line.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => Some(true),
        "n" | "no" => Some(false),
        _ => None,
    }
}

impl Session {
    pub fn new(color: bool) -> Self {
        Session { store: Store::new(), color, style: Style::plain(), asking: None, pending: None }
    }

    pub fn color(&self) -> bool {
        self.color
    }

    /// Folds an event into the store and returns what to print.
    pub fn on_event(&mut self, event: &Event, now: u64) -> Option<String> {
        self.store.apply(event, now);
        match event {
            Event::NamePreview { nick } => {
                // The name comes first; an install question already asked waits its turn.
                if let Some(Asking::Install { version }) = self.asking {
                    self.pending = Some(version);
                }
                self.asking = Some(Asking::Name { preview: nick.clone(), confirmed: false })
            }
            Event::Assigned { .. } if matches!(self.asking, Some(Asking::Name { .. })) => self.asking = None,
            _ => {}
        }
        // Creating or joining a room takes you there.
        if let Event::RoomCreated { name, .. } | Event::RoomJoined { name, .. } = event {
            self.store.switch(Conv::Room(name.clone()));
        }
        render::event(event, now, &self.style)
    }

    /// Handles a typed line. `shown` is the conversation its prompt showed:
    /// plain text goes there even if a join switched us while the user typed.
    /// If the prompt showed none, the user means where they've just arrived.
    pub fn on_line(&mut self, line: &str, shown: Option<&Conv>) -> Reaction {
        if let Some(Asking::Name { preview, confirmed }) = &self.asking {
            let (preview, confirmed) = (preview.clone(), *confirmed);
            return self.answer_name(preview, confirmed, line.trim());
        }
        if let Some(Asking::Install { .. } | Asking::Restart { .. }) = &self.asking {
            if let Some(yes) = yes_no(line) {
                let asking = self.asking.take().expect("matched above");
                return self.answer_update(asking, yes);
            }
            return self.on_input(parse_line(line), shown);
        }
        if let Some(asking) = self.asking.take()
            && !line.trim_start().starts_with('/')
        {
            return match asking {
                Asking::RoomName { preview, max } => self.answer_room_name(preview, max, line.trim()),
                Asking::HeadCount { name } => self.answer_head_count(name, line.trim()),
                Asking::Name { .. } | Asking::Install { .. } | Asking::Restart { .. } => {
                    unreachable!("answered above")
                }
            };
        }
        self.on_input(parse_line(line), shown)
    }

    /// The question the input is waiting on, if any.
    pub fn prompt(&self) -> Option<String> {
        self.asking.as_ref().map(|asking| match asking {
            Asking::Name { preview, confirmed: false } => {
                format!("you'll be @{preview} — Enter to keep, or type a new start")
            }
            Asking::Name { preview, confirmed: true } => format!("joining as @{preview}…"),
            Asking::RoomName { preview, .. } => {
                format!("new room #{preview} — Enter to keep it, or type another name")
            }
            Asking::HeadCount { name } => format!(
                "how many people in #{name}? {MIN_MEMBERS}–{MAX_MEMBERS}, Enter for {MAX_MEMBERS}, Esc to cancel"
            ),
            Asking::Install { version } => format!("v{version} is available — install it now? (y/n)"),
            Asking::Restart { version } => {
                format!("v{version} is installed — restart now? You'll leave your rooms. (y/n)")
            }
        })
    }

    /// Whether `line` answers the open question (rather than being a message or command).
    pub fn takes(&self, line: &str) -> bool {
        match &self.asking {
            None => false,
            Some(Asking::Install { .. } | Asking::Restart { .. }) => yes_no(line).is_some(),
            Some(_) => true,
        }
    }

    /// Asks whether to install `version`; says whether it's asked now. With
    /// another question open, it waits for `ask_pending` instead.
    pub fn offer_update(&mut self, version: Version) -> bool {
        self.pending = Some(version);
        self.ask_pending()
    }

    /// Asks the waiting install question if nothing else is being asked; says whether it did.
    pub fn ask_pending(&mut self) -> bool {
        if self.asking.is_some() {
            return false;
        }
        let Some(version) = self.pending.take() else {
            return false;
        };
        self.asking = Some(Asking::Install { version });
        true
    }

    /// Asks whether to restart into the installed `version`. If another
    /// question is open, returns a line to show instead.
    pub fn offer_restart(&mut self, version: Version) -> Option<String> {
        if self.asking.is_some() {
            return Some(format!("* v{version} starts next time you open corvo-lan"));
        }
        self.asking = Some(Asking::Restart { version });
        None
    }

    fn answer_update(&mut self, asking: Asking, yes: bool) -> Reaction {
        match (asking, yes) {
            (Asking::Install { .. }, true) => Reaction { update: Some(UpdateAction::Install), ..Default::default() },
            (Asking::Install { .. }, false) => Reaction::say("* ok — /update yes installs it later"),
            (Asking::Restart { .. }, true) => Reaction { restart: true, ..Default::default() },
            (Asking::Restart { version }, false) => {
                Reaction::say(format!("* v{version} starts next time you open corvo-lan"))
            }
            _ => Reaction::default(),
        }
    }

    /// Drops the question; says whether there was one. The name question can't be dropped.
    pub fn cancel_prompt(&mut self) -> bool {
        if matches!(self.asking, Some(Asking::Name { .. })) {
            return false;
        }
        self.asking.take().is_some()
    }

    /// Enter keeps the preview; a name asks for a new one. Only /quit gets past it.
    /// Once kept, the name is settled: later lines just say so.
    fn answer_name(&mut self, preview: String, confirmed: bool, answer: &str) -> Reaction {
        if answer.starts_with('/') && parse_line(answer) == Input::Quit {
            return self.on_input(Input::Quit, None);
        }
        if confirmed {
            if answer.is_empty() {
                return Reaction::default();
            }
            return Reaction::say(format!("! you're joining as @{preview} — restart to pick another name"));
        }
        if answer.starts_with('/') {
            return Reaction::say(format!("! pick your name first — Enter keeps @{preview}"));
        }
        let base = answer.strip_prefix('@').unwrap_or(answer).to_ascii_lowercase();
        // Typing out the name shown means keeping it, word and all.
        if base.is_empty() || base == preview {
            self.asking = Some(Asking::Name { preview, confirmed: true });
            return Reaction::send(Command::ConfirmName);
        }
        if base.len() > crate::names::MAX_BASE_LEN && base.chars().all(|c| c.is_ascii_alphanumeric()) {
            return Reaction::say("! just the first part, up to 6 letters");
        }
        if let Err(e) = crate::names::validate_base(&base) {
            return Reaction::say(format!("! {e}"));
        }
        // The same start again: they're likely trying to drop the word.
        let again = crate::names::base_of(&preview) == Some(base.as_str());
        let mut reaction = Reaction::send(Command::PreviewName { base });
        if again {
            reaction.print.push("* the word is always added — here's another".into());
        }
        reaction
    }

    /// Offers a room name for `base` that isn't one of ours.
    fn preview_room(&mut self, base: &str, max: Option<u8>) -> Reaction {
        if let Err(e) = crate::names::validate_base(base) {
            return Reaction::say(format!("! {e}"));
        }
        let ours =
            |n: &str| self.store.get(&Conv::Room(n.to_string())).is_some() || self.store.pending().any(|p| p == n);
        let picked = crate::names::pick_name(base, ours);
        match picked {
            Some(preview) => {
                self.asking = Some(Asking::RoomName { preview, max });
                Reaction::default()
            }
            None => Reaction::say(format!("! you're already in every #{base}… room")),
        }
    }

    /// Enter keeps the preview; a name asks for a new one (a bad one keeps the old).
    fn answer_room_name(&mut self, preview: String, max: Option<u8>, answer: &str) -> Reaction {
        if !answer.is_empty() && answer.chars().all(|c| c.is_ascii_digit()) {
            self.asking = Some(Asking::RoomName { preview: preview.clone(), max });
            return Reaction::say(format!("! Enter keeps #{preview} first — then it asks how many people"));
        }
        if !answer.is_empty() {
            let base = answer.strip_prefix('#').unwrap_or(answer).to_ascii_lowercase();
            let reaction = self.preview_room(&base, max);
            if self.asking.is_none() {
                self.asking = Some(Asking::RoomName { preview, max });
            }
            return reaction;
        }
        match max {
            Some(max) => Reaction::send(Command::CreateRoom { name: preview, max }),
            None => {
                self.asking = Some(Asking::HeadCount { name: preview });
                Reaction::default()
            }
        }
    }

    fn answer_head_count(&mut self, name: String, answer: &str) -> Reaction {
        if answer.is_empty() {
            return Reaction::send(Command::CreateRoom { name, max: MAX_MEMBERS });
        }
        match answer.parse::<u8>() {
            Ok(max) if (MIN_MEMBERS..=MAX_MEMBERS).contains(&max) => Reaction::send(Command::CreateRoom { name, max }),
            _ => {
                self.asking = Some(Asking::HeadCount { name });
                Reaction::say(format!(
                    "! a room holds {MIN_MEMBERS}–{MAX_MEMBERS} people — type a number, or Esc to cancel"
                ))
            }
        }
    }

    fn on_input(&mut self, input: Input, shown: Option<&Conv>) -> Reaction {
        match input {
            Input::Empty => Reaction::default(),
            Input::Error(e) => Reaction::say(format!("! {e}")),
            Input::Help => Reaction { help: true, ..Default::default() },
            Input::Quit => Reaction { quit: true, ..Default::default() },
            Input::Who => Reaction::send(Command::ListPeers),
            Input::Text(text) => match tidy_message(&text) {
                Ok(text) => match shown.or(self.store.current()) {
                    Some(Conv::Room(n)) => Reaction::send(Command::Send { to: Target::Room(n.clone()), text }),
                    Some(Conv::Dm(n)) => Reaction::send(Command::Send { to: Target::Dm(n.clone()), text }),
                    None => Reaction::say("! no conversation — /chat @nick or /create <name>"),
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
            Input::Create { base, max } => {
                let max = match max.as_deref().map(str::parse::<u8>) {
                    None => None,
                    Some(Ok(max)) if (MIN_MEMBERS..=MAX_MEMBERS).contains(&max) => Some(max),
                    Some(_) => return Reaction::say(format!("! a room holds {MIN_MEMBERS}–{MAX_MEMBERS} people")),
                };
                self.preview_room(&base, max)
            }
            Input::Invite { nick, room } => {
                let Some(room) = room.or_else(|| match self.store.current() {
                    Some(Conv::Room(name)) => Some(name.clone()),
                    _ => None,
                }) else {
                    return Reaction::say("! /invite works in a room tab, or name the room: /invite @nick #room");
                };
                Reaction::send(Command::Invite { nick, room })
            }
            Input::Kick { nick, room } => {
                let Some(room) = room.or_else(|| match self.store.current() {
                    Some(Conv::Room(name)) => Some(name.clone()),
                    _ => None,
                }) else {
                    return Reaction::say("! /kick works in a room tab, or name the room: /kick @nick #room");
                };
                Reaction::send(Command::Kick { room, nick })
            }
            Input::Accept(room) => match self.invited_room(room, "accept") {
                Ok(room) => Reaction::send(Command::Accept { room }),
                Err(e) => Reaction::say(format!("! {e}")),
            },
            Input::Decline(room) => match self.invited_room(room, "decline") {
                Ok(room) => {
                    self.store.drop_invite(&room);
                    Reaction::send(Command::Decline { room })
                }
                Err(e) => Reaction::say(format!("! {e}")),
            },
            Input::Approve { nick, room } => match self.waiting_room(&nick, room, "approve") {
                Ok(room) => Reaction::send(Command::Approve { room, nick }),
                Err(e) => Reaction::say(format!("! {e}")),
            },
            Input::Deny { nick, room } => match self.waiting_room(&nick, room, "deny") {
                Ok(room) => Reaction::send(Command::Deny { room, nick }),
                Err(e) => Reaction::say(format!("! {e}")),
            },
            Input::Leave(Some(name)) => Reaction::send(Command::LeaveRoom { name }),
            Input::Leave(None) => match self.store.current() {
                Some(Conv::Room(n)) => Reaction::send(Command::LeaveRoom { name: n.clone() }),
                _ => Reaction::say("! you're not in a room here — /leave #room"),
            },
            Input::Update { confirm } => {
                self.pending = None;
                if matches!(self.asking, Some(Asking::Install { .. })) {
                    self.asking = None;
                }
                Reaction {
                    update: Some(if confirm { UpdateAction::Install } else { UpdateAction::Check }),
                    ..Default::default()
                }
            }
            Input::Clear => Reaction { clear: true, ..Default::default() },
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

    /// The invitation `/accept` or `/decline` means: the room typed, else the only one.
    fn invited_room(&self, typed: Option<String>, cmd: &str) -> Result<String, String> {
        if let Some(room) = typed {
            return Ok(room);
        }
        let names: Vec<String> = self.store.invites().iter().map(|(_, room)| format!("#{room}")).collect();
        match names.as_slice() {
            [] => Err("no invitations".into()),
            [one] => Ok(one[1..].to_string()),
            many => {
                let (last, rest) = many.split_last().expect("at least two");
                Err(format!("invited to {} and {last} — say which: /{cmd} {}", rest.join(", "), many[0]))
            }
        }
    }

    /// `/chat #room key` for the clipboard, when a room's key is clicked.
    pub fn copy_join_line(&self, room: &str) -> Reaction {
        match self.store.get(&Conv::Room(room.to_string())).and_then(|c| c.key.clone()) {
            Some(key) => {
                let line = format!("/chat #{room} {key}");
                Reaction {
                    print: vec![format!("* copied: {line} (if it didn't copy, Shift/Option-drag to select it)")],
                    copy: Some(line),
                    ..Default::default()
                }
            }
            None => Reaction::say(format!("! you're not in #{room}")),
        }
    }

    /// The room `nick` is asking to join: the one typed, else the current
    /// room if they wait there, else the only room they wait in.
    fn waiting_room(&self, nick: &str, typed: Option<String>, cmd: &str) -> Result<String, String> {
        if let Some(room) = typed {
            return Ok(room);
        }
        let rooms: Vec<&String> = self
            .store
            .conversations()
            .iter()
            .filter(|c| c.waiting.contains_key(nick))
            .filter_map(|c| match &c.conv {
                Conv::Room(name) => Some(name),
                Conv::Dm(_) => None,
            })
            .collect();
        if let Some(Conv::Room(current)) = self.store.current()
            && rooms.contains(&current)
        {
            return Ok(current.clone());
        }
        match rooms.as_slice() {
            [one] => Ok((*one).clone()),
            [] => Err(format!("@{nick} isn't waiting to join any of your rooms")),
            many => {
                let names: Vec<String> = many.iter().map(|r| format!("#{r}")).collect();
                let (last, rest) = names.split_last().expect("at least two");
                Err(format!(
                    "@{nick} is waiting in {} and {last} — say which: /{cmd} @{nick} {}",
                    rest.join(", "),
                    names[0]
                ))
            }
        }
    }

    /// Shows `conv`; its tab already shows what's unread.
    fn switch(&mut self, conv: Conv) -> Reaction {
        self.store.switch(conv);
        Reaction::default()
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
        let mut s = Session::new(false);
        s.on_event(&Event::Assigned { nick: "annmoon".into(), renamed: false }, 0);
        s.on_event(&Event::PeerJoined { id: PeerId::test(2), nick: "bobroger".into() }, 0);
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
        assert_eq!(typed(&mut s, "hello").print, vec!["! no conversation — /chat @nick or /create <name>"]);
        assert_eq!(typed(&mut s, "/chat @bobroger"), Reaction::default());
        assert_eq!(s.store.current(), Some(&Conv::Dm("bobroger".into())));
        assert_eq!(typed(&mut s, "hello"), send(Target::Dm("bobroger".into()), "hello"));
    }

    #[test]
    fn creating_or_joining_a_room_switches_to_it() {
        let mut s = session();
        assert_eq!(typed(&mut s, "/create cat 5"), Reaction::default());
        let name = room_preview(&s);
        assert_eq!(typed(&mut s, ""), Reaction::send(Command::CreateRoom { name, max: 5 }));
        s.on_event(&Event::RoomCreated { name: "catpie".into(), key: "123-k7m9xq".into(), max: 13 }, 0);
        assert_eq!(s.store.current(), Some(&Conv::Room("catpie".into())));
        assert_eq!(typed(&mut s, "meow"), send(Target::Room("catpie".into()), "meow"));

        assert_eq!(
            typed(&mut s, "/chat #dogwolf 1-abcdef"),
            Reaction::send(Command::JoinRoom { name: "dogwolf".into(), key: "1-abcdef".into() })
        );
        assert_eq!(s.store.current(), Some(&Conv::Room("catpie".into())), "not until the join succeeds");
        s.on_event(
            &Event::RoomJoined {
                name: "dogwolf".into(),
                key: "k7m9xq".into(),
                max: 13,
                members: vec!["bobroger".into()],
            },
            0,
        );
        assert_eq!(s.store.current(), Some(&Conv::Room("dogwolf".into())));
        assert_eq!(typed(&mut s, "/chat #catpie"), Reaction::default(), "already in: just switch");
        assert_eq!(s.store.current(), Some(&Conv::Room("catpie".into())));
        assert_eq!(typed(&mut s, "/chat #nopemoon").print, vec!["! join with /chat #nopemoon <key>"]);
    }

    /// A session at startup, showing `preview` as the name it would get.
    fn naming(preview: &str) -> Session {
        let mut s = Session::new(false);
        s.on_event(&Event::NamePreview { nick: preview.into() }, 0);
        s
    }

    #[test]
    fn the_name_question_previews_and_confirms() {
        let mut s = naming("abinsanthroger");
        assert_eq!(s.prompt(), Some("you'll be @abinsanthroger — Enter to keep, or type a new start".into()));
        assert_eq!(typed(&mut s, "Luffy"), Reaction::send(Command::PreviewName { base: "luffy".into() }));
        s.on_event(&Event::NamePreview { nick: "luffysunny".into() }, 0);
        assert_eq!(s.prompt(), Some("you'll be @luffysunny — Enter to keep, or type a new start".into()));
        assert_eq!(typed(&mut s, ""), Reaction::send(Command::ConfirmName));
        assert!(s.prompt().is_some(), "asked until the name arrives");
        s.on_event(&Event::Assigned { nick: "luffysunny".into(), renamed: false }, 0);
        assert_eq!(s.prompt(), None);
    }

    #[test]
    fn after_enter_the_name_question_says_it_is_joining() {
        let mut s = naming("bobroger");
        assert_eq!(typed(&mut s, ""), Reaction::send(Command::ConfirmName));
        assert_eq!(s.prompt(), Some("joining as @bobroger…".into()));
        let joining = Reaction::say("! you're joining as @bobroger — restart to pick another name");
        assert_eq!(typed(&mut s, "zoro"), joining, "a name typed now isn't dropped silently");
        assert_eq!(typed(&mut s, "/who"), joining);
        assert_eq!(typed(&mut s, ""), Reaction::default(), "a second Enter does nothing");
        assert!(!s.cancel_prompt());
        assert!(typed(&mut s, "/quit").quit);
        s.on_event(&Event::Assigned { nick: "bobroger".into(), renamed: false }, 0);
        assert_eq!(s.prompt(), None);
    }

    #[test]
    fn the_name_question_only_ends_with_a_name() {
        let mut s = naming("bobroger");
        let question = s.prompt();
        assert!(!s.cancel_prompt(), "Esc can't drop it");
        assert_eq!(typed(&mut s, "Bob Smith"), Reaction::say("! name must be 1–6 letters or digits"));
        assert_eq!(typed(&mut s, "CHAT"), Reaction::say("! 'chat' is reserved"));
        assert_eq!(typed(&mut s, "/who"), Reaction::say("! pick your name first — Enter keeps @bobroger"));
        assert_eq!(
            typed(&mut s, "@Luffy"),
            Reaction::send(Command::PreviewName { base: "luffy".into() }),
            "an @ is fine"
        );
        assert_eq!(s.prompt(), question, "same question, same preview");
        assert!(typed(&mut s, "/quit").quit);
    }

    #[test]
    fn typing_the_name_shown_keeps_it() {
        for answer in ["bobRoger", "bobroger", "@bobRoger", " BOBROGER "] {
            let mut s = naming("bobroger");
            assert_eq!(typed(&mut s, answer), Reaction::send(Command::ConfirmName), "{answer:?}");
            assert_eq!(s.prompt(), Some("joining as @bobroger…".into()));
        }
    }

    #[test]
    fn typing_the_same_start_again_explains_the_word() {
        let mut s = naming("bobroger");
        let again = Reaction {
            commands: vec![Command::PreviewName { base: "bob".into() }],
            print: vec!["* the word is always added — here's another".into()],
            ..Default::default()
        };
        assert_eq!(typed(&mut s, "Bob"), again);
        assert_eq!(typed(&mut s, "luffy"), Reaction::send(Command::PreviewName { base: "luffy".into() }));
    }

    #[test]
    fn a_whole_name_typed_asks_for_just_the_start() {
        let mut s = naming("bobroger");
        let just_start = Reaction::say("! just the first part, up to 6 letters");
        assert_eq!(typed(&mut s, "bobkite"), just_start);
        assert_eq!(typed(&mut s, "abinsanth"), just_start);
        assert_eq!(typed(&mut s, "Bob Smith"), Reaction::say("! name must be 1–6 letters or digits"));
    }

    /// The room name the open question is offering.
    fn room_preview(s: &Session) -> String {
        match &s.asking {
            Some(Asking::RoomName { preview, .. }) => preview.clone(),
            other => panic!("not asking for a room name: {other:?}"),
        }
    }

    #[test]
    fn create_previews_the_room_name_first() {
        let mut s = session();
        assert_eq!(typed(&mut s, "/create CAT"), Reaction::default());
        let first = room_preview(&s);
        assert!(first.starts_with("cat") && crate::names::is_valid_name(&first), "{first}");
        assert_eq!(s.prompt(), Some(format!("new room #{first} — Enter to keep it, or type another name")));

        assert_eq!(typed(&mut s, "chat").print, vec!["! 'chat' is reserved"]);
        assert_eq!(typed(&mut s, "a b").print, vec!["! name must be 1–6 letters or digits"]);
        assert_eq!(room_preview(&s), first, "a bad name keeps the old preview");

        assert_eq!(typed(&mut s, "Dog"), Reaction::default());
        let dog = room_preview(&s);
        assert!(dog.starts_with("dog"), "{dog}");
        assert_eq!(typed(&mut s, ""), Reaction::default(), "Enter moves on to the head count");
        assert_eq!(s.prompt(), Some(format!("how many people in #{dog}? 2–13, Enter for 13, Esc to cancel")));
        assert_eq!(typed(&mut s, "5"), Reaction::send(Command::CreateRoom { name: dog, max: 5 }));
        assert_eq!(s.prompt(), None);
    }

    #[test]
    fn create_with_a_head_count_only_asks_the_name() {
        let mut s = session();
        typed(&mut s, "/create cat 2");
        let name = room_preview(&s);
        assert_eq!(typed(&mut s, ""), Reaction::send(Command::CreateRoom { name, max: 2 }));
        for bad in ["1", "14", "x", "300"] {
            assert_eq!(typed(&mut s, &format!("/create cat {bad}")).print, vec!["! a room holds 2–13 people"]);
            assert_eq!(s.prompt(), None);
        }
        assert_eq!(typed(&mut s, "/create chat").print, vec!["! 'chat' is reserved"]);
        assert_eq!(s.prompt(), None);
    }

    #[test]
    fn the_head_count_step_works_as_before() {
        let mut s = session();
        typed(&mut s, "/create cat");
        let name = room_preview(&s);
        typed(&mut s, "");
        assert_eq!(typed(&mut s, "20").print, vec!["! a room holds 2–13 people — type a number, or Esc to cancel"]);
        assert!(s.prompt().is_some_and(|p| p.starts_with("how many")), "still asking");
        assert_eq!(typed(&mut s, ""), Reaction::send(Command::CreateRoom { name, max: 13 }), "Enter takes the most");

        typed(&mut s, "/create cat");
        assert_eq!(typed(&mut s, "/who"), Reaction::send(Command::ListPeers), "a command drops the question");
        assert_eq!(s.prompt(), None);

        typed(&mut s, "/create cat");
        assert!(s.cancel_prompt(), "Esc drops the name step");
        typed(&mut s, "/create cat");
        typed(&mut s, "");
        assert!(s.cancel_prompt(), "and the head-count step");
        assert!(!s.cancel_prompt());
    }

    #[test]
    fn the_room_preview_avoids_rooms_you_are_in() {
        let mut s = session();
        // In every cat room but catwolf.
        for w in crate::words::WORDS.iter().filter(|w| **w != "wolf") {
            let name = format!("cat{w}");
            s.on_event(&Event::RoomCreated { name, key: "k7m9xq".into(), max: 13 }, 0);
        }
        typed(&mut s, "/create cat");
        assert_eq!(room_preview(&s), "catwolf");
    }

    /// We're in `rooms`, and bob is waiting to get into each.
    fn bob_waits_in(rooms: &[&str]) -> Session {
        let mut s = session();
        for room in rooms {
            s.on_event(&Event::RoomCreated { name: room.to_string(), key: "k7m9xq".into(), max: 13 }, 0);
            let request = Event::JoinRequest { room: room.to_string(), nick: "bobroger".into(), have: 0, need: 1 };
            s.on_event(&request, 0);
        }
        s
    }

    fn approve(room: &str) -> Reaction {
        Reaction::send(Command::Approve { room: room.into(), nick: "bobroger".into() })
    }

    #[test]
    fn invite_sends_to_the_current_or_named_room() {
        let mut s = bob_waits_in(&["catpie"]);
        s.store.switch(Conv::Room("catpie".into()));
        let invite = |room: &str| Reaction::send(Command::Invite { nick: "carolwolf".into(), room: room.into() });
        assert_eq!(typed(&mut s, "/invite @CarolWolf"), invite("catpie"));
        typed(&mut s, "/chat @bobroger");
        assert_eq!(typed(&mut s, "/invite @carolwolf #dogpie"), invite("dogpie"), "the node checks the room");
        assert_eq!(
            typed(&mut s, "/invite @carolwolf"),
            Reaction::say("! /invite works in a room tab, or name the room: /invite @nick #room")
        );
        assert_eq!(typed(&mut s, "/invite").print, vec!["! usage: /invite @nick [#room]"]);
    }

    #[test]
    fn kick_goes_to_the_current_or_named_room() {
        let mut s = bob_waits_in(&["catpie"]);
        s.store.switch(Conv::Room("catpie".into()));
        let kick = |room: &str| Reaction::send(Command::Kick { room: room.into(), nick: "carolwolf".into() });
        assert_eq!(typed(&mut s, "/kick @CarolWolf"), kick("catpie"));
        typed(&mut s, "/chat @bobroger");
        assert_eq!(typed(&mut s, "/kick @carolwolf #dogpie"), kick("dogpie"), "the node checks the room");
        assert_eq!(
            typed(&mut s, "/kick @carolwolf"),
            Reaction::say("! /kick works in a room tab, or name the room: /kick @nick #room")
        );
        assert!(HELP.contains("/kick @nick"));
    }

    #[test]
    fn accept_and_decline_find_the_invitation() {
        let mut s = session();
        assert_eq!(typed(&mut s, "/accept"), Reaction::say("! no invitations"));
        s.on_event(&Event::Invited { from: "bobroger".into(), room: "catpie".into() }, 0);
        assert_eq!(typed(&mut s, "/accept"), Reaction::send(Command::Accept { room: "catpie".into() }));
        s.on_event(&Event::Invited { from: "carolwolf".into(), room: "dogpie".into() }, 0);
        assert_eq!(
            typed(&mut s, "/decline"),
            Reaction::say("! invited to #catpie and #dogpie — say which: /decline #catpie")
        );
        assert_eq!(typed(&mut s, "/decline #dogpie"), Reaction::send(Command::Decline { room: "dogpie".into() }));
        assert_eq!(typed(&mut s, "/decline"), Reaction::send(Command::Decline { room: "catpie".into() }));
        assert!(s.store.invites().is_empty());

        s.on_event(&Event::Invited { from: "bobroger".into(), room: "owlpie".into() }, 0);
        s.on_event(&Event::JoinWaiting { name: "owlpie".into(), members: 2, needs: 1 }, 0);
        assert!(s.store.invites().is_empty(), "accepting it clears it");
        for room in ["apie", "bpie", "cpie"] {
            s.on_event(&Event::Invited { from: "bobroger".into(), room: room.into() }, 0);
        }
        let rooms = |s: &Session| s.store.invites().iter().map(|(_, r)| r.clone()).collect::<Vec<_>>();
        assert_eq!(rooms(&s), ["bpie", "cpie"], "two per inviter, like in the node");
        for (from, room) in [("carolwolf", "dpie"), ("carolwolf", "epie"), ("daveoak", "fpie"), ("erinowl", "gpie")] {
            s.on_event(&Event::Invited { from: from.into(), room: room.into() }, 0);
        }
        assert_eq!(rooms(&s), ["cpie", "dpie", "epie", "fpie", "gpie"], "five in all: the oldest goes");
    }

    #[test]
    fn the_join_line_is_copied_for_rooms_we_are_in() {
        let s = bob_waits_in(&["catpie"]);
        assert_eq!(
            s.copy_join_line("catpie"),
            Reaction {
                copy: Some("/chat #catpie k7m9xq".into()),
                print: vec![
                    "* copied: /chat #catpie k7m9xq (if it didn't copy, Shift/Option-drag to select it)".into()
                ],
                ..Default::default()
            }
        );
        assert_eq!(s.copy_join_line("nopemoon"), Reaction::say("! you're not in #nopemoon"));
    }

    #[test]
    fn approve_finds_the_room_they_wait_in() {
        let mut s = bob_waits_in(&["catpie"]);
        typed(&mut s, "/chat @bobroger");
        assert_eq!(typed(&mut s, "/approve @bobroger"), approve("catpie"));
        assert_eq!(
            typed(&mut s, "/deny @bobroger"),
            Reaction::send(Command::Deny { room: "catpie".into(), nick: "bobroger".into() })
        );
        assert_eq!(typed(&mut s, "/approve @bobroger #dogwolf"), approve("dogwolf"), "an explicit room is passed on");
        assert_eq!(
            typed(&mut s, "/approve @carolwolf").print,
            vec!["! @carolwolf isn't waiting to join any of your rooms"]
        );
    }

    #[test]
    fn approve_prefers_the_current_room() {
        let mut s = bob_waits_in(&["catpie", "dogwolf"]);
        s.store.switch(Conv::Room("catpie".into()));
        assert_eq!(typed(&mut s, "/approve @bobroger"), approve("catpie"));
    }

    #[test]
    fn approve_needs_a_room_when_ambiguous() {
        let mut s = bob_waits_in(&["catpie", "dogwolf"]);
        typed(&mut s, "/chat @bobroger");
        assert_eq!(
            typed(&mut s, "/approve @bobroger").print,
            vec!["! @bobroger is waiting in #catpie and #dogwolf — say which: /approve @bobroger #catpie"]
        );
    }

    #[test]
    fn text_goes_where_the_prompt_pointed_even_if_a_join_switched_us() {
        let mut s = session();
        typed(&mut s, "/chat @bobroger");
        typed(&mut s, "/chat #catpie 1-abcdef");
        // The user starts typing at the "@bobroger> " prompt...
        let shown = s.store.current().cloned();
        assert_eq!(shown, Some(Conv::Dm("bobroger".into())));
        // ...and the join completes meanwhile, switching the current conversation.
        s.on_event(&Event::RoomJoined { name: "catpie".into(), key: "k7m9xq".into(), max: 13, members: vec![] }, 0);
        assert_eq!(s.on_line("for bob only", shown.as_ref()), send(Target::Dm("bobroger".into()), "for bob only"));
        assert_eq!(s.store.current(), Some(&Conv::Room("catpie".into())), "the next prompt shows the room");
    }

    #[test]
    fn text_typed_right_after_creating_a_room_goes_to_it() {
        let mut s = session();
        // The prompt after /create still shows no conversation...
        typed(&mut s, "/create cat 123");
        let shown = s.store.current().cloned();
        assert_eq!(shown, None);
        // ...the room arrives while the user types, and that's where they mean.
        s.on_event(&Event::RoomCreated { name: "catpie".into(), key: "123-k7m9xq".into(), max: 13 }, 0);
        assert_eq!(s.on_line("first!", shown.as_ref()), send(Target::Room("catpie".into()), "first!"));
    }

    #[test]
    fn leave_uses_the_current_room_or_the_named_one() {
        let mut s = session();
        assert_eq!(typed(&mut s, "/leave").print, vec!["! you're not in a room here — /leave #room"]);
        s.on_event(&Event::RoomCreated { name: "catpie".into(), key: "1-abcdef".into(), max: 13 }, 0);
        assert_eq!(typed(&mut s, "/leave"), Reaction::send(Command::LeaveRoom { name: "catpie".into() }));
        assert_eq!(typed(&mut s, "/leave #dogwolf"), Reaction::send(Command::LeaveRoom { name: "dogwolf".into() }));
        s.on_event(&Event::RoomLeft { name: "catpie".into() }, 0);
        assert_eq!(s.store.current(), None);
    }

    #[test]
    fn chatting_with_yourself_or_someone_offline_is_refused() {
        let mut s = session();
        assert_eq!(typed(&mut s, "/chat @annmoon").print, vec!["! that's you"]);
        assert_eq!(typed(&mut s, "/chat @ghostmoon").print, vec!["! @ghostmoon is offline"]);
    }

    #[test]
    fn switching_marks_read_and_says_nothing() {
        let mut s = session();
        for text in ["one", "@annmoon two"] {
            s.on_event(
                &Event::Message {
                    conv: Conv::Dm("bobroger".into()),
                    from: "bobroger".into(),
                    text: text.into(),
                    ts: 0,
                },
                0,
            );
        }
        assert_eq!(typed(&mut s, "/chat @bobroger"), Reaction::default(), "the tab already shows the counts");
        let c = s.store.get(&Conv::Dm("bobroger".into())).unwrap();
        assert_eq!((c.unread, c.mentions), (0, 0));
    }

    #[test]
    fn help_lists_the_tui_commands_and_keys() {
        let mut s = session();
        for cmd in ["/help", "/h", "/?"] {
            let r = typed(&mut s, cmd);
            assert!(r.help, "{cmd} opens the help box");
            assert!(r.print.is_empty(), "{cmd} prints nothing to the corvo tab");
        }
        assert!(HELP.contains("/close") && HELP.contains("Shift-Tab") && HELP.contains("/quit"));
        assert!(!HELP.contains("/line") && !HELP.contains("/his") && !HELP.contains("/tui"));
        assert!(HELP.contains("/create <name>") && HELP.contains("/approve") && HELP.contains("/deny"));
    }

    #[test]
    fn lines_are_plain_even_with_colour_on() {
        let mut s = Session::new(true);
        assert!(s.color());
        let text = s.on_event(&Event::PeerJoined { id: PeerId::test(3), nick: "daveelk".into() }, 0).unwrap();
        assert!(!text.contains('\x1b'), "{text:?}");
    }

    #[test]
    fn msg_who_quit_and_errors() {
        let mut s = session();
        assert_eq!(typed(&mut s, "/msg @bobroger hi"), send(Target::Dm("bobroger".into()), "hi"));
        assert_eq!(typed(&mut s, "/who"), Reaction::send(Command::ListPeers));
        assert!(typed(&mut s, "/quit").quit);
        assert!(typed(&mut s, "/q").quit);
        assert_eq!(typed(&mut s, "/nick x").print, vec!["! names are assigned by the system and can't be changed"]);
    }

    #[test]
    fn names_without_a_sigil_are_guessed() {
        let mut s = session(); // me: annmoon; online: bobroger
        s.on_event(&Event::PeerJoined { id: PeerId::test(3), nick: "bobkite".into() }, 0);
        s.on_event(&Event::PeerJoined { id: PeerId::test(4), nick: "carolwolf".into() }, 0);
        s.on_event(&Event::RoomJoined { name: "catpie".into(), key: "k7m9xq".into(), max: 13, members: vec![] }, 0);
        s.on_event(&Event::RoomJoined { name: "carolpie".into(), key: "k7m9xq".into(), max: 13, members: vec![] }, 0);

        assert_eq!(typed(&mut s, "/chat catp"), Reaction::default());
        assert_eq!(s.store.current(), Some(&Conv::Room("catpie".into())));
        assert_eq!(typed(&mut s, "/c CAROLWOLF"), Reaction::default(), "an exact name wins, any case");
        assert_eq!(s.store.current(), Some(&Conv::Dm("carolwolf".into())));
        assert_eq!(
            typed(&mut s, "/chat carol").print,
            vec!["! \"carol\" matches @carolWolf, #carolPie — be more specific"]
        );
        assert_eq!(typed(&mut s, "/chat bob").print, vec!["! \"bob\" matches @bobKite, @bobRoger — be more specific"]);
        assert_eq!(
            typed(&mut s, "/chat ann").print,
            vec!["! no one or no room called \"ann\" — use @name for people or #name for rooms"],
            "never me"
        );
        assert_eq!(typed(&mut s, "/msg bobk hi"), send(Target::Dm("bobkite".into()), "hi"));
        assert_eq!(typed(&mut s, "/m catp hi").print, vec!["! no one called \"catp\" is online"], "/msg skips rooms");
    }

    #[test]
    fn a_long_list_of_guesses_is_cut_short() {
        let mut s = session();
        for i in 0..7 {
            s.on_event(&Event::PeerJoined { id: PeerId::test(10 + i), nick: format!("zed{i}owl") }, 0);
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
        assert_eq!(typed(&mut s, "/update y").update, Some(UpdateAction::Install));
        assert!(HELP.contains("/update"));
    }

    const V: Version = Version(0, 6, 0);

    #[test]
    fn the_update_question_takes_y_yes_n_no() {
        let mut s = session();
        assert!(s.offer_update(V));
        assert_eq!(s.prompt().as_deref(), Some("v0.6.0 is available — install it now? (y/n)"));
        assert!(s.takes("Y") && s.takes(" no ") && !s.takes("hello"));
        assert_eq!(typed(&mut s, "Y").update, Some(UpdateAction::Install));
        assert_eq!(s.prompt(), None);
        s.offer_update(V);
        assert_eq!(typed(&mut s, " no ").print, vec!["* ok — /update yes installs it later"]);
        assert_eq!(s.prompt(), None);
        s.offer_update(V);
        assert_eq!(typed(&mut s, "yes").update, Some(UpdateAction::Install));
        s.offer_update(V);
        typed(&mut s, "/update");
        assert_eq!(s.prompt(), None, "/update asks again itself");
    }

    #[test]
    fn other_text_passes_through_an_update_question() {
        let mut s = session();
        typed(&mut s, "/chat @bobroger");
        s.offer_update(V);
        assert_eq!(typed(&mut s, "hello"), send(Target::Dm("bobroger".into()), "hello"));
        assert!(s.prompt().is_some(), "the question stays");
        assert_eq!(typed(&mut s, "/who").commands, vec![Command::ListPeers]);
        assert!(s.prompt().is_some());
    }

    #[test]
    fn an_offer_during_another_question_waits_its_turn() {
        let mut s = Session::new(false);
        s.on_event(&Event::NamePreview { nick: "annmoon".into() }, 0);
        let asked = s.prompt();
        assert!(!s.offer_update(V), "not asked yet");
        assert_eq!(s.prompt(), asked, "the name question stays");
        assert!(!s.ask_pending(), "still busy");
        s.on_event(&Event::Assigned { nick: "annmoon".into(), renamed: false }, 0);
        assert!(s.ask_pending());
        assert_eq!(s.prompt().as_deref(), Some("v0.6.0 is available — install it now? (y/n)"));
        assert!(!s.ask_pending(), "asked once");

        typed(&mut s, "n");
        typed(&mut s, "/create cat");
        assert!(!s.offer_update(V));
        assert!(s.cancel_prompt());
        assert!(s.ask_pending(), "Esc on the create question lets it through");
        s.cancel_prompt();
        typed(&mut s, "/create cat");
        s.offer_update(V);
        typed(&mut s, "/update");
        s.cancel_prompt();
        assert!(!s.ask_pending(), "/update asks for itself, so the queued one goes");
    }

    #[test]
    fn the_name_question_puts_an_earlier_offer_back_in_line() {
        let mut s = Session::new(false);
        assert!(s.offer_update(V), "asked before the name preview arrived");
        s.on_event(&Event::NamePreview { nick: "annmoon".into() }, 0);
        assert!(s.prompt().is_some_and(|p| p.contains("you'll be @annmoon")));
        s.on_event(&Event::Assigned { nick: "annmoon".into(), renamed: false }, 0);
        assert!(s.ask_pending(), "the offer comes back");
    }

    #[test]
    fn the_restart_question() {
        let mut s = session();
        assert_eq!(s.offer_restart(V), None);
        assert_eq!(s.prompt().as_deref(), Some("v0.6.0 is installed — restart now? You'll leave your rooms. (y/n)"));
        assert!(typed(&mut s, "y").restart);
        s.offer_restart(V);
        assert_eq!(typed(&mut s, "n").print, vec!["* v0.6.0 starts next time you open corvo-lan"]);
        s.offer_update(V);
        assert_eq!(
            s.offer_restart(V).as_deref(),
            Some("* v0.6.0 starts next time you open corvo-lan"),
            "no restart question while another is open"
        );
    }

    #[test]
    fn esc_drops_an_update_question() {
        let mut s = session();
        assert!(!s.takes("y"), "nothing asked");
        s.offer_update(V);
        assert!(s.cancel_prompt());
        assert_eq!(s.prompt(), None);
    }

    #[test]
    fn clear_asks_the_ui_to_clear() {
        let mut s = session();
        assert_eq!(typed(&mut s, "/clear"), Reaction { clear: true, ..Default::default() });
        assert!(HELP.contains("  /clear               clear the screen"));
    }

    #[test]
    fn close_forgets_a_dm_but_not_a_room() {
        let mut s = session();
        typed(&mut s, "/chat @bobroger");
        assert_eq!(typed(&mut s, "/close").print, vec!["* closed @bobroger"]);
        assert!(s.store.get(&Conv::Dm("bobroger".into())).is_none());
        assert_eq!(typed(&mut s, "/close").print, vec!["! no chat to close here"]);
        s.on_event(&Event::RoomJoined { name: "catpie".into(), key: "k7m9xq".into(), max: 13, members: vec![] }, 0);
        assert_eq!(typed(&mut s, "/close").print, vec!["! use /leave for rooms"]);
        assert_eq!(typed(&mut s, "/close @ghostmoon").print, vec!["! no chat with @ghostmoon"]);
    }

    #[test]
    fn multi_line_messages_are_tidied_and_capped() {
        assert_eq!(tidy_message("hi\nthere\n\n  \n"), Ok("hi\nthere".into()));
        assert_eq!(tidy_message("  \n"), Err("message is empty".into()));
        let long = vec!["x"; MAX_MESSAGE_LINES + 1].join("\n");
        assert_eq!(tidy_message(&long), Err(format!("message has more than {MAX_MESSAGE_LINES} lines")));
        assert!(tidy_message(&vec!["x"; MAX_MESSAGE_LINES].join("\n")).is_ok());

        let mut s = session();
        typed(&mut s, "/chat @bobroger");
        assert_eq!(typed(&mut s, "one\ntwo\n\n"), send(Target::Dm("bobroger".into()), "one\ntwo"));
        assert_eq!(typed(&mut s, &long).print, vec![format!("! message has more than {MAX_MESSAGE_LINES} lines")]);
        assert_eq!(typed(&mut s, "/msg @bobroger a\nb\n"), send(Target::Dm("bobroger".into()), "a\nb"));
    }

    #[test]
    fn the_room_name_step_takes_a_hash_and_points_numbers_to_the_next_step() {
        let mut s = session();
        typed(&mut s, "/create cat");
        let first = room_preview(&s);
        assert_eq!(
            typed(&mut s, "5"),
            Reaction::say(format!("! Enter keeps #{first} first — then it asks how many people"))
        );
        assert_eq!(room_preview(&s), first);
        assert_eq!(typed(&mut s, "#Dog"), Reaction::default());
        assert!(room_preview(&s).starts_with("dog"), "{}", room_preview(&s));
    }

    #[test]
    fn the_room_preview_skips_rooms_we_are_waiting_for() {
        let mut s = session();
        for w in crate::words::WORDS.iter().filter(|w| !["wolf", "owl"].contains(w)) {
            let name = format!("cat{w}");
            s.on_event(&Event::RoomCreated { name, key: "k7m9xq".into(), max: 13 }, 0);
        }
        s.on_event(&Event::JoinWaiting { name: "catowl".into(), members: 2, needs: 1 }, 0);
        for _ in 0..30 {
            typed(&mut s, "/create cat");
            assert_eq!(room_preview(&s), "catwolf", "never the room we're knocking on");
            s.cancel_prompt();
        }
    }

    #[test]
    fn a_failed_accept_clears_the_invitation() {
        let mut s = session();
        s.on_event(&Event::Invited { from: "bobroger".into(), room: "catpie".into() }, 0);
        s.on_event(&Event::JoinEnded { name: "catpie".into(), why: crate::api::JoinEnd::NoRoom }, 0);
        assert!(s.store.invites().is_empty());
        assert_eq!(typed(&mut s, "/accept"), Reaction::say("! no invitations"));
    }

    #[test]
    fn invite_from_the_corvo_tab_names_the_room_syntax() {
        let mut s = session();
        s.store.clear_current();
        assert_eq!(
            typed(&mut s, "/invite @bobroger"),
            Reaction::say("! /invite works in a room tab, or name the room: /invite @nick #room")
        );
    }
}
