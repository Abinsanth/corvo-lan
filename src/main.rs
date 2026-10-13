use std::net::SocketAddr;

use anyhow::anyhow;
use clap::Parser;

use lan_chat::names;
use lan_chat::node::{self, NodeConfig};
use lan_chat::ui;

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
    /// Write debug logs to ./lan-chat.log
    #[arg(long)]
    debug: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    if args.debug {
        let file = std::fs::File::create("lan-chat.log")?;
        tracing_subscriber::fmt()
            .with_writer(std::sync::Mutex::new(file))
            .with_ansi(false)
            .with_max_level(tracing::Level::DEBUG)
            .init();
    }

    let base = args.nick.or_else(|| std::env::var("USER").ok()).unwrap_or_else(|| "anon".into());
    names::validate_base(&base).map_err(|e| anyhow!("invalid --nick {base:?}: {e}"))?;

    let handle = node::start(NodeConfig {
        base,
        listen: SocketAddr::from(([0, 0, 0, 0], args.port)),
        discovery: true,
        manual_peers: args.peers,
        name_wait: node::NAME_WAIT,
        fixed_word: None,
    })
    .await?;
    ui::line::run(handle, true).await
}
