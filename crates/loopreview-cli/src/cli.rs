//! Command-line surface (DESIGN.md §3.5).
//!
//! Bare `lr` is dispatch sugar (a piped patch, otherwise the working tree).
//! `lr diff` reviews a VCS diff and never reads stdin; `lr patch` reviews a
//! unified-diff patch from a file or stdin. Later milestones' verbs (`show`,
//! `pr`, `session`, `daemon`, `skill`) are reserved here so the namespace is
//! stable. `--help` / `--version` are handled by clap before the TTY guard.

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
    about = "Review a diff in an interactive terminal UI",
    long_about = "loopreview (lr) opens a diff for review in an interactive terminal UI.\n\n\
        With no subcommand it shows the working tree, or a patch piped in with `git diff | lr`.\n\
        `lr diff <target>` compares git refs; `lr patch <file>` reviews a saved patch.",
    disable_help_subcommand = true
)]
pub struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
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
    /// Reserved: commit review (arrives in M2).
    #[command(hide = true)]
    Show {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Review a GitHub pull request (by number, URL, or owner/repo#N).
    Pr {
        /// The pull request; omit and pass --detect to use the current branch.
        query: Option<String>,
        /// Detect the pull request for the current branch.
        #[arg(long)]
        detect: bool,
    },
    /// Inspect and steer a live review session (for agents and scripts).
    Session(SessionArgs),
    /// Reserved: a session daemon (a future alternative to per-session sockets).
    #[command(hide = true)]
    Daemon {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Print the bundled agent skill document (for `lr session`).
    Skill(SkillArgs),
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
    /// Add a comment thread at a line.
    Add {
        #[command(flatten)]
        target: Target,
        /// The file to comment on.
        #[arg(long)]
        file: String,
        /// Which side the line is measured on.
        #[arg(long, value_enum, default_value_t = LineSide::New)]
        side: LineSide,
        /// The 1-based line number.
        #[arg(long)]
        line: u32,
        /// The comment body (markdown).
        #[arg(long)]
        body: String,
        /// The comment author (required; agent comments are attributed).
        #[arg(long)]
        author: String,
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
        /// The reply author.
        #[arg(long)]
        author: String,
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

/// `lr skill <verb>`: the bundled agent skill document.
#[derive(Args, Debug)]
pub struct SkillArgs {
    /// The skill verb (defaults to `path`).
    #[command(subcommand)]
    pub verb: Option<SkillVerb>,
}

/// The `lr skill` verbs.
#[derive(Subcommand, Debug)]
pub enum SkillVerb {
    /// Write the skill document to disk and print its path.
    Path,
    /// Print the skill document to standard output.
    Show,
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
    /// `lr patch <file>`: a patch from a file.
    PatchFile(PathBuf),
    /// `lr patch`: a patch from standard input.
    PatchStdin,
    /// `lr pr [query] [--detect]`: review a GitHub pull request.
    Pr { query: Option<String>, detect: bool },
    /// A reserved verb that is not implemented yet.
    NotYet(&'static str),
}

/// What the parsed command line dispatches to: the review UI, or a control-plane
/// verb that runs headless (no terminal required).
pub enum Dispatch {
    /// Open the review UI.
    Tui(Invocation),
    /// Run a `lr session` verb against a live session.
    Session(SessionArgs),
    /// Run a `lr skill` verb.
    Skill(SkillArgs),
}

impl Cli {
    /// Split the parsed command line into a [`Dispatch`]. The control-plane verbs
    /// (`session`, `skill`) run without a terminal; everything else opens the UI.
    pub fn dispatch(self) -> Dispatch {
        let Cli {
            command,
            no_watch,
            exclude_untracked,
            mode,
        } = self;
        match command {
            Some(Command::Session(args)) => Dispatch::Session(args),
            Some(Command::Skill(args)) => Dispatch::Skill(args),
            command => Dispatch::Tui(
                Cli {
                    command,
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
        match self.command {
            None => Action::Dispatch,
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
            Some(Command::Pr { query, detect }) => Action::Pr { query, detect },
            Some(Command::Show { .. }) => Action::NotYet("`lr show` (commit review) arrives in M2"),
            // `session` and `skill` are peeled off by `dispatch` before this.
            Some(Command::Session(_)) => Action::NotYet("`lr session` is handled by dispatch"),
            Some(Command::Skill(_)) => Action::NotYet("`lr skill` is handled by dispatch"),
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
    fn pr_verb_parses_query_and_detect() {
        assert_eq!(
            action_of(&["lr", "pr", "123"]),
            Action::Pr {
                query: Some("123".to_string()),
                detect: false,
            }
        );
        assert_eq!(
            action_of(&["lr", "pr", "--detect"]),
            Action::Pr {
                query: None,
                detect: true,
            }
        );
    }

    #[test]
    fn daemon_stays_reserved() {
        assert!(matches!(
            action_of(&["lr", "daemon", "serve"]),
            Action::NotYet(_)
        ));
    }

    #[test]
    fn session_and_skill_dispatch_off_the_ui() {
        assert!(matches!(
            Cli::parse_from(["lr", "session", "list"]).dispatch(),
            Dispatch::Session(_)
        ));
        assert!(matches!(
            Cli::parse_from(["lr", "skill", "path"]).dispatch(),
            Dispatch::Skill(_)
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
                    assert_eq!(file, "a.rs");
                    assert_eq!(line, 10);
                    assert_eq!(author, "agent");
                    assert_eq!(side, LineSide::New);
                }
                other => panic!("expected comment add, got {other:?}"),
            },
            _ => panic!("expected a session dispatch"),
        }
    }
}
