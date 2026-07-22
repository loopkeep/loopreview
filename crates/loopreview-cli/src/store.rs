//! Local review store: one JSON document per repository under the user's config
//! directory. The store is keyed by the repository's shared git directory, so a
//! repo's worktrees share one review (the anchors' commit SHAs disambiguate).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use loopreview_core::Review;

use crate::config::config_dir;

/// The on-disk schema version, bumped when the format changes.
const SCHEMA_VERSION: u32 = 1;

/// The on-disk document: a [`Review`] plus a version and the repo it belongs to
/// (the repo path is recorded for humans inspecting the file).
#[derive(Serialize, Deserialize)]
struct StoreDoc {
    version: u32,
    repo: String,
    review: Review,
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

    /// Load the review, returning an empty one when the file does not exist.
    pub fn load(&self) -> Result<Review> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => {
                let doc: StoreDoc = serde_json::from_str(&text)
                    .with_context(|| format!("parsing review store {}", self.path.display()))?;
                Ok(doc.review)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Review::default()),
            Err(e) => {
                Err(e).with_context(|| format!("reading review store {}", self.path.display()))
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

    /// Save the review, creating the directory and writing atomically (via a
    /// temp file and rename) so a crash never truncates the store.
    pub fn save(&self, review: &Review) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let doc = StoreDoc {
            version: SCHEMA_VERSION,
            repo: self.repo.clone(),
            review: review.clone(),
        };
        let json = serde_json::to_string_pretty(&doc)?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, json).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("replacing {}", self.path.display()))?;
        Ok(())
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
}
