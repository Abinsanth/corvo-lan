//! Draws the TUI from the shared session and the TUI state. Pure: it only
//! writes into a ratatui `Frame`, so it's tested with `TestBackend`.

use std::time::Instant;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use super::state::{Editor, Hit, MAX_INPUT_ROWS, Tab, TuiState, tabs};
use crate::api::Conv;
use crate::convo::{LineKind, mentions};
use crate::ui::render::{Tone, clean, hhmm, nick_color, tone_of_notice};
use crate::ui::session::Session;

pub const SIDEBAR_WIDTH: u16 = 22;
pub const SIDEBAR_MIN_WIDTH: u16 = 70;
pub const MIN_WIDTH: u16 = 40;
pub const MIN_HEIGHT: u16 = 10;
pub const MAX_SENDER: usize = 16;
/// "HH:MM ".
const TIME_WIDTH: usize = 6;

fn tone_style(tone: Tone, color: bool) -> Style {
    let style = Style::default();
    match (tone, color) {
        (Tone::Dim, _) => style.add_modifier(Modifier::DIM),
        (_, false) | (Tone::Plain, _) => style,
        (Tone::Good, true) => style.fg(Color::Green),
        (Tone::Bad, true) => style.fg(Color::Red),
        (Tone::Warn, true) => style.fg(Color::Yellow),
    }
}

/// The same stable colour line mode uses (ANSI 31–36).
fn nick_style(nick: &str, color: bool) -> Style {
    if !color {
        return Style::default().add_modifier(Modifier::BOLD);
    }
    let fg = match nick_color(nick) {
        31 => Color::Red,
        32 => Color::Green,
        33 => Color::Yellow,
        34 => Color::Blue,
        35 => Color::Magenta,
        _ => Color::Cyan,
    };
    Style::default().fg(fg)
}

/// Greedy wrap by chars, breaking at the last space that fits.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    let mut rest: Vec<char> = text.chars().collect();
    while rest.len() > width {
        let cut = rest[..=width].iter().rposition(|c| *c == ' ').filter(|&i| i > 0).unwrap_or(width);
        out.push(rest[..cut].iter().collect::<String>().trim_end().to_string());
        let skip = if rest.get(cut) == Some(&' ') { cut + 1 } else { cut };
        rest.drain(..skip);
    }
    out.push(rest.into_iter().collect());
    out
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max { s.to_string() } else { s.chars().take(max - 1).chain(['…']).collect() }
}

/// Rows for one message: a prefix on the first row, then text wrapped and
/// continued under the text column. Each line of a multi-line text is cleaned.
fn message_rows(
    prefix: Vec<Span<'static>>,
    prefix_w: usize,
    text: &str,
    style: Style,
    width: usize,
) -> Vec<Line<'static>> {
    let body_w = width.saturating_sub(prefix_w).max(10);
    let mut out = Vec::new();
    let mut first = Some(prefix);
    for part in text.split('\n') {
        for piece in wrap(&clean(part), body_w) {
            let lead = first.take().unwrap_or_else(|| vec![Span::raw(" ".repeat(prefix_w))]);
            let mut spans = lead;
            spans.push(Span::styled(piece, style));
            out.push(Line::from(spans));
        }
    }
    out
}

