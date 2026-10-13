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

/// Replaces control characters (including newlines and ANSI escapes) so a peer
/// cannot rewrite our terminal.
pub fn clean(s: &str) -> String {
    s.chars().map(|c| if c.is_control() { '\u{fffd}' } else { c }).collect()
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

/// Runs the frontend until the user quits, then waits for the Node to say goodbye.
pub async fn run(handle: NodeHandle) -> anyhow::Result<()> {
    let NodeHandle { commands, mut events, task, .. } = handle;
    let mut editor = DefaultEditor::new()?;
    // The external printer redraws the prompt around output; it needs a real
    // terminal, so piped stdin falls back to plain println.
    let mut print: Box<dyn FnMut(String) + Send> = match editor.create_external_printer() {
        Ok(mut printer) => Box::new(move |s| {
            let _ = printer.print(s);
        }),
        Err(_) => Box::new(|s| println!("{s}")),
    };

    // rustyline blocks, so it lives on its own thread. It waits for `resume`
    // before each prompt so we never exit while the terminal is in raw mode.
    let (line_tx, mut line_rx) = mpsc::unbounded_channel::<Option<String>>();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        loop {
            match editor.readline("> ") {
                Ok(line) => {
                    let _ = editor.add_history_entry(line.as_str());
                    if line_tx.send(Some(line)).is_err() || resume_rx.recv().is_err() {
                        break;
                    }
                }
                Err(ReadlineError::Interrupted | ReadlineError::Eof) | Err(_) => {
                    let _ = line_tx.send(None);
                    break;
                }
            }
        }
    });

    loop {
        tokio::select! {
            event = events.recv() => match event {
                Some(event) => print(format_event(&event)),
                None => break,
            },
            line = line_rx.recv() => {
                let Some(line) = line.flatten() else {
                    let _ = commands.send(Command::Quit).await;
                    break;
                };
                match parse_line(&line) {
                    Input::Command(Command::Quit) => {
                        let _ = commands.send(Command::Quit).await;
                        break;
                    }
                    Input::Command(cmd) => commands.send(cmd).await?,
                    Input::Help => print(HELP.to_string()),
                    Input::Empty => {}
                    Input::Error(e) => print(format!("! {e}")),
                }
                let _ = resume_tx.send(());
            }
        }
    }
    drop(resume_tx);
    let _ = task.await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{PeerId, PeerInfo};

    fn say(s: &str) -> Input {
        Input::Command(Command::Say(s.into()))
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
