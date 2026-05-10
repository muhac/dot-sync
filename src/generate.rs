//! Hidden `completions` + `man` subcommands. Both reflect on the clap
//! `Cli::command()` tree to produce shell completions or a roff man
//! page; output goes to stdout so the user can pipe to wherever
//! distros expect it.
//!
//! These run before any `.sync.yaml` lookup — they generate from the
//! static CLI definition only.

use std::io;

use anyhow::{Context, Result};
use clap::CommandFactory;
use clap_complete::Shell;

use crate::cli::Cli;

const BIN_NAME: &str = "dot-sync";

pub fn completions(shell: Shell) -> Result<()> {
    let mut cmd = Cli::command();
    clap_complete::generate(shell, &mut cmd, BIN_NAME, &mut io::stdout());
    Ok(())
}

pub fn man() -> Result<()> {
    let cmd = Cli::command();
    let m = clap_mangen::Man::new(cmd);
    let mut buf: Vec<u8> = Vec::new();
    m.render(&mut buf)
        .context("failed to render man page roff source")?;
    use std::io::Write;
    io::stdout()
        .write_all(&buf)
        .context("failed to write man page to stdout")?;
    Ok(())
}
