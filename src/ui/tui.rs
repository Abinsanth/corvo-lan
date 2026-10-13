//! The full-screen frontend. (Filled in by later tasks.)

use super::session::Mode;
use super::{Ctx, Exit};

/// Until the TUI exists, hand straight back to line mode.
pub async fn run(ctx: &mut Ctx) -> anyhow::Result<Exit> {
    ctx.notify("* the full-screen UI isn't built yet; using line mode");
    Ok(Exit::Switch(Mode::Line))
}

pub fn install_panic_hook() {}
