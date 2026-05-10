mod add;
mod cli;
mod config;
mod discovery;
mod document;
mod path;
mod picker;
mod picker_state;
mod restore;
mod status;
mod sync;

use anyhow::Result;
use clap::Parser;

use crate::add::run as run_add;
use crate::cli::{Cli, Command, RestoreFlags, SyncCmdFlags, SyncFlags};
use crate::config::DotSyncConfig;
use crate::restore::{Pick, RestoreOptions, Side, run as run_restore};
use crate::status::run as run_status;
use crate::sync::{ConflictMode, Direction, SyncOptions, run as run_sync};

pub fn run() -> Result<()> {
    let cli = Cli::parse();

    // `add` is the only command that runs without an existing
    // `.sync.yaml` — it bootstraps the file when missing. Everything
    // else loads the config first.
    if let Command::Add(args) = cli.command {
        return run_add(args);
    }

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
        Command::Restore(flags) => dispatch_restore(&loaded, flags),
        Command::Add(_) => unreachable!("Add handled before config load"),
    }
}

fn dispatch_restore(config: &DotSyncConfig, flags: RestoreFlags) -> Result<()> {
    let side = if flags.source {
        Side::Source
    } else {
        // --target is the default; explicit --target also lands here.
        Side::Target
    };
    let pick = if let Some(n) = flags.pick {
        Pick::Index(n)
    } else if flags.at.is_some() {
        Pick::AtPrefix
    } else {
        Pick::Newest
    };
    run_restore(
        config,
        Some(&flags.name),
        RestoreOptions {
            side,
            pick,
            at: flags.at.as_deref(),
            list_only: flags.list,
            dry_run: flags.dry_run,
        },
    )
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
