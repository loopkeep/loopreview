//! Command-line surface (DESIGN.md §3.5).
//!
//! Bare `lr` is dispatch sugar (a piped patch, otherwise the working tree).
//! `lr diff` reviews a VCS diff and never reads stdin; `lr patch` reviews a
//! unified-diff patch from a file or stdin. Later milestones' verbs (`show`,
//! `pr`, `session`, `daemon`, `skill`) are reserved here so the namespace is
//! stable. `--help` / `--version` are handled by clap before the TTY guard.

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

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
    /// Reserved: control-plane session commands (arrive in M3).
    #[command(hide = true)]
    Session {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Reserved: control-plane daemon (arrives in M3).
    #[command(hide = true)]
    Daemon {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Reserved: agent skill docs (arrive in M3).
    #[command(hide = true)]
    Skill {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
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

impl Cli {
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
            Some(Command::Session { .. }) => {
                Action::NotYet("`lr session` (control plane) arrives in M3")
            }
            Some(Command::Daemon { .. }) => {
                Action::NotYet("`lr daemon` (control plane) arrives in M3")
            }
            Some(Command::Skill { .. }) => {
                Action::NotYet("`lr skill` (agent skill docs) arrives in M3")
            }
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
    fn reserved_verbs_report_their_milestone() {
        assert!(matches!(
            action_of(&["lr", "session", "list"]),
            Action::NotYet(_)
        ));
    }
}
