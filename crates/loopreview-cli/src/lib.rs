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
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;

use loopreview_core::{
    DiffError, DiffSource, FilePatchSource, RefSource, StdinPatchSource, WorktreeSource, git,
};

use cli::{Action, Cli};

/// A diff source usable from the watch thread.
type SharedSource = Arc<dyn DiffSource + Send + Sync>;

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
    let (action, no_watch) = Cli::parse().resolve();

    // A reserved verb is a usage error; report it regardless of the terminal.
    if let Action::NotYet(message) = action {
        bail!("{message}");
    }

    // TTY guard, applied after argument processing: the UI draws to stdout and
    // needs a real terminal there. A piped stdin is fine — it can be the patch.
    if !io::stdout().is_terminal() {
        bail!(
            "loopreview needs an interactive terminal to draw the diff — it can't render to a \
             pipe or a file. Run it in a terminal (you can still pipe a patch in: `git diff | lr`)."
        );
    }

    let (source, live) = build_source(action)?;
    let label = source.describe();
    let diff = source
        .load()
        .with_context(|| format!("loading diff ({label})"))?;

    // Live sources (working tree, ref) auto-refresh unless disabled. A watched
    // session opens even when currently empty so changes appear as they land.
    let watch = live && !no_watch;
    if !watch && diff.is_empty() {
        println!("No changes to review.");
        return Ok(());
    }

    ui::run(label, diff, source, watch)
}

/// Choose the diff source from the resolved action and the environment, and
/// report whether it is a live source (one worth watching).
fn build_source(action: Action) -> Result<(SharedSource, bool)> {
    match action {
        // Bare `lr`: a piped patch when stdin is redirected, else the worktree.
        Action::Dispatch => {
            if io::stdin().is_terminal() {
                Ok((Arc::new(WorktreeSource::new(repo_root()?)), true))
            } else {
                Ok((Arc::new(StdinPatchSource::new()), false))
            }
        }
        Action::Worktree { staged, pathspec } => {
            reject_piped_stdin_for_diff()?;
            let source = WorktreeSource::new(repo_root()?)
                .staged(staged)
                .pathspec(pathspec);
            Ok((Arc::new(source), true))
        }
        Action::Ref { target, pathspec } => {
            reject_piped_stdin_for_diff()?;
            let source = RefSource::new(repo_root()?, target).pathspec(pathspec);
            Ok((Arc::new(source), true))
        }
        Action::PatchFile(path) => Ok((Arc::new(FilePatchSource::new(path)), false)),
        Action::PatchStdin => {
            if io::stdin().is_terminal() {
                bail!(
                    "no patch on standard input — pass a file (`lr patch <file>`) or pipe one in \
                     (`git diff | lr patch`)."
                );
            }
            Ok((Arc::new(StdinPatchSource::new()), false))
        }
        // Handled in try_run before the terminal is touched.
        Action::NotYet(_) => unreachable!("reserved verbs are reported earlier"),
    }
}

/// `lr diff` reviews VCS changes and deliberately ignores stdin; guide the user
/// who piped a patch into it toward the sugar or `lr patch`.
fn reject_piped_stdin_for_diff() -> Result<()> {
    if !io::stdin().is_terminal() {
        bail!(
            "`lr diff` reviews VCS changes and does not read stdin. To review a piped patch, use \
             `lr` or `lr patch`."
        );
    }
    Ok(())
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
