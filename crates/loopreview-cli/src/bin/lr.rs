//! The `lr` binary: a short alias for `loopreview`, sharing the same `run`.

fn main() -> std::process::ExitCode {
    loopreview_cli::run()
}
