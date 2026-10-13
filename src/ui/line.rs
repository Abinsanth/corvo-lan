//! Line-based frontend. `Session` is the pure part (input + events → commands
//! + output, backed by the conversation store); `run` wires it to rustyline.

use std::sync::{Arc, Mutex};

use rustyline::error::ReadlineError;
use rustyline::{DefaultEditor, ExternalPrinter as _};
use tokio::sync::mpsc;

use super::input::{parse_line, Input};
use super::render::{self, Style};
use crate::api::{Command, Conv, Event, Target};
use crate::convo::Store;
use crate::node::NodeHandle;
use crate::protocol::now_millis;

pub const HELP: &str = "\
commands:
  <text>                 send to the current conversation (shown in the prompt)
  /msg @nick <text>      one-off private message
  /chat @nick            switch to a private chat
  /chat #room <key>      join a room (just /chat #room once you're in)
  /create <name> <key>   create a private room; share its name and key to invite
  /leave [#room]         leave the current or named room
  /rooms                 your rooms and who's in them
  /chats                 your conversations, with unread counts
  /history [n]           show the last n messages here (default 20)
  /who                   connected people
  /quit                  leave (also Ctrl-C / Ctrl-D)
  //text                 send a message starting with '/'";

/// What the UI loop should do after an input line.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Reaction {
    pub commands: Vec<Command>,
    pub print: Vec<String>,
    pub quit: bool,
}

impl Reaction {
    fn say(s: impl Into<String>) -> Self {
        Reaction { print: vec![s.into()], ..Default::default() }
    }

    fn send(cmd: Command) -> Self {
        Reaction { commands: vec![cmd], ..Default::default() }
    }
}

pub struct Session {
    pub store: Store,
    style: Style,
}

impl Session {
    pub fn new(style: Style) -> Self {
        Session { store: Store::new(), style }
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

    pub fn on_line(&mut self, line: &str) -> Reaction {
        match parse_line(line) {
            Input::Empty => Reaction::default(),
            Input::Error(e) => Reaction::say(format!("! {e}")),
            Input::Help => Reaction::say(HELP),
            Input::Quit => Reaction { quit: true, ..Default::default() },
            Input::Who => Reaction::send(Command::ListPeers),
            Input::Text(text) => match self.store.current() {
                Some(Conv::Room(n)) => Reaction::send(Command::Send { to: Target::Room(n.clone()), text }),
                Some(Conv::Dm(n)) => Reaction::send(Command::Send { to: Target::Dm(n.clone()), text }),
                None => Reaction::say("! no conversation — /chat @nick or /create <name> <key>"),
            },
            Input::Msg { to, text } => Reaction::send(Command::Send { to: Target::Dm(to), text }),
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
            Input::Create { base, part } => Reaction::send(Command::CreateRoom { base, key_part: part }),
            Input::Leave(Some(name)) => Reaction::send(Command::LeaveRoom { name }),
            Input::Leave(None) => match self.store.current() {
                Some(Conv::Room(n)) => Reaction::send(Command::LeaveRoom { name: n.clone() }),
                _ => Reaction::say("! you're not in a room here — /leave #room"),
            },
            Input::Rooms => Reaction::say(render::rooms(&self.store, &self.style)),
            Input::Chats => Reaction::say(render::chats(&self.store, &self.style)),
            Input::History(n) => {
                let Some(conv) = self.store.current().cloned() else {
                    return Reaction::say("! no conversation — /chat @nick or /chat #room");
                };
                let lines = self.store.history(n);
                if lines.is_empty() {
                    return Reaction::say("* nothing here yet");
                }
                Reaction { print: lines.iter().map(|l| render::line(&conv, l, &self.style)).collect(), ..Default::default() }
            }
        }
    }

    fn switch(&mut self, conv: Conv) -> Reaction {
        let unread = self.store.get(&conv).map_or(0, |c| c.unread);
        self.store.switch(conv.clone());
        if unread > 0 {
            let name = render::conv_name(&conv, &self.style);
            Reaction::say(format!("* {name}: {unread} unread — /history to see them"))
        } else {
            Reaction::default()
        }
    }
}

/// Runs `print` on its own thread. Sending never blocks, so the UI task can't
/// stall on rustyline's printer (which blocks while a prompt isn't being read).
pub fn spawn_printer(mut print: impl FnMut(String) + Send + 'static) -> std::sync::mpsc::Sender<String> {
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        for msg in rx {
            print(msg);
        }
    });
    tx
}

