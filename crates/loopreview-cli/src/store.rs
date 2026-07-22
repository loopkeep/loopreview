//! Local review store: one JSON document per repository under the user's config
//! directory. The store is keyed by the repository's shared git directory, so a
//! repo's worktrees share one review (the anchors' commit SHAs disambiguate).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use loopreview_core::{CommentKind, Review};

use crate::config::config_dir;

/// The on-disk schema version, bumped when the format changes.
const SCHEMA_VERSION: u32 = 1;

/// The on-disk document for one repository: the working-tree review plus, keyed
/// by `owner/repo#number`, the draft-only reviews for pull requests reviewed in
/// this repo (published PR comments are always re-pulled, never stored).
#[derive(Serialize, Deserialize)]
struct StoreDoc {
    version: u32,
    repo: String,
    review: Review,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pr_drafts: HashMap<String, Review>,
}

impl StoreDoc {
    fn empty(repo: String) -> StoreDoc {
        StoreDoc {
            version: SCHEMA_VERSION,
            repo,
            review: Review::default(),
            pr_drafts: HashMap::new(),
        }
    }
}

/// A handle to the review store file for one repository.
pub struct Store {
    path: PathBuf,
    repo: String,
}

impl Store {
    /// Construct a store at an explicit path (for tests, to keep them off the
    /// real config directory).
    #[cfg(test)]
    pub(crate) fn at(path: PathBuf, repo: impl Into<String>) -> Store {
        Store {
            path,
            repo: repo.into(),
        }
    }

    /// The store for the repository whose shared git directory is `common_dir`,
    /// or `None` when no config directory can be determined.
    pub fn for_repo(common_dir: &Path) -> Option<Store> {
        let repo = common_dir.to_string_lossy().into_owned();
        let key = repo_key(&repo);
        let dir = config_dir()?.join("loopreview").join("reviews");
        Some(Store {
            path: dir.join(format!("{key}.json")),
            repo,
        })
    }

