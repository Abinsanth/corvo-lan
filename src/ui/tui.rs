//! The full-screen frontend: owns the terminal while it runs and hands it back
//! cleanly on every exit, including panics.

pub mod state;
pub mod view;

use std::io::{Stdout, Write, stdout};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste, EnableFocusChange,
    EnableMouseCapture, Event as TermEvent, KeyboardEnhancementFlags, MouseButton, MouseEventKind,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::style::Print;
use ratatui::crossterm::terminal::{
    Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, SetTitle, disable_raw_mode, enable_raw_mode,
    supports_keyboard_enhancement,
};
use tokio::sync::mpsc;

use self::state::{Action, Hit, TuiState, WHEEL_STEP, action_for, goes_to_status};
use super::clip;
use super::complete::Candidates;
use super::render::{Tone, tone_of, tone_of_text};
use super::session::{Reaction, Session};
use super::{Ctx, Notice};
use crate::api::{Command, Conv, Event};
use crate::protocol::now_millis;

const POLL: Duration = Duration::from_millis(100);
/// Redraws this often anyway, so an error flash goes away on time.
const TICK: Duration = Duration::from_millis(250);

/// Disambiguated keys (so Shift/Alt+Enter are distinguishable) with shifted
/// characters reported as typed. Not REPORT_ALL_KEYS: it would send unshifted keys.
const KEYBOARD_FLAGS: KeyboardEnhancementFlags =
    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES.union(KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS);

/// xterm's title stack: save the window title on entry and put it back on exit.
/// Terminals without one ignore both.
const SAVE_TITLE: &str = "\x1b[22;0t";
const RESTORE_TITLE: &str = "\x1b[23;0t";

/// Best-effort and idempotent: puts the terminal back the way line mode expects.
fn restore_terminal() {
    let _ = restore_to(&mut stdout());
    let _ = disable_raw_mode();
}

/// The escape codes `restore_terminal` writes.
fn restore_to(out: &mut impl Write) -> std::io::Result<()> {
    execute!(
        out,
        PopKeyboardEnhancementFlags,
        DisableBracketedPaste,
        DisableMouseCapture,
        DisableFocusChange,
        LeaveAlternateScreen,
        // Our unread count mustn't outlive us where there's no title stack.
        SetTitle(""),
        Print(RESTORE_TITLE)
    )
}

/// Restores the terminal before a panic message prints, so it stays usable.
/// Only for a panic on the installing (UI) thread: one in a tokio task is caught
/// there, and the TUI keeps running on the terminal it set up.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    let ui_thread = std::thread::current().id();
    std::panic::set_hook(Box::new(move |info| {
        if std::thread::current().id() == ui_thread {
            restore_terminal();
        }
        previous(info);
    }));
}

/// Owns the terminal setup; dropping it restores the terminal.
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> std::io::Result<Self> {
        enable_raw_mode()?;
        let guard = TerminalGuard;
        execute!(
            stdout(),
            Print(SAVE_TITLE),
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste,
            EnableFocusChange
        )?;
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
    if state.help {
        match action {
            Action::Insert('q' | 'Q') | Action::Esc => state.help = false,
            Action::Quit => return KeyOutcome::Quit,
            Action::Redraw => return KeyOutcome::Redraw,
            _ => {}
        }
        return KeyOutcome::Stay;
    }
    let edits =
        matches!(action, Action::Insert(_) | Action::Backspace | Action::Delete | Action::KillRow | Action::KillWord);
    if state.popup.is_some() {
        match action {
            Action::Complete | Action::Down => {
                state.move_selection(1);
                return KeyOutcome::Stay;
            }
            Action::Up => {
                state.move_selection(-1);
                return KeyOutcome::Stay;
            }
            Action::Send => {
                state.accept();
                return KeyOutcome::Stay;
            }
            Action::Esc => {
                state.popup = None;
                return KeyOutcome::Stay;
            }
            _ if edits => {}
            _ => state.popup = None,
        }
    }
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
        Action::Esc if state.editor.is_empty() && session.cancel_prompt() => {}
        Action::Esc if state.editor.is_empty() => state.scroll.to_bottom(),
        Action::Esc => state.editor.clear(),
        Action::Complete => state.complete(&Candidates::from_store(&session.store)),
    }
    if edits {
        state.refresh_popup(&Candidates::from_store(&session.store));
    }
    KeyOutcome::Stay
}