/// Consecutive non-fatal readline errors tolerated before giving up on input.
const MAX_READ_ERRORS: u32 = 3;

/// Whether the input thread should stop after `err` (the `consecutive`-th in a row).
fn readline_gives_up(err: &ReadlineError, consecutive: u32) -> bool {
    matches!(err, ReadlineError::Interrupted | ReadlineError::Eof) || consecutive >= MAX_READ_ERRORS
}

/// Runs the frontend until the user quits, then waits for the Node to say goodbye.
pub async fn run(handle: NodeHandle) -> anyhow::Result<()> {
    let NodeHandle { commands, mut events, task, .. } = handle;
    let mut editor = DefaultEditor::new()?;
    // The external printer redraws the prompt around output; it needs a real
    // terminal, so piped stdin falls back to plain println.
    let out = match editor.create_external_printer() {
        Ok(mut printer) => spawn_printer(move |s| {
            let _ = printer.print(s);
        }),
        Err(_) => spawn_printer(|s| println!("{s}")),
    };
    let print = |s: String| {
        let _ = out.send(s);
    };

    // rustyline blocks, so it lives on its own thread. It waits for `resume`
    // before each prompt so we never exit while the terminal is in raw mode,
    // and reads the prompt then so it follows conversation switches.
    let prompt = Arc::new(Mutex::new(String::from("> ")));
    let (line_tx, mut lines) = mpsc::unbounded_channel::<Option<String>>();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel::<()>();
    let thread_prompt = prompt.clone();
    std::thread::spawn(move || {
        let mut errors = 0;
        loop {
            let p = thread_prompt.lock().map(|p| p.clone()).unwrap_or_else(|_| "> ".into());
            match editor.readline(&p) {
                Ok(line) => {
                    errors = 0;
                    let _ = editor.add_history_entry(line.as_str());
                    if line_tx.send(Some(line)).is_err() || resume_rx.recv().is_err() {
                        break;
                    }
                }
                Err(e) => {
                    errors += 1;
                    if readline_gives_up(&e, errors) {
                        let _ = line_tx.send(None);
                        break;
                    }
                }
            }
        }
    });

    let mut session = Session::new(Style::detect());
    let result = event_loop(&mut session, &commands, &mut events, &mut lines, &resume_tx, &prompt, print).await;
    drop(resume_tx);
    let _ = task.await;
    result
}

