//! Open a URL in the user's default browser.
//!
//! Each platform hands a URL to its desktop launcher — `open` on macOS,
//! `xdg-open` on the free desktops, `cmd /C start` on Windows. The choice is a
//! pure function of the OS name ([`argv`]) so it can be unit-tested for every
//! platform from any host; [`open_url`] wires it to the current OS and actually
//! spawns the command. The caller treats a spawn failure (no launcher on `PATH`,
//! a non-zero exit) as "couldn't open" and shows the URL instead.

use std::io;
use std::process::{Command, Stdio};

/// The launcher program and its arguments for opening `url` on `os` (as spelled
/// by [`std::env::consts::OS`], e.g. `"macos"`, `"linux"`, `"windows"`).
///
/// Windows goes through `cmd /C start`, whose first quoted argument is the new
/// window's title — an empty `""` keeps `start` from mistaking the URL for one.
/// Every other OS is treated as a free desktop and uses `xdg-open`.
fn argv(os: &str, url: &str) -> (&'static str, Vec<String>) {
    match os {
        "macos" => ("open", vec![url.to_string()]),
        "windows" => (
            "cmd",
            vec!["/C".into(), "start".into(), String::new(), url.to_string()],
        ),
        _ => ("xdg-open", vec![url.to_string()]),
    }
}

/// Open `url` in the default browser via the current platform's launcher.
///
/// Errors when the launcher can't be spawned (not on `PATH`) or exits non-zero,
/// so the caller can fall back to showing the URL. Output is silenced so the
/// launcher can't scribble over the TUI.
pub fn open_url(url: &str) -> io::Result<()> {
    let (program, args) = argv(std::env::consts::OS, url);
    let status = Command::new(program)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("{program} exited with {status}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_platform_launcher() {
        let url = "https://github.com/owner/repo/pull/1";
        assert_eq!(argv("macos", url), ("open", vec![url.to_string()]));
        assert_eq!(argv("linux", url), ("xdg-open", vec![url.to_string()]));
        // An unknown unix falls through to xdg-open rather than failing.
        assert_eq!(argv("freebsd", url), ("xdg-open", vec![url.to_string()]));
        assert_eq!(
            argv("windows", url),
            (
                "cmd",
                vec![
                    "/C".to_string(),
                    "start".to_string(),
                    String::new(),
                    url.to_string(),
                ],
            ),
            "the empty title guards against a URL that looks like a start option",
        );
    }
}
