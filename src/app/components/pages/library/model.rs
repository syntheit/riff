use std::cell::{Cell, Ref, RefCell};
use std::ops::Deref;
use std::rc::Rc;

use gettextrs::gettext;

use crate::app::components::{CardListModel, ImageShape};
use crate::app::models::{CardKind, CardModel};
use crate::app::state::{HomeState, ScreenName};
use crate::app::{ActionDispatcher, AppAction, AppModel, BrowserAction, ListStore};

/// Sentinel id for the synthetic pinned "Liked Songs" row. Opening it pushes the
/// saved-tracks page rather than resolving against a Spotify item id.
const LIKED_SONGS_ID: &str = "__riff_liked_songs__";

/// Which slice of the library is currently shown. `All` unifies every type;
/// the others narrow to a single saved-content type. Not persisted — Spotify
/// resets the filter each visit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LibraryFilter {
    #[default]
    All,
    Playlists,
    Albums,
    Artists,
}

/// Read-model for the unified library screen.
///
/// Keeps the four separate `HomeState` stores untouched (each has its own
/// pagination/API) and exposes a filtered view over them. Single-type filters
/// return that type's existing `ListStore` directly; the `All` filter returns a
/// combined store that is reconciled in place (append-only, no rebind) so
/// paging doesn't flash the FlowBox.
pub struct LibraryModel {
    app_model: Rc<AppModel>,
    dispatcher: Box<dyn ActionDispatcher>,
    filter: Cell<LibraryFilter>,
    combined: RefCell<ListStore<CardModel>>,
}

impl LibraryModel {
    pub fn new(app_model: Rc<AppModel>, dispatcher: Box<dyn ActionDispatcher>) -> Self {
        Self {
            app_model,
            dispatcher,
            filter: Cell::new(LibraryFilter::All),
            combined: RefCell::new(ListStore::new()),
        }
    }

    pub fn filter(&self) -> LibraryFilter {
        self.filter.get()
    }

    pub fn set_filter(&self, filter: LibraryFilter) {
        self.filter.set(filter);
    }

