use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "dot-sync")]
#[command(about = "Sync selected fields between structured config files")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Args)]
pub struct SyncFlags {
    /// Target name from .sync.yaml. Omit to process all targets.
    pub name: Option<String>,

    /// Show planned changes without writing files.
    #[arg(long)]
    pub dry_run: bool,

    /// Create a timestamped backup before writing.
    #[arg(long)]
    pub backup: bool,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Show configuration and file health without syncing.
    Status {
        /// Target name from .sync.yaml. Omit to inspect all targets.
        name: Option<String>,
    },

    /// Pull selected fields from target into source.
    Pull(#[command(flatten)] SyncFlags),

    /// Push selected fields from source into target.
    Push(#[command(flatten)] SyncFlags),

    /// Reconcile selected fields between source and target in both directions.
    Sync(#[command(flatten)] SyncFlags),
}
