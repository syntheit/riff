use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::Rc;

// Local playlist/album/artist pins.
//
// Spotify's real "pinned" library items are not exposed by the Web API, so riff
// keeps its own set of pinned item ids on disk. Pinned ids float to the top of the
// library list (above the active sort order), Spotify-style. Persisted to a single
// JSON file in riff's cache dir so pins survive restarts.
//
// This mirrors the membership_cache pattern (versioned, load()/save(), a shared
// Rc handle) so it slots into the codebase's existing local-state conventions.

/// Schema version of the on-disk pin file. A file whose `version` differs is
/// discarded on load (treated as empty) so a format change never resurrects stale
/// pins in the wrong shape.
const PINNED_STORE_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct PinnedFile {
    // Defaults to 0 when absent (an old, unversioned file), which never equals the
    // current version, so such files are discarded on load.
    #[serde(default)]
    version: u32,
    /// Pinned item ids, in the order the user pinned them (most-recent last).
    ids: Vec<String>,
}

impl Default for PinnedFile {
    fn default() -> Self {
        Self {
            version: PINNED_STORE_VERSION,
            ids: Vec::new(),
        }
    }
}

impl PinnedFile {
    fn path() -> PathBuf {
        let mut path: PathBuf = glib::user_cache_dir();
        path.push("riff");
        glib::mkdir_with_parents(&path, 0o744);
        path.push("pinned.json");
        path
    }

    fn load() -> Self {
        let path = Self::path();
        let parsed: Self = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
            Err(_) => return Self::default(),
        };
        if parsed.version != PINNED_STORE_VERSION {
            return Self::default();
        }
        parsed
    }

    fn save(&self) {
        let path = Self::path();
        if let Ok(bytes) = serde_json::to_vec(self) {
            let _ = std::fs::write(&path, bytes);
        }
    }
}

/// Shared, cheap-to-clone handle around the pin set. The library model and the
/// long-press drawer both hold a clone; every mutation persists immediately (the
/// set is tiny — a handful of ids — so debouncing is unnecessary).
#[derive(Clone)]
pub struct PinnedStore {
    /// The set for O(1) membership tests.
    set: Rc<RefCell<HashSet<String>>>,
    /// The insertion order, so a freshly-pinned item can sort after older pins.
    order: Rc<RefCell<Vec<String>>>,
}

impl PinnedStore {
    pub fn load() -> Self {
        let file = PinnedFile::load();
        let set: HashSet<String> = file.ids.iter().cloned().collect();
        Self {
            set: Rc::new(RefCell::new(set)),
            order: Rc::new(RefCell::new(file.ids)),
        }
    }

    pub fn is_pinned(&self, id: &str) -> bool {
        self.set.borrow().contains(id)
    }

    /// Pin `id` (no-op if already pinned). Persists immediately. Returns true if
    /// the set changed.
    pub fn pin(&self, id: &str) -> bool {
        if !self.set.borrow_mut().insert(id.to_string()) {
            return false;
        }
        self.order.borrow_mut().push(id.to_string());
        self.save();
        true
    }

    /// Unpin `id` (no-op if not pinned). Persists immediately. Returns true if the
    /// set changed.
    pub fn unpin(&self, id: &str) -> bool {
        if !self.set.borrow_mut().remove(id) {
            return false;
        }
        self.order.borrow_mut().retain(|x| x != id);
        self.save();
        true
    }

    /// Toggle the pin for `id`, returning the new pinned state.
    pub fn toggle(&self, id: &str) -> bool {
        if self.is_pinned(id) {
            self.unpin(id);
            false
        } else {
            self.pin(id);
            true
        }
    }

    /// The 0-based pin rank (order in which the item was pinned). Used as a stable
    /// tiebreaker so pinned items keep a consistent top-of-list order. Returns a
    /// large sentinel for unpinned ids so they always sort after pinned ones.
    pub fn rank(&self, id: &str) -> usize {
        self.order
            .borrow()
            .iter()
            .position(|x| x == id)
            .unwrap_or(usize::MAX)
    }

    fn save(&self) {
        let file = PinnedFile {
            version: PINNED_STORE_VERSION,
            ids: self.order.borrow().clone(),
        };
        file.save();
    }
}
