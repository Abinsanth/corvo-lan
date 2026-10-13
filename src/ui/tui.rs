//! The full-screen frontend: owns the terminal while it runs and hands it back
//! cleanly on every exit, including panics.

pub mod state;
pub mod view;

use std::io::{Stdout, stdout};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event as TermEvent,
    KeyboardEnhancementFlags, MouseButton, MouseEventKind, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode, supports_keyboard_enhancement,
};
use tokio::sync::mpsc;

use self::state::{Action, Hit, TuiState, WHEEL_STEP, action_for, goes_to_status};
use super::render::{tone_of, tone_of_text};
use super::session::{Mode, Reaction, Session};
use super::{Ctx, Exit};
use crate::api::{Command, Conv, Event};
use crate::protocol::now_millis;

const POLL: Duration = Duration::from_millis(100);
/// Redraws this often anyway, so an error flash goes away on time.
const TICK: Duration = Duration::from_millis(250);

/// Disambiguated keys (so Shift/Alt+Enter are distinguishable) with shifted
/// characters reported as typed. Not REPORT_ALL_KEYS: it would send unshifted keys.
const KEYBOARD_FLAGS: KeyboardEnhancementFlags =
    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES.union(KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS);

/// Best-effort and idempotent: puts the terminal back the way line mode expects.
fn restore_terminal() {
    let _ = execute!(
        stdout(),
        PopKeyboardEnhancementFlags,
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen
    );
    let _ = disable_raw_mode();
}

/// Restores the terminal before a panic message prints, so it stays usable.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        previous(info);
    }));
}

