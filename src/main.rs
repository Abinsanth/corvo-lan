use std::net::SocketAddr;

use anyhow::anyhow;
use clap::Parser;

use corvo_lan::identity;
use corvo_lan::names;
use corvo_lan::node::{self, NodeConfig};
use corvo_lan::ui;
use corvo_lan::update;

/// Serverless peer-to-peer chat for your local network.
#[derive(Parser)]
#[command(version)]
struct Args {
    /// Base of your name, 1-6 letters or digits (default: your login name); the system adds a word
    #[arg(long)]
    nick: Option<String>,
    /// TCP port to listen on (0 = pick a free one)
    #[arg(long, default_value_t = 0)]
    port: u16,
    /// Connect to a peer directly, e.g. 192.168.1.20:54321 (repeatable)
    #[arg(long = "peer", value_name = "IP:PORT")]
    peers: Vec<SocketAddr>,
    /// Write debug logs to ./corvo-lan.log
    #[arg(long)]
    debug: bool,
    /// After an update's restart: come back as this name if it's still free
    #[arg(long, hide = true, value_name = "NAME")]
    resume_as: Option<String>,
    /// Set by an update's restart, so a start never forces two updates in a row
    #[arg(long, hide = true)]
    after_update: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    if args.debug {
        let file = std::fs::File::create("corvo-lan.log")?;
        tracing_subscriber::fmt()
            .with_writer(std::sync::Mutex::new(file))
            .with_ansi(false)
            .with_max_level(tracing::Level::DEBUG)
            .init();
    }

    // A Windows /update leaves the old exe beside the new one until now.
    if cfg!(windows)
        && let Some(dir) = std::env::current_exe().ok().as_deref().and_then(std::path::Path::parent)
    {
        update::remove_old_exe(dir);
    }

    // Noted now: an update replaces the file, and a restart runs it from here.
    let exe = update::running_exe().ok();

    // Windows sets USERNAME rather than USER.
    let login = std::env::var("USER").or_else(|_| std::env::var("USERNAME")).ok();
    let resume = args.resume_as.filter(|name| names::is_valid_name(name));
    let ask_name = args.nick.is_none() && resume.is_none();
    let base = match resume.as_deref().and_then(names::base_of) {
        Some(base) => base.to_string(),
        None => args.nick.map(|n| n.to_ascii_lowercase()).unwrap_or_else(|| names::default_base(login.as_deref())),
    };
    names::validate_base(&base).map_err(|e| anyhow!("invalid --nick {base:?}: {e}"))?;

    // Holds the identity lock until the app exits.
    let identity = identity::load()?;
    let handle = node::start(NodeConfig {
        keys: identity.keys.clone(),
        known_peers: identity.known_peers.clone(),
        notice: identity.notice.clone(),
        base,
        listen: SocketAddr::from(([0, 0, 0, 0], args.port)),
        discovery: true,
        manual_peers: args.peers,
        name_wait: node::NAME_WAIT,
        fixed_word: None,
        prefer: resume,
        ask_name,
    })
    .await?;
    let exit = ui::run(handle, ui::Options { exe: exe.clone(), after_update: args.after_update }).await?;
    if let (ui::Exit::Restart { name }, Some(exe)) = (exit, exe) {
        // The new version takes the identity lock, so let go of it first.
        drop(identity);
        let args = update::restart_args(std::env::args_os().skip(1), name.as_deref());
        update::relaunch(&exe, args).map_err(|e| anyhow!("couldn't restart corvo-lan: {e}"))?;
    }
    Ok(())
}
