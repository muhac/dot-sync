use clap::{ArgGroup, Args, Parser, Subcommand};

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

#[derive(Debug, Args)]
#[command(group = ArgGroup::new("conflict").required(false).multiple(false))]
pub struct SyncCmdFlags {
    #[command(flatten)]
    pub common: SyncFlags,

    /// On conflicting listed fields, keep target's value (default).
    #[arg(long, group = "conflict")]
    pub target_wins: bool,

    /// On conflicting listed fields, overwrite target with source's value.
    #[arg(long, group = "conflict")]
    pub source_wins: bool,

    /// Bail out if any listed field differs between source and target.
    #[arg(long, group = "conflict")]
    pub fail_on_conflict: bool,
}

#[derive(Debug, Args)]
#[command(group = ArgGroup::new("restore_side").required(false).multiple(false))]
#[command(group = ArgGroup::new("restore_pick").required(false).multiple(false))]
pub struct RestoreFlags {
    /// Target name from .sync.yaml.
    pub name: String,

    /// Restore the source file (default: target).
    #[arg(long, group = "restore_side")]
    pub source: bool,

    /// Restore the target file. (default)
    #[arg(long, group = "restore_side")]
    pub target: bool,

    /// Pick by 1-based index from the listed candidates.
    #[arg(long, group = "restore_pick", value_name = "N")]
    pub pick: Option<usize>,

    /// Pick by timestamp prefix (e.g. 20260510 or 20260510-15).
    #[arg(long, group = "restore_pick", value_name = "PREFIX")]
    pub at: Option<String>,

    /// List available snapshots without restoring.
    #[arg(long)]
    pub list: bool,

    /// Show what would be restored without writing.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct AddArgs {
    /// Target name in .sync.yaml. New targets are created; existing
    /// targets get fields appended.
    pub name: String,

    /// Format string written to .sync.yaml. Inferred from --source /
    /// --target extension (.toml / .json / .jsonc) when omitted.
    #[arg(long)]
    pub format: Option<String>,

    /// Path to the managed source fragment. Required for new targets;
    /// optional when appending fields to an existing target.
    #[arg(long)]
    pub source: Option<String>,

    /// Path to the real config file the application reads.
    #[arg(long)]
    pub target: Option<String>,

    /// Sync path to add. Repeat to add multiple. When omitted and
    /// stdin/stdout is a TTY, an interactive picker discovers fields
    /// from the source or target file.
    #[arg(long = "field")]
    pub fields: Vec<String>,

    /// Show what would be written to .sync.yaml without modifying it.
    #[arg(long)]
    pub dry_run: bool,
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
    Sync(#[command(flatten)] SyncCmdFlags),

    /// Restore a previous snapshot of source or target.
    Restore(#[command(flatten)] RestoreFlags),

    /// Add a sync target or fields to .sync.yaml. Bootstraps the file
    /// if it doesn't exist. With --field, runs non-interactively;
    /// without --field on a TTY, opens an interactive tree picker.
    Add(#[command(flatten)] AddArgs),

    /// Print a shell completion script for `dot-sync` to stdout.
    /// Hidden — surfaced via README rather than the main --help.
    #[command(hide = true)]
    Completions {
        /// Shell to generate completions for.
        shell: clap_complete::Shell,
    },

    /// Print the `dot-sync` man page (roff source) to stdout.
    /// Hidden — surfaced via README rather than the main --help.
    #[command(hide = true)]
    Man,
}
