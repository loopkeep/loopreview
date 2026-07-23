//! The command-line surface.
//!
//! Bare `lr` is dispatch sugar: a GitHub reference (`lr 123` / `lr owner/repo#N`
//! / a URL) or `lr --detect` opens a pull request; otherwise a piped patch, else
//! the working tree. `lr diff` reviews a VCS diff and never reads stdin; `lr
//! patch` reviews a unified-diff patch from a file or stdin; `lr show [target]`
//! reviews one commit's own changes. The hidden `daemon` verb stays reserved for
//! a later milestone so the namespace stays stable (`session` and `update` are
//! implemented; there is no `pr` verb — a bare reference is the entry point).
//! `--help` / `--version` are handled by clap before the TTY guard.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

/// The diff layout to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum LayoutMode {
    /// Choose unified or split by terminal width.
    Auto,
    /// Always unified (one column).
    Unified,
    /// Always side-by-side (old | new).
    Split,
}

/// Which side of the diff a line number is measured on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum LineSide {
    /// The original ("before") version.
    Old,
    /// The changed ("after") version.
    New,
}

/// A review event a `wait` can subscribe to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum WaitEvent {
    /// A new comment thread was created.
    Comment,
    /// A reply was added to a thread.
    Reply,
    /// A thread was resolved or reopened.
    Resolve,
    /// A pull-request review was submitted.
    Submit,
    /// The diff was reloaded.
    Reload,
}

/// Parsed command line for loopreview.
#[derive(Parser, Debug)]
#[command(
    name = "loopreview",
    version,
    about = "Review a diff, or a GitHub pull request, in an interactive terminal UI",
    long_about = "loopreview (lr) opens a diff for review in an interactive terminal UI.\n\n\
        With no arguments it shows the working tree, or a patch piped in with `git diff | lr`.\n\
        `lr <ref>` reviews a GitHub pull request — a number (`123`), `#N`, `owner/repo#N`, or a URL \
        (quote `#N`, since most shells treat a bare `#` as a comment); `lr --detect` opens the \
        current branch's pull request.\n\
        `lr diff <target>` compares git refs; `lr patch <file>` reviews a saved patch; \
        `lr show [commit]` reviews one commit's own changes.",
    disable_help_subcommand = true,
    args_conflicts_with_subcommands = true
)]
pub struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    /// A GitHub pull request to review: a number (`123`), `#N`, `owner/repo#N`,
    /// or a URL. Quote `#N` — most shells treat a bare `#` as a comment.
    #[arg(value_name = "PR")]
    target: Option<String>,
    /// Review the pull request for the current branch (no reference needed).
    #[arg(long)]
    detect: bool,
    /// Do not auto-refresh a live diff as the working tree changes.
    #[arg(long, global = true)]
    no_watch: bool,
    /// Do not include untracked files in a working-tree review.
    #[arg(long, global = true)]
    exclude_untracked: bool,
    /// Diff layout: auto (by width), unified, or split.
    #[arg(long, value_enum, default_value_t = LayoutMode::Auto, global = true)]
    mode: LayoutMode,
}

/// The fully-resolved command line: what to do and how.
pub struct Invocation {
    /// The diff to review.
    pub action: Action,
    /// Whether auto-refresh is disabled.
    pub no_watch: bool,
    /// Whether untracked files are excluded from a working-tree review.
    pub exclude_untracked: bool,
    /// The requested diff layout.
    pub mode: LayoutMode,
}