    /// Read the whole document, or an empty one when the file does not exist.
    fn read_doc(&self) -> Result<StoreDoc> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => serde_json::from_str(&text)
                .with_context(|| format!("parsing review store {}", self.path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Ok(StoreDoc::empty(self.repo.clone()))
            }
            Err(e) => {
                Err(e).with_context(|| format!("reading review store {}", self.path.display()))
            }
        }
    }

    /// Write the document atomically (temp file + rename) so a crash never
    /// truncates the store. The temp name is unique per process and call so two
    /// writers never clobber each other's temp file.
    fn write_doc(&self, doc: &StoreDoc) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let json = serde_json::to_string_pretty(doc)?;
        static TMP_SEQ: AtomicU64 = AtomicU64::new(0);
        let tmp = self.path.with_extension(format!(
            "json.{}.{}.tmp",
            std::process::id(),
            TMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&tmp, json).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("replacing {}", self.path.display()))?;
        Ok(())
    }

    /// Read the document, apply `update`, and write it back — all while holding an
    /// advisory lock on a sibling `.lock` file, and re-reading inside the lock, so
    /// two TUIs sharing a repo's store (worktrees share one review) cannot lose
    /// each other's changes to a last-writer-wins overwrite. The lock is
    /// best-effort: if it cannot be taken (an exotic filesystem), the re-read plus
    /// per-id merge still avoids dropping another writer's threads.
    fn locked_update(&self, update: impl FnOnce(&mut StoreDoc)) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let lock_file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(self.path.with_extension("lock"))
            .ok();
        let mut lock = lock_file.map(fd_lock::RwLock::new);
        // Held for the whole read-modify-write below (drops before `lock`).
        let _guard = lock.as_mut().and_then(|l| l.write().ok());

        let mut doc = self.read_doc()?;
        update(&mut doc);
        self.write_doc(&doc)
    }

    /// Load the working-tree review, empty when the file does not exist.
    pub fn load(&self) -> Result<Review> {
        let mut review = self.read_doc()?.review;
        // A working-tree review never submits to GitHub, so all its comments are
        // local notes — mark them, regardless of what an older store recorded
        // (old data defaults to `Draft`; PR drafts, loaded separately, keep it).
        mark_local(&mut review);
        Ok(review)
    }

    /// Load the working-tree review, recovering from a corrupt or unreadable
    /// store rather than failing: the bad file is moved aside to `.bak` (never
    /// deleted) and an empty review is returned, with the backup path so the
    /// caller can warn. A review-store fault must not stop plain diff viewing.
    pub fn load_or_recover(&self) -> (Review, Option<PathBuf>) {
        match self.load() {
            Ok(review) => (review, None),
            Err(_) => {
                let backup = self.path.with_extension("json.bak");
                match std::fs::rename(&self.path, &backup) {
                    Ok(()) => (Review::default(), Some(backup)),
                    Err(_) => (Review::default(), None),
                }
            }
        }
    }

    /// Delete the store file, if it exists (closing the review).
    pub fn delete(&self) -> Result<()> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("removing {}", self.path.display())),
        }
    }

    /// Save the working-tree review, preserving any stored PR drafts and merging
    /// (by thread and comment id) with whatever another window may have written
    /// since this one loaded, so no comment is silently overwritten.
    pub fn save(&self, review: &Review) -> Result<()> {
        self.locked_update(|doc| merge_review(&mut doc.review, review))
    }

    /// Load the stored draft review for a pull request (keyed `owner/repo#N`).
    pub fn load_pr_drafts(&self, key: &str) -> Result<Review> {
        Ok(self
            .read_doc()?
            .pr_drafts
            .get(key)
            .cloned()
            .unwrap_or_default())
    }

    /// Save (or clear) the draft review for a pull request, preserving the rest.
    /// An empty draft set clears the entry (all submitted or discarded); a
    /// non-empty one is merged by id with any concurrently-written drafts.
    pub fn save_pr_drafts(&self, key: &str, drafts: &Review) -> Result<()> {
        self.locked_update(|doc| {
            if drafts.is_empty() {
                doc.pr_drafts.remove(key);
            } else {
                merge_review(doc.pr_drafts.entry(key.to_string()).or_default(), drafts);
            }
        })
    }

    /// Replace a pull request's stored draft set outright (not the union merge),
    /// clearing the entry when empty. Used after a submit to drop the drafts that
    /// were just published, so a repeat submit finds nothing and a re-pull won't
    /// duplicate them.
    pub fn replace_pr_drafts(&self, key: &str, drafts: &Review) -> Result<()> {
        self.locked_update(|doc| {
            if drafts.is_empty() {
                doc.pr_drafts.remove(key);
            } else {
                doc.pr_drafts.insert(key.to_string(), drafts.clone());
            }
        })
    }

    /// Remove a thread (or a single comment within it) from the working-tree
    /// review — a targeted deletion, unlike the union [`save`], so a withdrawn
    /// draft does not come back on the next merge. A thread emptied of comments
    /// is dropped.
    pub fn remove(&self, thread_id: &str, comment_id: Option<&str>) -> Result<()> {
        self.locked_update(|doc| remove_from(&mut doc.review, thread_id, comment_id))
    }

    /// The same targeted deletion for a pull request's draft set; the key is
    /// dropped when its last draft goes.
    pub fn remove_pr_draft(
        &self,
        key: &str,
        thread_id: &str,
        comment_id: Option<&str>,
    ) -> Result<()> {
        self.locked_update(|doc| {
            if let Some(review) = doc.pr_drafts.get_mut(key) {
                remove_from(review, thread_id, comment_id);
                if review.is_empty() {
                    doc.pr_drafts.remove(key);
                }
            }
        })
    }
}

/// Mark every comment in `review` as a local note (the working-tree review is
/// never submitted).
fn mark_local(review: &mut Review) {
    for thread in &mut review.threads {
        for comment in &mut thread.comments {
            comment.kind = CommentKind::Local;
        }
    }
}

/// Remove `thread_id` (or just `comment_id` within it) from `review`; a thread
/// left with no comments is dropped.
fn remove_from(review: &mut Review, thread_id: &str, comment_id: Option<&str>) {
    match comment_id {
        Some(cid) => {
            if let Some(thread) = review.threads.iter_mut().find(|t| t.id == thread_id) {
                thread.comments.retain(|c| c.id != cid);
            }
            review.threads.retain(|t| !t.comments.is_empty());
        }
        None => review.threads.retain(|t| t.id != thread_id),
    }
}

