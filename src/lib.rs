mod cli;
mod config;
mod document;
mod path;
mod status;
mod sync;

use anyhow::Result;
use clap::Parser;

use crate::cli::{Cli, Command, SyncCmdFlags, SyncFlags};
use crate::config::DotSyncConfig;
use crate::status::run as run_status;
use crate::sync::{ConflictMode, Direction, SyncOptions, run as run_sync};

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    let loaded = DotSyncConfig::load_from_current_dir()?;

    match cli.command {
        Command::Status { name } => run_status(&loaded, name.as_deref()),
        Command::Pull(flags) => {
            dispatch_sync(&loaded, Direction::Pull, flags, ConflictMode::TargetWins)
        }
        Command::Push(flags) => {
            dispatch_sync(&loaded, Direction::Push, flags, ConflictMode::SourceWins)
        }
        Command::Sync(cmd) => {
            let mode = resolve_conflict_mode(&cmd);
            dispatch_sync(&loaded, Direction::Sync, cmd.common, mode)
        }
    }
}

fn resolve_conflict_mode(cmd: &SyncCmdFlags) -> ConflictMode {
    if cmd.fail_on_conflict {
        ConflictMode::FailOnConflict
    } else if cmd.source_wins {
        ConflictMode::SourceWins
    } else {
        // target_wins is the default; clap's ArgGroup makes the trio mutually exclusive.
        ConflictMode::TargetWins
    }
}

fn dispatch_sync(
    config: &DotSyncConfig,
    direction: Direction,
    flags: SyncFlags,
    conflict: ConflictMode,
) -> Result<()> {
    let SyncFlags {
        name,
        dry_run,
        backup,
    } = flags;
    run_sync(
        config,
        name.as_deref(),
        direction,
        SyncOptions {
            dry_run,
            backup,
            conflict,
        },
    )
}
