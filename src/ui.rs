//! The UI: a shared `Session` and two frontends (line mode and the full-screen
//! TUI) that take turns owning the terminal.

pub mod complete;
pub mod input;
pub mod line;
pub mod render;
pub mod session;
pub mod tui;
pub mod updates;

use std::io::IsTerminal;

use tokio::sync::mpsc;

use self::render::{Style, Tone, tone_of_text};
use self::session::{Mode, Reaction, Session};
use self::updates::Updater;
use crate::api::{Command, Event};
use crate::node::NodeHandle;

pub struct Options {
    /// Start in line mode (`--line`).
    pub line: bool,
    pub check_updates: bool,
}

/// Why a frontend gave the terminal back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    Switch(Mode),
    Quit,
}

/// Output from background work (`/update`), shown by whichever frontend is active.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub text: String,
    pub tone: Tone,
}

impl Notice {
    pub fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        Notice { tone: tone_of_text(&text), text }
    }
}

/// Everything the frontends share; it outlives each of their turns.
pub struct Ctx {
    pub session: Session,
    pub commands: mpsc::Sender<Command>,
    pub events: mpsc::UnboundedReceiver<Event>,
    pub notices: mpsc::UnboundedReceiver<Notice>,
    pub notice_tx: mpsc::UnboundedSender<Notice>,
    pub updater: Updater,
}

impl Ctx {
    pub fn notify(&self, text: impl Into<String>) {
        let _ = self.notice_tx.send(Notice::new(text));
    }

    /// Carries out a reaction's commands and update; says whether to leave.
    /// The caller shows `reaction.print` first.
    pub async fn act(&mut self, reaction: Reaction) -> anyhow::Result<Option<Exit>> {
        if reaction.quit {
            let _ = self.commands.send(Command::Quit).await;
            return Ok(Some(Exit::Quit));
        }
        for cmd in reaction.commands {
            self.commands.send(cmd).await?;
        }
        if let Some(action) = reaction.update {
            self.updater.start(action);
        }
        Ok(reaction.switch.map(Exit::Switch))
    }
}

/// Runs the frontends in turn until one quits.
pub async fn mode_loop(
    start: Mode,
    ctx: &mut Ctx,
    mut line: impl AsyncFnMut(&mut Ctx) -> anyhow::Result<Exit>,
    mut tui: impl AsyncFnMut(&mut Ctx) -> anyhow::Result<Exit>,
) -> anyhow::Result<()> {
    let mut mode = start;
    loop {
        ctx.session.set_mode(mode);
        let result = match mode {
            Mode::Line => line(ctx).await,
            Mode::Tui => tui(ctx).await,
        };
        let exit = match result {
            Ok(exit) => exit,
            Err(e) => {
                // Without this the node never stops and the process hangs, still on the LAN.
                let _ = ctx.commands.send(Command::Quit).await;
                return Err(e);
            }
        };
        match exit {
            Exit::Quit => return Ok(()),
            Exit::Switch(next) => mode = next,
        }
    }
}