/// Ctrl-L: clears the open tab, like `/clear` (the caller repaints).
fn on_redraw(session: &Session, state: &mut TuiState) {
    state.clear_view(session, now_millis());
}

/// Output from the updater. Returns true to restart into a new version now.
fn on_notice(session: &mut Session, state: &mut TuiState, notice: Notice, now: Instant) -> bool {
    match notice {
        Notice::Line { text, tone } => {
            state.push_status(now_millis(), &text, tone);
            if text.starts_with("! ") {
                state.flash(&text, now);
            }
        }
        Notice::Progress(text) => state.push_progress(now_millis(), &text),
        Notice::Offer(version) => {
            if session.offer_update(version) {
                update_question(None, session, state, now);
            }
        }
        Notice::Installed { version, forced: true } => {
            say(state, &format!("* installed v{version} — restarting"), now);
            return true;
        }
        Notice::Installed { version, forced: false } => {
            update_question(session.offer_restart(version), session, state, now)
        }
    }
    false
}

/// Asks a waiting install question once the open one is answered.
fn ask_pending(session: &mut Session, state: &mut TuiState, now: Instant) {
    if session.ask_pending() {
        update_question(None, session, state, now);
    }
}

/// Shows the question just asked, or `instead` if another was already open.
/// Unlike `ask`, it stays on the current tab: these come unasked.
fn update_question(instead: Option<String>, session: &Session, state: &mut TuiState, now: Instant) {
    match instead {
        Some(line) => say(state, &line, now),
        None => {
            if let Some(question) = session.prompt() {
                state.push_status(now_millis(), &question, Tone::Warn);
                state.flash(&question, now);
            }
        }
    }
}

/// Shows `text` in the corvo tab, flashing it if it's an error.
fn say(state: &mut TuiState, text: &str, now: Instant) {
    let tone = tone_of_text(text);
    state.push_status(now_millis(), text, tone);
    if text.starts_with("! ") {
        state.flash(text, now);
    }
}

/// Typing, clicking or pasting means the window has focus, even if the
/// terminal never said it came back.
fn note_input(state: &mut TuiState, ev: &TermEvent) {
    if !state.focused && matches!(ev, TermEvent::Key(_) | TermEvent::Mouse(_) | TermEvent::Paste(_)) {
        on_focus(state, true);
    }
}

/// The terminal window gained or lost focus. Coming back counts as seeing what's open.
fn on_focus(state: &mut TuiState, focused: bool) {
    state.focused = focused;
    if focused {
        state.away = 0;
    }
}