/// Routes input lines to the Node and events to `print` until the user quits.
/// Returns only while no prompt is active, so the terminal is never left raw.
async fn event_loop(
    session: &mut Session,
    commands: &mpsc::Sender<Command>,
    events: &mut mpsc::UnboundedReceiver<Event>,
    lines: &mut mpsc::UnboundedReceiver<Option<String>>,
    resume: &std::sync::mpsc::Sender<()>,
    prompt: &Mutex<String>,
    print: impl Fn(String),
) -> anyhow::Result<()> {
    loop {
        tokio::select! {
            event = events.recv() => match event {
                Some(event) => {
                    if let Some(text) = session.on_event(&event, now_millis()) {
                        print(text);
                    }
                    if let Ok(mut p) = prompt.lock() {
                        *p = session.prompt();
                    }
                }
                None => {
                    // A prompt is showing; wait for it to return before exiting.
                    print("! chat core stopped unexpectedly; press Enter to exit".into());
                    lines.recv().await;
                    return Ok(());
                }
            },
            line = lines.recv() => {
                let Some(line) = line.flatten() else {
                    let _ = commands.send(Command::Quit).await;
                    return Ok(());
                };
                let reaction = session.on_line(&line);
                for text in reaction.print {
                    print(text);
                }
                if reaction.quit {
                    let _ = commands.send(Command::Quit).await;
                    return Ok(());
                }
                for cmd in reaction.commands {
                    commands.send(cmd).await?;
                }
                if let Ok(mut p) = prompt.lock() {
                    *p = session.prompt();
                }
                let _ = resume.send(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::PeerId;

    fn session() -> Session {
        let mut s = Session::new(Style::plain());
        s.on_event(&Event::Assigned { nick: "annMoon".into(), renamed: false }, 0);
        s.on_event(&Event::PeerJoined { id: PeerId(2), nick: "bobRoger".into() }, 0);
        s
    }

    fn send(to: Target, text: &str) -> Reaction {
        Reaction::send(Command::Send { to, text: text.into() })
    }

    #[test]
    fn plain_text_goes_to_the_current_conversation() {
        let mut s = session();
        assert_eq!(s.on_line("hello").print, vec!["! no conversation — /chat @nick or /create <name> <key>"]);
        assert_eq!(s.on_line("/chat @bobRoger"), Reaction::default());
        assert_eq!(s.prompt(), "@bobRoger> ");
        assert_eq!(s.on_line("hello"), send(Target::Dm("bobRoger".into()), "hello"));
    }

    #[test]
    fn creating_or_joining_a_room_switches_to_it() {
        let mut s = session();
        assert_eq!(
            s.on_line("/create cat 123"),
            Reaction::send(Command::CreateRoom { base: "cat".into(), key_part: "123".into() })
        );
        s.on_event(&Event::RoomCreated { name: "catPieR".into(), key: "123-k7m9xq".into() }, 0);
        assert_eq!(s.prompt(), "#catPieR> ");
        assert_eq!(s.on_line("meow"), send(Target::Room("catPieR".into()), "meow"));

        assert_eq!(
            s.on_line("/chat #dogFoxR 1-abcdef"),
            Reaction::send(Command::JoinRoom { name: "dogFoxR".into(), key: "1-abcdef".into() })
        );
        assert_eq!(s.prompt(), "#catPieR> ", "not until the join succeeds");
        s.on_event(&Event::RoomJoined { name: "dogFoxR".into(), members: vec!["bobRoger".into()] }, 0);
        assert_eq!(s.prompt(), "#dogFoxR> ");
        assert_eq!(s.on_line("/chat #catPieR"), Reaction::default(), "already in: just switch");
        assert_eq!(s.prompt(), "#catPieR> ");
        assert_eq!(s.on_line("/chat #nopeMoonR").print, vec!["! join with /chat #nopeMoonR <key>"]);
    }

    #[test]
    fn leave_uses_the_current_room_or_the_named_one() {
        let mut s = session();
        assert_eq!(s.on_line("/leave").print, vec!["! you're not in a room here — /leave #room"]);
        s.on_event(&Event::RoomCreated { name: "catPieR".into(), key: "1-abcdef".into() }, 0);
        assert_eq!(s.on_line("/leave"), Reaction::send(Command::LeaveRoom { name: "catPieR".into() }));
        assert_eq!(s.on_line("/leave #dogFoxR"), Reaction::send(Command::LeaveRoom { name: "dogFoxR".into() }));
        s.on_event(&Event::RoomLeft { name: "catPieR".into() }, 0);
        assert_eq!(s.prompt(), "> ");
    }

    #[test]
    fn chatting_with_yourself_or_someone_offline_is_refused() {
        let mut s = session();
        assert_eq!(s.on_line("/chat @annMoon").print, vec!["! that's you"]);
        assert_eq!(s.on_line("/chat @ghostMoon").print, vec!["! @ghostMoon is offline"]);
    }

    #[test]
    fn switching_mentions_unread_and_history_shows_them() {
        let mut s = session();
        s.on_event(&Event::Message { conv: Conv::Dm("bobRoger".into()), from: "bobRoger".into(), text: "one".into(), ts: 0 }, 0);
        s.on_event(&Event::Message { conv: Conv::Dm("bobRoger".into()), from: "bobRoger".into(), text: "two".into(), ts: 0 }, 0);
        assert_eq!(s.on_line("/chat @bobRoger").print, vec!["* @bobRoger: 2 unread — /history to see them"]);
        let shown = s.on_line("/history 1").print;
        assert_eq!(shown.len(), 1);
        assert!(shown[0].ends_with("@bobRoger → you: two"), "{shown:?}");
    }

    #[test]
    fn msg_who_quit_and_errors() {
        let mut s = session();
        assert_eq!(s.on_line("/msg @bobRoger hi"), send(Target::Dm("bobRoger".into()), "hi"));
        assert_eq!(s.on_line("/who"), Reaction::send(Command::ListPeers));
        assert!(s.on_line("/quit").quit);
        assert_eq!(s.on_line("/nick x").print, vec!["! names are assigned by the system and can't be changed"]);
        assert_eq!(s.on_line("/history").print, vec!["! no conversation — /chat @nick or /chat #room"]);
    }

    #[test]
    fn only_ctrl_c_eof_or_repeated_errors_end_input() {
        assert!(readline_gives_up(&ReadlineError::Interrupted, 1));
        assert!(readline_gives_up(&ReadlineError::Eof, 1));
        let glitch = || ReadlineError::Io(std::io::Error::other("glitch"));
        assert!(!readline_gives_up(&glitch(), 1));
        assert!(!readline_gives_up(&glitch(), 2));
        assert!(readline_gives_up(&glitch(), 3));
    }

    #[tokio::test]
    async fn when_the_core_stops_we_wait_for_the_prompt_before_exiting() {
        use std::time::Duration;
        let (cmd_tx, _cmd_rx) = mpsc::channel(4);
        let (event_tx, mut events) = mpsc::unbounded_channel();
        let (line_tx, mut lines) = mpsc::unbounded_channel();
        let (resume_tx, _resume_rx) = std::sync::mpsc::channel();
        let printed = Arc::new(Mutex::new(Vec::<String>::new()));
        let log = printed.clone();
        drop(event_tx); // the Node is gone
        let ui = tokio::spawn(async move {
            let print = move |s: String| log.lock().unwrap().push(s);
            let prompt = Mutex::new(String::new());
            let mut session = Session::new(Style::plain());
            event_loop(&mut session, &cmd_tx, &mut events, &mut lines, &resume_tx, &prompt, print).await
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        // Exiting now would leave rustyline's prompt (and raw mode) behind.
        assert!(!ui.is_finished());
        assert!(printed.lock().unwrap().iter().any(|s| s.contains("press Enter")));
        line_tx.send(Some(String::new())).unwrap(); // user presses Enter
        tokio::time::timeout(Duration::from_secs(1), ui).await.unwrap().unwrap().unwrap();
    }

    #[test]
    fn printer_never_blocks_the_caller_and_keeps_order() {
        use std::time::{Duration, Instant};
        let (gate_tx, gate_rx) = std::sync::mpsc::channel::<()>();
        let (seen_tx, seen_rx) = std::sync::mpsc::channel::<String>();
        // A sink that blocks until released, like rustyline's printer in raw mode.
        let out = spawn_printer(move |s| {
            let _ = gate_rx.recv();
            let _ = seen_tx.send(s);
        });
        let start = Instant::now();
        for i in 0..100 {
            out.send(i.to_string()).unwrap();
        }
        assert!(start.elapsed() < Duration::from_millis(100));
        for _ in 0..100 {
            gate_tx.send(()).unwrap();
        }
        let got: Vec<String> = (0..100).map(|_| seen_rx.recv_timeout(Duration::from_secs(1)).unwrap()).collect();
        assert_eq!(got, (0..100).map(|i| i.to_string()).collect::<Vec<_>>());
    }
}
