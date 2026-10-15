//! The TUI's state that isn't in the shared session: the input box, history,
//! scroll position, the corvo (status) tab, the error flash and click targets.
//! Pure, so it's tested without a terminal.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Position, Rect};

use crate::api::{Conv, Event};
use crate::convo::mentions;
use crate::ui::complete::{Candidates, common_prefix, complete};
use crate::ui::render::Tone;
use crate::ui::session::Session;

pub const MAX_INPUT_ROWS: usize = 5;
pub const HISTORY_LEN: usize = 100;
pub const STATUS_LEN: usize = 500;
pub const FLASH_FOR: Duration = Duration::from_secs(5);
pub const WHEEL_STEP: usize = 3;
/// The bell rings at most once in this long, however many messages arrive.
pub const BELL_GAP: Duration = Duration::from_secs(1);

/// The multi-line input box. `cursor` is a byte index on a char boundary.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Editor {
    text: String,
    cursor: usize,
}

impl Editor {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn set(&mut self, text: String) {
        self.cursor = text.len();
        self.text = text;
    }

    pub fn clear(&mut self) {
        self.set(String::new());
    }

    pub fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.text)
    }

    pub fn insert(&mut self, c: char) {
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    /// Pasted text: CRLF and lone CR become newlines.
    pub fn insert_str(&mut self, s: &str) {
        let s = s.replace("\r\n", "\n").replace('\r', "\n").replace('\t', "    ");
        self.text.insert_str(self.cursor, &s);
        self.cursor += s.len();
    }

    pub fn newline(&mut self) {
        self.insert('\n');
    }

    pub fn backspace(&mut self) {
        if let Some(c) = self.text[..self.cursor].chars().next_back() {
            self.cursor -= c.len_utf8();
            self.text.remove(self.cursor);
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.text.len() {
            self.text.remove(self.cursor);
        }
    }

    pub fn left(&mut self) {
        if let Some(c) = self.text[..self.cursor].chars().next_back() {
            self.cursor -= c.len_utf8();
        }
    }

    pub fn right(&mut self) {
        if let Some(c) = self.text[self.cursor..].chars().next() {
            self.cursor += c.len_utf8();
        }
    }

    fn row_start(&self) -> usize {
        self.text[..self.cursor].rfind('\n').map_or(0, |i| i + 1)
    }

    fn row_end(&self) -> usize {
        self.text[self.cursor..].find('\n').map_or(self.text.len(), |i| self.cursor + i)
    }

    pub fn home(&mut self) {
        self.cursor = self.row_start();
    }

    pub fn end(&mut self) {
        self.cursor = self.row_end();
    }

    /// Ctrl-U: delete from the start of the row to the cursor.
    pub fn kill_to_row_start(&mut self) {
        let start = self.row_start();
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
    }

    /// Ctrl-W: delete the word before the cursor, and the spaces after it.
    pub fn kill_word(&mut self) {
        let start = self.row_start();
        let trimmed = self.text[start..self.cursor].trim_end_matches(' ');
        let from = start + trimmed.rfind(' ').map_or(0, |i| i + 1);
        self.text.replace_range(from..self.cursor, "");
        self.cursor = from;
    }

    pub fn rows(&self) -> Vec<&str> {
        self.text.split('\n').collect()
    }

    /// (row, column in chars).
    pub fn cursor_pos(&self) -> (usize, usize) {
        let row = self.text[..self.cursor].matches('\n').count();
        (row, self.text[self.row_start()..self.cursor].chars().count())
    }

    /// The text up to the cursor.
    pub fn before_cursor(&self) -> &str {
        &self.text[..self.cursor]
    }

    /// Replaces the text from byte `start` up to the cursor with `with`.
    pub fn replace_before_cursor(&mut self, start: usize, with: &str) {
        self.text.replace_range(start..self.cursor, with);
        self.cursor = start + with.len();
    }

    /// Byte offset of `col` chars into row `row`, clamped to its end.
    fn offset(&self, row: usize, col: usize) -> usize {
        let start: usize = self.text.split('\n').take(row).map(|r| r.len() + 1).sum();
        let line = self.text.split('\n').nth(row).unwrap_or("");
        start + line.char_indices().nth(col).map_or(line.len(), |(i, _)| i)
    }

    /// Moves to the same column one row up; false on the first row.
    pub fn up(&mut self) -> bool {
        let (row, col) = self.cursor_pos();
        if row == 0 {
            return false;
        }
        self.cursor = self.offset(row - 1, col);
        true
    }

    /// Moves to the same column one row down; false on the last row.
    pub fn down(&mut self) -> bool {
        let (row, col) = self.cursor_pos();
        if row + 1 >= self.rows().len() {
            return false;
        }
        self.cursor = self.offset(row + 1, col);
        true
    }
}

/// Sent lines, browsed with ↑/↓ from the input's first or last row.
#[derive(Debug, Default)]
pub struct History {
    entries: Vec<String>,
    at: Option<usize>,
    /// What was being typed when browsing started.
    draft: String,
}

impl History {
    pub fn push(&mut self, entry: &str) {
        self.at = None;
        if entry.trim().is_empty() || self.entries.last().is_some_and(|last| last == entry) {
            return;
        }
        self.entries.push(entry.to_string());
        if self.entries.len() > HISTORY_LEN {
            self.entries.remove(0);
        }
    }

    pub fn prev(&mut self, current: &str) -> Option<String> {
        let at = match self.at {
            None if self.entries.is_empty() => return None,
            None => {
                self.draft = current.to_string();
                self.entries.len() - 1
            }
            Some(0) => return None,
            Some(i) => i - 1,
        };
        self.at = Some(at);
        Some(self.entries[at].clone())
    }

    /// The next newer entry, then the draft, then nothing.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Option<String> {
        let at = self.at?;
        if at + 1 < self.entries.len() {
            self.at = Some(at + 1);
            Some(self.entries[at + 1].clone())
        } else {
            self.at = None;
            Some(std::mem::take(&mut self.draft))
        }
    }
}

