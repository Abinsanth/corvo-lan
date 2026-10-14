use std::net::SocketAddr;

use anyhow::anyhow;
use clap::Parser;

use corvo_lan::identity::Keys;
use corvo_lan::names;
use corvo_lan::node::{self, NodeConfig};
use corvo_lan::ui;
use corvo_lan::update;

/// Serverless peer-to-peer chat for your local network.
#[derive(Parser)]
#[command(version)]
struct Args {
    /// Base of your name, 1-9 letters or digits (default: $USER); the system adds a word
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
    /// Start in line mode instead of the full-screen UI
    #[arg(long)]
    line: bool,
    /// Don't check GitHub for a newer version at startup (or set CORVO_LAN_NO_UPDATE_CHECK=1)
    #[arg(long)]
    no_update_check: bool,
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

    let base = args.nick.or_else(|| std::env::var("USER").ok()).unwrap_or_else(|| "anon".into());
    names::validate_base(&base).map_err(|e| anyhow!("invalid --nick {base:?}: {e}"))?;

    let handle = node::start(NodeConfig {
        keys: Keys::generate(),
        base,
        listen: SocketAddr::from(([0, 0, 0, 0], args.port)),
        discovery: true,
        manual_peers: args.peers,
        name_wait: node::NAME_WAIT,
        fixed_word: None,
    })
    .await?;
    let env_off = update::check_disabled_by(std::env::var("CORVO_LAN_NO_UPDATE_CHECK").ok().as_deref());
    ui::run(handle, ui::Options { line: args.line, check_updates: !args.no_update_check && !env_off }).await
}
