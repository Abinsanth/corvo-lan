//! Line-based frontend: stdin lines -> `Command`, `Event` -> printed lines.

use rustyline::error::ReadlineError;
use rustyline::{DefaultEditor, ExternalPrinter as _};
use tokio::sync::mpsc;

use crate::api::{Command, Event, LeaveReason};
use crate::node::NodeHandle;

pub const HELP: &str = "\
commands:
  <text>              send to everyone
  /msg <nick> <text>  private message (use nick#abcd if two peers share a nick)
  /who                list connected peers
  /nick <new>         change your nick
  /quit               leave (also Ctrl-C / Ctrl-D)
  //text              send a message starting with '/'";

#[derive(Debug, PartialEq, Eq)]
pub enum Input {
    Command(Command),
    Help,
    Empty,
    Error(String),
}

pub fn parse_line(line: &str) -> Input {
    if line.trim().is_empty() {
        return Input::Empty;
    }
    if let Some(escaped) = line.strip_prefix("//") {
        return Input::Command(Command::Say(format!("/{escaped}")));
    }
    let Some(rest) = line.strip_prefix('/') else {
        return Input::Command(Command::Say(line.to_string()));
    };
    let (name, args) = match rest.split_once(char::is_whitespace) {
        Some((name, args)) => (name, args.trim()),
        None => (rest, ""),
    };
    match name {
        "msg" => match args.split_once(char::is_whitespace) {
            Some((to, text)) if !text.trim().is_empty() => {
                Input::Command(Command::Direct { to: to.to_string(), text: text.trim_start().to_string() })
            }
            _ => Input::Error("usage: /msg <nick> <text>".into()),
        },
        "who" => Input::Command(Command::ListPeers),
        "nick" if !args.is_empty() => Input::Command(Command::SetNick(args.to_string())),
        "nick" => Input::Error("usage: /nick <new>".into()),
        "quit" | "exit" => Input::Command(Command::Quit),
        "help" => Input::Help,
        other => Input::Error(format!("unknown command /{other}, try /help")),
    }
}

/// Replaces control characters (including newlines and ANSI escapes) and bidi
/// overrides so a peer cannot rewrite our terminal or reverse displayed text.
/// Zero-width joiners and variation selectors stay, since emoji need them.
pub fn clean(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() || is_bidi_control(c) { '\u{fffd}' } else { c })
        .collect()
}

fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

