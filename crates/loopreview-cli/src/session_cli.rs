//! `lr session`: the control-plane client.
//!
//! Each verb resolves the target session from the registry (by id, by repo, or
//! — when only one is live — automatically), connects to its socket, sends one
//! request, and prints the reply as a table or, with `--json`, as JSON for an
//! agent to parse. These commands run headless: they are the path an agent takes
//! to read and steer a review a human is watching.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;

use loopreview_control::client::Client;
use loopreview_control::protocol::{self, EventKind, Reply, Request};
use loopreview_control::registry::{self, SessionRecord};

use loopreview_core::{Side, git};

use crate::cli::{CommentAction, LineSide, SessionArgs, SessionVerb, Target, WaitEvent};

/// Run a `lr session` verb.
pub fn run(args: SessionArgs) -> Result<()> {
    match args.verb {
        SessionVerb::List { json } => list(json),
        SessionVerb::Get { target, json } => {
            let Reply::Session(info) = call(&target, Request::Get)? else {
                bail!("unexpected reply");
            };
            if json {
                emit(&info)
            } else {
                println!("session {}", info.id);
                println!("  pid:    {}", info.pid);
                println!("  repo:   {}", info.repo.as_deref().unwrap_or("-"));
                println!("  source: {}", info.source);
                Ok(())
            }
        }
        SessionVerb::Context { target, json } => {
            let Reply::Context(info) = call(&target, Request::Context)? else {
                bail!("unexpected reply");
            };
            if json {
                emit(&info)
            } else {
                println!("view: {}", info.view);
                match (info.file.as_deref(), info.line) {
                    (Some(file), Some(line)) => {
                        let side = info.side.map(side_word).unwrap_or("new");
                        println!("cursor: {file}:{line} ({side})");
                    }
                    (Some(file), None) => println!("cursor: {file}"),
                    _ => println!("cursor: -"),
                }
                if let Some(thread) = &info.thread {
                    println!("thread: {thread}");
                }
                println!("event-seq: {}", info.event_seq);
                Ok(())
            }
        }
        SessionVerb::Review {
            target,
            patch,
            json,
        } => {
            let Reply::Review(info) = call(
                &target,
                Request::Review {
                    include_patch: patch,
                },
            )?
            else {
                bail!("unexpected reply");
            };
            if json {
                emit(&info)
            } else {
                print_review(&info);
                Ok(())
            }
        }
        SessionVerb::Navigate {
            target,
            thread,
            file,
            side,
            line,
            json,
        } => {
            if thread.is_none() && (file.is_none() || line.is_none()) {
                bail!("navigate needs --thread, or --file with --line");
            }
            let request = Request::Navigate(protocol::Navigate {
                side: file.as_ref().map(|_| core_side(side)),
                thread,
                file,
                line,
            });
            let Reply::Navigate(result) = call(&target, request)? else {
                bail!("unexpected reply");
            };
            if json {
                emit(&result)
            } else {
                if result.moved {
                    match (result.file.as_deref(), result.line) {
                        (Some(file), Some(line)) => println!("moved to {file}:{line}"),
                        (Some(file), None) => println!("moved to {file}"),
                        _ => println!("moved"),
                    }
                } else {
                    match (result.file.as_deref(), result.line) {
                        (Some(file), Some(line)) => {
                            println!("not found: {file}:{line} is not in the current diff")
                        }
                        _ => println!("not found"),
                    }
                }
                Ok(())
            }
        }
        SessionVerb::Reload { target, json } => {
            let Reply::Reload(result) = call(&target, Request::Reload)? else {
                bail!("unexpected reply");
            };
            if json {
                emit(&result)
            } else {
                println!(
                    "{}",
                    if result.started {
                        "reloading…"
                    } else {
                        "reloaded"
                    }
                );
                Ok(())
            }
        }
        SessionVerb::Comment { action } => comment(action),
        SessionVerb::Wait {
            target,
            events,
            after,
            timeout,
            json,
        } => {
            let request = Request::Wait(protocol::Wait {
                events: events.iter().copied().map(event_kind).collect(),
                after,
                timeout_ms: timeout.map(|s| s.saturating_mul(1000)),
            });
            let Reply::Wait(result) = call(&target, request)? else {
                bail!("unexpected reply");
            };
            if json {
                emit(&result)?;
            } else {
                match &result.event {
                    Some(event) => {
                        let on = event
                            .thread
                            .as_ref()
                            .map(|t| format!(" on {t}"))
                            .unwrap_or_default();
                        println!("{} (seq {}){on}", event.kind.as_str(), event.seq);
                    }
                    None => println!("timed out (no event; latest seq {})", result.event_seq),
                }
            }
            // A timeout (no event) exits non-zero so scripts can branch on it,
            // while the result is still printed to stdout.
            if result.event.is_none() {
                std::process::exit(1);
            }
            Ok(())
        }
    }
}