fn conversation_rows(session: &Session, conv: &Conv, width: usize, color: bool) -> Vec<Line<'static>> {
    let Some(c) = session.store.get(conv) else {
        return Vec::new();
    };
    let me = session.store.me().unwrap_or("you").to_string();
    let sender_of = |kind: &LineKind| match kind {
        LineKind::Msg { from, .. } => Some(format!("@{}", clean(from))),
        LineKind::Mine { .. } => Some(format!("@{}", clean(&me))),
        LineKind::Notice(_) => None,
    };
    let sender_w = c.lines.iter().filter_map(|l| sender_of(&l.kind)).map(|s| s.chars().count()).max().unwrap_or(1);
    let sender_w = sender_w.clamp(1, MAX_SENDER);
    let dim = tone_style(Tone::Dim, color);
    let mut out = Vec::new();
    for l in &c.lines {
        let time = Span::styled(format!("{} ", hhmm(l.ts)), dim);
        match &l.kind {
            LineKind::Notice(text) => {
                let prefix = vec![time, Span::styled("* ", dim)];
                out.extend(message_rows(prefix, TIME_WIDTH + 2, text, tone_style(tone_of_notice(text), color), width));
            }
            kind => {
                let sender = sender_of(kind).unwrap_or_default();
                let nick = sender.trim_start_matches('@').to_string();
                let label = format!("{:<sender_w$} ", truncate(&sender, sender_w));
                let text = match kind {
                    LineKind::Msg { text, .. } | LineKind::Mine { text } => text.as_str(),
                    LineKind::Notice(_) => "",
                };
                let body_style = match kind {
                    LineKind::Msg { text, .. } if session.store.me().is_some_and(|me| mentions(text, me)) => {
                        tone_style(Tone::Warn, color).add_modifier(Modifier::BOLD)
                    }
                    _ => Style::default(),
                };
                let prefix = vec![time, Span::styled(label, nick_style(&nick, color))];
                out.extend(message_rows(prefix, TIME_WIDTH + sender_w + 1, text, body_style, width));
            }
        }
    }
    out
}

fn status_rows(state: &TuiState, width: usize) -> Vec<Line<'static>> {
    let dim = tone_style(Tone::Dim, state.color);
    let mut out = Vec::new();
    for l in state.status_lines() {
        let prefix = vec![Span::styled(format!("{} ", hhmm(l.ts)), dim)];
        out.extend(message_rows(prefix, TIME_WIDTH, &l.text, tone_style(l.tone, state.color), width));
    }
    out
}

fn tab_label(tab: &Tab, session: &Session) -> (String, bool) {
    match tab {
        Tab::Status => ("corvo".to_string(), false),
        Tab::Conv(conv) => {
            let c = session.store.get(conv);
            let name = match conv {
                Conv::Room(n) => format!("#{}", clean(n)),
                Conv::Dm(n) => format!("@{}", clean(n)),
            };
            let (unread, mentioned) = c.map_or((0, 0), |c| (c.unread, c.mentions));
            let label = match (unread, mentioned) {
                (0, _) => name,
                (u, 0) => format!("{name} ({u})"),
                (u, m) => format!("{name} ({u} @{m})"),
            };
            (label, c.is_some_and(|c| c.offline))
        }
    }
}

