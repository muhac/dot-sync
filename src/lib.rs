mod cli;
mod config;
mod document;
mod path;
mod sync;

use anyhow::Result;
use clap::Parser;

use crate::cli::{Cli, Command};
use crate::config::DotSyncConfig;
use crate::sync::{Direction, SyncOptions, run as run_sync};

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    let loaded = DotSyncConfig::load_from_current_dir()?;

    match cli.command {
        Command::Pull {
            name,
            dry_run,
            backup,
        } => run_sync(
            &loaded,
            name.as_deref(),
            Direction::Pull,
            SyncOptions { dry_run, backup },
        ),
        Command::Push {
            name,
            dry_run,
            backup,
        } => run_sync(
            &loaded,
            name.as_deref(),
            Direction::Push,
            SyncOptions { dry_run, backup },
        ),
        Command::Sync {
            name,
            dry_run,
            backup,
        } => run_sync(
            &loaded,
            name.as_deref(),
            Direction::Sync,
            SyncOptions { dry_run, backup },
        ),
    }
}