/// Where the message pane is. `top` is the first visible row counted from the
/// top, so new rows at the bottom don't move a scrolled view; `None` follows the end.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Scroll {
    top: Option<usize>,
    unseen: usize,
}

impl Scroll {
    pub fn top(&self, total: usize, height: usize) -> usize {
        let max = total.saturating_sub(height);
        self.top.map_or(max, |t| t.min(max))
    }

    pub fn is_scrolled(&self) -> bool {
        self.top.is_some()
    }

    pub fn unseen(&self) -> usize {
        self.unseen
    }

    pub fn by(&mut self, delta: isize, total: usize, height: usize) {
        let max = total.saturating_sub(height);
        let next = (self.top(total, height) as isize + delta).clamp(0, max as isize) as usize;
        if next >= max { self.to_bottom() } else { self.top = Some(next) }
    }

    pub fn page_up(&mut self, total: usize, height: usize) {
        self.by(-(height.max(1) as isize), total, height);
    }

    pub fn page_down(&mut self, total: usize, height: usize) {
        self.by(height.max(1) as isize, total, height);
    }

    pub fn to_bottom(&mut self) {
        self.top = None;
        self.unseen = 0;
    }

    /// A message arrived in the view; counted only while scrolled up.
    pub fn note_new(&mut self) {
        if self.top.is_some() {
            self.unseen += 1;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tab {
    /// `corvo`: command output, notices and errors.
    Status,
    Conv(Conv),
}

/// `corvo` first, then conversations in the store's order.
pub fn tabs(session: &Session) -> Vec<Tab> {
    std::iter::once(Tab::Status)
        .chain(session.store.conversations().iter().map(|c| Tab::Conv(c.conv.clone())))
        .collect()
}

/// Something clickable on screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hit {
    Tab(usize),
    Person(String),
    /// A room's name or key in the sidebar: copies its invite.
    Invite(String),
}

/// Completion choices shown above the input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Popup {
    pub matches: Vec<String>,
    pub selected: usize,
    /// Byte offset in the input where the word being completed starts.
    pub start: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Send,
    Newline,
    Insert(char),
    Backspace,
    Delete,
    Left,
    Right,
    Home,
    End,
    Up,
    Down,
    NextTab,
    PrevTab,
    GoTab(usize),
    PageUp,
    PageDown,
    Esc,
    Complete,
    KillRow,
    KillWord,
    Quit,
    Redraw,
}

/// What a key press means. Shift+Enter needs a terminal that reports it;
/// Alt+Enter and Ctrl-J work everywhere.
pub fn action_for(key: KeyEvent) -> Option<Action> {
    if key.kind == KeyEventKind::Release {
        return None;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    Some(match key.code {
        KeyCode::Enter if shift || alt => Action::Newline,
        KeyCode::Enter => Action::Send,
        KeyCode::Char('c') if ctrl => Action::Quit,
        KeyCode::Char('l') if ctrl => Action::Redraw,
        KeyCode::Char('u') if ctrl => Action::KillRow,
        KeyCode::Char('w') if ctrl => Action::KillWord,
        KeyCode::Char('j') if ctrl => Action::Newline,
        KeyCode::Char(c @ '1'..='9') if alt => Action::GoTab(c as usize - '1' as usize),
        KeyCode::Char(_) if ctrl || alt => return None,
        KeyCode::Char(c) => Action::Insert(c),
        KeyCode::Backspace => Action::Backspace,
        KeyCode::Delete => Action::Delete,
        KeyCode::Left if alt => Action::PrevTab,
        KeyCode::Right if alt => Action::NextTab,
        KeyCode::Left => Action::Left,
        KeyCode::Right => Action::Right,
        KeyCode::Home => Action::Home,
        KeyCode::End => Action::End,
        KeyCode::Up => Action::Up,
        KeyCode::Down => Action::Down,
        KeyCode::Tab => Action::Complete,
        KeyCode::BackTab => Action::NextTab,
        KeyCode::PageUp => Action::PageUp,
        KeyCode::PageDown => Action::PageDown,
        KeyCode::Esc => Action::Esc,
        _ => return None,
    })
}

/// Events shown in the corvo tab. Chat lines and member changes already live
/// in their conversation.
pub fn goes_to_status(event: &Event) -> bool {
    !matches!(
        event,
        Event::Message { .. }
            | Event::Sent { .. }
            | Event::MemberJoined { .. }
            | Event::MemberLeft { .. }
            | Event::Approved { .. }
            | Event::RequestEnded { .. }
            | Event::Delivered { .. }
            | Event::NotDelivered { .. }
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusLine {
    pub ts: u64,
    pub text: String,
    pub tone: Tone,
}

pub struct TuiState {
    pub editor: Editor,
    pub history: History,
    pub scroll: Scroll,
    /// Showing the corvo tab rather than the store's current conversation.
    pub on_status: bool,
    pub color: bool,
    /// (total rows, visible rows) of the message pane at the last draw.
    pub view: (usize, usize),
    /// The completion list, while open.
    pub popup: Option<Popup>,
    /// The help box is open: it takes every key until q, Q or Esc.
    pub help: bool,
    /// Our LAN address for the status bar, e.g. "ip 192.168.1.9 · port 54321".
    pub lan: String,
    /// Ring the terminal bell for DMs and mentions (`/bell`). Off at every start.
    pub bell: bool,
    /// The terminal window has focus. Assumed until the terminal says otherwise.
    pub focused: bool,
    /// Messages that arrived in the open conversation while the window was in
    /// the background; the store counts those as read.
    pub away: u32,
    status: VecDeque<StatusLine>,
    flash: Option<(String, Instant)>,
    hits: Vec<(Rect, Hit)>,
    last_ring: Option<Instant>,
}

impl TuiState {
    pub fn new(color: bool) -> Self {
        TuiState {
            editor: Editor::default(),
            history: History::default(),
            scroll: Scroll::default(),
            on_status: true,
            color,
            view: (0, 0),
            popup: None,
            help: false,
            lan: String::new(),
            bell: false,
            focused: true,
            away: 0,
            status: VecDeque::new(),
            flash: None,
            hits: Vec::new(),
            last_ring: None,
        }
    }

    pub fn current_tab(&self, session: &Session) -> usize {
        if self.on_status {
            return 0;
        }
        let current = session.store.current();
        tabs(session).iter().position(|t| matches!(t, Tab::Conv(c) if Some(c) == current)).unwrap_or(0)
    }

    pub fn select_tab(&mut self, session: &mut Session, idx: usize) {
        self.popup = None;
        // A question left open would take whatever is typed in the new tab.
        session.cancel_prompt();
        match tabs(session).get(idx) {
            Some(Tab::Status) => {
                session.store.clear_current();
                self.on_status = true;
            }
            Some(Tab::Conv(conv)) => {
                session.store.switch(conv.clone());
                self.on_status = false;
            }
            None => return,
        }
        self.scroll.to_bottom();
    }

    pub fn step_tab(&mut self, session: &mut Session, delta: isize) {
        let n = tabs(session).len() as isize;
        let next = (self.current_tab(session) as isize + delta).rem_euclid(n);
        self.select_tab(session, next as usize);
    }

    /// Tab: completes the word before the cursor. One match goes straight in;
    /// several fill their shared start and open the list.
    pub fn complete(&mut self, cands: &Candidates) {
        let Some(c) = complete(self.editor.before_cursor(), cands) else {
            return;
        };
        if let [only] = c.matches.as_slice() {
            self.editor.replace_before_cursor(c.start, &format!("{only} "));
            return;
        }
        let prefix = common_prefix(&c.matches);
        if prefix.chars().count() > self.editor.before_cursor()[c.start..].chars().count() {
            self.editor.replace_before_cursor(c.start, &prefix);
        }
        self.popup = Some(Popup { matches: c.matches, selected: 0, start: c.start });
    }

    /// ↑/↓ (and Tab) in the open list; wraps.
    pub fn move_selection(&mut self, delta: isize) {
        if let Some(p) = &mut self.popup {
            p.selected = (p.selected as isize + delta).rem_euclid(p.matches.len() as isize) as usize;
        }
    }

    /// Enter in the open list: puts the choice in the input and closes it.
    pub fn accept(&mut self) {
        if let Some(p) = self.popup.take() {
            self.editor.replace_before_cursor(p.start, &format!("{} ", p.matches[p.selected]));
        }
    }

    /// After an edit with the list open: re-runs completion, closing the list
    /// when nothing matches.
    pub fn refresh_popup(&mut self, cands: &Candidates) {
        if self.popup.is_some() {
            self.popup = complete(self.editor.before_cursor(), cands).map(|c| Popup {
                matches: c.matches,
                selected: 0,
                start: c.start,
            });
        }
    }

    pub fn push_status(&mut self, ts: u64, text: &str, tone: Tone) {
        self.status.push_back(StatusLine { ts, text: text.to_string(), tone });
        if self.status.len() > STATUS_LEN {
            self.status.pop_front();
        }
    }

    pub fn status_lines(&self) -> impl Iterator<Item = &StatusLine> {
        self.status.iter()
    }

    pub fn flash(&mut self, text: &str, now: Instant) {
        self.flash = Some((text.to_string(), now));
    }

    pub fn flash_text(&self, now: Instant) -> Option<&str> {
        self.flash.as_ref().filter(|(_, at)| now.saturating_duration_since(*at) < FLASH_FOR).map(|(t, _)| t.as_str())
    }

    /// A DM, or a message that mentions us, arriving where we aren't looking:
    /// another tab, the corvo tab, or a window in the background.
    pub fn worth_a_ring(&self, session: &Session, event: &Event) -> bool {
        let Event::Message { conv, text, .. } = event else {
            return false;
        };
        let for_me = matches!(conv, Conv::Dm(_)) || session.store.me().is_some_and(|me| mentions(text, me));
        let watching = self.focused && !self.on_status && session.store.current() == Some(conv);
        for_me && !watching
    }

    /// Whether to ring now: the bell is on and hasn't rung within `BELL_GAP`.
    pub fn ring(&mut self, now: Instant) -> bool {
        if !self.bell || self.last_ring.is_some_and(|t| now.saturating_duration_since(t) < BELL_GAP) {
            return false;
        }
        self.last_ring = Some(now);
        true
    }

    /// Flips the bell; returns what to tell the user.
    pub fn toggle_bell(&mut self) -> &'static str {
        self.bell = !self.bell;
        if self.bell { "* bell on — rings for DMs and mentions of you" } else { "* bell off" }
    }

    pub fn clear_hits(&mut self) {
        self.hits.clear();
    }

    pub fn add_hit(&mut self, area: Rect, hit: Hit) {
        self.hits.push((area, hit));
    }

    pub fn hit(&self, col: u16, row: u16) -> Option<Hit> {
        self.hits.iter().find(|(area, _)| area.contains(Position::new(col, row))).map(|(_, hit)| hit.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::PeerId;
    use crate::ui::complete::Candidates;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

    #[test]
    fn the_bell_rings_at_most_once_a_gap() {
        let mut t = TuiState::new(false);
        let t0 = Instant::now();
        assert!(!t.ring(t0), "off at start");
        assert_eq!(t.toggle_bell(), "* bell on — rings for DMs and mentions of you");
        assert!(t.ring(t0));
        assert!(!t.ring(t0 + BELL_GAP / 2), "a burst rings once");
        assert!(t.ring(t0 + BELL_GAP));
        assert_eq!(t.toggle_bell(), "* bell off");
        assert!(!t.ring(t0 + BELL_GAP * 3));
    }

    fn ed(text: &str) -> Editor {
        let mut e = Editor::default();
        e.set(text.into());
        e
    }

    #[test]
    fn editor_edits_and_moves_by_chars() {
        let mut e = Editor::default();
        for c in "héllo".chars() {
            e.insert(c);
        }
        e.left();
        e.left();
        e.backspace();
        assert_eq!(e.text(), "hélo");
        e.delete();
        assert_eq!(e.text(), "héo");
        e.home();
        e.insert('>');
        e.end();
        e.insert('!');
        assert_eq!(e.text(), ">héo!");
        assert_eq!(e.take(), ">héo!");
        assert!(e.is_empty());
    }

    #[test]
    fn newlines_rows_and_vertical_moves() {
        let mut e = ed("first line");
        e.newline();
        e.insert_str("second\r\nthird");
        assert_eq!(e.rows(), vec!["first line", "second", "third"]);
        assert_eq!(e.cursor_pos(), (2, 5));
        assert!(e.up());
        assert_eq!(e.cursor_pos(), (1, 5));
        assert!(e.up());
        assert_eq!(e.cursor_pos(), (0, 5));
        assert!(!e.up(), "already on the first row");
        assert!(e.down() && e.down());
        assert!(!e.down(), "already on the last row");
        e.home();
        assert_eq!(e.cursor_pos(), (2, 0));
    }

    #[test]
    fn pasted_tabs_become_spaces() {
        let mut e = ed("");
        e.insert_str("a\tb\r\n\tc");
        assert_eq!(e.text(), "a    b\n    c");
        assert_eq!(e.cursor_pos(), (1, 5));
    }

    #[test]
    fn ctrl_u_and_ctrl_w_stay_on_the_row() {
        let mut e = ed("keep\nsay hello  world");
        e.kill_word();
        assert_eq!(e.text(), "keep\nsay hello  ");
        e.kill_word();
        assert_eq!(e.text(), "keep\nsay ");
        e.kill_to_row_start();
        assert_eq!(e.text(), "keep\n");
        e.kill_to_row_start();
        assert_eq!(e.text(), "keep\n");
    }

    #[test]
    fn history_browses_and_keeps_the_draft() {
        let mut h = History::default();
        assert_eq!(h.prev("draft"), None);
        h.push("one");
        h.push("two");
        h.push("two");
        h.push("  ");
        assert_eq!(h.prev("draft").as_deref(), Some("two"));
        assert_eq!(h.prev("ignored").as_deref(), Some("one"));
        assert_eq!(h.prev("ignored"), None);
        assert_eq!(h.next().as_deref(), Some("two"));
        assert_eq!(h.next().as_deref(), Some("draft"));
        assert_eq!(h.next(), None);
        for i in 0..HISTORY_LEN + 5 {
            h.push(&format!("m{i}"));
        }
        let mut oldest = None;
        while let Some(e) = h.prev("") {
            oldest = Some(e);
        }
        assert_eq!(oldest.as_deref(), Some("m5"));
    }

    #[test]
    fn scrolling_pages_counts_new_lines_and_returns() {
        let mut s = Scroll::default();
        assert_eq!(s.top(100, 10), 90);
        s.page_up(100, 10);
        assert_eq!(s.top(100, 10), 80);
        s.note_new();
        s.note_new();
        assert_eq!(s.unseen(), 2);
        assert_eq!(s.top(102, 10), 80, "new lines don't move the view");
        s.by(-500, 102, 10);
        assert_eq!(s.top(102, 10), 0);
        s.page_down(102, 10);
        s.by(1000, 102, 10);
        assert!(!s.is_scrolled() && s.unseen() == 0);
        s.note_new();
        assert_eq!(s.unseen(), 0, "at the bottom, nothing is unseen");
        s.page_up(5, 10);
        assert!(!s.is_scrolled(), "nothing to scroll when it all fits");
    }

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn keys_map_to_actions() {
        let none = KeyModifiers::NONE;
        assert_eq!(action_for(key(KeyCode::Enter, none)), Some(Action::Send));
        assert_eq!(action_for(key(KeyCode::Enter, KeyModifiers::SHIFT)), Some(Action::Newline));
        assert_eq!(action_for(key(KeyCode::Enter, KeyModifiers::ALT)), Some(Action::Newline));
        assert_eq!(action_for(key(KeyCode::Char('j'), KeyModifiers::CONTROL)), Some(Action::Newline));
        assert_eq!(action_for(key(KeyCode::Char('x'), none)), Some(Action::Insert('x')));
        assert_eq!(action_for(key(KeyCode::Char('X'), KeyModifiers::SHIFT)), Some(Action::Insert('X')));
        assert_eq!(action_for(key(KeyCode::Char('c'), KeyModifiers::CONTROL)), Some(Action::Quit));
        assert_eq!(action_for(key(KeyCode::Char('3'), KeyModifiers::ALT)), Some(Action::GoTab(2)));
        assert_eq!(action_for(key(KeyCode::Tab, none)), Some(Action::Complete));
        assert_eq!(action_for(key(KeyCode::BackTab, KeyModifiers::SHIFT)), Some(Action::NextTab));
        assert_eq!(action_for(key(KeyCode::Left, KeyModifiers::ALT)), Some(Action::PrevTab));
        assert_eq!(action_for(key(KeyCode::Right, KeyModifiers::ALT)), Some(Action::NextTab));
        assert_eq!(action_for(key(KeyCode::Left, none)), Some(Action::Left));
        assert_eq!(action_for(key(KeyCode::Char('q'), KeyModifiers::ALT)), None);
        let mut release = key(KeyCode::Char('x'), none);
        release.kind = KeyEventKind::Release;
        assert_eq!(action_for(release), None);
    }

    fn session_with_tabs() -> Session {
        let mut s = Session::new(false);
        s.on_event(&Event::PeerJoined { id: PeerId::test(2), nick: "bobroger".into() }, 0);
        s.on_event(&Event::RoomJoined { name: "catpie".into(), key: "k7m9xq".into(), max: 13, members: vec![] }, 0);
        s.on_event(
            &Event::Message { conv: Conv::Dm("bobroger".into()), from: "bobroger".into(), text: "x".into(), ts: 0 },
            0,
        );
        s
    }

    fn bobs() -> Candidates {
        Candidates { nicks: vec!["bobroger".into(), "bobkite".into(), "carolwolf".into()], ..Default::default() }
    }

    #[test]
    fn tab_completes_one_match_or_opens_the_list() {
        let mut t = TuiState::new(false);
        t.editor.set("hi @car".into());
        t.complete(&bobs());
        assert_eq!(t.editor.text(), "hi @carolWolf ");
        assert_eq!(t.popup, None);

        t.editor.set("é @b".into());
        t.complete(&bobs());
        assert_eq!(t.editor.text(), "é @bob", "fills the shared prefix");
        assert_eq!(t.popup.as_ref().unwrap().matches, vec!["@bobRoger", "@bobKite"]);
        t.move_selection(1);
        t.accept();
        assert_eq!(t.editor.text(), "é @bobKite ");
        assert_eq!(t.popup, None);

        t.editor.set("line one\n@car".into());
        t.complete(&bobs());
        assert_eq!(t.editor.text(), "line one\n@carolWolf ");

        t.editor.set("@zed".into());
        t.complete(&bobs());
        assert_eq!((t.editor.text(), t.popup.is_none()), ("@zed", true));
    }

    #[test]
    fn selection_wraps_and_typing_keeps_the_list_in_step() {
        let mut t = TuiState::new(false);
        t.editor.set("@b".into());
        t.complete(&bobs());
        t.move_selection(-1);
        assert_eq!(t.popup.as_ref().unwrap().selected, 1);
        t.editor.insert('k');
        t.refresh_popup(&bobs());
        assert_eq!(t.popup.as_ref().unwrap().matches, vec!["@bobKite"]);
        assert_eq!(t.popup.as_ref().unwrap().selected, 0);
        t.editor.insert(' ');
        t.refresh_popup(&bobs());
        assert_eq!(t.popup, None, "a space ends the word");
    }

    #[test]
    fn editing_past_the_word_closes_the_list() {
        let mut t = TuiState::new(false);
        t.editor.set("hi @b".into());
        t.complete(&bobs());
        for _ in 0..5 {
            t.editor.backspace();
            t.refresh_popup(&bobs());
        }
        assert_eq!(t.popup, None);
        t.accept(); // nothing open: must not touch the text or panic
        assert_eq!(t.editor.text(), "hi");
    }

    #[test]
    fn switching_tabs_closes_the_list() {
        let mut s = session_with_tabs();
        let mut t = TuiState::new(false);
        t.editor.set("@b".into());
        t.complete(&bobs());
        t.select_tab(&mut s, 1);
        assert_eq!(t.popup, None);
    }

    #[test]
    fn tabs_cycle_and_jump() {
        let mut s = session_with_tabs();
        let mut t = TuiState::new(false);
        assert_eq!(
            tabs(&s),
            vec![Tab::Status, Tab::Conv(Conv::Room("catpie".into())), Tab::Conv(Conv::Dm("bobroger".into()))]
        );
        assert_eq!(t.current_tab(&s), 0);
        t.step_tab(&mut s, 1);
        assert_eq!(t.current_tab(&s), 1);
        t.step_tab(&mut s, 1);
        assert_eq!(s.store.current(), Some(&Conv::Dm("bobroger".into())));
        assert_eq!(s.store.get(&Conv::Dm("bobroger".into())).unwrap().unread, 0, "viewing marks read");
        t.step_tab(&mut s, 1);
        assert!(t.on_status, "wraps to corvo");
        t.step_tab(&mut s, -1);
        assert_eq!(t.current_tab(&s), 2);
        t.select_tab(&mut s, 9);
        assert_eq!(t.current_tab(&s), 2, "out of range does nothing");
    }

    #[test]
    fn messages_count_as_unread_while_on_the_corvo_tab() {
        let mut s = session_with_tabs();
        let mut t = TuiState::new(false);
        t.select_tab(&mut s, 2);
        t.select_tab(&mut s, 0);
        let bob = Conv::Dm("bobroger".into());
        s.on_event(&Event::Message { conv: bob.clone(), from: "bobroger".into(), text: "hi".into(), ts: 0 }, 0);
        assert_eq!(s.store.get(&bob).unwrap().unread, 1);
        assert_eq!(s.store.current(), None);
        assert_eq!(t.current_tab(&s), 0);
    }

    #[test]
    fn removing_the_current_tab_falls_back() {
        let mut s = session_with_tabs();
        let mut t = TuiState::new(false);
        t.select_tab(&mut s, 2);
        s.store.close(&Conv::Dm("bobroger".into()));
        assert_eq!(t.current_tab(&s), 0, "no current conversation means the corvo tab");
        assert!(tabs(&s).len() == 2);
        t.step_tab(&mut s, 1);
        assert_eq!(t.current_tab(&s), 1);
    }

    #[test]
    fn status_lines_are_capped_and_flash_expires() {
        let mut t = TuiState::new(false);
        for i in 0..STATUS_LEN + 3 {
            t.push_status(0, &format!("l{i}"), Tone::Plain);
        }
        assert_eq!(t.status_lines().count(), STATUS_LEN);
        assert_eq!(t.status_lines().next().unwrap().text, "l3");
        let t0 = Instant::now();
        t.flash("! nope", t0);
        assert_eq!(t.flash_text(t0 + Duration::from_secs(4)), Some("! nope"));
        assert_eq!(t.flash_text(t0 + FLASH_FOR), None);
    }

    #[test]
    fn clicks_hit_what_was_drawn_there() {
        let mut t = TuiState::new(false);
        t.add_hit(Rect::new(0, 0, 7, 1), Hit::Tab(0));
        t.add_hit(Rect::new(60, 3, 20, 1), Hit::Person("bobroger".into()));
        assert_eq!(t.hit(3, 0), Some(Hit::Tab(0)));
        assert_eq!(t.hit(79, 3), Some(Hit::Person("bobroger".into())));
        assert_eq!(t.hit(80, 3), None);
        t.clear_hits();
        assert_eq!(t.hit(3, 0), None);
    }

    #[test]
    fn only_chat_lines_stay_out_of_the_corvo_tab() {
        let msg = Event::Message { conv: Conv::Dm("b".into()), from: "b".into(), text: "x".into(), ts: 0 };
        assert!(!goes_to_status(&msg));
        assert!(!goes_to_status(&Event::Sent { conv: Conv::Dm("b".into()), text: "x".into(), ts: 0, id: Some(1) }));
        assert!(!goes_to_status(&Event::Delivered { id: 1 }));
        assert!(!goes_to_status(&Event::NotDelivered { ids: vec![1] }));
        assert!(!goes_to_status(&Event::MemberJoined { room: "r".into(), nick: "b".into() }));
        assert!(goes_to_status(&Event::Error("x".into())));
        assert!(goes_to_status(&Event::RoomCreated { name: "r".into(), key: "k".into(), max: 13 }));
        assert!(goes_to_status(&Event::PeerJoined { id: PeerId::test(1), nick: "b".into() }));
        let approved = Event::Approved { room: "r".into(), nick: "b".into(), by: "c".into(), have: 1, need: 2 };
        assert!(!goes_to_status(&approved));
        let ended = Event::RequestEnded { room: "r".into(), nick: "b".into(), why: crate::api::RequestEnd::Gone };
        assert!(!goes_to_status(&ended));
        assert!(goes_to_status(&Event::JoinRequest { room: "r".into(), nick: "b".into(), have: 0, need: 1 }));
    }
}