/// Runs the UI until the user quits, then waits for the Node to say goodbye.
pub async fn run(handle: NodeHandle, opts: Options) -> anyhow::Result<()> {
    let NodeHandle { commands, events, task, .. } = handle;
    tui::install_panic_hook();
    let (notice_tx, notices) = mpsc::unbounded_channel();
    let updater = Updater::new(notice_tx.clone());
    if opts.check_updates {
        updater.check_at_startup();
    }
    let style = Style::detect();
    let start = if opts.line || !std::io::stdout().is_terminal() { Mode::Line } else { Mode::Tui };
    let mut ctx = Ctx { session: Session::new(style), commands, events, notices, notice_tx, updater };
    let mut line = line::LineFrontend::new(style, start == Mode::Tui)?;
    let result = mode_loop(start, &mut ctx, async |ctx: &mut Ctx| line.run(ctx).await, tui::run).await;
    line.shutdown();
    let _ = task.await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::PeerId;

    fn ctx() -> (Ctx, mpsc::UnboundedSender<Event>, mpsc::Receiver<Command>) {
        let (commands, cmd_rx) = mpsc::channel(8);
        let (event_tx, events) = mpsc::unbounded_channel();
        let (notice_tx, notices) = mpsc::unbounded_channel();
        let ctx = Ctx {
            session: Session::new(Style::plain()),
            commands,
            events,
            notices,
            notice_tx: notice_tx.clone(),
            updater: Updater::new(notice_tx),
        };
        (ctx, event_tx, cmd_rx)
    }

    fn joined(n: u64) -> Event {
        Event::PeerJoined { id: PeerId::test(n), nick: format!("p{n}Fox") }
    }

    #[tokio::test]
    async fn mode_loop_hands_events_and_notices_across_switches() {
        let (mut ctx, event_tx, _cmd_rx) = ctx();
        for n in 1..=4 {
            event_tx.send(joined(n)).unwrap();
        }
        ctx.notify("* first");
        let seen = std::cell::RefCell::new(Vec::<String>::new());
        let mut line_runs = 0;
        // Each frontend consumes one event and one notice if there is one, then hands over.
        let step = async |ctx: &mut Ctx, who: &str| {
            let e = ctx.events.recv().await.unwrap();
            ctx.session.on_event(&e, 0);
            if let Ok(n) = ctx.notices.try_recv() {
                seen.borrow_mut().push(format!("{who}:{}", n.text));
            }
        };
        mode_loop(
            Mode::Line,
            &mut ctx,
            async |ctx: &mut Ctx| {
                assert_eq!(ctx.session.mode(), Mode::Line);
                ctx.notify("* second");
                step(ctx, "line").await;
                line_runs += 1;
                Ok(if line_runs == 1 { Exit::Switch(Mode::Tui) } else { Exit::Quit })
            },
            async |ctx: &mut Ctx| {
                assert_eq!(ctx.session.mode(), Mode::Tui);
                step(ctx, "tui").await;
                step(ctx, "tui").await;
                Ok(Exit::Switch(Mode::Line))
            },
        )
        .await
        .unwrap();
        assert_eq!(line_runs, 2);
        for n in 1..=4 {
            assert!(ctx.session.store.is_online(&format!("p{n}Fox")), "event {n} lost");
        }
        assert_eq!(*seen.borrow(), vec!["line:* first", "tui:* second", "line:* second"]);
    }

    #[tokio::test]
    async fn act_sends_commands_and_reports_quit_or_switch() {
        let (mut ctx, _event_tx, mut cmd_rx) = ctx();
        let quit = Reaction { quit: true, ..Default::default() };
        assert_eq!(ctx.act(quit).await.unwrap(), Some(Exit::Quit));
        assert_eq!(cmd_rx.recv().await, Some(Command::Quit));
        let switch = Reaction { commands: vec![Command::ListPeers], switch: Some(Mode::Tui), ..Default::default() };
        assert_eq!(ctx.act(switch).await.unwrap(), Some(Exit::Switch(Mode::Tui)));
        assert_eq!(cmd_rx.recv().await, Some(Command::ListPeers));
        assert_eq!(Notice::new("! boom").tone, Tone::Bad);
    }

    #[tokio::test]
    async fn a_failing_frontend_still_stops_the_node() {
        let (mut ctx, _event_tx, mut cmd_rx) = ctx();
        let r = mode_loop(
            Mode::Line,
            &mut ctx,
            async |_: &mut Ctx| Err(anyhow::anyhow!("boom")),
            async |_: &mut Ctx| Ok(Exit::Quit),
        )
        .await;
        assert!(r.is_err());
        let sent = tokio::time::timeout(std::time::Duration::from_secs(2), cmd_rx.recv()).await;
        assert_eq!(sent, Ok(Some(Command::Quit)));
    }
}
