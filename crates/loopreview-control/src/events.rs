//! The event log a session publishes and a `wait` blocks on.
//!
//! A review UI appends an [`Event`] each time a comment is added, a reply lands,
//! a thread is resolved, a review is submitted, or the diff reloads. Each event
//! gets a monotonic sequence number. A control client's `wait` blocks until an
//! event with a sequence number past a given point (and of a requested kind)
//! appears, or a timeout elapses — so events are never missed between two
//! sequential `wait` calls, which is what makes an agent's "ask, then wait for
//! the human's reply" loop reliable.

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::protocol::{Event, EventKind};

/// How many recent events to retain. A client that falls this far behind may
/// miss events; in practice a `wait` follows within milliseconds of the
/// sequence number it was handed.
const CAPACITY: usize = 512;

/// A thread-safe, bounded log of review events with a notification for waiters.
pub struct EventLog {
    state: Mutex<State>,
    signal: Condvar,
}

struct State {
    seq: u64,
    buffer: VecDeque<Event>,
}

impl EventLog {
    /// Create an empty log.
    pub fn new() -> EventLog {
        EventLog {
            state: Mutex::new(State {
                seq: 0,
                buffer: VecDeque::new(),
            }),
            signal: Condvar::new(),
        }
    }

    /// Append an event, waking any waiters. Returns its sequence number.
    pub fn append(&self, kind: EventKind, thread: Option<String>) -> u64 {
        let mut state = self.state.lock().unwrap();
        state.seq += 1;
        let event = Event {
            seq: state.seq,
            kind,
            thread,
            at: now(),
        };
        state.buffer.push_back(event);
        while state.buffer.len() > CAPACITY {
            state.buffer.pop_front();
        }
        let seq = state.seq;
        drop(state);
        self.signal.notify_all();
        seq
    }

    /// The most recent sequence number (0 when no events have occurred).
    pub fn latest_seq(&self) -> u64 {
        self.state.lock().unwrap().seq
    }

    /// Block until an event after `after` matching `kinds` (empty means any
    /// kind) is available, or `timeout` elapses (`None` waits forever). Returns
    /// the matching event (or `None` on timeout) together with the latest
    /// sequence number, so a caller can chain the next wait without a gap.
    pub fn wait(
        &self,
        kinds: &[EventKind],
        after: u64,
        timeout: Option<Duration>,
    ) -> (Option<Event>, u64) {
        let mut state = self.state.lock().unwrap();
        let deadline = timeout.map(|t| Instant::now() + t);
        loop {
            if let Some(event) = match_event(&state.buffer, kinds, after) {
                return (Some(event), state.seq);
            }
            match deadline {
                None => state = self.signal.wait(state).unwrap(),
                Some(deadline) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return (None, state.seq);
                    }
                    let (next, timed_out) = self
                        .signal
                        .wait_timeout(state, deadline - now)
                        .map(|(s, r)| (s, r.timed_out()))
                        .unwrap();
                    state = next;
                    if timed_out {
                        let event = match_event(&state.buffer, kinds, after);
                        return (event, state.seq);
                    }
                }
            }
        }
    }
}

impl Default for EventLog {
    fn default() -> EventLog {
        EventLog::new()
    }
}

/// The earliest buffered event past `after` whose kind is in `kinds` (or any
/// kind when `kinds` is empty).
fn match_event(buffer: &VecDeque<Event>, kinds: &[EventKind], after: u64) -> Option<Event> {
    buffer
        .iter()
        .find(|event| event.seq > after && (kinds.is_empty() || kinds.contains(&event.kind)))
        .cloned()
}

/// Seconds since the Unix epoch.
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn append_assigns_monotonic_sequence_numbers() {
        let log = EventLog::new();
        assert_eq!(log.append(EventKind::Comment, None), 1);
        assert_eq!(log.append(EventKind::Reply, Some("t".into())), 2);
        assert_eq!(log.latest_seq(), 2);
    }

    #[test]
    fn wait_returns_an_already_buffered_event() {
        let log = EventLog::new();
        log.append(EventKind::Comment, None);
        let seq = log.append(EventKind::Reply, Some("t1".into()));
        // Waiting for a reply after seq 1 finds the buffered one immediately.
        let (event, latest) = log.wait(&[EventKind::Reply], 1, Some(Duration::from_secs(1)));
        let event = event.expect("the buffered reply matches");
        assert_eq!(event.seq, seq);
        assert_eq!(event.thread.as_deref(), Some("t1"));
        assert_eq!(latest, seq);
    }

    #[test]
    fn wait_filters_by_kind() {
        let log = EventLog::new();
        log.append(EventKind::Comment, None);
        // Only a comment exists; waiting for a reply times out.
        let (event, _) = log.wait(&[EventKind::Reply], 0, Some(Duration::from_millis(50)));
        assert!(event.is_none());
    }

    #[test]
    fn wait_times_out_when_no_event_arrives() {
        let log = EventLog::new();
        let (event, latest) = log.wait(&[], 0, Some(Duration::from_millis(50)));
        assert!(event.is_none());
        assert_eq!(latest, 0);
    }

    #[test]
    fn wait_wakes_when_an_event_is_appended() {
        let log = Arc::new(EventLog::new());
        let waiter = {
            let log = log.clone();
            thread::spawn(move || log.wait(&[EventKind::Resolve], 0, Some(Duration::from_secs(5))))
        };
        // Give the waiter a moment to block, then publish.
        thread::sleep(Duration::from_millis(20));
        let seq = log.append(EventKind::Resolve, Some("t2".into()));
        let (event, _) = waiter.join().unwrap();
        let event = event.expect("the appended resolve wakes the waiter");
        assert_eq!(event.seq, seq);
        assert_eq!(event.kind, EventKind::Resolve);
    }

    #[test]
    fn empty_kinds_matches_any_event() {
        let log = EventLog::new();
        log.append(EventKind::Submit, None);
        let (event, _) = log.wait(&[], 0, Some(Duration::from_millis(50)));
        assert_eq!(event.unwrap().kind, EventKind::Submit);
    }
}