/// Puts a new question in the corvo tab and shows it, so it's in plain view
/// rather than only at the bottom of the screen.
fn ask(session: &mut Session, state: &mut TuiState, question: &str, now: Instant) {
    state.push_status(now_millis(), question, Tone::Warn);
    state.flash(question, now);
    if !state.on_status {
        session.store.clear_current();
        state.on_status = true;
        state.scroll.to_bottom();
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

/// A click on a room's key: copy its join line and flash what was copied.
fn on_copy(session: &Session, state: &mut TuiState, room: &str, now: Instant) -> Reaction {
    let mut reaction = session.copy_join_line(room);
    for line in std::mem::take(&mut reaction.print) {
        say(state, &line, now);
        state.flash(&line, now);
    }
    reaction
}

/// A line the user sent. Returns the reaction for `Ctx::act`, with its printed
/// lines already moved to the corvo tab; `None` if nothing should happen.
fn on_send(session: &mut Session, state: &mut TuiState, text: String, now: Instant) -> Option<Reaction> {
    let asking = session.prompt().is_some();
    if text.trim().is_empty() && !asking {
        return None;
    }
    if !text.trim().is_empty() {
        state.history.push(&text);
    }
    if state.on_status && !text.trim().is_empty() && !text.starts_with('/') && !session.takes(&text) {
        say(state, "! this is the corvo tab — switch to a chat (Shift-Tab) to send messages", now);
        return None;
    }
    let before = session.store.current().cloned();
    let asked = session.prompt();
    let shown = if state.on_status { None } else { before.clone() };
    let mut reaction = session.on_line(&text, shown.as_ref());
    if reaction.help {
        state.popup = None;
        state.help = true;
    }
    if reaction.clear {
        state.clear_view(session, now_millis());
    }
    let complained = reaction.print.iter().any(|l| l.starts_with("! "));
    for line in std::mem::take(&mut reaction.print) {
        say(state, &line, now);
    }
    if session.store.current().cloned() != before {
        follow_current(state, before, session);
    }
    if let Some(question) = session.prompt().filter(|q| !complained && Some(q) != asked.as_ref()) {
        ask(session, state, &question, now);
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
        if matches!(event, Event::Error(_) | Event::JoinEnded { .. }) || tone_of(event) == Tone::Warn {
            state.flash(&text, now);
        }
    }
    if let Event::Message { conv, .. } | Event::Sent { conv, .. } = event
        && !state.on_status
        && session.store.current() == Some(conv)
    {
        if matches!(event, Event::Sent { .. }) {
            state.scroll.to_bottom();
        } else {
            state.scroll.note_new();
        }
    }
    if let Event::Message { conv, .. } = event
        && !state.focused
        && !state.on_status
        && session.store.current() == Some(conv)
    {
        state.away += 1;
    }
    follow_current(state, before, session);
    if matches!(event, Event::NamePreview { .. })
        && let Some(question) = session.prompt()
    {
        ask(session, state, &question, now);
    }
}

type Term = Terminal<CrosstermBackend<Stdout>>;

/// A ratatui terminal on a cleared screen. Never `Terminal::clear`: it asks the
/// terminal for the cursor position and fails after a timeout if there's no answer.
fn open_terminal() -> std::io::Result<Term> {
    let terminal = Terminal::new(CrosstermBackend::new(stdout()))?;
    execute!(stdout(), Clear(ClearType::All))?;
    Ok(terminal)
}

/// Runs until the user quits. The caller has checked there's a terminal.
pub async fn run(ctx: &mut Ctx) -> anyhow::Result<()> {
    // Any setup failure drops the guard, restoring the terminal before the error prints.
    let (mut terminal, guard) = match TerminalGuard::enter().and_then(|guard| Ok((open_terminal()?, guard))) {
        Ok(setup) => setup,
        Err(e) => {
            restore_terminal();
            return Err(anyhow::anyhow!("corvo-lan needs a terminal ({e})"));
        }
    };
    let (tx, mut input) = mpsc::unbounded_channel();
    let reader = InputThread::start(tx);
    let mut state = TuiState::new(ctx.session.color());
    state.lan = ctx.lan.clone();
    state.on_status = ctx.session.store.current().is_none();
    let mut tick = tokio::time::interval(TICK);
    let mut title = String::new();

    loop {
        let now = Instant::now();
        ask_pending(&mut ctx.session, &mut state, now);
        let want = view::window_title(&ctx.session, &state);
        if want != title {
            let _ = execute!(stdout(), SetTitle(&want));
            title = want;
        }
        terminal.draw(|f| view::draw(f, &ctx.session, &mut state, now))?;
        tokio::select! {
            ev = input.recv() => {
                let Some(ev) = ev else {
                    let _ = ctx.commands.send(Command::Quit).await;
                    break;
                };
                if on_terminal_event(ctx, &mut state, &mut terminal, ev).await? {
                    break;
                }
            }
            event = ctx.events.recv() => match event {
                Some(event) => on_event(&mut ctx.session, &mut state, &event, Instant::now()),
                None => {
                    drop(reader);
                    drop(guard);
                    return Err(anyhow::anyhow!("chat core stopped unexpectedly"));
                }
            },
            Some(notice) = ctx.notices.recv() => {
                if on_notice(&mut ctx.session, &mut state, notice, Instant::now()) {
                    ctx.restart = true;
                    let _ = ctx.commands.send(Command::Quit).await;
                    break;
                }
            }
            _ = tick.tick() => {}
        }
    }
    drop(reader);
    drop(guard);
    Ok(())
}

async fn on_terminal_event(
    ctx: &mut Ctx,
    state: &mut TuiState,
    terminal: &mut Term,
    ev: TermEvent,
) -> anyhow::Result<bool> {
    let now = Instant::now();
    note_input(state, &ev);
    let reaction = match ev {
        TermEvent::Key(key) => match action_for(key).map(|a| apply_key(&mut ctx.session, state, a)) {
            Some(KeyOutcome::Send(text)) => on_send(&mut ctx.session, state, text, now),
            Some(KeyOutcome::Quit) => Some(Reaction { quit: true, ..Default::default() }),
            Some(KeyOutcome::Redraw) => {
                on_redraw(&ctx.session, state);
                // A fresh terminal has an empty back buffer, so the next draw repaints
                // everything on the cleared screen; if it can't be built, keep the old one.
                if let Ok(fresh) = open_terminal() {
                    *terminal = fresh;
                }
                None
            }
            Some(KeyOutcome::Stay) | None => None,
        },
        TermEvent::FocusGained => {
            on_focus(state, true);
            None
        }
        TermEvent::FocusLost => {
            on_focus(state, false);
            None
        }
        TermEvent::Paste(_) | TermEvent::Mouse(_) if state.help => None,
        TermEvent::Paste(text) => {
            state.popup = None;
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
                    Some(Hit::Invite(room)) => {
                        let reaction = on_copy(&ctx.session, state, &room, now);
                        return act(ctx, Some(reaction)).await;
                    }
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

/// Says whether to quit.
async fn act(ctx: &mut Ctx, reaction: Option<Reaction>) -> anyhow::Result<bool> {
    match reaction {
        Some(mut reaction) => {
            if let Some(text) = reaction.copy.take() {
                clip::copy(&text);
            }
            ctx.act(reaction).await
        }
        None => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Conv, Event, PeerId, Target};

    fn session() -> Session {
        let mut s = Session::new(false);
        s.on_event(&Event::Assigned { nick: "annmoon".into(), renamed: false }, 0);
        s.on_event(&Event::PeerJoined { id: PeerId::test(2), nick: "bobroger".into() }, 0);
        s
    }

    #[test]
    fn typing_brings_the_window_back_even_without_a_focus_event() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut state = TuiState::new(false);
        on_focus(&mut state, false);
        state.away = 2;
        note_input(&mut state, &TermEvent::FocusLost);
        assert!(!state.focused, "only input counts");
        note_input(&mut state, &TermEvent::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)));
        assert!(state.focused);
        assert_eq!(state.away, 0);
    }

    const V: crate::update::Version = crate::update::Version(0, 6, 0);

    #[test]
    fn an_offer_is_asked_without_switching_tabs() {
        let mut s = session();
        let mut state = TuiState::new(false);
        let now = Instant::now();
        on_send(&mut s, &mut state, "/chat @bobroger".into(), now);
        assert!(!on_notice(&mut s, &mut state, Notice::Offer(V), now));
        assert!(!state.on_status, "still on the chat");
        assert_eq!(last_status(&state), ("v0.6.0 is available — install it now? (y/n)", Tone::Warn));
        assert!(state.flash_text(now).is_some());
        assert!(s.prompt().is_some());
        let lines = state.status_lines().count();
        on_notice(&mut s, &mut state, Notice::Offer(V), now);
        assert_eq!(state.status_lines().count(), lines, "already asking: nothing to show");
    }

    #[test]
    fn a_queued_offer_is_asked_once_the_name_is_picked() {
        let mut s = Session::new(false);
        let mut state = TuiState::new(false);
        let now = Instant::now();
        on_event(&mut s, &mut state, &Event::NamePreview { nick: "annmoon".into() }, now);
        on_notice(&mut s, &mut state, Notice::Offer(V), now);
        assert!(s.prompt().is_some_and(|p| p.contains("you'll be @annmoon")));
        ask_pending(&mut s, &mut state, now);
        assert!(s.prompt().is_some_and(|p| p.contains("you'll be")), "not while the name is open");
        on_event(&mut s, &mut state, &Event::Assigned { nick: "annmoon".into(), renamed: false }, now);
        ask_pending(&mut s, &mut state, now);
        assert_eq!(last_status(&state), ("v0.6.0 is available — install it now? (y/n)", Tone::Warn));
    }

    #[test]
    fn forced_installed_restarts_at_once() {
        let mut s = session();
        let mut state = TuiState::new(false);
        let now = Instant::now();
        assert!(on_notice(&mut s, &mut state, Notice::Installed { version: V, forced: true }, now));
        assert_eq!(last_status(&state).0, "* installed v0.6.0 — restarting");
        assert!(!on_notice(&mut s, &mut state, Notice::Installed { version: V, forced: false }, now));
        assert_eq!(last_status(&state).0, "v0.6.0 is installed — restart now? You'll leave your rooms. (y/n)");
        assert!(on_send(&mut s, &mut state, "y".into(), now).unwrap().restart);
    }

    #[test]
    fn updater_lines_and_progress_reach_the_corvo_tab() {
        let mut s = session();
        let mut state = TuiState::new(false);
        let now = Instant::now();
        on_notice(&mut s, &mut state, Notice::Progress("* downloading v0.6.0  10%".into()), now);
        on_notice(&mut s, &mut state, Notice::Progress("* downloading v0.6.0  90%".into()), now);
        assert_eq!(state.status_lines().count(), 1);
        on_notice(&mut s, &mut state, Notice::new("! couldn't reach GitHub (offline?)"), now);
        assert_eq!(last_status(&state), ("! couldn't reach GitHub (offline?)", Tone::Bad));
        assert!(state.flash_text(now).is_some());
    }

    #[test]
    fn an_open_update_question_does_not_let_chat_through_on_corvo() {
        let mut s = session();
        let mut state = TuiState::new(false);
        let now = Instant::now();
        on_notice(&mut s, &mut state, Notice::Offer(V), now);
        assert_eq!(on_send(&mut s, &mut state, "hello".into(), now), None);
        assert!(last_status(&state).0.starts_with("! this is the corvo tab"));
        assert_eq!(on_send(&mut s, &mut state, String::new(), now), Some(Reaction::default()), "Enter is harmless");
        let r = on_send(&mut s, &mut state, "y".into(), now).unwrap();
        assert_eq!(r.update, Some(crate::ui::updates::UpdateAction::Install));
    }

    #[test]
    fn clear_sets_a_mark_on_the_open_tab() {
        let mut s = session();
        let mut state = TuiState::new(false);
        let now = Instant::now();
        on_send(&mut s, &mut state, "/clear".into(), now);
        assert!(state.cleared_at(None).is_some(), "the corvo tab");
        on_send(&mut s, &mut state, "/chat @bobroger".into(), now);
        assert_eq!(apply_key(&mut s, &mut state, Action::Redraw), KeyOutcome::Redraw);
        on_redraw(&s, &mut state);
        assert!(state.cleared_at(Some(&Conv::Dm("bobroger".into()))).is_some(), "Ctrl-L clears too");
    }

    #[test]
    fn messages_in_view_count_while_the_window_is_away() {
        let mut s = session();
        let mut state = TuiState::new(false);
        let now = Instant::now();
        on_send(&mut s, &mut state, "/chat @bobroger".into(), now);
        let dm = Event::Message { conv: Conv::Dm("bobroger".into()), from: "bobroger".into(), text: "x".into(), ts: 0 };
        on_event(&mut s, &mut state, &dm, now);
        assert_eq!(state.away, 0, "seen as it arrived");
        on_focus(&mut state, false);
        on_event(&mut s, &mut state, &dm, now);
        on_event(&mut s, &mut state, &dm, now);
        assert_eq!(view::window_title(&s, &state), "(2) corvo-lan");
        on_focus(&mut state, true);
        assert_eq!(view::window_title(&s, &state), "corvo-lan", "coming back counts as seeing them");
    }

    #[test]
    fn typing_on_the_corvo_tab_is_refused() {
        let mut s = session();
        let mut state = TuiState::new(false);
        let now = Instant::now();
        s.on_line("/chat @bobroger", None);
        state.on_status = true;
        assert_eq!(on_send(&mut s, &mut state, "hello".into(), now), None);
        let last = state.status_lines().last().unwrap();
        assert!(last.text.starts_with("! this is the corvo tab"), "{}", last.text);
        assert!(state.flash_text(now).is_some());
        let r = on_send(&mut s, &mut state, "/who".into(), now).unwrap();
        assert_eq!(r.commands, vec![crate::api::Command::ListPeers]);
    }

    #[test]
    fn the_name_question_flashes_and_survives_a_tab_switch() {
        let mut s = Session::new(false);
        let mut state = TuiState::new(false);
        let now = Instant::now();
        on_event(&mut s, &mut state, &Event::NamePreview { nick: "bobroger".into() }, now);
        assert!(state.flash_text(now).is_some_and(|t| t.contains("you'll be @bobroger")));
        state.select_tab(&mut s, 0);
        apply_key(&mut s, &mut state, Action::Esc);
        assert!(s.prompt().is_some_and(|p| p.contains("@bobroger")));
        let r = on_send(&mut s, &mut state, String::new(), now).expect("Enter answers from the corvo tab");
        assert_eq!(r.commands, vec![crate::api::Command::ConfirmName]);
    }

    fn last_status(state: &TuiState) -> (&str, Tone) {
        let l = state.status_lines().last().unwrap();
        (&l.text, l.tone)
    }

    #[test]
    fn the_name_question_is_asked_in_the_corvo_tab() {
        let mut s = Session::new(false);
        let mut state = TuiState::new(false);
        let now = Instant::now();
        on_event(&mut s, &mut state, &Event::NamePreview { nick: "bobroger".into() }, now);
        assert!(state.on_status);
        assert_eq!(last_status(&state), ("you'll be @bobroger — Enter to keep it, or type a name", Tone::Warn));
        on_event(&mut s, &mut state, &Event::NamePreview { nick: "luffysunny".into() }, now);
        assert!(last_status(&state).0.contains("@luffysunny"), "each new preview is asked again");
        on_send(&mut s, &mut state, String::new(), now);
        assert_eq!(last_status(&state).0, "joining as @luffysunny…");
        let count = state.status_lines().count();
        on_send(&mut s, &mut state, String::new(), now);
        assert_eq!(state.status_lines().count(), count, "an unchanged question isn't repeated");
    }

    #[test]
    fn the_create_questions_move_a_chat_to_the_corvo_tab() {
        let mut s = session();
        let mut state = TuiState::new(false);
        let now = Instant::now();
        on_send(&mut s, &mut state, "/chat @bobroger".into(), now);
        assert!(!state.on_status);
        on_send(&mut s, &mut state, "/create cat".into(), now);
        assert!(state.on_status, "the question shows in the corvo tab");
        assert!(last_status(&state).0.starts_with("new room #cat"));
        on_send(&mut s, &mut state, String::new(), now);
        assert!(last_status(&state).0.starts_with("how many people in #cat"));
        let r = on_send(&mut s, &mut state, "3".into(), now).unwrap();
        let [crate::api::Command::CreateRoom { name, .. }] = r.commands.as_slice() else { panic!("{r:?}") };
        let created = Event::RoomCreated { name: name.clone(), key: "k7m9xq".into(), max: 3 };
        on_event(&mut s, &mut state, &created, now);
        assert!(!state.on_status, "the new room's tab opens");
    }

    #[test]
    fn the_create_questions_are_answered_from_any_tab_and_esc_drops_them() {
        let mut s = session();
        let mut state = TuiState::new(false);
        let now = Instant::now();
        state.on_status = true;
        on_send(&mut s, &mut state, "/create cat".into(), now);
        assert!(state.flash_text(now).is_some_and(|t| t.contains("new room #cat")), "the name step flashes");
        on_send(&mut s, &mut state, String::new(), now).expect("Enter keeps the name");
        assert!(state.flash_text(now).is_some_and(|t| t.contains("how many people")), "then the head count");
        let r = on_send(&mut s, &mut state, "4".into(), now).expect("not refused on the corvo tab");
        assert!(matches!(r.commands.as_slice(),
            [crate::api::Command::CreateRoom { name, max: 4 }] if name.starts_with("cat")));

        on_send(&mut s, &mut state, "/create cat".into(), now);
        state.editor.insert('d');
        apply_key(&mut s, &mut state, Action::Esc);
        assert!(s.prompt().is_some(), "the first Esc clears the input");
        apply_key(&mut s, &mut state, Action::Esc);
        assert_eq!(s.prompt(), None);
        assert_eq!(on_send(&mut s, &mut state, String::new(), now), None, "Enter does nothing again");
    }

    #[test]
    fn switching_tabs_drops_the_create_question() {
        let mut s = session();
        let mut state = TuiState::new(false);
        let now = Instant::now();
        s.on_line("/chat @bobroger", None);
        state.on_status = false;
        on_send(&mut s, &mut state, "/create cat".into(), now);
        assert!(state.on_status, "the question is asked in the corvo tab");
        state.step_tab(&mut s, 1);
        assert_eq!(s.prompt(), None);
        assert!(!state.on_status);
        let r = on_send(&mut s, &mut state, "10".into(), now).unwrap();
        assert_eq!(
            r.commands,
            vec![crate::api::Command::Send { to: Target::Dm("bobroger".into()), text: "10".into() }]
        );
    }

    #[test]
    fn a_bad_answer_keeps_its_error_on_screen() {
        let mut s = session();
        let mut state = TuiState::new(false);
        let now = Instant::now();
        on_send(&mut s, &mut state, "/create cat".into(), now);
        on_send(&mut s, &mut state, String::new(), now);
        on_send(&mut s, &mut state, "hello".into(), now);
        assert!(s.prompt().is_some());
        assert!(state.flash_text(now).is_some_and(|t| t.starts_with("! a room holds")), "{:?}", state.flash_text(now));
    }

    #[test]
    fn clicking_a_key_copies_the_join_line() {
        let mut s = session();
        let mut state = TuiState::new(false);
        let now = Instant::now();
        s.on_event(&Event::RoomCreated { name: "catpie".into(), key: "k7m9xq".into(), max: 13 }, 0);
        let r = on_copy(&s, &mut state, "catpie", now);
        assert_eq!(r.copy.as_deref(), Some("/chat #catpie k7m9xq"));
        assert!(state.flash_text(now).is_some_and(|t| t.starts_with("* copied: /chat #catpie k7m9xq")));
    }

    #[test]
    fn sending_goes_to_the_tab_and_command_output_to_corvo() {
        let mut s = session();
        let mut state = TuiState::new(false);
        let now = Instant::now();
        let r = on_send(&mut s, &mut state, "/chat @bobroger".into(), now).unwrap();
        assert!(r.commands.is_empty());
        assert!(!state.on_status, "opening a chat shows its tab");
        let r = on_send(&mut s, &mut state, "hi\nthere".into(), now).unwrap();
        assert_eq!(
            r.commands,
            vec![crate::api::Command::Send { to: Target::Dm("bobroger".into()), text: "hi\nthere".into() }]
        );
        assert_eq!(state.history.prev("").as_deref(), Some("hi\nthere"));
        let r = on_send(&mut s, &mut state, "/chat @ghostmoon".into(), now).unwrap();
        assert!(r.print.is_empty(), "printed lines were moved into the corvo tab");
        assert_eq!(state.status_lines().last().unwrap().text, "! @ghostmoon is offline");
        assert_eq!(state.flash_text(now), Some("! @ghostmoon is offline"));
        assert_eq!(s.store.current(), Some(&Conv::Dm("bobroger".into())));
    }

    #[test]
    fn events_feed_the_corvo_tab_and_follow_joins() {
        let mut s = session();
        let mut state = TuiState::new(false);
        let now = Instant::now();
        on_event(&mut s, &mut state, &Event::Error("no room #x with that key".into()), now);
        assert_eq!(state.status_lines().last().unwrap().text, "! no room #x with that key");
        assert!(state.flash_text(now).is_some());
        on_event(
            &mut s,
            &mut state,
            &Event::RoomJoined { name: "catpie".into(), key: "k7m9xq".into(), max: 13, members: vec![] },
            now,
        );
        assert!(!state.on_status, "joining shows the room");
        state.scroll.page_up(100, 10);
        let msg =
            Event::Message { conv: Conv::Room("catpie".into()), from: "bobroger".into(), text: "x".into(), ts: 0 };
        on_event(&mut s, &mut state, &msg, now);
        assert_eq!(state.scroll.unseen(), 1);
        on_event(&mut s, &mut state, &Event::RoomLeft { name: "catpie".into() }, now);
        assert!(state.on_status, "leaving the room you're viewing falls back to corvo");
        let warn = Event::Info("@bobmoon has a different key from the bob you met before (last seen 2026-10-12). Compare fingerprints in /who.".into());
        let later = now + Duration::from_secs(60);
        on_event(&mut s, &mut state, &warn, later);
        assert!(state.flash_text(later).is_some_and(|t| t.contains("different key")), "warnings flash");
    }

    #[test]
    fn sending_jumps_to_the_latest_but_others_messages_are_counted() {
        let mut s = session();
        let mut state = TuiState::new(false);
        let now = Instant::now();
        on_event(
            &mut s,
            &mut state,
            &Event::RoomJoined { name: "catpie".into(), key: "k7m9xq".into(), max: 13, members: vec![] },
            now,
        );
        let room = Conv::Room("catpie".into());
        state.scroll.page_up(100, 10);
        let msg = Event::Message { conv: room.clone(), from: "bobroger".into(), text: "x".into(), ts: 0 };
        on_event(&mut s, &mut state, &msg, now);
        assert_eq!(state.scroll.unseen(), 1);
        on_event(&mut s, &mut state, &Event::Sent { conv: room, text: "mine".into(), ts: 0, id: None }, now);
        assert_eq!(state.scroll.unseen(), 0, "our own message isn't news");
        assert!(!state.scroll.is_scrolled(), "sending shows the latest messages");
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
    fn tab_completes_and_the_list_takes_enter_esc_and_arrows() {
        let mut s = session(); // me: annmoon; online: bobroger
        s.on_event(&Event::PeerJoined { id: PeerId::test(3), nick: "bobkite".into() }, 0);
        let mut state = TuiState::new(false);
        state.editor.set("@b".into());
        apply_key(&mut s, &mut state, Action::Complete);
        assert_eq!(state.popup.as_ref().unwrap().matches, vec!["@bobKite", "@bobRoger"]);
        apply_key(&mut s, &mut state, Action::Down);
        apply_key(&mut s, &mut state, Action::Up);
        apply_key(&mut s, &mut state, Action::Complete);
        assert_eq!(state.popup.as_ref().unwrap().selected, 1, "Tab moves down the list");
        assert_eq!(apply_key(&mut s, &mut state, Action::Send), KeyOutcome::Stay, "Enter picks, doesn't send");
        assert_eq!(state.editor.text(), "@bobRoger ");

        state.editor.set("@b".into());
        apply_key(&mut s, &mut state, Action::Complete);
        apply_key(&mut s, &mut state, Action::Esc);
        assert!(state.popup.is_none());
        assert_eq!(state.editor.text(), "@bob", "Esc closes the list and keeps the text");

        apply_key(&mut s, &mut state, Action::Complete);
        apply_key(&mut s, &mut state, Action::Insert('r'));
        assert_eq!(state.popup.as_ref().unwrap().matches, vec!["@bobRoger"], "typing refreshes the list");
        apply_key(&mut s, &mut state, Action::Left);
        assert!(state.popup.is_none(), "moving the cursor closes it");
    }

    #[test]
    fn shift_tab_switches_tabs_even_with_the_list_open() {
        let mut s = session();
        s.on_event(&Event::PeerJoined { id: PeerId::test(3), nick: "bobkite".into() }, 0);
        s.on_line("/chat @bobroger", None);
        let mut state = TuiState::new(false);
        state.on_status = true;
        state.editor.set("@b".into());
        apply_key(&mut s, &mut state, Action::Complete);
        apply_key(&mut s, &mut state, Action::NextTab);
        assert!(state.popup.is_none());
        assert_eq!(state.current_tab(&s), 1);
    }

    #[test]
    fn help_opens_a_box_that_takes_every_key_until_q_or_esc() {
        let mut s = session();
        s.on_line("/chat @bobroger", None);
        let mut state = TuiState::new(false);
        let now = Instant::now();
        let r = on_send(&mut s, &mut state, "/h".into(), now).unwrap();
        assert!(state.help, "/h opens the box");
        assert!(r.commands.is_empty());
        assert!(state.status_lines().next().is_none(), "nothing is printed to the corvo tab");
        let tab = state.current_tab(&s);
        for key in [Action::Insert('x'), Action::Send, Action::NextTab, Action::Complete, Action::PageUp] {
            assert_eq!(apply_key(&mut s, &mut state, key.clone()), KeyOutcome::Stay, "{key:?}");
        }
        assert!(state.help, "other keys leave it open");
        assert!(state.editor.is_empty(), "typing doesn't reach the input");
        assert_eq!(state.current_tab(&s), tab, "tabs don't switch");
        assert!(!state.scroll.is_scrolled());
        assert_eq!(apply_key(&mut s, &mut state, Action::Quit), KeyOutcome::Quit, "Ctrl-C still quits");
        for close in [Action::Insert('q'), Action::Insert('Q'), Action::Esc] {
            state.help = true;
            assert_eq!(apply_key(&mut s, &mut state, close.clone()), KeyOutcome::Stay);
            assert!(!state.help, "{close:?} closes it");
            assert!(state.editor.is_empty(), "{close:?} isn't typed");
        }
    }

    /// Windows writes some of these through the console API, not as escape codes.
    #[cfg(unix)]
    #[test]
    fn leaving_clears_our_title_before_restoring_the_saved_one() {
        let mut out = Vec::new();
        restore_to(&mut out).unwrap();
        let out = String::from_utf8(out).unwrap();
        let clear = out.find("\x1b]0;\x07").expect("title cleared, for terminals without a title stack");
        assert!(out.ends_with(RESTORE_TITLE), "{out:?}");
        assert!(clear < out.len() - RESTORE_TITLE.len(), "cleared first, so a saved title wins: {out:?}");
    }

    #[test]
    fn keyboard_flags_report_shifted_characters() {
        use ratatui::crossterm::event::KeyboardEnhancementFlags as F;
        assert!(KEYBOARD_FLAGS.contains(F::DISAMBIGUATE_ESCAPE_CODES));
        assert!(KEYBOARD_FLAGS.contains(F::REPORT_ALTERNATE_KEYS));
        assert!(!KEYBOARD_FLAGS.contains(F::REPORT_ALL_KEYS_AS_ESCAPE_CODES));
    }
}
