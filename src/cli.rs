use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "ds")]
#[command(about = "Sync selected fields between repo config fragments and app configs")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Pull selected fields from target into source.
    Pull {
        /// Target name from dot.sync.yaml. Omit to process all targets.
        name: Option<String>,

        /// Show planned changes without writing files.
        #[arg(long)]
        dry_run: bool,

        /// Create a timestamped backup before writing.
        #[arg(long)]
        backup: bool,
    },

    /// Push selected fields from source into target.
    Push {
        /// Target name from dot.sync.yaml. Omit to process all targets.
        name: Option<String>,

        /// Show planned changes without writing files.
        #[arg(long)]
        dry_run: bool,

        /// Create a timestamped backup before writing.
        #[arg(long)]
        backup: bool,
    },

    /// Pull from target to source, then fill target from source.
    Sync {
        /// Target name from dot.sync.yaml. Omit to process all targets.
        name: Option<String>,

        /// Show planned changes without writing files.
        #[arg(long)]
        dry_run: bool,

        /// Create a timestamped backup before writing.
        #[arg(long)]
        backup: bool,
    },
}
