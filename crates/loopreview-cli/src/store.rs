//! Local review store: one JSON document per repository under the user's config
//! directory. The store is keyed by the repository's shared git directory, so a
//! repo's worktrees share one review (the anchors' commit SHAs disambiguate).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use std::collections::HashMap;

use loopreview_core::Review;

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
    /// truncates the store.
    fn write_doc(&self, doc: &StoreDoc) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let json = serde_json::to_string_pretty(doc)?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, json).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("replacing {}", self.path.display()))?;
        Ok(())
    }

    /// Load the working-tree review, empty when the file does not exist.
    pub fn load(&self) -> Result<Review> {
        Ok(self.read_doc()?.review)
    }

    /// Delete the store file, if it exists (closing the review).
    pub fn delete(&self) -> Result<()> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("removing {}", self.path.display())),
        }
    }

    /// Save the working-tree review, preserving any stored PR drafts.
    pub fn save(&self, review: &Review) -> Result<()> {
        let mut doc = self.read_doc()?;
        doc.review = review.clone();
        self.write_doc(&doc)
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
    pub fn save_pr_drafts(&self, key: &str, drafts: &Review) -> Result<()> {
        let mut doc = self.read_doc()?;
        if drafts.is_empty() {
            doc.pr_drafts.remove(key);
        } else {
            doc.pr_drafts.insert(key.to_string(), drafts.clone());
        }
        self.write_doc(&doc)
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
                }],
            }],
        };
        store.save(&review).unwrap();
        assert_eq!(store.load().unwrap(), review);
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
}
