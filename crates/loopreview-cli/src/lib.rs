//! loopreview-cli: the terminal UI wired on top of [`loopreview_core`].
//!
//! Both the `loopreview` and `lr` binaries call [`run`]; the interesting layers
//! are the argument surface ([`cli`]), the syntax-highlight layer
//! ([`highlight`]), and the ratatui review UI ([`ui`]).

mod cli;
mod highlight;
mod store;
mod textarea;
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

use cli::{Action, Cli, Invocation, LayoutMode};

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
    let Invocation {
        action,
        no_watch,
        exclude_untracked,
        mode,
    } = Cli::parse().resolve();

    // A reserved verb is a usage error; report it regardless of the terminal.
    if let Action::NotYet(message) = &action {
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

    let (source, repo_dir) = build_source(action, exclude_untracked)?;
    let label = source.describe();
    let diff = source
        .load()
        .with_context(|| format!("loading diff ({label})"))?;

    // Live sources (working tree, ref) auto-refresh unless disabled; the watch
    // root is the repo directory. A watched session opens even when currently
    // empty so changes appear as they land.
    let watch_root = if no_watch { None } else { repo_dir.clone() };
    if watch_root.is_none() && diff.is_empty() {
        println!("No changes to review.");
        return Ok(());
    }

    // Review store + author, for git-backed sources. The store is keyed by the
    // shared git directory so a repo's worktrees share one review.
    let store = repo_dir
        .as_deref()
        .and_then(git::common_dir)
        .and_then(|common| store::Store::for_repo(&common));
    let review = store
        .as_ref()
        .map(store::Store::load)
        .transpose()?
        .unwrap_or_default();
    let author = repo_dir
        .as_deref()
        .and_then(|dir| git::config(dir, "user.name"))
        .unwrap_or_else(|| "you".to_string());

    ui::run(ui::Session {
        label,
        diff,
        source,
        watch_root,
        mode: layout_mode(mode),
        review,
        store,
        author,
    })
}

/// Map the CLI layout choice to the UI's layout mode.
fn layout_mode(mode: LayoutMode) -> ui::Mode {
    match mode {
        LayoutMode::Auto => ui::Mode::Auto,
        LayoutMode::Unified => ui::Mode::Unified,
        LayoutMode::Split => ui::Mode::SideBySide,
    }
}

/// Choose the diff source from the resolved action and the environment. Returns
/// the directory to watch for a live source, or `None` for a static one.
fn build_source(
    action: Action,
    exclude_untracked: bool,
) -> Result<(SharedSource, Option<PathBuf>)> {
    match action {
        // Bare `lr`: a piped patch when stdin is redirected, else the worktree.
        Action::Dispatch => {
            if io::stdin().is_terminal() {
                let root = repo_root()?;
                let source =
                    WorktreeSource::new(root.clone()).include_untracked(!exclude_untracked);
                Ok((Arc::new(source), Some(root)))
            } else {
                Ok((Arc::new(StdinPatchSource::new()), None))
            }
        }
        Action::Worktree { staged, pathspec } => {
            reject_piped_stdin_for_diff()?;
            let root = repo_root()?;
            let source = WorktreeSource::new(root.clone())
                .staged(staged)
                .pathspec(pathspec)
                .include_untracked(!exclude_untracked);
            Ok((Arc::new(source), Some(root)))
        }
        Action::Ref { target, pathspec } => {
            reject_piped_stdin_for_diff()?;
            let root = repo_root()?;
            let source = RefSource::new(root.clone(), target).pathspec(pathspec);
            Ok((Arc::new(source), Some(root)))
        }
        Action::PatchFile(path) => Ok((Arc::new(FilePatchSource::new(path)), None)),
        Action::PatchStdin => {
            if io::stdin().is_terminal() {
                bail!(
                    "no patch on standard input — pass a file (`lr patch <file>`) or pipe one in \
                     (`git diff | lr patch`)."
                );
            }
            Ok((Arc::new(StdinPatchSource::new()), None))
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
