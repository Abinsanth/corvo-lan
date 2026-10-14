//! Line-based frontend: rustyline on its own thread, output through a printer
//! thread. It shares the `Session` with the TUI via `Ctx`.

use std::sync::{Arc, Mutex};

use rustyline::error::ReadlineError;
use rustyline::{DefaultEditor, ExternalPrinter as _};
use tokio::sync::mpsc;

use super::render::Style;
use super::{Ctx, Exit};
use crate::api::{Command, Conv};
use crate::protocol::now_millis;

type Typed = Option<(String, Option<Conv>)>;

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

/// Whether a line-mode turn opens with the banner: every turn that follows the TUI.
fn banner_due(entered: bool, from_tui: bool) -> bool {
    entered || from_tui
}

pub struct LineFrontend {
    out: std::sync::mpsc::Sender<String>,
    prompt: Arc<Mutex<(String, Option<Conv>)>>,
    lines: mpsc::UnboundedReceiver<Typed>,
    /// Lets the input thread show one prompt. Dropped on shutdown.
    resume: Option<std::sync::mpsc::Sender<()>>,
    style: Style,
    /// Has run before, so a later turn came from the TUI.
    entered: bool,
    /// The program started in the TUI, so even the first turn came from it.
    from_tui: bool,
}

impl LineFrontend {
    /// Starts the input thread paused: it shows no prompt (and reads nothing)
    /// until `run` asks, so it never competes with the TUI for the keyboard.
    pub fn new(style: Style, from_tui: bool) -> anyhow::Result<Self> {
        let mut editor = DefaultEditor::new()?;
        // The external printer redraws the prompt around output; it needs a real
        // terminal, so piped stdin falls back to plain println.
        let out = match editor.create_external_printer() {
            Ok(mut printer) => spawn_printer(move |s| {
                // Without a trailing newline the next prompt's redraw erases the line.
                let _ = printer.print(format!("{s}\n"));
            }),
            Err(_) => spawn_printer(|s| println!("{s}")),
        };
        let prompt = Arc::new(Mutex::new((String::from("> "), None)));
        let (line_tx, lines) = mpsc::unbounded_channel::<Typed>();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel::<()>();
        let thread_prompt = prompt.clone();
        std::thread::spawn(move || {
            let mut errors = 0;
            'prompts: while resume_rx.recv().is_ok() {
                loop {
                    let (text, shown) = thread_prompt.lock().map(|p| p.clone()).unwrap_or_else(|_| ("> ".into(), None));
                    match editor.readline(&text) {
                        Ok(line) => {
                            errors = 0;
                            let _ = editor.add_history_entry(line.as_str());
                            if line_tx.send(Some((line, shown))).is_err() {
                                break 'prompts;
                            }
                            continue 'prompts;
                        }
                        Err(e) => {
                            errors += 1;
                            if readline_gives_up(&e, errors) {
                                let _ = line_tx.send(None);
                                break 'prompts;
                            }
                        }
                    }
                }
            }
        });
        Ok(LineFrontend { out, prompt, lines, resume: Some(resume_tx), style, entered: false, from_tui })
    }

    fn print(&self, s: impl Into<String>) {
        let _ = self.out.send(s.into());
    }

    /// Updates the prompt and lets the input thread show it.
    fn ready(&self, ctx: &Ctx) {
        if let Ok(mut p) = self.prompt.lock() {
            *p = ctx.session.prompt_state();
        }
        if let Some(resume) = &self.resume {
            let _ = resume.send(());
        }
    }

    /// Owns the terminal until the user quits or switches to the TUI. Returns
    /// only while no prompt is showing, so the terminal is never left raw.
    pub async fn run(&mut self, ctx: &mut Ctx) -> anyhow::Result<Exit> {
        if banner_due(self.entered, self.from_tui) {
            for line in ctx.session.line_banner() {
                self.print(line);
            }
        }
        self.entered = true;
        self.ready(ctx);
        loop {
            tokio::select! {
                event = ctx.events.recv() => match event {
                    Some(event) => {
                        if let Some(text) = ctx.session.on_event(&event, now_millis()) {
                            self.print(text);
                        }
                        if let Ok(mut p) = self.prompt.lock() {
                            *p = ctx.session.prompt_state();
                        }
                    }
                    None => {
                        // A prompt is showing; wait for it to return before exiting.
                        self.print("! chat core stopped unexpectedly; press Enter to exit");
                        self.lines.recv().await;
                        return Ok(Exit::Quit);
                    }
                },
                Some(notice) = ctx.notices.recv() => self.print(self.style.tone(notice.tone, &notice.text)),
                line = self.lines.recv() => {
                    let Some((line, shown)) = line.flatten() else {
                        let _ = ctx.commands.send(Command::Quit).await;
                        return Ok(Exit::Quit);
                    };
                    let mut reaction = ctx.session.on_line(&line, shown.as_ref());
                    for text in std::mem::take(&mut reaction.print) {
                        self.print(self.style.toned(&text));
                    }
                    if let Some(exit) = ctx.act(reaction).await? {
                        return Ok(exit);
                    }
                    self.ready(ctx);
                }
            }
        }
    }

    /// Lets the input thread finish.
    pub fn shutdown(&mut self) {
        self.resume = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn banner_shows_on_every_turn_after_the_tui() {
        // Started in line mode: no banner on the first turn, one on each later turn.
        assert!(!banner_due(false, false));
        assert!(banner_due(true, false));
        // Started in the TUI: even the first line-mode turn came from it.
        assert!(banner_due(false, true));
        assert!(banner_due(true, true));
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
