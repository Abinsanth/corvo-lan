//! Line-based frontend: wires `Session` to rustyline.

use std::sync::{Arc, Mutex};

use rustyline::error::ReadlineError;
use rustyline::{DefaultEditor, ExternalPrinter as _};
use tokio::sync::mpsc;

use super::render::Style;
use super::session::Session;
use super::updates::Updater;
use crate::api::{Command, Conv, Event};
use crate::node::NodeHandle;
use crate::protocol::now_millis;

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
pub async fn run(handle: NodeHandle, check_updates: bool) -> anyhow::Result<()> {
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

    let updater = Updater::new(out.clone());
    if check_updates {
        updater.check_at_startup();
    }

    // rustyline blocks, so it lives on its own thread. It waits for `resume`
    // before each prompt so we never exit while the terminal is in raw mode,
    // and reads the prompt then so it follows conversation switches.
    let prompt = Arc::new(Mutex::new((String::from("> "), None)));
    let (line_tx, mut lines) = mpsc::unbounded_channel::<Option<(String, Option<Conv>)>>();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel::<()>();
    let thread_prompt = prompt.clone();
    std::thread::spawn(move || {
        let mut errors = 0;
        loop {
            let (text, shown) = thread_prompt.lock().map(|p| p.clone()).unwrap_or_else(|_| ("> ".into(), None));
            match editor.readline(&text) {
                Ok(line) => {
                    errors = 0;
                    let _ = editor.add_history_entry(line.as_str());
                    if line_tx.send(Some((line, shown))).is_err() || resume_rx.recv().is_err() {
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
    let result =
        event_loop(&mut session, &commands, &mut events, &mut lines, &resume_tx, &prompt, &updater, print).await;
    drop(resume_tx);
    let _ = task.await;
    result
}

/// Routes input lines to the Node and events to `print` until the user quits.
/// Returns only while no prompt is active, so the terminal is never left raw.
#[allow(clippy::too_many_arguments)] // each is a separate channel or handle `run` owns
async fn event_loop(
    session: &mut Session,
    commands: &mpsc::Sender<Command>,
    events: &mut mpsc::UnboundedReceiver<Event>,
    lines: &mut mpsc::UnboundedReceiver<Option<(String, Option<Conv>)>>,
    resume: &std::sync::mpsc::Sender<()>,
    prompt: &Mutex<(String, Option<Conv>)>,
    updater: &Updater,
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
                        *p = session.prompt_state();
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
                let Some((line, shown)) = line.flatten() else {
                    let _ = commands.send(Command::Quit).await;
                    return Ok(());
                };
                let reaction = session.on_line(&line, shown.as_ref());
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
                if let Some(action) = reaction.update {
                    updater.start(action);
                }
                if let Ok(mut p) = prompt.lock() {
                    *p = session.prompt_state();
                }
                let _ = resume.send(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            let prompt = Mutex::new((String::new(), None));
            let mut session = Session::new(Style::plain());
            let updater = Updater::new(std::sync::mpsc::channel().0);
            event_loop(&mut session, &cmd_tx, &mut events, &mut lines, &resume_tx, &prompt, &updater, print).await
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        // Exiting now would leave rustyline's prompt (and raw mode) behind.
        assert!(!ui.is_finished());
        assert!(printed.lock().unwrap().iter().any(|s| s.contains("press Enter")));
        line_tx.send(Some((String::new(), None))).unwrap(); // user presses Enter
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