/// The subcommands loopreview accepts.
#[derive(Subcommand, Debug)]
enum Command {
    /// Review a VCS diff (does not read stdin).
    Diff {
        /// A diff target such as `main...` or `HEAD~3`; omit for the working tree.
        target: Option<String>,
        /// Review only staged changes (the index vs HEAD).
        #[arg(long)]
        staged: bool,
        /// Limit the diff to these paths (after `--`).
        #[arg(last = true, value_name = "PATHSPEC")]
        pathspec: Vec<String>,
    },
    /// Review a unified-diff patch from a file, or stdin when no file is given.
    Patch {
        /// Patch file to read; reads standard input when omitted.
        file: Option<PathBuf>,
    },
    /// Review a single commit's own changes, like `git show`.
    Show {
        /// The commit to show; omit for `HEAD`. Compared against its first parent
        /// (a merge commit uses its first parent too), or the empty tree for a
        /// root commit.
        target: Option<String>,
        /// Limit the diff to these paths (after `--`).
        #[arg(last = true, value_name = "PATHSPEC")]
        pathspec: Vec<String>,
    },
    /// Inspect and steer a live review session (for agents and scripts).
    Session(SessionArgs),
    /// Reserved: a session daemon (a future alternative to per-session sockets).
    #[command(hide = true)]
    Daemon {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Update loopreview to the latest GitHub release.
    Update {
        /// Only report whether a newer release exists; do not install it.
        #[arg(long)]
        check: bool,
    },
}

/// `lr session <verb>`: the control-plane client.
#[derive(Args, Debug)]
pub struct SessionArgs {
    /// The session verb.
    #[command(subcommand)]
    pub verb: SessionVerb,
}

/// The `lr session` verbs.
#[derive(Subcommand, Debug)]
pub enum SessionVerb {
    /// List the live review sessions.
    List {
        /// Emit JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Show a session's identity and diff source.
    Get {
        #[command(flatten)]
        target: Target,
        /// Emit JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show the reviewer's current focus (cursor and view).
    Context {
        #[command(flatten)]
        target: Target,
        /// Emit JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show the diff structure and the review's threads.
    Review {
        #[command(flatten)]
        target: Target,
        /// Include each hunk's lines (the raw diff text).
        #[arg(long)]
        patch: bool,
        /// Emit JSON.
        #[arg(long)]
        json: bool,
    },
    /// Move the reviewer's cursor and view.
    Navigate {
        #[command(flatten)]
        target: Target,
        /// Jump to the line a thread is anchored to.
        #[arg(long)]
        thread: Option<String>,
        /// Jump to a line in this file (with `--line`).
        #[arg(long)]
        file: Option<String>,
        /// Which side `--line` is measured on.
        #[arg(long, value_enum, default_value_t = LineSide::New)]
        side: LineSide,
        /// The 1-based line to move to.
        #[arg(long)]
        line: Option<u32>,
        /// Emit JSON.
        #[arg(long)]
        json: bool,
    },
    /// Reload the session's diff source.
    Reload {
        #[command(flatten)]
        target: Target,
        /// Emit JSON.
        #[arg(long)]
        json: bool,
    },
    /// Read or leave review comments.
    Comment {
        /// The comment action.
        #[command(subcommand)]
        action: CommentAction,
    },
    /// Block until a review event occurs (or a timeout).
    Wait {
        #[command(flatten)]
        target: Target,
        /// The event kinds to wait for (repeatable); omit to wait for any.
        #[arg(long = "for", value_enum, value_name = "EVENT")]
        events: Vec<WaitEvent>,
        /// Report only events after this sequence number.
        #[arg(long)]
        after: Option<u64>,
        /// Give up after this many seconds.
        #[arg(long)]
        timeout: Option<u64>,
        /// Emit JSON.
        #[arg(long)]
        json: bool,
    },
}

/// The `lr session comment` actions.
#[derive(Subcommand, Debug)]
pub enum CommentAction {
    /// Add a comment thread at a line, or (with --conversation) on the PR as a whole.
    Add {
        #[command(flatten)]
        target: Target,
        /// The file to comment on (a line comment; omit with --conversation).
        #[arg(long)]
        file: Option<String>,
        /// Which side the line is measured on.
        #[arg(long, value_enum, default_value_t = LineSide::New)]
        side: LineSide,
        /// The 1-based line number (omit with --conversation).
        #[arg(long)]
        line: Option<u32>,
        /// Comment on the PR conversation as a whole, not tied to a line.
        #[arg(long)]
        conversation: bool,
        /// The comment body (markdown).
        #[arg(long)]
        body: String,
        /// The comment author. Name yourself with a stable identifier (a model
        /// or role name); omitted, it defaults to `agent`.
        #[arg(long)]
        author: Option<String>,
        /// Queue as a draft to submit (pull requests only). Without it an agent
        /// comment is a local note that is never sent to GitHub.
        #[arg(long)]
        draft: bool,
        /// Emit JSON.
        #[arg(long)]
        json: bool,
    },
    /// Reply to a thread.
    Reply {
        #[command(flatten)]
        target: Target,
        /// The thread to reply to.
        #[arg(long)]
        thread: String,
        /// The reply body (markdown).
        #[arg(long)]
        body: String,
        /// The reply author (defaults to `agent`).
        #[arg(long)]
        author: Option<String>,
        /// Queue this reply as a draft to submit (pull requests only).
        #[arg(long)]
        draft: bool,
        /// Emit JSON.
        #[arg(long)]
        json: bool,
    },
    /// Resolve a thread (or reopen it with `--reopen`).
    Resolve {
        #[command(flatten)]
        target: Target,
        /// The thread to change.
        #[arg(long)]
        thread: String,
        /// Reopen instead of resolving.
        #[arg(long)]
        reopen: bool,
        /// The actor requesting the change.
        #[arg(long)]
        author: String,
        /// Emit JSON.
        #[arg(long)]
        json: bool,
    },
    /// List the review's threads.
    List {
        #[command(flatten)]
        target: Target,
        /// Emit JSON.
        #[arg(long)]
        json: bool,
    },
    /// Edit a comment's body (your own draft/local comment only; never published).
    Edit {
        #[command(flatten)]
        target: Target,
        /// The comment to edit (a comment id).
        #[arg(long)]
        comment: String,
        /// The new body (markdown), replacing the old one.
        #[arg(long)]
        body: String,
        /// The editing author; must match the comment's author. Defaults to
        /// `agent`.
        #[arg(long)]
        author: Option<String>,
        /// Emit JSON.
        #[arg(long)]
        json: bool,
    },
    /// Withdraw an unpublished comment or thread (a draft or local note; never
    /// published).
    #[command(group(
        clap::ArgGroup::new("rm_target")
            .required(true)
            .args(["comment", "thread"]),
    ))]
    Rm {
        #[command(flatten)]
        target: Target,
        /// The comment to withdraw (a comment id; its thread goes too if it
        /// empties).
        #[arg(long)]
        comment: Option<String>,
        /// The thread to withdraw (a thread id; removes the whole draft thread).
        #[arg(long)]
        thread: Option<String>,
        /// Emit JSON.
        #[arg(long)]
        json: bool,
    },
}

/// How to select the session a verb acts on.
#[derive(Args, Debug, Clone)]
pub struct Target {
    /// The session id (needed only when several sessions share a repo).
    pub session: Option<String>,
    /// Select the session by repository (defaults to the current directory).
    #[arg(long, value_name = "PATH")]
    pub repo: Option<PathBuf>,
}

/// What loopreview should do, resolved from the command line (before consulting
/// the terminal for dispatch).
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    /// Bare `lr`: choose a piped patch or the working tree at run time.
    Dispatch,
    /// `lr diff [--staged] [-- pathspec]`: the working tree or the index.
    Worktree { staged: bool, pathspec: Vec<String> },
    /// `lr diff <target> [-- pathspec]`: a ref comparison.
    Ref {
        target: String,
        pathspec: Vec<String>,
    },
    /// `lr show [target] [-- pathspec]`: one commit's own changes (default HEAD).
    Show {
        target: String,
        pathspec: Vec<String>,
    },
    /// `lr patch <file>`: a patch from a file.
    PatchFile(PathBuf),
    /// `lr patch`: a patch from standard input.
    PatchStdin,
    /// A GitHub pull request, from a bare `lr <ref>` or `lr --detect`.
    Pr { query: Option<String>, detect: bool },
    /// A reserved verb that is not implemented yet.
    NotYet(&'static str),
    /// The arguments don't name anything to review — report `message` and exit.
    Invalid(String),
}

/// What the parsed command line dispatches to: the review UI, or a control-plane
/// verb that runs headless (no terminal required).
pub enum Dispatch {
    /// Open the review UI.
    Tui(Invocation),
    /// Run a `lr session` verb against a live session.
    Session(SessionArgs),
    /// Run `lr update` (self-update from GitHub Releases).
    Update {
        /// Only check for a newer release; do not install.
        check: bool,
    },
}

impl Cli {
    /// Split the parsed command line into a [`Dispatch`]. The control-plane verb
    /// `session` runs without a terminal; everything else opens the UI.
    pub fn dispatch(self) -> Dispatch {
        let Cli {
            command,
            target,
            detect,
            no_watch,
            exclude_untracked,
            mode,
        } = self;
        match command {
            Some(Command::Session(args)) => Dispatch::Session(args),
            Some(Command::Update { check }) => Dispatch::Update { check },
            command => Dispatch::Tui(
                Cli {
                    command,
                    target,
                    detect,
                    no_watch,
                    exclude_untracked,
                    mode,
                }
                .resolve(),
            ),
        }
    }

