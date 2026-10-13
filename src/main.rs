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
    /// Your display name (default: $USER)
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

    let nick = args
        .nick
        .or_else(|| std::env::var("USER").ok())
        .unwrap_or_else(|| "anon".into());
    names::validate_nick(&nick).map_err(|e| anyhow!("invalid nick {nick:?}: {e} (pass --nick)"))?;

    let handle = node::start(NodeConfig {
        nick,
        listen: SocketAddr::from(([0, 0, 0, 0], args.port)),
        discovery: true,
        manual_peers: args.peers,
    })
    .await?;
    ui::line::run(handle).await
}