pub fn format_event(event: &Event) -> String {
    match event {
        Event::PeerJoined { nick, .. } => format!("* {} joined", clean(nick)),
        Event::PeerLeft { nick, reason: LeaveReason::Quit, .. } => format!("* {} left", clean(nick)),
        Event::PeerLeft { nick, reason: LeaveReason::Lost, .. } => {
            format!("* {} left (connection lost)", clean(nick))
        }
        Event::NickChanged { old, new, .. } => format!("* {} is now {}", clean(old), clean(new)),
        Event::Message { from_nick, text, direct: false, .. } => format!("<{}> {}", clean(from_nick), clean(text)),
        Event::Message { from_nick, text, direct: true, .. } => {
            format!("[DM from {}] {}", clean(from_nick), clean(text))
        }
        Event::DirectSent { to, text } => format!("[DM to {}] {}", clean(to), clean(text)),
        Event::Peers(peers) if peers.is_empty() => "* no peers connected".into(),
        Event::Peers(peers) => {
            let mut out = format!("* {} peer(s) online:", peers.len());
            for p in peers {
                out.push_str(&format!("\n  {:<20} {}  {}", clean(&p.nick), p.id, p.addr));
            }
            out
        }
        Event::Error(msg) => format!("! {}", clean(msg)),
        Event::Info(msg) => format!("* {}", clean(msg)),
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
    // before each prompt so we never exit while the terminal is in raw mode.
    let (line_tx, mut lines) = mpsc::unbounded_channel::<Option<String>>();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        let mut errors = 0;
        loop {
            match editor.readline("> ") {
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

    let result = event_loop(&commands, &mut events, &mut lines, &resume_tx, print).await;
    drop(resume_tx);
    let _ = task.await;
    result
}

/// Routes input lines to the Node and events to `print` until the user quits.
/// Returns only while no prompt is active, so the terminal is never left raw.
async fn event_loop(
    commands: &mpsc::Sender<Command>,
    events: &mut mpsc::UnboundedReceiver<Event>,
    lines: &mut mpsc::UnboundedReceiver<Option<String>>,
    resume: &std::sync::mpsc::Sender<()>,
    print: impl Fn(String),
) -> anyhow::Result<()> {
    loop {
        tokio::select! {
            event = events.recv() => match event {
                Some(event) => print(format_event(&event)),
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
                match parse_line(&line) {
                    Input::Command(Command::Quit) => {
                        let _ = commands.send(Command::Quit).await;
                        return Ok(());
                    }
                    Input::Command(cmd) => commands.send(cmd).await?,
                    Input::Help => print(HELP.to_string()),
                    Input::Empty => {}
                    Input::Error(e) => print(format!("! {e}")),
                }
                let _ = resume.send(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{PeerId, PeerInfo};

    fn say(s: &str) -> Input {
        Input::Command(Command::Say(s.into()))
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
        use std::sync::{Arc, Mutex};
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
            event_loop(&cmd_tx, &mut events, &mut lines, &resume_tx, print).await
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        // Exiting now would leave rustyline's prompt (and raw mode) behind.
        assert!(!ui.is_finished());
        assert!(printed.lock().unwrap().iter().any(|s| s.contains("press Enter")));
        line_tx.send(Some(String::new())).unwrap(); // user presses Enter
        tokio::time::timeout(Duration::from_secs(1), ui).await.unwrap().unwrap().unwrap();
    }

    #[test]
    fn plain_text_is_a_room_message() {
        assert_eq!(parse_line("hello there"), say("hello there"));
        assert_eq!(parse_line("   "), Input::Empty);
        assert_eq!(parse_line("//shrug"), say("/shrug"));
    }

    #[test]
    fn msg_command() {
        assert_eq!(
            parse_line("/msg bob hi  there"),
            Input::Command(Command::Direct { to: "bob".into(), text: "hi  there".into() })
        );
        assert_eq!(parse_line("/msg bob"), Input::Error("usage: /msg <nick> <text>".into()));
        assert_eq!(parse_line("/msg bob    "), Input::Error("usage: /msg <nick> <text>".into()));
        assert_eq!(parse_line("/msg"), Input::Error("usage: /msg <nick> <text>".into()));
    }

    #[test]
    fn other_commands() {
        assert_eq!(parse_line("/who"), Input::Command(Command::ListPeers));
        assert_eq!(parse_line("/nick robert"), Input::Command(Command::SetNick("robert".into())));
        assert_eq!(parse_line("/nick"), Input::Error("usage: /nick <new>".into()));
        assert_eq!(parse_line("/quit"), Input::Command(Command::Quit));
        assert_eq!(parse_line("/exit"), Input::Command(Command::Quit));
        assert_eq!(parse_line("/help"), Input::Help);
        assert_eq!(parse_line("/dance"), Input::Error("unknown command /dance, try /help".into()));
    }

    #[test]
    fn clean_strips_bidi_overrides_but_keeps_emoji() {
        assert_eq!(clean("a\u{202E}b\u{2067}c"), "a\u{fffd}b\u{fffd}c");
        assert_eq!(clean("\u{2764}\u{FE0F} \u{1F468}\u{200D}\u{1F469}"), "\u{2764}\u{FE0F} \u{1F468}\u{200D}\u{1F469}");
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

    #[test]
    fn events_render_and_strip_control_chars() {
        let id = PeerId(1);
        let msg = |direct| Event::Message {
            from_id: id,
            from_nick: "bob".into(),
            text: "hi\u{1b}[2J\nthere".into(),
            direct,
            ts: 0,
        };
        assert_eq!(format_event(&msg(false)), "<bob> hi\u{fffd}[2J\u{fffd}there");
        assert_eq!(format_event(&msg(true)), "[DM from bob] hi\u{fffd}[2J\u{fffd}there");
        assert_eq!(
            format_event(&Event::PeerLeft { id, nick: "bob".into(), reason: LeaveReason::Lost }),
            "* bob left (connection lost)"
        );
        assert_eq!(format_event(&Event::Peers(vec![])), "* no peers connected");
        let peers = Event::Peers(vec![PeerInfo { id, nick: "bob".into(), addr: "10.0.0.2:5000".parse().unwrap() }]);
        assert!(format_event(&peers).contains("bob"));
        assert!(format_event(&peers).contains("10.0.0.2:5000"));
    }
}