/// Run a `lr session comment` action.
fn comment(action: CommentAction) -> Result<()> {
    match action {
        CommentAction::Add {
            target,
            file,
            side,
            line,
            conversation,
            body,
            author,
            draft,
            json,
        } => {
            // Side is only meaningful for a line comment.
            let side = if conversation {
                None
            } else {
                Some(core_side(side))
            };
            let request = Request::CommentAdd(protocol::CommentAdd {
                file,
                side,
                line,
                body,
                author: author.unwrap_or_else(|| "agent".to_string()),
                draft,
                conversation,
            });
            let Reply::Comment(result) = call(&target, request)? else {
                bail!("unexpected reply");
            };
            if json {
                emit(&result)
            } else {
                print_comment(&result);
                Ok(())
            }
        }
        CommentAction::Reply {
            target,
            thread,
            body,
            author,
            draft,
            json,
        } => {
            let request = Request::CommentReply(protocol::CommentReply {
                thread,
                body,
                author: author.unwrap_or_else(|| "agent".to_string()),
                draft,
            });
            let Reply::Comment(result) = call(&target, request)? else {
                bail!("unexpected reply");
            };
            if json {
                emit(&result)
            } else {
                print_comment(&result);
                Ok(())
            }
        }
        CommentAction::Resolve {
            target,
            thread,
            reopen,
            author,
            json,
        } => {
            let request = Request::CommentResolve(protocol::CommentResolve {
                thread,
                resolved: !reopen,
                author,
            });
            let Reply::Resolve(result) = call(&target, request)? else {
                bail!("unexpected reply");
            };
            if json {
                emit(&result)
            } else {
                println!(
                    "{} thread {}",
                    if result.resolved {
                        "resolved"
                    } else {
                        "reopened"
                    },
                    result.thread
                );
                Ok(())
            }
        }
        CommentAction::Edit {
            target,
            comment,
            body,
            author,
            json,
        } => {
            let request = Request::CommentEdit(protocol::CommentEdit {
                id: comment,
                body,
                author: author.unwrap_or_else(|| "agent".to_string()),
            });
            let Reply::Comment(result) = call(&target, request)? else {
                bail!("unexpected reply");
            };
            if json {
                emit(&result)
            } else {
                print_comment(&result);
                Ok(())
            }
        }
        CommentAction::Rm {
            target,
            comment,
            thread,
            json,
        } => {
            // Clap's arg group guarantees exactly one of comment/thread; either
            // id resolves to its comment or thread on the server side.
            let id = comment.or(thread).unwrap_or_default();
            let request = Request::CommentRm(protocol::CommentRm { id });
            let Reply::Removed(result) = call(&target, request)? else {
                bail!("unexpected reply");
            };
            if json {
                emit(&result)
            } else {
                println!(
                    "removed {} from thread {}",
                    if result.removed_thread {
                        "the thread"
                    } else {
                        "the comment"
                    },
                    result.thread
                );
                Ok(())
            }
        }
        CommentAction::List { target, json } => {
            let Reply::Threads { threads } = call(&target, Request::CommentList)? else {
                bail!("unexpected reply");
            };
            if json {
                emit(&threads)
            } else {
                if threads.is_empty() {
                    println!("No threads.");
                }
                for thread in &threads {
                    println!("{}", thread_line(thread));
                }
                Ok(())
            }
        }
    }
}

/// List the live sessions.
fn list(json: bool) -> Result<()> {
    let sessions = registry::list(&sessions_dir()?);
    if json {
        return emit(&sessions);
    }
    if sessions.is_empty() {
        println!("No live review sessions. Start one with `lr` in a repository.");
        return Ok(());
    }
    for session in &sessions {
        println!(
            "{}  {}  {}  (pid {})",
            session.id,
            session.repo.as_deref().unwrap_or("-"),
            session.source,
            session.pid
        );
    }
    Ok(())
}

// -- session resolution --------------------------------------------------------

/// Connect to the resolved session and send one request.
fn call(target: &Target, request: Request) -> Result<Reply> {
    let record = resolve(target)?;
    let mut client =
        Client::connect(&record.socket).map_err(|e| anyhow!("{e} (session {})", record.id))?;
    client.call(&request).map_err(|e| anyhow!("{e}"))
}

