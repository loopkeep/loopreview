//! Central wrapper around child-process invocation.
//!
//! Every external call (`gh`, `git`) funnels through here so spawning and error
//! classification live in one place. GitHub authentication and rate limiting are
//! left entirely to `gh`; this crate never handles a token directly. The pure
//! response parsing the callers do afterwards lives in [`crate::pull`] and
//! [`crate::push`] and is tested against captured fixture strings rather than by
//! shelling out.

use std::ffi::OsStr;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::error::{GithubError, classify};

/// Result of running a child process.
pub(crate) struct Output {
    /// Exit status code (`-1` when terminated by a signal).
    pub(crate) status: i32,
    /// Captured standard output.
    pub(crate) stdout: String,
    /// Captured standard error.
    pub(crate) stderr: String,
}

impl Output {
    /// True when the process exited successfully.
    pub(crate) fn ok(&self) -> bool {
        self.status == 0
    }
}

/// Run `program` with `args` in `cwd`, optionally feeding `stdin`.
///
/// Returns the captured output regardless of exit status; callers decide whether
/// a non-zero status is fatal (see [`run_ok`]). Only a failure to *spawn* the
/// process is an immediate error — a missing executable becomes
/// [`GithubError::NotInstalled`].
pub(crate) fn run<S: AsRef<OsStr>>(
    program: &str,
    args: &[S],
    cwd: &Path,
    stdin: Option<&str>,
) -> Result<Output, GithubError> {
    let mut command = Command::new(program);
    command.args(args).current_dir(cwd);
    command.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());

    let mut child = command.spawn().map_err(|source| {
        if source.kind() == std::io::ErrorKind::NotFound {
            GithubError::NotInstalled {
                program: program.to_string(),
            }
        } else {
            GithubError::Spawn {
                program: program.to_string(),
                source,
            }
        }
    })?;

    if let Some(input) = stdin {
        let mut sink = child.stdin.take().ok_or_else(|| GithubError::Spawn {
            program: program.to_string(),
            source: std::io::Error::other("child stdin was not piped as expected"),
        })?;
        sink.write_all(input.as_bytes())
            .map_err(|source| GithubError::Spawn {
                program: program.to_string(),
                source,
            })?;
        // Drop closes the pipe so the child sees EOF.
    }

    let out = child
        .wait_with_output()
        .map_err(|source| GithubError::Spawn {
            program: program.to_string(),
            source,
        })?;

    Ok(Output {
        status: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

/// Run a command and require a successful exit, returning captured stdout.
///
/// A non-zero exit is turned into the most specific [`GithubError`] the stderr
/// justifies (auth / network / generic command failure).
pub(crate) fn run_ok<S: AsRef<OsStr>>(
    program: &str,
    args: &[S],
    cwd: &Path,
    stdin: Option<&str>,
) -> Result<String, GithubError> {
    let out = run(program, args, cwd, stdin)?;
    if out.ok() {
        Ok(out.stdout)
    } else {
        Err(classify(program, out.status, out.stderr))
    }
}
