//! loopreview-cli: the terminal UI wired on top of [`loopreview_core`].
//!
//! Both the `loopreview` and `lr` binaries call [`run`]; the interesting layers
//! are the argument surface ([`cli`]), the syntax-highlight layer
//! ([`highlight`]), and the ratatui review UI ([`ui`]).

mod cli;
mod config;
mod control;
mod highlight;
mod keys;
mod markdown;
mod opener;
mod palette;
mod prsync;
mod session_cli;
mod store;
mod textarea;
mod ui;
mod update;

use std::io::{self, IsTerminal};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;

use loopreview_core::{
    DiffError, DiffSource, FilePatchSource, RefSource, ShowSource, StdinPatchSource,
    WorktreeSource, git,
};

use cli::{Action, Cli, Dispatch, Invocation, LayoutMode};

/// A diff source usable from the watch thread.
type SharedSource = Arc<dyn DiffSource + Send + Sync>;

/// Entry point shared by both binaries: run loopreview, mapping any error to a
/// non-zero exit code after printing it.
pub fn run() -> ExitCode {
    update::cleanup_stale_windows(); // Windows: sweep a prior update's `.old` binaries (a no-op elsewhere).
    match try_run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("loopreview: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn try_run() -> Result<()> {
    // The `pr` / `issue` verbs are gone, but `args_conflicts_with_subcommands`
    // makes `lr pr 123` an opaque clap conflict error before any app code runs.
    // Intercept that muscle memory in raw argv, ahead of clap, and point at the
    // new grammar. (`args_os` → lossy keeps a non-UTF-8 arg from panicking; clap
    // reports it in its own terms just after.)
    let argv: Vec<String> = std::env::args_os()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    if let Some(message) = cli::removed_verb_hint(&argv) {
        bail!("{message}");
    }

    // clap handles `--help` / `--version` (printing and exiting) before this.
    // The control-plane verbs run headless (an agent has no terminal); only the
    // review UI needs a TTY, so split them off before the TTY guard.
    let Invocation {
        action,
        no_watch,
        exclude_untracked,
        mode,
    } = match Cli::parse().dispatch() {
        Dispatch::Session(args) => return session_cli::run(args),
        Dispatch::Update { check } => return update::run(check),
        Dispatch::Tui(invocation) => invocation,
    };

    // A reserved verb or an unrecognized argument is a usage error; report it
    // regardless of the terminal.
    match &action {
        Action::NotYet(message) => bail!("{message}"),
        Action::Invalid(message) => bail!("{message}"),
        _ => {}
    }

    // TTY guard, applied after argument processing: the UI draws to stdout and
    // needs a real terminal there. A piped stdin is fine — it can be the patch.
    if !io::stdout().is_terminal() {
        bail!(
            "loopreview needs an interactive terminal to draw the diff — it can't render to a \
             pipe or a file. Run it in a terminal (you can still pipe a patch in: `git diff | lr`)."
        );
    }

    // A pull-request review loads its diff and comments over the network, so it
    // opens on a spinner and fetches on a background thread. A piped patch and a
    // pull-request reference don't mix — refuse rather than silently ignore one.
    if let Action::Pr { query, detect } = action {
        if !io::stdin().is_terminal() {
            bail!(
                "a piped patch and a pull-request reference don't mix — drop one (review the patch \
                 with `lr patch`, or open the pull request without a pipe)."
            );
        }
        return run_pr(query, detect, mode);
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
    // A corrupt or unreadable store must not stop plain diff viewing: recover by
    // moving the bad file aside and starting fresh, then warn in the status line.
    let (review, notice) = match store.as_ref() {
        Some(store) => {
            let (review, backup) = store.load_or_recover();
            let notice = backup.map(|path| {
                format!(
                    "review store was unreadable — backed it up to {} and started fresh",
                    path.display()
                )
            });
            (review, notice)
        }
        None => (loopreview_core::Review::default(), None),
    };
    let author = repo_dir
        .as_deref()
        .and_then(|dir| git::config(dir, "user.name"))
        .unwrap_or_else(|| "you".to_string());

    let (cfg, keymap, cfg_notice) = load_config()?;
    ui::run(ui::Session {
        label,
        diff,
        source,
        watch_root,
        mode: layout_mode(mode),
        review,
        store,
        author,
        split_min_width: cfg.split_min_width,
        auto_collapse_files: cfg.auto_collapse_files,
        auto_collapse_lines: cfg.auto_collapse_lines,
        sidebar_mode: cfg.sidebar,
        sidebar_min_content: cfg.sidebar_min_content,
        sidebar_width: cfg.sidebar_width,
        composer_enter: cfg.composer_enter,
        keymap,
        repo_dir,
        loader: None,
        notice: notice.or(cfg_notice),
    })
}

/// Load the config and build the key map, failing fast (before the UI opens) on
/// an invalid key binding so the error is legible.
fn load_config() -> Result<(config::Config, keys::Keymap, Option<String>)> {
    let (cfg, notice) = config::Config::load();
    let keymap = keys::Keymap::from_overrides(&cfg.keys).map_err(|errors| {
        anyhow!(
            "invalid key bindings in config.toml:\n  {}",
            errors.join("\n  ")
        )
    })?;
    Ok((cfg, keymap, notice))
}

/// Review a GitHub pull request: resolve, fetch the diff, and pull comments on a
/// background thread while the UI shows a spinner.
fn run_pr(query: Option<String>, detect: bool, mode: LayoutMode) -> Result<()> {
    let dir = repo_root()?;
    let pr_query = prsync::query(query, detect).map_err(|m| anyhow!(m))?;
    let author = git::config(&dir, "user.name").unwrap_or_else(|| "you".to_string());
    let (cfg, keymap, cfg_notice) = load_config()?;

    // The PR's drafts persist in this repo's store; the loader re-attaches them
    // after the pull (the same merge the refresh action uses).
    let common = git::common_dir(&dir);
    let store = common.as_deref().and_then(store::Store::for_repo);
    let session_dir = Some(dir.clone());
    let draft_common = common;
    let loader: ui::Loader = Box::new(move |progress| {
        // Resolve the reference to its true type, then open a PR (with its diff)
        // or an issue (conversation only, no diff).
        let (label, diff, threads, pr, issue, key) =
            match prsync::fetch_subject(dir, pr_query, progress)? {
                prsync::Opened::Pr {
                    handle,
                    label,
                    diff,
                    threads,
                } => {
                    let key = handle.pr_key();
                    (label, diff, threads, Some(handle), None, key)
                }
                prsync::Opened::Issue {
                    handle,
                    label,
                    threads,
                } => {
                    let key = handle.draft_key();
                    (
                        label,
                        loopreview_core::Diff::default(),
                        threads,
                        None,
                        Some(handle),
                        key,
                    )
                }
            };
        // Drafts persist under the same `owner/repo#N` key whether PR or issue.
        let (review, stale_cleaned) = match draft_common.as_deref().and_then(store::Store::for_repo)
        {
            Some(store) => {
                let drafts = store.load_pr_drafts(&key).unwrap_or_default();
                // Orphan drops are surfaced on an explicit refresh; at first load
                // there is no prior view to have shown the note, so only the stale
                // ghost count feeds the startup notice.
                let (threads, cleaned, _orphans) = prsync::merge_drafts(&drafts, threads);
                (loopreview_core::Review { threads }, cleaned)
            }
            None => (loopreview_core::Review { threads }, 0),
        };
        Ok(ui::Loaded {
            label,
            diff,
            review,
            stale_cleaned,
            pr,
            issue,
            pr_key: Some(key),
        })
    });

    ui::run(ui::Session {
        label: "pull request".to_string(),
        diff: loopreview_core::Diff::default(),
        // Unused: a PR is not file-watched (watch_root is None below).
        source: Arc::new(StdinPatchSource::new()),
        watch_root: None,
        mode: layout_mode(mode),
        review: loopreview_core::Review::default(),
        store,
        author,
        split_min_width: cfg.split_min_width,
        auto_collapse_files: cfg.auto_collapse_files,
        auto_collapse_lines: cfg.auto_collapse_lines,
        sidebar_mode: cfg.sidebar,
        sidebar_min_content: cfg.sidebar_min_content,
        sidebar_width: cfg.sidebar_width,
        composer_enter: cfg.composer_enter,
        keymap,
        repo_dir: session_dir,
        loader: Some(loader),
        notice: cfg_notice,
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
        Action::Show { target, pathspec } => {
            reject_piped_stdin_for_diff()?;
            let root = repo_root()?;
            let source = ShowSource::new(root.clone(), target).pathspec(pathspec);
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
        // Handled in try_run before build_source is reached.
        Action::Pr { .. } => unreachable!("pull requests are handled by run_pr"),
        Action::NotYet(_) | Action::Invalid(_) => {
            unreachable!("usage errors are reported earlier")
        }
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
