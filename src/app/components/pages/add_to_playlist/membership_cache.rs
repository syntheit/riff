use serde::{Deserialize, Serialize};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

// On-disk membership cache: for each playlist we remember the snapshot_id it was
// fetched at and the full set of track ids it contained. An entry is valid only
// while the live snapshot_id still matches; a mismatch means the playlist changed
// and the entry must be re-fetched.
//
// Keyed lookups are cheap and the whole thing persists to a single JSON file in
// riff's cache dir, so membership answers survive restarts and accumulate as the
// user opens the drawer over time. The first launch on a fresh machine starts
// empty and fills in lazily, one visible/searched playlist at a time.

#[derive(Serialize, Deserialize, Clone)]
pub struct MembershipEntry {
    pub snapshot_id: String,
    pub track_ids: HashSet<String>,
}

#[derive(Serialize, Deserialize, Default)]
pub struct MembershipCache {
    entries: HashMap<String, MembershipEntry>,
}

impl MembershipCache {
    fn path() -> PathBuf {
        let mut path: PathBuf = glib::user_cache_dir();
        path.push("riff");
        glib::mkdir_with_parents(&path, 0o744);
        path.push("playlist_membership.json");
        path
    }

    // Read the cache from disk. A missing or malformed file yields an empty cache
    // rather than an error — the worst case is a round of lazy re-fetches.
    pub fn load() -> Self {
        let path = Self::path();
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) {
        let path = Self::path();
        if let Ok(bytes) = serde_json::to_vec(self) {
            let _ = std::fs::write(&path, bytes);
        }
    }

    // Whether the track is in the playlist according to a valid (snapshot-matching)
    // cache entry. Returns None when we have no valid entry and must fetch.
    pub fn contains(
        &self,
        playlist_id: &str,
        snapshot_id: Option<&str>,
        track_id: &str,
    ) -> Option<bool> {
        let entry = self.entries.get(playlist_id)?;
        match snapshot_id {
            Some(snap) if snap == entry.snapshot_id => Some(entry.track_ids.contains(track_id)),
            _ => None,
        }
    }

    pub fn track_ids(&self, playlist_id: &str) -> Option<&HashSet<String>> {
        self.entries.get(playlist_id).map(|e| &e.track_ids)
    }

    pub fn insert(&mut self, playlist_id: String, snapshot_id: String, track_ids: HashSet<String>) {
        self.entries.insert(
            playlist_id,
            MembershipEntry {
                snapshot_id,
                track_ids,
            },
        );
    }
}

// A shared, debounced handle around the cache. Reads/writes go through the inner
// RefCell; saves are coalesced so a burst of lazy fetches (e.g. scrolling the
// whole list) produces at most one disk write per debounce window.
#[derive(Clone)]
pub struct SharedMembershipCache {
    inner: Rc<RefCell<MembershipCache>>,
    save_pending: Rc<Cell<bool>>,
}

impl SharedMembershipCache {
    pub fn load() -> Self {
        Self {
            inner: Rc::new(RefCell::new(MembershipCache::load())),
            save_pending: Rc::new(Cell::new(false)),
        }
    }

    pub fn borrow(&self) -> std::cell::Ref<'_, MembershipCache> {
        self.inner.borrow()
    }

    pub fn borrow_mut(&self) -> std::cell::RefMut<'_, MembershipCache> {
        self.inner.borrow_mut()
    }

    // Schedule a save on the main context, collapsing repeated calls within the
    // window into a single write.
    pub fn schedule_save(&self) {
        if self.save_pending.replace(true) {
            return;
        }
        let inner = self.inner.clone();
        let pending = self.save_pending.clone();
        glib::timeout_add_local_once(Duration::from_secs(2), move || {
            pending.set(false);
            inner.borrow().save();
        });
    }
}
