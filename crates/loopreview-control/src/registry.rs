//! The session registry: one JSON record per running review UI.
//!
//! There is no central daemon. Each UI writes a [`SessionRecord`] into a
//! sessions directory when it starts and removes it when it exits. `lr session
//! list` reads the directory and, because a crashed UI cannot clean up after
//! itself, drops any record that is no longer live — its process has exited, or
//! (guarding against a reused pid) nothing is listening on its socket — reclaiming
//! the leftover record and, on Unix, its socket file. The caller owns the
//! directory path so this module stays free of any environment convention and is
//! easy to test.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::transport;

/// A registered review session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRecord {
    /// The session id (also the record and socket file stem).
    pub id: String,
    /// The process id hosting the session, for liveness checks.
    pub pid: u32,
    /// The stored socket identifier (see [`crate::transport::socket_id`]).
    pub socket: String,
    /// The repository root, when the session is git-backed.
    pub repo: Option<String>,
    /// A human-readable description of the diff source.
    pub source: String,
    /// When the session started, seconds since the Unix epoch.
    pub started_at: u64,
}

/// The record file path for `id` under `dir`.
fn record_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.json"))
}

/// Write `record` into `dir`, creating the directory if needed. The write is
/// atomic (temp file + rename) so a reader never sees a half-written record.
pub fn register(dir: &Path, record: &SessionRecord) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let json = serde_json::to_string_pretty(record)?;
    let path = record_path(dir, &record.id);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, &path)
}

/// Remove the record for `id` (and, on Unix, its socket file). Missing files are
/// not an error — deregistration is best-effort on exit.
pub fn remove(dir: &Path, id: &str) {
    if let Ok(record) = read(&record_path(dir, id)) {
        remove_socket(&record.socket);
    }
    let _ = std::fs::remove_file(record_path(dir, id));
}

/// Read every live session from `dir`, discarding (and cleaning up) records that
/// are no longer live. A record is live only when its process is still running
/// **and** something answers on its socket: a session binds its listener before
/// it registers, so a live record is always reachable, while a record whose pid
/// has been reused by an unrelated process fails the socket probe and is
/// reclaimed. Records are returned sorted by start time, oldest first, for stable
/// listing.
pub fn list(dir: &Path) -> Vec<SessionRecord> {
    let mut records = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return records;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(record) = read(&path) else { continue };
        if process_alive(record.pid) && transport::is_reachable(&record.socket) {
            records.push(record);
        } else {
            // A crashed session, or a record left by a since-reused pid: reclaim
            // its record and socket file.
            remove_socket(&record.socket);
            let _ = std::fs::remove_file(&path);
        }
    }
    records.sort_by_key(|r| (r.started_at, r.id.clone()));
    records
}

/// Read and parse one record file.
fn read(path: &Path) -> std::io::Result<SessionRecord> {
    let text = std::fs::read_to_string(path)?;
    serde_json::from_str(&text).map_err(std::io::Error::from)
}

/// Remove a leftover Unix socket file (a no-op for a Windows pipe name).
fn remove_socket(socket: &str) {
    #[cfg(not(windows))]
    {
        let _ = std::fs::remove_file(socket);
    }
    #[cfg(windows)]
    {
        let _ = socket;
    }
}

/// Whether the process with `pid` is still running.
#[cfg(unix)]
pub fn process_alive(pid: u32) -> bool {
    // `kill(pid, 0)` sends no signal but performs the permission and existence
    // checks: success (or a permission error) means the process exists; ESRCH
    // means it does not.
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if result == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Whether the process with `pid` is still running.
#[cfg(windows)]
pub fn process_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return false;
        }
        let mut code: u32 = 0;
        let ok = GetExitCodeProcess(handle, &mut code) != 0;
        CloseHandle(handle);
        ok && code as i32 == STILL_ACTIVE
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn temp_dir() -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("lr-reg-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// A unique socket id for one test session, so parallel tests never collide.
    fn unique_socket(id: &str) -> String {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        crate::transport::socket_id(&format!("reg-{}-{n}-{id}", std::process::id()))
    }

    fn record_with_socket(id: &str, pid: u32, socket: &str) -> SessionRecord {
        SessionRecord {
            id: id.to_string(),
            pid,
            socket: socket.to_string(),
            repo: Some("/repo".to_string()),
            source: "working tree".to_string(),
            started_at: 100,
        }
    }

    /// A record for a live session: the current pid plus a real listener the
    /// caller must keep alive for the duration of the test.
    fn live_record(id: &str) -> (SessionRecord, crate::transport::Listener) {
        let socket = unique_socket(id);
        let listener = crate::transport::listen(&socket).unwrap();
        (
            record_with_socket(id, std::process::id(), &socket),
            listener,
        )
    }

    #[test]
    fn current_process_is_alive() {
        assert!(process_alive(std::process::id()));
    }

    #[test]
    fn a_very_high_pid_is_not_alive() {
        // PIDs this large are not assigned on the platforms we target.
        assert!(!process_alive(4_000_000_000));
    }

    #[test]
    fn register_then_list_round_trips() {
        let dir = temp_dir();
        let (record, _listener) = live_record("abc");
        register(&dir, &record).unwrap();
        let listed = list(&dir);
        assert_eq!(listed, vec![record]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_drops_dead_sessions_and_reclaims_their_records() {
        let dir = temp_dir();
        let (live, _listener) = live_record("live");
        register(&dir, &live).unwrap();
        register(
            &dir,
            &record_with_socket("dead", 4_000_000_000, &unique_socket("dead")),
        )
        .unwrap();
        let listed = list(&dir);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "live");
        // The dead session's record file is gone after listing.
        assert!(!record_path(&dir, "dead").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_reclaims_a_record_whose_pid_was_reused() {
        // A crashed session's pid can be reassigned to an unrelated process, so a
        // live-looking pid is not enough: with nothing listening on the recorded
        // socket, the record must be reclaimed rather than reported alive.
        let dir = temp_dir();
        let stale = record_with_socket("reused", std::process::id(), &unique_socket("reused"));
        register(&dir, &stale).unwrap();
        let listed = list(&dir);
        assert!(listed.is_empty(), "an unreachable session is not listed");
        assert!(!record_path(&dir, "reused").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_deletes_the_record() {
        let dir = temp_dir();
        let (record, _listener) = live_record("gone");
        register(&dir, &record).unwrap();
        remove(&dir, "gone");
        assert!(list(&dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