/// Merge `mine` into `into` by id: an incoming thread replaces the same-id thread
/// (merging their comments by id, keeping both sides' comments and taking the
/// incoming resolved state) or is appended when new. Threads present only in
/// `into` — another window's additions — are kept. Threads are never deleted
/// here; a review is only fully removed by deleting its store file.
fn merge_review(into: &mut Review, mine: &Review) {
    for thread in &mine.threads {
        match into.threads.iter_mut().find(|t| t.id == thread.id) {
            Some(existing) => {
                for comment in &thread.comments {
                    match existing.comments.iter_mut().find(|c| c.id == comment.id) {
                        Some(current) => *current = comment.clone(),
                        None => existing.comments.push(comment.clone()),
                    }
                }
                existing.comments.sort_by_key(|c| c.created_at);
                existing.state = thread.state;
                existing.anchor = thread.anchor.clone();
            }
            None => into.threads.push(thread.clone()),
        }
    }
}

/// A stable 64-bit FNV-1a hash of `repo`, hex-encoded, for the store filename.
fn repo_key(repo: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in repo.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use loopreview_core::{Anchor, Comment, Side, Thread, ThreadState};
    use std::sync::atomic::{AtomicU32, Ordering};

    fn temp_store() -> Store {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("lr-store-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Store {
            path: dir.join("review.json"),
            repo: "test-repo".to_string(),
        }
    }

    #[test]
    fn repo_key_is_stable_and_hex() {
        assert_eq!(repo_key("abc"), repo_key("abc"));
        assert_ne!(repo_key("abc"), repo_key("abd"));
        assert_eq!(repo_key("abc").len(), 16);
    }

    #[test]
    fn load_missing_store_is_empty() {
        let store = temp_store();
        assert!(store.load().unwrap().is_empty());
    }

    #[test]
    fn save_then_load_round_trips() {
        let store = temp_store();
        let review = Review {
            threads: vec![Thread {
                id: "t1".to_string(),
                anchor: Anchor::line("src/lib.rs", Side::New, 10),
                state: ThreadState::Open,
                comments: vec![Comment {
                    id: "c1".to_string(),
                    author: "tester".to_string(),
                    body: "looks off".to_string(),
                    created_at: 42,
                    remote_id: None,
                    kind: loopreview_core::CommentKind::Draft,
                }],
            }],
        };
        store.save(&review).unwrap();
        // load() normalizes a working-tree review's comments to local notes.
        let mut expected = review.clone();
        expected.threads[0].comments[0].kind = CommentKind::Local;
        assert_eq!(store.load().unwrap(), expected);
        let _ = std::fs::remove_dir_all(store.path.parent().unwrap());
    }

    #[test]
    fn working_tree_loads_local_while_pr_drafts_stay_draft() {
        let store = temp_store();
        let mk = |id: &str| Review {
            threads: vec![Thread {
                id: id.to_string(),
                anchor: Anchor::line("a", Side::New, 1),
                state: ThreadState::Open,
                comments: vec![Comment {
                    id: format!("{id}-c"),
                    author: "me".to_string(),
                    body: "b".to_string(),
                    created_at: 0,
                    remote_id: None,
                    kind: CommentKind::Draft, // as an old store (or a fresh save) records
                }],
            }],
        };
        store.save(&mk("wt")).unwrap();
        store.save_pr_drafts("owner/repo#1", &mk("pr")).unwrap();
        // The working-tree review comes back as local notes...
        assert!(store.load().unwrap().threads[0].comments[0].is_local());
        // ...while the PR draft set keeps its draft kind (it will be submitted).
        assert!(store.load_pr_drafts("owner/repo#1").unwrap().threads[0].comments[0].is_draft());
        let _ = std::fs::remove_dir_all(store.path.parent().unwrap());
    }

    #[test]
    fn pr_drafts_round_trip_and_coexist_with_the_review() {
        let store = temp_store();
        let worktree = Review {
            threads: vec![Thread {
                id: "wt".to_string(),
                anchor: Anchor::line("a", Side::New, 1),
                state: ThreadState::Open,
                comments: vec![Comment {
                    id: "c".to_string(),
                    author: "me".to_string(),
                    body: "b".to_string(),
                    created_at: 0,
                    remote_id: None,
                    // A working-tree review loads as local notes, so expect that.
                    kind: CommentKind::Local,
                }],
            }],
        };
        let drafts = Review {
            threads: vec![Thread {
                id: "d".to_string(),
                anchor: Anchor::line("a", Side::New, 2),
                state: ThreadState::Open,
                comments: vec![Comment {
                    id: "dc".to_string(),
                    author: "me".to_string(),
                    body: "draft".to_string(),
                    created_at: 0,
                    remote_id: None,
                    kind: loopreview_core::CommentKind::Draft,
                }],
            }],
        };
        store.save(&worktree).unwrap();
        store.save_pr_drafts("o/r#7", &drafts).unwrap();

        // Both coexist; saving one preserves the other.
        assert_eq!(store.load().unwrap(), worktree);
        assert_eq!(store.load_pr_drafts("o/r#7").unwrap(), drafts);
        assert!(store.load_pr_drafts("o/r#9").unwrap().is_empty());

        // Clearing the drafts (all submitted) removes the entry.
        store.save_pr_drafts("o/r#7", &Review::default()).unwrap();
        assert!(store.load_pr_drafts("o/r#7").unwrap().is_empty());
        assert_eq!(store.load().unwrap(), worktree);

        let _ = std::fs::remove_dir_all(store.path.parent().unwrap());
    }

    fn thread_of(id: &str, comments: &[(&str, &str)]) -> Thread {
        Thread {
            id: id.to_string(),
            anchor: Anchor::line("a.rs", Side::New, 1),
            state: ThreadState::Open,
            comments: comments
                .iter()
                .map(|(cid, body)| Comment {
                    id: cid.to_string(),
                    author: "me".to_string(),
                    body: body.to_string(),
                    created_at: 0,
                    remote_id: None,
                    kind: loopreview_core::CommentKind::Draft,
                })
                .collect(),
        }
    }

    #[test]
    fn concurrent_saves_merge_by_thread_and_comment_id() {
        let store = temp_store();
        // Two windows loaded the same empty store; each adds a different thread.
        store
            .save(&Review {
                threads: vec![thread_of("t1", &[("c1", "from A")])],
            })
            .unwrap();
        store
            .save(&Review {
                threads: vec![thread_of("t2", &[("c2", "from B")])],
            })
            .unwrap();
        // The second save must not drop the first window's thread.
        let mut ids: Vec<String> = store
            .load()
            .unwrap()
            .threads
            .iter()
            .map(|t| t.id.clone())
            .collect();
        ids.sort();
        assert_eq!(ids, vec!["t1".to_string(), "t2".to_string()]);

        // Two windows reply to the same thread with different comment ids; both
        // replies survive.
        store
            .save(&Review {
                threads: vec![thread_of("t1", &[("c1", "root"), ("ca", "A reply")])],
            })
            .unwrap();
        store
            .save(&Review {
                threads: vec![thread_of("t1", &[("c1", "root"), ("cb", "B reply")])],
            })
            .unwrap();
        let loaded = store.load().unwrap();
        let t1 = loaded.threads.iter().find(|t| t.id == "t1").unwrap();
        let mut cids: Vec<&str> = t1.comments.iter().map(|c| c.id.as_str()).collect();
        cids.sort_unstable();
        assert_eq!(cids, vec!["c1", "ca", "cb"]);

        let _ = std::fs::remove_dir_all(store.path.parent().unwrap());
    }

    #[test]
    fn load_or_recover_backs_up_a_corrupt_store() {
        let store = temp_store();
        std::fs::create_dir_all(store.path.parent().unwrap()).unwrap();
        std::fs::write(&store.path, b"{ not valid json").unwrap();

        let (review, backup) = store.load_or_recover();
        assert!(review.is_empty());
        let backup = backup.expect("a backup path is returned");
        assert!(backup.exists(), "the corrupt file is preserved");
        assert!(!store.path.exists(), "the corrupt file is moved aside");
        // A subsequent load starts clean.
        assert!(store.load().unwrap().is_empty());

        let _ = std::fs::remove_dir_all(store.path.parent().unwrap());
    }
}