/// Owns the terminal setup; dropping it restores the terminal.
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> std::io::Result<Self> {
        enable_raw_mode()?;
        let guard = TerminalGuard;
        execute!(stdout(), EnterAlternateScreen, EnableMouseCapture, EnableBracketedPaste)?;
        if supports_keyboard_enhancement().unwrap_or(false) {
            execute!(stdout(), PushKeyboardEnhancementFlags(KEYBOARD_FLAGS))?;
        }
        Ok(guard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

/// Reads terminal events on a thread; stopped and joined before we give the
/// terminal back, so line mode is the only reader afterwards.
struct InputThread {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl InputThread {
    fn start(tx: mpsc::UnboundedSender<TermEvent>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let handle = std::thread::spawn(move || {
            while !flag.load(Ordering::SeqCst) {
                match event::poll(POLL) {
                    Ok(true) => match event::read() {
                        Ok(ev) => {
                            if tx.send(ev).is_err() {
                                return;
                            }
                        }
                        Err(_) => return,
                    },
                    Ok(false) => {}
                    Err(_) => return,
                }
            }
        });
        InputThread { stop, handle: Some(handle) }
    }
}

impl Drop for InputThread {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum KeyOutcome {
    Stay,
    Send(String),
    Quit,
    Redraw,
}

/// Applies an editing, scrolling or tab key. Sending and quitting are left to the caller.
fn apply_key(session: &mut Session, state: &mut TuiState, action: Action) -> KeyOutcome {
    let (total, height) = state.view;
    match action {
        Action::Send => return KeyOutcome::Send(state.editor.take()),
        Action::Quit => return KeyOutcome::Quit,
        Action::Redraw => return KeyOutcome::Redraw,
        Action::Newline => state.editor.newline(),
        Action::Insert(c) => state.editor.insert(c),
        Action::Backspace => state.editor.backspace(),
        Action::Delete => state.editor.delete(),
        Action::Left => state.editor.left(),
        Action::Right => state.editor.right(),
        Action::Home => state.editor.home(),
        Action::End => state.editor.end(),
        Action::KillRow => state.editor.kill_to_row_start(),
        Action::KillWord => state.editor.kill_word(),
        Action::Up => {
            if !state.editor.up()
                && let Some(entry) = state.history.prev(state.editor.text())
            {
                state.editor.set(entry);
            }
        }
        Action::Down => {
            if !state.editor.down()
                && let Some(entry) = state.history.next()
            {
                state.editor.set(entry);
            }
        }
        Action::NextTab => state.step_tab(session, 1),
        Action::PrevTab => state.step_tab(session, -1),
        Action::GoTab(i) => state.select_tab(session, i),
        Action::PageUp => state.scroll.page_up(total, height),
        Action::PageDown => state.scroll.page_down(total, height),
        Action::Esc if state.editor.is_empty() => state.scroll.to_bottom(),
        Action::Esc => state.editor.clear(),
    }
    KeyOutcome::Stay
}

/// Shows `text` in the corvo tab, flashing it if it's an error.
fn say(state: &mut TuiState, text: &str, now: Instant) {
    let tone = tone_of_text(text);
    state.push_status(now_millis(), text, tone);
    if text.starts_with("! ") {
        state.flash(text, now);
    }
}

/// Keeps the view on the store's current conversation when it changes, and
/// falls back to corvo when there isn't one.
fn follow_current(state: &mut TuiState, before: Option<Conv>, session: &Session) {
    let now_current = session.store.current().cloned();
    if now_current.is_none() {
        state.on_status = true;
    } else if now_current != before {
        state.on_status = false;
        state.scroll.to_bottom();
    }
}

/// A line the user sent. Returns the reaction for `Ctx::act`, with its printed
/// lines already moved to the corvo tab; `None` if nothing should happen.
fn on_send(session: &mut Session, state: &mut TuiState, text: String, now: Instant) -> Option<Reaction> {
    if text.trim().is_empty() {
        return None;
    }
    state.history.push(&text);
    if state.on_status && !text.starts_with('/') {
        say(state, "! this is the corvo tab — switch to a chat (Tab) to send messages", now);
        return None;
    }
    let before = session.store.current().cloned();
    let shown = if state.on_status { None } else { before.clone() };
    let mut reaction = session.on_line(&text, shown.as_ref());
    for line in std::mem::take(&mut reaction.print) {
        say(state, &line, now);
    }
    if session.store.current().cloned() != before {
        follow_current(state, before, session);
    }
    Some(reaction)
}

/// An event from the node.
fn on_event(session: &mut Session, state: &mut TuiState, event: &Event, now: Instant) {
    let before = session.store.current().cloned();
    let text = session.on_event(event, now_millis());
    if goes_to_status(event)
        && let Some(text) = text
    {
        state.push_status(now_millis(), &text, tone_of(event));
        if matches!(event, Event::Error(_)) {
            state.flash(&text, now);
        }
    }
    if let Event::Message { conv, .. } | Event::Sent { conv, .. } = event
        && !state.on_status
        && session.store.current() == Some(conv)
    {
        state.scroll.note_new();
    }
    follow_current(state, before, session);
}

pub async fn run(ctx: &mut Ctx) -> anyhow::Result<Exit> {
    let guard = match TerminalGuard::enter() {
        Ok(guard) => guard,
        Err(e) => {
            restore_terminal();
            ctx.notify(format!("* couldn't start the full-screen UI ({e}); using line mode"));
            return Ok(Exit::Switch(Mode::Line));
        }
    };
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout()))?;
    terminal.clear()?;
    let (tx, mut input) = mpsc::unbounded_channel();
    let reader = InputThread::start(tx);
    let mut state = TuiState::new(ctx.session.color());
    state.on_status = ctx.session.store.current().is_none();
    let mut tick = tokio::time::interval(TICK);

    let exit = loop {
        let now = Instant::now();
        terminal.draw(|f| view::draw(f, &ctx.session, &mut state, now))?;
        tokio::select! {
            ev = input.recv() => {
                let Some(ev) = ev else {
                    let _ = ctx.commands.send(Command::Quit).await;
                    break Exit::Quit;
                };
                if let Some(exit) = on_terminal_event(ctx, &mut state, &mut terminal, ev).await? {
                    break exit;
                }
            }
            event = ctx.events.recv() => match event {
                Some(event) => on_event(&mut ctx.session, &mut state, &event, Instant::now()),
                None => {
                    drop(reader);
                    drop(guard);
                    eprintln!("! chat core stopped unexpectedly");
                    return Ok(Exit::Quit);
                }
            },
            Some(notice) = ctx.notices.recv() => {
                state.push_status(now_millis(), &notice.text, notice.tone);
                if notice.text.starts_with("! ") {
                    state.flash(&notice.text, Instant::now());
                }
            }
            _ = tick.tick() => {}
        }
    };
    drop(reader);
    drop(guard);
    Ok(exit)
}

async fn on_terminal_event(
    ctx: &mut Ctx,
    state: &mut TuiState,
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ev: TermEvent,
) -> anyhow::Result<Option<Exit>> {
    let now = Instant::now();
    let reaction = match ev {
        TermEvent::Key(key) => match action_for(key).map(|a| apply_key(&mut ctx.session, state, a)) {
            Some(KeyOutcome::Send(text)) => on_send(&mut ctx.session, state, text, now),
            Some(KeyOutcome::Quit) => Some(Reaction { quit: true, ..Default::default() }),
            Some(KeyOutcome::Redraw) => {
                terminal.clear()?;
                None
            }
            Some(KeyOutcome::Stay) | None => None,
        },
        TermEvent::Paste(text) => {
            state.editor.insert_str(&text);
            None
        }
        TermEvent::Mouse(m) => {
            let (total, height) = state.view;
            match m.kind {
                MouseEventKind::ScrollUp => state.scroll.by(-(WHEEL_STEP as isize), total, height),
                MouseEventKind::ScrollDown => state.scroll.by(WHEEL_STEP as isize, total, height),
                MouseEventKind::Down(MouseButton::Left) => match state.hit(m.column, m.row) {
                    Some(Hit::Tab(i)) => state.select_tab(&mut ctx.session, i),
                    Some(Hit::Person(nick)) if Some(nick.as_str()) != ctx.session.store.me() => {
                        let reaction = on_send(&mut ctx.session, state, format!("/chat @{nick}"), now);
                        return act(ctx, reaction).await;
                    }
                    _ => {}
                },
                _ => {}
            }
            None
        }
        _ => None,
    };
    act(ctx, reaction).await
}

async fn act(ctx: &mut Ctx, reaction: Option<Reaction>) -> anyhow::Result<Option<Exit>> {
    match reaction {
        Some(reaction) => ctx.act(reaction).await,
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Conv, Event, PeerId, Target};
    use crate::ui::render::Style;

    fn session() -> Session {
        let mut s = Session::new(Style::plain());
        s.set_mode(Mode::Tui);
        s.on_event(&Event::Assigned { nick: "annMoon".into(), renamed: false }, 0);
        s.on_event(&Event::PeerJoined { id: PeerId(2), nick: "bobRoger".into() }, 0);
        s
    }

    #[test]
    fn typing_on_the_corvo_tab_is_refused() {
        let mut s = session();
        let mut state = TuiState::new(false);
        let now = Instant::now();
        s.on_line("/chat @bobRoger", None);
        state.on_status = true;
        assert_eq!(on_send(&mut s, &mut state, "hello".into(), now), None);
        let last = state.status_lines().last().unwrap();
        assert!(last.text.starts_with("! this is the corvo tab"), "{}", last.text);
        assert!(state.flash_text(now).is_some());
        let r = on_send(&mut s, &mut state, "/who".into(), now).unwrap();
        assert_eq!(r.commands, vec![crate::api::Command::ListPeers]);
    }

    #[test]
    fn sending_goes_to_the_tab_and_command_output_to_corvo() {
        let mut s = session();
        let mut state = TuiState::new(false);
        let now = Instant::now();
        let r = on_send(&mut s, &mut state, "/chat @bobRoger".into(), now).unwrap();
        assert!(r.commands.is_empty());
        assert!(!state.on_status, "opening a chat shows its tab");
        let r = on_send(&mut s, &mut state, "hi\nthere".into(), now).unwrap();
        assert_eq!(
            r.commands,
            vec![crate::api::Command::Send { to: Target::Dm("bobRoger".into()), text: "hi\nthere".into() }]
        );
        assert_eq!(state.history.prev("").as_deref(), Some("hi\nthere"));
        let r = on_send(&mut s, &mut state, "/chat @ghostMoon".into(), now).unwrap();
        assert!(r.print.is_empty(), "printed lines were moved into the corvo tab");
        assert_eq!(state.status_lines().last().unwrap().text, "! @ghostMoon is offline");
        assert_eq!(state.flash_text(now), Some("! @ghostMoon is offline"));
        assert_eq!(s.store.current(), Some(&Conv::Dm("bobRoger".into())));
    }

    #[test]
    fn events_feed_the_corvo_tab_and_follow_joins() {
        let mut s = session();
        let mut state = TuiState::new(false);
        let now = Instant::now();
        on_event(&mut s, &mut state, &Event::Error("no room #x with that key".into()), now);
        assert_eq!(state.status_lines().last().unwrap().text, "! no room #x with that key");
        assert!(state.flash_text(now).is_some());
        on_event(&mut s, &mut state, &Event::RoomJoined { name: "catPieR".into(), members: vec![] }, now);
        assert!(!state.on_status, "joining shows the room");
        state.scroll.page_up(100, 10);
        let msg =
            Event::Message { conv: Conv::Room("catPieR".into()), from: "bobRoger".into(), text: "x".into(), ts: 0 };
        on_event(&mut s, &mut state, &msg, now);
        assert_eq!(state.scroll.unseen(), 1);
        on_event(&mut s, &mut state, &Event::RoomLeft { name: "catPieR".into() }, now);
        assert!(state.on_status, "leaving the room you're viewing falls back to corvo");
    }

    #[test]
    fn keys_edit_scroll_and_switch() {
        let mut s = session();
        let mut state = TuiState::new(false);
        state.view = (100, 10);
        assert_eq!(apply_key(&mut s, &mut state, Action::Insert('a')), KeyOutcome::Stay);
        apply_key(&mut s, &mut state, Action::Newline);
        apply_key(&mut s, &mut state, Action::Insert('b'));
        assert_eq!(state.editor.text(), "a\nb");
        apply_key(&mut s, &mut state, Action::Esc);
        assert!(state.editor.is_empty());
        apply_key(&mut s, &mut state, Action::PageUp);
        assert!(state.scroll.is_scrolled());
        apply_key(&mut s, &mut state, Action::Esc);
        assert!(!state.scroll.is_scrolled(), "Esc on an empty input goes back to the latest");
        assert_eq!(apply_key(&mut s, &mut state, Action::Send), KeyOutcome::Send(String::new()));
        assert_eq!(apply_key(&mut s, &mut state, Action::Quit), KeyOutcome::Quit);
        state.editor.set("hi".into());
        state.history.push("older");
        apply_key(&mut s, &mut state, Action::Up);
        assert_eq!(state.editor.text(), "older");
        apply_key(&mut s, &mut state, Action::Down);
        assert_eq!(state.editor.text(), "hi");
    }

    #[test]
    fn keyboard_flags_report_shifted_characters() {
        use ratatui::crossterm::event::KeyboardEnhancementFlags as F;
        assert!(KEYBOARD_FLAGS.contains(F::DISAMBIGUATE_ESCAPE_CODES));
        assert!(KEYBOARD_FLAGS.contains(F::REPORT_ALTERNATE_KEYS));
        assert!(!KEYBOARD_FLAGS.contains(F::REPORT_ALL_KEYS_AS_ESCAPE_CODES));
    }
}
