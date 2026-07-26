//! `komet-tui` — the terminal viewport, as a standalone binary.
//!
//! A thin wrapper over [`komet_tui::cli`]: the same run path the shipped
//! `komet tui` subcommand uses. This binary exists for development — it links a
//! terminal backend and nothing else, so it builds in seconds without gpui —
//! but it is not the installed command; `komet tui` is. Keeping both on one
//! shared entry point is what stops them drifting.

use clap::Parser;
use komet_tui::cli::TuiArgs;

#[derive(Parser)]
#[command(
    name = "komet-tui",
    about = "Terminal viewport for komet — attaches to an engine that outlives it"
)]
struct Cli {
    #[command(flatten)]
    tui: TuiArgs,
}

fn main() -> anyhow::Result<()> {
    komet_tui::cli::run(Cli::parse().tui)
}
