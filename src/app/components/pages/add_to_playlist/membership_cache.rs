use serde::{Deserialize, Serialize};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

// On-disk membership index: for each owned playlist we remember the snapshot_id it
// was indexed at and the full set of track ids it contained. An entry is stale once
// the live snapshot_id no longer matches; a background sync re-fetches those.
//
// The set holds both the track id and its `linked_from` id (when the track was
// relinked per market), so a membership test matches either. Persisting to a single
// JSON file in riff's cache dir means later launches only re-fetch the playlists
// that actually changed.

// Schema version of the on-disk membership index. Bump this whenever the format
// or the indexing logic changes in a way that makes older caches wrong: `load()`
// discards any file whose `version` differs, forcing a clean re-sync. This is the
// guard against a bad build silently poisoning the cache with incomplete data and
// having `is_fresh` keep serving it. Bumped from the implicit v0 (unversioned).
const MEMBERSHIP_CACHE_VERSION: u32 = 1;

#[derive(Serialize, Deserialize, Clone)]
pub struct MembershipEntry {
    pub snapshot_id: String,
    pub track_ids: HashSet<String>,
}

#[derive(Serialize, Deserialize)]
pub struct MembershipIndex {
    // Serialized schema tag. Defaults to 0 when absent (an old, unversioned file),
    // which never equals the current version, so such files are discarded on load.
    #[serde(default)]
    version: u32,
    entries: HashMap<String, MembershipEntry>,
}

impl Default for MembershipIndex {
    fn default() -> Self {
        Self {
            version: MEMBERSHIP_CACHE_VERSION,
            entries: HashMap::new(),
        }
    }
}

impl MembershipIndex {
    fn path() -> PathBuf {
        let mut path: PathBuf = glib::user_cache_dir();
        path.push("riff");
        glib::mkdir_with_parents(&path, 0o744);
        path.push("playlist_membership.json");
        path
    }

    // Read the index from disk. A missing, malformed, or version-mismatched file
    // yields an empty (current-version) index rather than an error — the worst case
    // is a full re-sync. Discarding on version mismatch is what stops an incompatible
    // cache (e.g. one an old build wrote with incomplete data) from being served.
    pub fn load() -> Self {
        let path = Self::path();
        let parsed: Self = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
            Err(_) => return Self::default(),
        };
        if parsed.version != MEMBERSHIP_CACHE_VERSION {
            return Self::default();
        }
        parsed
    }

    pub fn save(&self) {
        let path = Self::path();
        if let Ok(bytes) = serde_json::to_vec(self) {
            let _ = std::fs::write(&path, bytes);
        }
    }

    // Whether the current entry (if any) was indexed at this snapshot. A miss means
    // the playlist still needs syncing.
    pub fn is_fresh(&self, playlist_id: &str, snapshot_id: Option<&str>) -> bool {
        match (self.entries.get(playlist_id), snapshot_id) {
            (Some(entry), Some(snap)) => entry.snapshot_id == snap,
            _ => false,
        }
    }

    // Synchronous membership test used by the drawer rows. Returns false when the
    // playlist has not been indexed yet (cold run); the background sync fills it in.
    pub fn contains(&self, playlist_id: &str, track_id: &str) -> bool {
        self.entries
            .get(playlist_id)
            .map(|e| e.track_ids.contains(track_id))
            .unwrap_or(false)
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

// Lightweight description of an owned playlist the drawer can list without a fetch.
// Populated by the background sync from the authoritative saved-playlists response.
#[derive(Clone)]
pub struct OwnedPlaylist {
    pub id: String,
    pub title: String,
    pub art: Option<String>,
    pub snapshot_id: Option<String>,
}

// Shared, debounced handle around the index plus the owned-playlist list. Both the
// background sync and the drawer hold a clone; reads/writes go through the inner
// RefCells. Saves are coalesced so a burst of sequential syncs produces at most one
// disk write per debounce window.
#[derive(Clone)]
pub struct SharedMembershipCache {
    index: Rc<RefCell<MembershipIndex>>,
    owned: Rc<RefCell<Vec<OwnedPlaylist>>>,
    syncing: Rc<Cell<bool>>,
    save_pending: Rc<Cell<bool>>,
}

impl SharedMembershipCache {
    pub fn load() -> Self {
        Self {
            index: Rc::new(RefCell::new(MembershipIndex::load())),
            owned: Rc::new(RefCell::new(Vec::new())),
            syncing: Rc::new(Cell::new(false)),
            save_pending: Rc::new(Cell::new(false)),
        }
    }

    pub fn index(&self) -> std::cell::Ref<'_, MembershipIndex> {
        self.index.borrow()
    }

    pub fn index_mut(&self) -> std::cell::RefMut<'_, MembershipIndex> {
        self.index.borrow_mut()
    }

    // The owned playlists the drawer should list, as last resolved by the sync.
    pub fn owned_playlists(&self) -> Vec<OwnedPlaylist> {
        self.owned.borrow().clone()
    }

    pub fn set_owned_playlists(&self, playlists: Vec<OwnedPlaylist>) {
        *self.owned.borrow_mut() = playlists;
    }

    // Whether a background sync is currently walking the owned playlists. Drives the
    // subtle "indexing…" hint while some rows may still read as unchecked.
    pub fn is_syncing(&self) -> bool {
        self.syncing.get()
    }

    pub fn set_syncing(&self, syncing: bool) {
        self.syncing.set(syncing);
    }

    // Schedule a save on the main context, collapsing repeated calls within the
    // window into a single write.
    pub fn schedule_save(&self) {
        if self.save_pending.replace(true) {
            return;
        }
        let index = self.index.clone();
        let pending = self.save_pending.clone();
        glib::timeout_add_local_once(Duration::from_secs(2), move || {
            pending.set(false);
            index.borrow().save();
        });
    }
}