    fn state(&self) -> Option<Ref<'_, HomeState>> {
        self.app_model.map_state_opt(|s| s.browser.home_state())
    }

    /// Kick off (or refresh) every underlying store. The individual pages own
    /// their own fetch cadence; here we prime all three at once.
    pub fn refresh_all(&self) {
        let api = self.app_model.get_spotify();
        let Some(state) = self.state() else {
            return;
        };
        let albums_batch = state.next_albums_page.batch_size;
        let playlists_batch = state.next_playlists_page.batch_size;
        drop(state);

        self.dispatcher
            .call_spotify_and_dispatch(move || async move {
                api.get_saved_albums(0, albums_batch)
                    .await
                    .map(|albums| BrowserAction::SetLibraryContent(albums).into())
            });
        let api = self.app_model.get_spotify();
        self.dispatcher
            .call_spotify_and_dispatch(move || async move {
                api.get_saved_playlists(0, playlists_batch)
                    .await
                    .map(|playlists| BrowserAction::SetPlaylistsContent(playlists).into())
            });
        let api = self.app_model.get_spotify();
        self.dispatcher
            .call_spotify_and_dispatch(move || async move {
                let (artists, cursor) = api.get_followed_artists(None, 30).await?;
                Ok(BrowserAction::SetSavedArtists(artists, cursor).into())
            });
    }

    /// Whether any underlying store still has an unfetched page for the current
    /// filter. Under `All` this is true while any of the three has more.
    fn has_more_albums(&self) -> bool {
        self.state()
            .map(|s| s.next_albums_page.next_offset.is_some())
            .unwrap_or(false)
    }

    fn has_more_playlists(&self) -> bool {
        self.state()
            .map(|s| s.next_playlists_page.next_offset.is_some())
            .unwrap_or(false)
    }

    fn has_more_artists(&self) -> bool {
        self.state()
            .map(|s| s.artists_cursor.as_ref().is_some_and(|c| !c.is_empty()))
            .unwrap_or(false)
    }

    fn load_more_albums(&self) {
        let Some(state) = self.state() else { return };
        let batch_size = state.next_albums_page.batch_size;
        let Some(offset) = state.next_albums_page.next_offset else {
            return;
        };
        drop(state);
        self.app_model.update_state(
            BrowserAction::ConsumeNextPage(crate::app::PaginationTarget::SavedAlbums).into(),
        );
        let api = self.app_model.get_spotify();
        self.dispatcher
            .call_spotify_and_dispatch(move || async move {
                api.get_saved_albums(offset, batch_size)
                    .await
                    .map(|albums| BrowserAction::AppendLibraryContent(albums).into())
            });
    }

    fn load_more_playlists(&self) {
        let Some(state) = self.state() else { return };
        let batch_size = state.next_playlists_page.batch_size;
        let Some(offset) = state.next_playlists_page.next_offset else {
            return;
        };
        drop(state);
        self.app_model.update_state(
            BrowserAction::ConsumeNextPage(crate::app::PaginationTarget::SavedPlaylists).into(),
        );
        let api = self.app_model.get_spotify();
        self.dispatcher
            .call_spotify_and_dispatch(move || async move {
                api.get_saved_playlists(offset, batch_size)
                    .await
                    .map(|playlists| BrowserAction::AppendPlaylistsContent(playlists).into())
            });
    }

    fn load_more_artists(&self) {
        let Some(state) = self.state() else { return };
        let cursor = state.artists_cursor.clone();
        drop(state);
        let after = match cursor {
            Some(ref c) if c.is_empty() => return,
            Some(c) => Some(c),
            None => return,
        };
        self.app_model.update_state(
            BrowserAction::ConsumeNextPage(crate::app::PaginationTarget::SavedArtists).into(),
        );
        let api = self.app_model.get_spotify();
        self.dispatcher
            .call_spotify_and_dispatch(move || async move {
                let (artists, cursor) = api.get_followed_artists(after, 30).await?;
                Ok(BrowserAction::AppendSavedArtists(artists, cursor).into())
            });
    }

    /// Rebuild the combined store from the three source stores. Appends only the
    /// items that aren't already present (matched by id) so paging under `All`
    /// grows the store in place rather than replacing it (avoids scroll flash).
    /// The synthetic "Liked Songs" row is pinned first via its insertion order.
    pub fn reconcile_combined(&self) {
        let Some(state) = self.state() else { return };
        let mut combined = self.combined.borrow_mut();
        let existing: std::collections::HashSet<String> = combined.iter().map(|c| c.id()).collect();

        // Pin "Liked Songs" once, before any real content. Added first so the
        // CardList assigns it the lowest insertion position (sorts first under
        // the default Recent order).
        if !existing.contains(LIKED_SONGS_ID) {
            combined.extend(std::iter::once(liked_songs_card()));
        }

        let sources = [&state.playlists, &state.albums, &state.artists];
        let new_cards: Vec<CardModel> = sources
            .iter()
            .flat_map(|store| store.iter())
            .filter(|card| !existing.contains(&card.id()))
            .collect();
        if !new_cards.is_empty() {
            combined.extend(new_cards.into_iter());
        }
    }

    /// Drop everything from the combined store (used when leaving `All`).
    pub fn clear_combined(&self) {
        self.combined.borrow_mut().replace_all(std::iter::empty());
    }

    pub fn open_item(&self, id: String) {
        if id == LIKED_SONGS_ID {
            self.dispatcher
                .dispatch(BrowserAction::NavigationPush(ScreenName::SavedTracks).into());
            return;
        }
        // The combined store carries a mix of kinds; resolve by looking up the
        // card so we dispatch the correct view action.
        match self.card_kind_for(&id) {
            CardKind::Album => self.dispatcher.dispatch(AppAction::ViewAlbum(id)),
            CardKind::Artist => self.dispatcher.dispatch(AppAction::ViewArtist(id)),
            _ => self.dispatcher.dispatch(AppAction::ViewPlaylist(id)),
        }
    }

    fn card_kind_for(&self, id: &str) -> CardKind {
        let Some(state) = self.state() else {
            return CardKind::None;
        };
        for store in [&state.playlists, &state.albums, &state.artists] {
            if let Some(card) = store.iter().find(|c| c.id() == id) {
                return card.card_kind();
            }
        }
        CardKind::None
    }

    /// The backing `gio::ListStore` for the current filter, so the virtualized view
    /// can point its model chain straight at it (updates propagate via
    /// `items-changed` — no mirroring). Returns None before login/state exists.
    pub fn current_source_store(&self) -> Option<gio::ListStore> {
        Some(CardListModel::get_store(self)?.inner().clone())
    }

    /// Find the `CardModel` for an id across every source store. Used by the
    /// long-press drawer to render the item card and pick the right unsave/unfollow
    /// action. The synthetic "Liked Songs" row is resolved separately by the caller.
    pub fn card_for(&self, id: &str) -> Option<CardModel> {
        let state = self.state()?;
        for store in [&state.playlists, &state.albums, &state.artists] {
            if let Some(card) = store.iter().find(|c| c.id() == id) {
                return Some(card);
            }
        }
        None
    }

    /// User-facing empty-state copy for the current filter.
    pub fn empty_title(&self) -> String {
        match self.filter.get() {
            LibraryFilter::All => gettext("Your library is empty."),
            LibraryFilter::Playlists => gettext("You have no saved playlists."),
            LibraryFilter::Albums => gettext("You have no saved albums."),
            LibraryFilter::Artists => gettext("You have no followed artists."),
        }
    }

    pub fn empty_description(&self) -> String {
        match self.filter.get() {
            LibraryFilter::All => gettext("Saved albums, playlists and artists appear here."),
            LibraryFilter::Playlists => gettext("Your playlists will be shown here."),
            LibraryFilter::Albums => gettext("Your saved albums will be shown here."),
            LibraryFilter::Artists => gettext("Your followed artists will be shown here."),
        }
    }
}