fn draw_tabs(frame: &mut Frame, area: Rect, session: &Session, state: &mut TuiState) {
    let list = tabs(session);
    let current = state.current_tab(session);
    let labels: Vec<(String, bool)> = list.iter().map(|t| tab_label(t, session)).collect();
    let widths: Vec<usize> = labels.iter().map(|(l, _)| l.chars().count() + 2).collect();
    let avail = area.width as usize;
    // Scroll the strip right until the current tab fits, leaving room for ‹ and ›.
    let mut start = 0;
    while start < current && widths[start..=current].iter().sum::<usize>() + (current - start) + 2 > avail {
        start += 1;
    }
    let mut spans = Vec::new();
    let mut x = area.x;
    if start > 0 {
        spans.push(Span::raw("‹"));
        x += 1;
    }
    let mut shown_all = true;
    for (i, (label, offline)) in labels.iter().enumerate().skip(start) {
        let w = widths[i] as u16;
        if x + w + 1 > area.x + area.width {
            shown_all = false;
            break;
        }
        let mut style = if *offline { tone_style(Tone::Dim, state.color) } else { Style::default() };
        if i == current {
            style = style.add_modifier(Modifier::REVERSED);
        }
        spans.push(Span::styled(format!(" {label} "), style));
        state.add_hit(Rect::new(x, area.y, w, 1), Hit::Tab(i));
        x += w;
        if i + 1 < labels.len() {
            spans.push(Span::styled("│", tone_style(Tone::Dim, state.color)));
            x += 1;
        }
    }
    if !shown_all {
        spans.push(Span::raw("›"));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_sidebar(frame: &mut Frame, area: Rect, session: &Session, state: &mut TuiState) {
    let block = Block::new().borders(Borders::LEFT).border_style(tone_style(Tone::Dim, state.color));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let me = session.store.me().unwrap_or_default().to_string();
    let mut lines: Vec<Line> = Vec::new();
    let mut people: Vec<(usize, String)> = Vec::new();
    let person = |nick: &str, online: bool| {
        let (dot, style) = if online {
            ("●", tone_style(Tone::Good, state.color))
        } else {
            ("○", tone_style(Tone::Dim, state.color))
        };
        Line::from(Span::styled(format!(" {dot} {}", truncate(&clean(nick), SIDEBAR_WIDTH as usize - 4)), style))
    };
    if let (false, Some(Conv::Room(room))) = (state.on_status, session.store.current()) {
        lines
            .push(Line::from(Span::styled(format!("#{}", clean(room)), Style::default().add_modifier(Modifier::BOLD))));
        if let Some(c) = session.store.get(&Conv::Room(room.clone())) {
            for m in &c.members {
                people.push((lines.len(), m.clone()));
                lines.push(person(m, session.store.is_online(m)));
            }
        }
        lines.push(Line::raw(""));
    }
    lines.push(Line::from(Span::styled("online", tone_style(Tone::Dim, state.color))));
    for nick in session.store.online().filter(|n| **n != me) {
        people.push((lines.len(), nick.clone()));
        lines.push(person(nick, true));
    }
    for (row, nick) in people {
        if (row as u16) < inner.height {
            state.add_hit(Rect::new(inner.x, inner.y + row as u16, inner.width, 1), Hit::Person(nick));
        }
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

/// The input as visual rows (no prompts) and the cursor as (row, col). Long
/// rows are cut into `width`-char chunks so the cursor maps exactly.
fn input_layout(editor: &Editor, width: usize) -> (Vec<String>, (usize, usize)) {
    let width = width.max(1);
    let (crow, ccol) = editor.cursor_pos();
    let mut out = Vec::new();
    let mut cursor = (0, 0);
    for (i, row) in editor.rows().into_iter().enumerate() {
        let chars: Vec<char> = row.chars().collect();
        if i == crow {
            let at_full_end = ccol > 0 && ccol % width == 0 && ccol == chars.len();
            cursor = if at_full_end {
                (out.len() + ccol / width - 1, width)
            } else {
                (out.len() + ccol / width, ccol % width)
            };
        }
        if chars.is_empty() {
            out.push(String::new());
        } else {
            out.extend(chars.chunks(width).map(|c| c.iter().collect::<String>()));
        }
    }
    (out, cursor)
}

fn draw_input(frame: &mut Frame, area: Rect, state: &TuiState) {
    let (rows, (crow, ccol)) = input_layout(&state.editor, (area.width as usize).saturating_sub(2));
    let height = area.height as usize;
    let first = crow.saturating_sub(height.saturating_sub(1));
    let lines: Vec<Line> = if state.editor.is_empty() {
        vec![Line::from(vec![Span::raw("> "), Span::styled("type a message…", tone_style(Tone::Dim, state.color))])]
    } else {
        rows.iter()
            .enumerate()
            .skip(first)
            .take(height)
            .map(|(i, r)| Line::raw(format!("{}{r}", if i == 0 { "> " } else { "  " })))
            .collect()
    };
    frame.render_widget(Paragraph::new(lines), area);
    let x = (area.x + 2 + ccol as u16).min(area.x + area.width.saturating_sub(1));
    frame.set_cursor_position((x, area.y + (crow - first) as u16));
}

/// Most completion choices shown at once; the list scrolls with the selection.
pub const POPUP_ROWS: usize = 6;

/// The completion list, just above the input and lined up with the word.
/// Shrinks to the rows that fit; closes it if none do, so a list nobody can
/// see never takes Enter or the arrows.
fn draw_popup(frame: &mut Frame, input: Rect, state: &mut TuiState) {
    // Below the tab row, with a border above and below.
    let room = (input.y as usize).saturating_sub(3);
    let Some(p) = &state.popup else {
        return;
    };
    let rows = p.matches.len().min(POPUP_ROWS).min(room);
    if rows == 0 {
        state.popup = None;
        return;
    }
    let height = rows as u16 + 2;
    let longest = p.matches.iter().map(|m| m.chars().count()).max().unwrap_or(0) as u16;
    let width = (longest + 2).min(input.width);
    let before = &state.editor.before_cursor()[..p.start];
    let col = before[before.rfind('\n').map_or(0, |i| i + 1)..].chars().count() as u16;
    let x = (input.x + 2 + col).min(input.x + input.width - width);
    let area = Rect::new(x, input.y - height, width, height);
    let first = p.selected.saturating_sub(rows - 1);
    let lines: Vec<Line> = p
        .matches
        .iter()
        .enumerate()
        .skip(first)
        .take(rows)
        .map(|(i, m)| {
            let style =
                if i == p.selected { Style::default().add_modifier(Modifier::REVERSED) } else { Style::default() };
            Line::from(Span::styled(clean(m), style))
        })
        .collect();
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().border_style(tone_style(Tone::Dim, state.color))),
        area,
    );
}

fn draw_status_bar(frame: &mut Frame, area: Rect, session: &Session, state: &TuiState, now: Instant) {
    let line = if let Some(flash) = state.flash_text(now) {
        Line::from(Span::styled(flash.to_string(), tone_style(Tone::Bad, state.color)))
    } else if state.scroll.is_scrolled() {
        Line::from(Span::styled(
            format!("↓ {} new · Esc: back to latest", state.scroll.unseen()),
            tone_style(Tone::Dim, state.color),
        ))
    } else {
        let me = session.store.me().unwrap_or("…");
        let online = session.store.online().count();
        Line::from(Span::styled(
            format!("@{me} · {online} online · Alt+Enter: new line · /help"),
            tone_style(Tone::Dim, state.color),
        ))
    };
    frame.render_widget(Paragraph::new(line), area);
}

pub fn draw(frame: &mut Frame, session: &Session, state: &mut TuiState, now: Instant) {
    let area = frame.area();
    state.clear_hits();
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        state.popup = None;
        frame.render_widget(Paragraph::new("terminal too small, make it bigger"), area);
        return;
    }
    let input_rows =
        input_layout(&state.editor, (area.width as usize).saturating_sub(2)).0.len().clamp(1, MAX_INPUT_ROWS) as u16;
    let [tabs_a, body_a, sep_a, input_a, status_a] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(input_rows),
        Constraint::Length(1),
    ])
    .areas(area);
    let (msgs_a, side_a) = if area.width >= SIDEBAR_MIN_WIDTH {
        let [m, s] = Layout::horizontal([Constraint::Min(1), Constraint::Length(SIDEBAR_WIDTH)]).areas(body_a);
        (m, Some(s))
    } else {
        (body_a, None)
    };

    draw_tabs(frame, tabs_a, session, state);

    let width = msgs_a.width as usize;
    let rows = match (state.on_status, session.store.current()) {
        (false, Some(conv)) => conversation_rows(session, conv, width, state.color),
        _ => status_rows(state, width),
    };
    let height = msgs_a.height as usize;
    let top = state.scroll.top(rows.len(), height);
    state.view = (rows.len(), height);
    let visible: Vec<Line> = rows.into_iter().skip(top).take(height).collect();
    frame.render_widget(Paragraph::new(visible), msgs_a);

    if let Some(side) = side_a {
        draw_sidebar(frame, side, session, state);
    }
    frame.render_widget(
        Paragraph::new("─".repeat(sep_a.width as usize)).style(tone_style(Tone::Dim, state.color)),
        sep_a,
    );
    draw_input(frame, input_a, state);
    draw_popup(frame, input_a, state);
    draw_status_bar(frame, status_a, session, state, now);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Event, PeerId};
    use crate::ui::render::Style;
    use crate::ui::session::Mode;
    use crate::ui::tui::state::FLASH_FOR;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::style::Color;

    fn session() -> Session {
        let mut s = Session::new(Style::plain());
        s.set_mode(Mode::Tui);
        s.on_event(&Event::Assigned { nick: "annMoon".into(), renamed: false }, 0);
        s.on_event(&Event::PeerJoined { id: PeerId::test(2), nick: "bobRoger".into() }, 0);
        s.on_event(&Event::PeerJoined { id: PeerId::test(3), nick: "carolFox".into() }, 0);
        s.on_event(&Event::RoomJoined { name: "catPieR".into(), members: vec!["bobRoger".into()] }, 0);
        s
    }

    fn render(s: &Session, state: &mut TuiState, w: u16, h: u16) -> Buffer {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| draw(f, s, state, Instant::now())).unwrap();
        term.backend().buffer().clone()
    }

    fn rows(buf: &Buffer) -> Vec<String> {
        (0..buf.area.height)
            .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>())
            .collect()
    }

    fn find(buf: &Buffer, needle: &str) -> Option<(u16, u16)> {
        rows(buf)
            .iter()
            .enumerate()
            .find_map(|(y, row)| row.find(needle).map(|i| (row[..i].chars().count() as u16, y as u16)))
    }

    #[test]
    fn tabs_show_unread_and_the_corvo_tab_comes_first() {
        let mut s = session();
        s.on_event(
            &Event::Message { conv: Conv::Dm("bobRoger".into()), from: "bobRoger".into(), text: "psst".into(), ts: 0 },
            0,
        );
        s.on_event(
            &Event::Message { conv: Conv::Dm("bobRoger".into()), from: "bobRoger".into(), text: "hey".into(), ts: 0 },
            0,
        );
        let mut state = TuiState::new(true);
        state.select_tab(&mut s, 1);
        let buf = render(&s, &mut state, 80, 20);
        let top = &rows(&buf)[0];
        assert!(top.starts_with(" corvo "), "{top}");
        assert!(top.contains("#catPieR") && top.contains("@bobRoger (2)"), "{top}");
        let (x, y) = find(&buf, "#catPieR").unwrap();
        assert!(buf[(x, y)].modifier.contains(ratatui::style::Modifier::REVERSED), "current tab is reversed");
        assert_eq!(state.hit(x, 0), Some(Hit::Tab(1)));
    }

    #[test]
    fn multi_line_messages_align_under_the_text() {
        let mut s = session();
        let text = "first line\n[14:02] #catPieR @alice: fake\nthis third line is long enough that it has to wrap onto another row";
        s.on_event(
            &Event::Message { conv: Conv::Room("catPieR".into()), from: "bobRoger".into(), text: text.into(), ts: 0 },
            0,
        );
        let mut state = TuiState::new(true);
        state.select_tab(&mut s, 1);
        let buf = render(&s, &mut state, 60, 20);
        let (x0, y0) = find(&buf, "first line").unwrap();
        let (x1, y1) = find(&buf, "[14:02] #catPieR @alice: fake").unwrap();
        let (x2, _) = find(&buf, "this third line").unwrap();
        assert_eq!((x1, y1), (x0, y0 + 1), "continuation starts at the text column");
        assert_eq!(x2, x0);
        let (sx, sy) = find(&buf, "@bobRoger").unwrap();
        assert_eq!(sy, y0);
        assert!(sx < x0);
        assert!(
            rows(&buf).iter().any(|r| r.contains("wrap onto another row") && r.find("wrap").unwrap() >= x0 as usize)
        );
    }

    #[test]
    fn sidebar_lists_members_and_online_people_with_green_dots() {
        let mut s = session();
        let mut state = TuiState::new(true);
        state.select_tab(&mut s, 1);
        let buf = render(&s, &mut state, 80, 20);
        let (x, y) = find(&buf, "● bobRoger").unwrap();
        assert_eq!(buf[(x, y)].fg, Color::Green);
        assert!(find(&buf, "online").is_some());
        assert!(find(&buf, "● carolFox").is_some());
        assert_eq!(state.hit(x + 3, y), Some(Hit::Person("bobRoger".into())));
    }

    #[test]
    fn narrow_terminals_hide_the_sidebar_and_tiny_ones_say_so() {
        let mut s = session();
        let mut state = TuiState::new(true);
        state.select_tab(&mut s, 1);
        assert!(find(&render(&s, &mut state, 69, 20), "● carolFox").is_none());
        let tiny = render(&s, &mut state, 39, 9);
        assert!(find(&tiny, "terminal too small").is_some());
    }

    #[test]
    fn corvo_tab_shows_status_lines_in_their_tone() {
        let s = session();
        let mut state = TuiState::new(true);
        state.push_status(0, "! no room #x with that key", Tone::Bad);
        state.push_status(0, "* 2 peer(s) online:\n  @bobRoger", Tone::Plain);
        let buf = render(&s, &mut state, 80, 20);
        let (x, y) = find(&buf, "! no room").unwrap();
        assert_eq!(buf[(x, y)].fg, Color::Red);
        let (px, py) = find(&buf, "* 2 peer(s) online:").unwrap();
        let (bx, by) = find(&buf, "@bobRoger").unwrap();
        assert_eq!((bx, by), (px + 2, py + 1));
    }

    #[test]
    fn status_bar_flashes_errors_then_shows_who_and_hints() {
        let s = session();
        let mut state = TuiState::new(true);
        let mut term = Terminal::new(TestBackend::new(80, 20)).unwrap();
        let now = Instant::now();
        state.flash("! @ghost is offline", now);
        term.draw(|f| draw(f, &s, &mut state, now)).unwrap();
        let buf = term.backend().buffer().clone();
        let (x, y) = find(&buf, "! @ghost is offline").unwrap();
        assert_eq!(y, 19);
        assert_eq!(buf[(x, y)].fg, Color::Red);
        term.draw(|f| draw(f, &s, &mut state, now + FLASH_FOR)).unwrap();
        let bottom = rows(term.backend().buffer()).pop().unwrap();
        assert!(bottom.contains("@annMoon · 2 online · Alt+Enter: new line · /help"), "{bottom}");
    }

    #[test]
    fn overflowing_tabs_keep_the_current_one_visible() {
        let mut s = session();
        for i in 0..12 {
            s.on_event(&Event::RoomJoined { name: format!("room{i:02}LongNameR"), members: vec![] }, 0);
        }
        let mut state = TuiState::new(true);
        let last = tabs(&s).len() - 1;
        state.select_tab(&mut s, last);
        let top = rows(&render(&s, &mut state, 60, 20))[0].clone();
        assert!(top.contains("#room11LongNameR"), "{top}");
        assert!(top.starts_with('‹'), "{top}");
    }

    #[test]
    fn the_input_grows_to_five_rows_and_keeps_the_cursor_visible() {
        let s = session();
        let mut state = TuiState::new(false);
        state.editor.set((1..=7).map(|i| format!("row{i}")).collect::<Vec<_>>().join("\n"));
        let buf = render(&s, &mut state, 80, 20);
        assert!(find(&buf, "row7").is_some() && find(&buf, "row3").is_some());
        assert!(find(&buf, "row2").is_none(), "only 5 rows show");
        let (_, y7) = find(&buf, "row7").unwrap();
        assert_eq!(y7, 18, "last input row sits above the status bar");
        assert!(rows(&buf)[14].starts_with("  row3"), "continuation rows use a 2-space prompt");
    }

    #[test]
    fn the_completion_list_sits_above_the_input_lined_up_with_the_word() {
        let s = session(); // me: annMoon; online: bobRoger, carolFox
        let mut state = TuiState::new(true);
        state.editor.set("hi @".into());
        state.complete(&crate::ui::complete::Candidates::from_store(&s.store));
        let buf = render(&s, &mut state, 80, 20);
        // Input on row 18; a 2-item list is 4 rows tall (with borders): rows 14–17.
        // "hi " is 3 columns after the 2-column prompt, plus 1 for the border.
        assert_eq!(find(&buf, "@bobRoger"), Some((6, 15)));
        assert_eq!(find(&buf, "@carolFox"), Some((6, 16)));
        assert!(buf[(6, 15)].modifier.contains(Modifier::REVERSED), "the choice is highlighted");
        assert!(!buf[(6, 16)].modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn a_short_terminal_still_shows_the_list_it_is_using() {
        let mut s = session();
        for i in 0..6 {
            s.on_event(&Event::PeerJoined { id: PeerId::test(10 + i), nick: format!("zed{i}Owl") }, 0);
        }
        let mut state = TuiState::new(true);
        state.editor.set("@".into());
        state.complete(&crate::ui::complete::Candidates::from_store(&s.store));
        let buf = render(&s, &mut state, 80, 10);
        assert!(find(&buf, "@bobRoger").is_some(), "{:#?}", rows(&buf));
        assert!(state.popup.is_some());
        render(&s, &mut state, 39, 9);
        assert!(state.popup.is_none(), "a list that can't be shown can't take keys");
    }

    #[test]
    fn mentions_are_bold_yellow_and_counted_on_the_tab() {
        let mut s = session();
        s.store.clear_current();
        let room = Conv::Room("catPieR".into());
        for text in ["ping @annMoon", "plain words"] {
            s.on_event(&Event::Message { conv: room.clone(), from: "bobRoger".into(), text: text.into(), ts: 0 }, 0);
        }
        let mut state = TuiState::new(true);
        let top = rows(&render(&s, &mut state, 80, 20))[0].clone();
        assert!(top.contains("#catPieR (2 @1)"), "{top}");
        state.select_tab(&mut s, 1);
        let buf = render(&s, &mut state, 80, 20);
        let (x, y) = find(&buf, "ping @annMoon").unwrap();
        assert_eq!(buf[(x, y)].fg, Color::Yellow);
        assert!(buf[(x, y)].modifier.contains(Modifier::BOLD));
        let (px, py) = find(&buf, "plain words").unwrap();
        assert_eq!(buf[(px, py)].fg, Color::Reset);
    }

    #[test]
    fn wrapping_prefers_spaces_and_splits_long_words() {
        assert_eq!(wrap("hello there world", 11), vec!["hello there", "world"]);
        assert_eq!(wrap("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
        assert_eq!(wrap("", 5), vec![""]);
    }

    #[test]
    fn long_lines_wrap_in_the_input_and_the_cursor_follows() {
        let s = session();
        let mut state = TuiState::new(false);
        state.editor.set("x".repeat(120));
        let mut term = Terminal::new(TestBackend::new(80, 20)).unwrap();
        term.draw(|f| draw(f, &s, &mut state, Instant::now())).unwrap();
        let r = rows(term.backend().buffer());
        assert_eq!(r[17].trim_end(), format!("> {}", "x".repeat(78)));
        assert_eq!(r[18].trim_end(), format!("  {}", "x".repeat(42)));
        let pos = term.get_cursor_position().unwrap();
        assert_eq!((pos.x, pos.y), (2 + 42, 18));
    }

    fn layout_of(text: &str, width: usize) -> (Vec<String>, (usize, usize)) {
        let mut e = Editor::default();
        e.set(text.into());
        input_layout(&e, width)
    }

    #[test]
    fn input_layout_splits_rows_and_maps_the_cursor() {
        assert_eq!(layout_of("abcdefgh", 4), (vec!["abcd".to_string(), "efgh".into()], (1, 4)));
        assert_eq!(layout_of("ab\ncdefg", 4), (vec!["ab".to_string(), "cdef".into(), "g".into()], (2, 1)));
        assert_eq!(layout_of("", 4), (vec![String::new()], (0, 0)));
    }
}
