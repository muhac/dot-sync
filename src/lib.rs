mod cli;
mod config;
mod document;
mod path;
mod status;
mod sync;

use anyhow::Result;
use clap::Parser;

use crate::cli::{Cli, Command, SyncFlags};
use crate::config::DotSyncConfig;
use crate::status::run as run_status;
use crate::sync::{Direction, SyncOptions, run as run_sync};

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    let loaded = DotSyncConfig::load_from_current_dir()?;

    match cli.command {
        Command::Status { name } => run_status(&loaded, name.as_deref()),
        Command::Pull(flags) => dispatch_sync(&loaded, Direction::Pull, flags),
        Command::Push(flags) => dispatch_sync(&loaded, Direction::Push, flags),
        Command::Sync(flags) => dispatch_sync(&loaded, Direction::Sync, flags),
    }
}

fn dispatch_sync(config: &DotSyncConfig, direction: Direction, flags: SyncFlags) -> Result<()> {
    let SyncFlags {
        name,
        dry_run,
        backup,
    } = flags;
    run_sync(
        config,
        name.as_deref(),
        direction,
        SyncOptions { dry_run, backup },
    )
}
