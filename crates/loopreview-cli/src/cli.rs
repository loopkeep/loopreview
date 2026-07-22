//! Command-line surface: `loopreview [diff] [target]`, plus `--help` and
//! `--version` (handled by clap before the TTY guard runs).

use clap::Parser;

/// Parsed command line for loopreview.
#[derive(Parser, Debug)]
#[command(
    name = "loopreview",
    version,
    about = "Review a diff in an interactive terminal UI",
    long_about = "loopreview (lr) opens a diff for review in an interactive terminal UI.\n\n\
        With no arguments it shows the working tree (or a patch piped in with `git diff | lr`).\n\
        Give a target such as `main...` or `HEAD~3` to compare git refs.",
    disable_help_subcommand = true
)]
pub struct Cli {
    /// The literal `diff`, or a diff target such as `main...` or `HEAD~3`.
    #[arg(value_name = "DIFF | TARGET")]
    first: Option<String>,
    /// A diff target, used when the first argument is the literal `diff`.
    #[arg(value_name = "TARGET")]
    second: Option<String>,
}

/// What the user asked loopreview to show.
#[derive(Debug, PartialEq, Eq)]
pub enum Request {
    /// Compare against a specific `git diff` target.
    Target(String),
    /// No explicit target: review the working tree, or a patch from stdin.
    Default,
}

impl Cli {
    /// Resolve the parsed arguments into a [`Request`].
    pub fn into_request(self) -> Result<Request, String> {
        resolve(self.first.as_deref(), self.second.as_deref())
    }
}

/// Resolve the two optional positional arguments, allowing an optional leading
/// `diff` keyword before the target.
fn resolve(first: Option<&str>, second: Option<&str>) -> Result<Request, String> {
    match (first, second) {
        (None, _) => Ok(Request::Default),
        (Some("diff"), None) => Ok(Request::Default),
        (Some("diff"), Some(target)) => Ok(Request::Target(target.to_string())),
        (Some(target), None) => Ok(Request::Target(target.to_string())),
        (Some(_), Some(extra)) => Err(format!(
            "unexpected argument '{extra}' — usage: loopreview [diff] [target]"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_arguments_is_default() {
        assert_eq!(resolve(None, None), Ok(Request::Default));
    }

    #[test]
    fn bare_diff_keyword_is_default() {
        assert_eq!(resolve(Some("diff"), None), Ok(Request::Default));
    }

    #[test]
    fn diff_with_target() {
        assert_eq!(
            resolve(Some("diff"), Some("main...")),
            Ok(Request::Target("main...".to_string()))
        );
    }

    #[test]
    fn bare_target_without_keyword() {
        assert_eq!(
            resolve(Some("HEAD~3"), None),
            Ok(Request::Target("HEAD~3".to_string()))
        );
    }

    #[test]
    fn two_targets_without_keyword_is_an_error() {
        assert!(resolve(Some("main"), Some("dev")).is_err());
    }

    #[test]
    fn clap_accepts_the_documented_forms() {
        // Guards against the arg definitions drifting from the grammar above.
        assert_eq!(Cli::parse_from(["lr"]).into_request(), Ok(Request::Default));
        assert_eq!(
            Cli::parse_from(["lr", "diff", "main..."]).into_request(),
            Ok(Request::Target("main...".to_string()))
        );
        assert_eq!(
            Cli::parse_from(["lr", "main..."]).into_request(),
            Ok(Request::Target("main...".to_string()))
        );
    }
}