    /// Resolve the parsed command line into an [`Invocation`].
    pub fn resolve(self) -> Invocation {
        let no_watch = self.no_watch;
        let exclude_untracked = self.exclude_untracked;
        let mode = self.mode;
        Invocation {
            action: self.action(),
            no_watch,
            exclude_untracked,
            mode,
        }
    }

    /// Resolve the parsed command line into an [`Action`].
    pub fn action(self) -> Action {
        let Cli {
            command,
            target,
            detect,
            ..
        } = self;
        match command {
            // Bare `lr`: a GitHub reference (or `--detect`) opens a pull request;
            // otherwise dispatch (a piped patch or the working tree) at run time.
            None => match (detect, target) {
                (true, Some(_)) => Action::Invalid(
                    "`--detect` opens the current branch's pull request — drop the reference (or drop `--detect`)"
                        .to_string(),
                ),
                (true, None) => Action::Pr {
                    query: None,
                    detect: true,
                },
                (false, Some(reference))
                    if loopreview_github::parse_pr_query(&reference).is_some() =>
                {
                    Action::Pr {
                        query: Some(reference),
                        detect: false,
                    }
                }
                (false, Some(reference)) => Action::Invalid(format!(
                    "`{reference}` isn't something to review — pass a pull request (a number, `#N`, \
                     `owner/repo#N`, or a URL), a git diff (`lr diff {reference}`), or a patch (`lr patch`)"
                )),
                (false, None) => Action::Dispatch,
            },
            Some(Command::Diff {
                target: Some(target),
                pathspec,
                ..
            }) => Action::Ref { target, pathspec },
            Some(Command::Diff {
                target: None,
                staged,
                pathspec,
            }) => Action::Worktree { staged, pathspec },
            Some(Command::Patch { file: Some(path) }) => Action::PatchFile(path),
            Some(Command::Patch { file: None }) => Action::PatchStdin,
            Some(Command::Show { target, pathspec }) => Action::Show {
                target: target.unwrap_or_else(|| "HEAD".to_string()),
                pathspec,
            },
            // `session` is peeled off by `dispatch` before this.
            Some(Command::Session(_)) => Action::NotYet("`lr session` is handled by dispatch"),
            Some(Command::Update { .. }) => Action::NotYet("`lr update` is handled by dispatch"),
            Some(Command::Daemon { .. }) => Action::NotYet(
                "`lr daemon` is reserved; this build hosts a socket per session (see `lr session`)",
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn action_of(args: &[&str]) -> Action {
        Cli::parse_from(args).action()
    }

    #[test]
    fn bare_invocation_dispatches() {
        assert_eq!(action_of(&["lr"]), Action::Dispatch);
    }

    #[test]
    fn diff_without_target_is_the_working_tree() {
        assert_eq!(
            action_of(&["lr", "diff"]),
            Action::Worktree {
                staged: false,
                pathspec: vec![]
            }
        );
    }

    #[test]
    fn diff_staged_flag() {
        assert_eq!(
            action_of(&["lr", "diff", "--staged"]),
            Action::Worktree {
                staged: true,
                pathspec: vec![]
            }
        );
    }

    #[test]
    fn diff_with_target_and_pathspec() {
        assert_eq!(
            action_of(&["lr", "diff", "main...", "--", "src/"]),
            Action::Ref {
                target: "main...".to_string(),
                pathspec: vec!["src/".to_string()],
            }
        );
    }

    #[test]
    fn show_defaults_to_head() {
        assert_eq!(
            action_of(&["lr", "show"]),
            Action::Show {
                target: "HEAD".to_string(),
                pathspec: vec![],
            }
        );
    }

    #[test]
    fn show_with_target_and_pathspec() {
        assert_eq!(
            action_of(&["lr", "show", "HEAD~2", "--", "src/"]),
            Action::Show {
                target: "HEAD~2".to_string(),
                pathspec: vec!["src/".to_string()],
            }
        );
    }

    #[test]
    fn patch_from_file_or_stdin() {
        assert_eq!(
            action_of(&["lr", "patch", "changes.diff"]),
            Action::PatchFile(PathBuf::from("changes.diff"))
        );
        assert_eq!(action_of(&["lr", "patch"]), Action::PatchStdin);
    }

    #[test]
    fn no_watch_is_a_global_flag() {
        let inv = Cli::parse_from(["lr", "--no-watch"]).resolve();
        assert_eq!(inv.action, Action::Dispatch);
        assert!(inv.no_watch);
        assert!(
            Cli::parse_from(["lr", "diff", "--no-watch"])
                .resolve()
                .no_watch
        );
        assert!(!Cli::parse_from(["lr", "diff"]).resolve().no_watch);
    }

    #[test]
    fn mode_defaults_to_auto_and_parses_globally() {
        assert_eq!(Cli::parse_from(["lr"]).resolve().mode, LayoutMode::Auto);
        assert_eq!(
            Cli::parse_from(["lr", "--mode", "unified"]).resolve().mode,
            LayoutMode::Unified
        );
        assert_eq!(
            Cli::parse_from(["lr", "diff", "--mode", "split"])
                .resolve()
                .mode,
            LayoutMode::Split
        );
    }

    #[test]
    fn a_bare_github_reference_opens_a_pull_request() {
        for reference in [
            "123",
            "#123",
            "octo/hello#123",
            "https://github.com/octo/hello/pull/123",
        ] {
            assert_eq!(
                action_of(&["lr", reference]),
                Action::Pr {
                    query: Some(reference.to_string()),
                    detect: false,
                },
                "`{reference}` opens a pull request",
            );
        }
    }

    #[test]
    fn detect_opens_the_current_branch_pr() {
        assert_eq!(
            action_of(&["lr", "--detect"]),
            Action::Pr {
                query: None,
                detect: true,
            }
        );
    }

    #[test]
    fn detect_and_a_reference_conflict() {
        assert!(matches!(
            action_of(&["lr", "--detect", "123"]),
            Action::Invalid(_)
        ));
    }

    #[test]
    fn a_non_reference_positional_is_rejected() {
        // Not a PR reference and not a subcommand — a helpful error, not a silent
        // worktree open.
        assert!(matches!(
            action_of(&["lr", "foo.patch"]),
            Action::Invalid(_)
        ));
    }

    #[test]
    fn the_pr_verb_is_gone() {
        // `pr` is no longer a subcommand — bare `lr pr` is a non-reference
        // positional (rejected), and `lr pr 123` is a clap parse error.
        assert!(matches!(action_of(&["lr", "pr"]), Action::Invalid(_)));
        assert!(Cli::try_parse_from(["lr", "pr", "123"]).is_err());
    }

    #[test]
    fn daemon_stays_reserved() {
        assert!(matches!(
            action_of(&["lr", "daemon", "serve"]),
            Action::NotYet(_)
        ));
    }

    #[test]
    fn session_dispatches_off_the_ui() {
        assert!(matches!(
            Cli::parse_from(["lr", "session", "list"]).dispatch(),
            Dispatch::Session(_)
        ));
        assert!(matches!(
            Cli::parse_from(["lr", "diff"]).dispatch(),
            Dispatch::Tui(_)
        ));
    }

    #[test]
    fn session_comment_add_parses_required_fields() {
        let cli = Cli::parse_from([
            "lr", "session", "comment", "add", "--file", "a.rs", "--line", "10", "--body", "hi",
            "--author", "agent",
        ]);
        match cli.dispatch() {
            Dispatch::Session(args) => match args.verb {
                SessionVerb::Comment {
                    action:
                        CommentAction::Add {
                            file,
                            line,
                            author,
                            side,
                            ..
                        },
                } => {
                    assert_eq!(file.as_deref(), Some("a.rs"));
                    assert_eq!(line, Some(10));
                    assert_eq!(author.as_deref(), Some("agent"));
                    assert_eq!(side, LineSide::New);
                }
                other => panic!("expected comment add, got {other:?}"),
            },
            _ => panic!("expected a session dispatch"),
        }
    }

    #[test]
    fn session_comment_rm_parses_a_named_target() {
        // Regression: the target must be named. A positional id sat after
        // Target's optional positional `session`, which clap rejects (a
        // non-required positional before a required one) — a parse-time panic.
        let rm = |args: &[&str]| {
            let mut full = vec!["lr", "session", "comment", "rm"];
            full.extend_from_slice(args);
            match Cli::parse_from(full).dispatch() {
                Dispatch::Session(a) => match a.verb {
                    SessionVerb::Comment {
                        action:
                            CommentAction::Rm {
                                comment, thread, ..
                            },
                    } => (comment, thread),
                    other => panic!("expected comment rm, got {other:?}"),
                },
                _ => panic!("expected a session dispatch"),
            }
        };
        assert_eq!(rm(&["--comment", "c1"]), (Some("c1".to_string()), None));
        assert_eq!(rm(&["--thread", "t1"]), (None, Some("t1".to_string())));
        // Exactly one target is required.
        assert!(Cli::try_parse_from(["lr", "session", "comment", "rm"]).is_err());
        assert!(
            Cli::try_parse_from([
                "lr",
                "session",
                "comment",
                "rm",
                "--comment",
                "c",
                "--thread",
                "t",
            ])
            .is_err()
        );
    }
}
