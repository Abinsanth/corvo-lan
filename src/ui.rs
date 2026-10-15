//! The UI: a shared `Session` and the full-screen TUI.

pub mod clip;
pub mod complete;
pub mod input;
pub mod render;
pub mod session;
pub mod tui;
pub mod updates;

use std::io::IsTerminal;
use std::path::PathBuf;

use tokio::sync::mpsc;

use self::render::{Style, Tone, tone_of_text};
use self::session::{Reaction, Session};
use self::updates::Updater;
use crate::api::{Command, Event};
use crate::discovery;
use crate::node::NodeHandle;
use crate::update::Version;

pub struct Options {
    /// This binary, captured at startup: an update replaces it, and a restart runs it.
    pub exe: Option<PathBuf>,
}

/// How the UI ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exit {
    Quit,
    /// Start the newly installed version, keeping `name` if there was one.
    Restart {
        name: Option<String>,
    },
}

/// Output from background work (`/update`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    /// A line for the corvo tab.
    Line { text: String, tone: Tone },
    /// How far a download got; replaces the previous progress line.
    Progress(String),
    /// `version` can be installed: ask whether to.
    Offer(Version),
    /// `version` is installed. `forced` restarts at once; otherwise ask.
    Installed { version: Version, forced: bool },
}

impl Notice {
    pub fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        Notice::Line { tone: tone_of_text(&text), text }
    }
}

/// Everything the UI works with.
pub struct Ctx {
    pub session: Session,
    pub commands: mpsc::Sender<Command>,
    pub events: mpsc::UnboundedReceiver<Event>,
    pub notices: mpsc::UnboundedReceiver<Notice>,
    pub notice_tx: mpsc::UnboundedSender<Notice>,
    pub updater: Updater,
    /// Our LAN address for the status bar.
    pub lan: String,
    /// The user chose to restart into a newly installed version.
    pub restart: bool,
}

impl Ctx {
    pub fn notify(&self, text: impl Into<String>) {
        let _ = self.notice_tx.send(Notice::new(text));
    }

    /// Carries out a reaction's commands and update; says whether to quit.
    /// The caller shows `reaction.print` first.
    pub async fn act(&mut self, reaction: Reaction) -> anyhow::Result<bool> {
        if reaction.quit || reaction.restart {
            self.restart |= reaction.restart;
            let _ = self.commands.send(Command::Quit).await;
            return Ok(true);
        }
        for cmd in reaction.commands {
            self.commands.send(cmd).await?;
        }
        if let Some(action) = reaction.update {
            self.updater.start(action);
        }
        Ok(false)
    }
}

/// Runs `ui` if there's a terminal. Whatever happens, the node is told to quit,
/// so the process never stays on the LAN without a UI. `tui::run` words its own
/// setup errors ("corvo-lan needs a terminal (...)"), so only the no-terminal
/// case is worded here.
async fn run_ui(
    ctx: &mut Ctx,
    is_terminal: bool,
    ui: impl AsyncFnOnce(&mut Ctx) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let result = if is_terminal {
        ui(ctx).await
    } else {
        Err(anyhow::anyhow!("corvo-lan needs a terminal (output isn't a terminal)"))
    };
    if result.is_err() {
        let _ = ctx.commands.send(Command::Quit).await;
    }
    result
}

/// Runs the full-screen UI until the user quits, then waits for the Node to say goodbye.
pub async fn run(handle: NodeHandle, opts: Options) -> anyhow::Result<Exit> {
    let NodeHandle { commands, events, task, local_addr, .. } = handle;
    let lan = tui::view::lan_label(discovery::lan_ip(), local_addr.port());
    tui::install_panic_hook();
    let (notice_tx, notices) = mpsc::unbounded_channel();
    let updater = Updater::new(notice_tx.clone(), opts.exe);
    updater.check_at_startup();
    let session = Session::new(Style::detect().color);
    let mut ctx = Ctx { session, commands, events, notices, notice_tx, updater, lan, restart: false };
    let result = run_ui(&mut ctx, std::io::stdout().is_terminal(), tui::run).await;
    let _ = task.await;
    result?;
    Ok(if ctx.restart { Exit::Restart { name: ctx.session.store.me().map(str::to_string) } } else { Exit::Quit })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The next command the UI sent, or `None` after 2s, so a missing Quit fails instead of hanging.
    async fn next(cmd_rx: &mut mpsc::Receiver<Command>) -> Option<Command> {
        tokio::time::timeout(std::time::Duration::from_secs(2), cmd_rx.recv()).await.ok().flatten()
    }

    fn ctx() -> (Ctx, mpsc::UnboundedSender<Event>, mpsc::Receiver<Command>) {
        let (commands, cmd_rx) = mpsc::channel(8);
        let (event_tx, events) = mpsc::unbounded_channel();
        let (notice_tx, notices) = mpsc::unbounded_channel();
        let ctx = Ctx {
            session: Session::new(false),
            commands,
            events,
            notices,
            notice_tx: notice_tx.clone(),
            updater: Updater::new(notice_tx, None),
            lan: String::new(),
            restart: false,
        };
        (ctx, event_tx, cmd_rx)
    }

    #[tokio::test]
    async fn a_restart_quits_and_is_remembered() {
        let (mut ctx, _event_tx, mut cmd_rx) = ctx();
        assert!(ctx.act(Reaction { restart: true, ..Default::default() }).await.unwrap());
        assert!(ctx.restart);
        assert_eq!(cmd_rx.recv().await, Some(Command::Quit));
    }

    #[tokio::test]
    async fn act_sends_commands_and_reports_quit() {
        let (mut ctx, _event_tx, mut cmd_rx) = ctx();
        let quit = Reaction { quit: true, ..Default::default() };
        assert!(ctx.act(quit).await.unwrap());
        assert_eq!(cmd_rx.recv().await, Some(Command::Quit));
        assert!(!ctx.act(Reaction::send(Command::ListPeers)).await.unwrap());
        assert_eq!(cmd_rx.recv().await, Some(Command::ListPeers));
        assert_eq!(Notice::new("! boom"), Notice::Line { text: "! boom".into(), tone: Tone::Bad });
    }

    #[tokio::test]
    async fn no_terminal_stops_the_node() {
        let (mut ctx, _event_tx, mut cmd_rx) = ctx();
        let r = run_ui(&mut ctx, false, async |_: &mut Ctx| panic!("no UI without a terminal")).await;
        assert_eq!(r.unwrap_err().to_string(), "corvo-lan needs a terminal (output isn't a terminal)");
        assert_eq!(next(&mut cmd_rx).await, Some(Command::Quit));
    }

    #[tokio::test]
    async fn a_failing_ui_still_stops_the_node() {
        let (mut ctx, _event_tx, mut cmd_rx) = ctx();
        let r = run_ui(&mut ctx, true, async |_: &mut Ctx| Err(anyhow::anyhow!("boom"))).await;
        assert!(r.is_err());
        assert_eq!(next(&mut cmd_rx).await, Some(Command::Quit));
    }
}