/// Resolve the target session: by id, else by repository (explicit `--repo` or
/// the current directory), else — when exactly one session is live — that one.
fn resolve(target: &Target) -> Result<SessionRecord> {
    let sessions = registry::list(&sessions_dir()?);
    if sessions.is_empty() {
        bail!("no live review sessions — start one with `lr` in a repository");
    }
    if let Some(id) = &target.session {
        return sessions
            .into_iter()
            .find(|s| &s.id == id)
            .ok_or_else(|| anyhow!("no live session with id {id} (see `lr session list`)"));
    }

    let explicit = target.repo.is_some();
    let repo = match &target.repo {
        Some(path) => Some(repo_root(path)?),
        None => current_repo(),
    };
    if let Some(repo) = &repo {
        let mut matches: Vec<SessionRecord> = sessions
            .iter()
            .filter(|s| s.repo.as_deref() == Some(repo.as_str()))
            .cloned()
            .collect();
        match matches.len() {
            1 => return Ok(matches.pop().unwrap()),
            n if n > 1 => {
                bail!(
                    "several sessions are open on {repo}; pass a session id (see `lr session list`)"
                )
            }
            _ if explicit => bail!("no live session for {repo}"),
            _ => {}
        }
    }
    // No repo context (or an implicit repo with no match): fall back to the sole
    // session when there is exactly one, mirroring hunk's convenience.
    if sessions.len() == 1 {
        return Ok(sessions.into_iter().next().unwrap());
    }
    bail!("several live sessions; select one with a session id or --repo (see `lr session list`)")
}

/// The git repository root of `path`, as a string, for matching a record's repo.
fn repo_root(path: &Path) -> Result<String> {
    let root = git::repo_root(path)
        .with_context(|| format!("resolving the repository for {}", path.display()))?;
    Ok(root.to_string_lossy().into_owned())
}

/// The current directory's repository root, if it is inside one.
fn current_repo() -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    git::repo_root(&cwd)
        .ok()
        .map(|root| root.to_string_lossy().into_owned())
}

/// The sessions registry directory.
fn sessions_dir() -> Result<PathBuf> {
    crate::config::sessions_dir().context("no config directory to read sessions from")
}

// -- formatting ----------------------------------------------------------------

/// Print a value as pretty JSON.
fn emit<T: Serialize>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

/// Print a review summary (files and threads).
fn print_review(info: &protocol::ReviewInfo) {
    println!("{}", info.source);
    let (added, removed): (u32, u32) = info
        .files
        .iter()
        .fold((0, 0), |(a, r), f| (a + f.added, r + f.removed));
    println!(
        "{} file{} changed, +{added} -{removed}",
        info.files.len(),
        plural(info.files.len())
    );
    for file in &info.files {
        let rename = file
            .old_path
            .as_deref()
            .map(|old| format!(" (from {old})"))
            .unwrap_or_default();
        let binary = if file.binary { " [binary]" } else { "" };
        println!(
            "  {:8} {}{rename}{binary}  +{} -{}",
            file.status, file.path, file.added, file.removed
        );
    }
    if !info.threads.is_empty() {
        println!(
            "{} thread{}:",
            info.threads.len(),
            plural(info.threads.len())
        );
        for thread in &info.threads {
            println!("{}", thread_line(thread));
        }
    }
}

/// A one-line summary of a thread.
fn thread_line(thread: &protocol::ThreadInfo) -> String {
    let loc = match (&thread.anchor.file, thread.anchor.end) {
        (Some(file), Some(line)) => format!("{file}:{line}"),
        (Some(file), None) => file.clone(),
        _ => "review".to_string(),
    };
    let outdated = if thread.outdated { " outdated" } else { "" };
    let count = thread.comments.len();
    format!(
        "  {} [{}{outdated}] {loc} — {count} comment{}",
        thread.id,
        thread.state,
        plural(count)
    )
}

/// Print the outcome of a comment mutation.
fn print_comment(result: &protocol::CommentResult) {
    let draft = if result.draft {
        " (draft — the reviewer submits it with Ctrl-S)"
    } else {
        ""
    };
    println!(
        "added comment {} to thread {}{draft}",
        result.comment, result.thread
    );
}

/// `""` for one, `"s"` for any other count.
fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// The wire side for a CLI side flag.
fn core_side(side: LineSide) -> Side {
    match side {
        LineSide::Old => Side::Old,
        LineSide::New => Side::New,
    }
}

/// A word for a side, for human output.
fn side_word(side: Side) -> &'static str {
    match side {
        Side::Old => "old",
        Side::New => "new",
    }
}

/// The wire event kind for a CLI wait-event flag.
fn event_kind(event: WaitEvent) -> EventKind {
    match event {
        WaitEvent::Comment => EventKind::Comment,
        WaitEvent::Reply => EventKind::Reply,
        WaitEvent::Resolve => EventKind::Resolve,
        WaitEvent::Submit => EventKind::Submit,
        WaitEvent::Reload => EventKind::Reload,
    }
}
