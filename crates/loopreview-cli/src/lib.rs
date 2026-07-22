//! loopreview-cli: the terminal UI wired on top of [`loopreview_core`].
//!
//! Both the `loopreview` and `lr` binaries call [`run`]; the interesting layers
//! are the argument surface ([`cli`]), the syntax-highlight layer
//! ([`highlight`]), and the ratatui review UI ([`ui`]).

mod cli;
mod highlight;
mod ui;

use std::io::{self, IsTerminal};
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;

use loopreview_core::{DiffError, DiffSource, RefSource, StdinPatchSource, WorktreeSource, git};

use cli::{Cli, Request};

/// Entry point shared by both binaries: run loopreview, mapping any error to a
/// non-zero exit code after printing it.
pub fn run() -> ExitCode {
    match try_run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("loopreview: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn try_run() -> Result<()> {
    // clap handles `--help` / `--version` (printing and exiting) before this.
    let request = Cli::parse()
        .into_request()
        .map_err(|message| anyhow!(message))?;

    // TTY guard, applied after argument processing: the UI draws to stdout and
    // needs a real terminal there. A piped stdin is fine — that is the patch.
    if !io::stdout().is_terminal() {
        bail!(
            "loopreview needs an interactive terminal to draw the diff — it can't render to a \
             pipe or a file. Run it in a terminal (you can still pipe a patch in: `git diff | lr`)."
        );
    }

    let source = build_source(&request)?;
    let label = source.describe();
    let diff = source
        .load()
        .with_context(|| format!("loading diff ({label})"))?;

    if diff.is_empty() {
        println!("No changes to review.");
        return Ok(());
    }

    ui::run(label, diff)
}

/// Choose the diff source from the request and the environment: an explicit
/// target is a ref comparison; otherwise the working tree when run
/// interactively, or a patch on standard input when one is piped in.
fn build_source(request: &Request) -> Result<Box<dyn DiffSource>> {
    match request {
        Request::Target(target) => Ok(Box::new(RefSource::new(repo_root()?, target.clone()))),
        Request::Default => {
            if io::stdin().is_terminal() {
                Ok(Box::new(WorktreeSource::new(repo_root()?)))
            } else {
                Ok(Box::new(StdinPatchSource::new()))
            }
        }
    }
}

/// The repository root of the current directory, with a friendly error when the
/// directory is not inside a git repository.
fn repo_root() -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("getting the current directory")?;
    git::repo_root(&cwd).map_err(|error| match error {
        DiffError::NotARepository { .. } => {
            anyhow!("not inside a git repository — cd into one, or pipe a patch: `git diff | lr`")
        }
        other => anyhow::Error::new(other),
    })
}