impl CardListModel for LibraryModel {
    fn get_store(&self) -> Option<impl Deref<Target = ListStore<CardModel>> + '_> {
        match self.filter.get() {
            LibraryFilter::All => Some(StoreRef::Combined(self.combined.borrow())),
            LibraryFilter::Playlists => {
                Some(StoreRef::State(Ref::map(self.state()?, |s| &s.playlists)))
            }
            LibraryFilter::Albums => Some(StoreRef::State(Ref::map(self.state()?, |s| &s.albums))),
            LibraryFilter::Artists => {
                Some(StoreRef::State(Ref::map(self.state()?, |s| &s.artists)))
            }
        }
    }

    fn refresh(&self) {
        self.refresh_all();
    }

    fn has_items(&self) -> bool {
        self.get_store().map(|s| s.len() > 0).unwrap_or(false)
    }

    fn has_more(&self) -> bool {
        match self.filter.get() {
            LibraryFilter::All => {
                self.has_more_albums() || self.has_more_playlists() || self.has_more_artists()
            }
            LibraryFilter::Playlists => self.has_more_playlists(),
            LibraryFilter::Albums => self.has_more_albums(),
            LibraryFilter::Artists => self.has_more_artists(),
        }
    }

    fn load_more(&self) {
        // Under `All`, page whichever source still has more, one at a time.
        match self.filter.get() {
            LibraryFilter::All => {
                if self.has_more_playlists() {
                    self.load_more_playlists();
                } else if self.has_more_albums() {
                    self.load_more_albums();
                } else if self.has_more_artists() {
                    self.load_more_artists();
                }
            }
            LibraryFilter::Playlists => self.load_more_playlists(),
            LibraryFilter::Albums => self.load_more_albums(),
            LibraryFilter::Artists => self.load_more_artists(),
        }
    }

    fn open_item(&self, id: String) {
        LibraryModel::open_item(self, id);
    }

    fn image_shape(&self) -> ImageShape {
        // Default shape; per-item round images (artists) override this in the
        // FlowBox child factory via `CardModel::is_round`.
        ImageShape::Square
    }
}

/// Build the pinned "Liked Songs" pseudo-item. It carries the sentinel id so the
/// FlowBox activation resolves to the saved-tracks page, and reads as a playlist.
fn liked_songs_card() -> CardModel {
    CardModel::new(
        LIKED_SONGS_ID,
        None,
        &gettext("Liked Songs"),
        "",
        None,
        None,
        None,
    )
    .with_kind(CardKind::Playlist)
}

/// A `Deref` target that can point either at a borrowed `HomeState` store or the
/// model's own combined store, so `get_store` can return both from one signature.
enum StoreRef<'a> {
    State(Ref<'a, ListStore<CardModel>>),
    Combined(Ref<'a, ListStore<CardModel>>),
}

impl Deref for StoreRef<'_> {
    type Target = ListStore<CardModel>;
    fn deref(&self) -> &Self::Target {
        match self {
            StoreRef::State(r) => r,
            StoreRef::Combined(r) => r,
        }
    }
}
