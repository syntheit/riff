use std::cell::RefCell;
use std::collections::HashSet;
use std::ops::Deref;
use std::rc::Rc;
use std::time::Duration;

use crate::app::dispatch::ActionDispatcher;
use crate::app::models::*;
use crate::app::state::{AppAction, AppModel, BrowserAction, PlaybackAction};

pub struct SearchResultsModel {
    app_model: Rc<AppModel>,
    dispatcher: Box<dyn ActionDispatcher>,
    queued_song: RefCell<Option<SongDescription>>,
}

impl SearchResultsModel {
    pub fn new(app_model: Rc<AppModel>, dispatcher: Box<dyn ActionDispatcher>) -> Self {
        Self {
            queued_song: Default::default(),
            app_model,
            dispatcher,
        }
    }

    // Kept for parity with the other pages' models; the search screen is a bottom
    // tab (no header/back button) so nothing calls this today.
    #[allow(dead_code)]
    pub fn go_back(&self) {
        self.dispatcher
            .dispatch(BrowserAction::NavigationPop.into());
    }

    pub fn search(&self, query: String) {
        self.dispatcher
            .dispatch(BrowserAction::Search(query).into());
    }

    fn get_query(&self) -> Option<impl Deref<Target = String> + '_> {
        self.app_model
            .map_state_opt(|s| Some(&s.browser.search_state()?.query).filter(|s| !s.is_empty()))
    }

    pub fn fetch_results(&self) {
        let api = self.app_model.get_spotify();
        if let Some(query) = self.get_query() {
            let query = query.to_owned();
            self.dispatcher
                .call_spotify_and_dispatch(move || async move {
                    // Fetch a fuller page per type: the redesigned search shows one
                    // dense, unified list, so we want enough of each kind to fill it.
                    api.search(&query, 0, 20)
                        .await
                        .map(|results| BrowserAction::SetSearchResults(Box::new(results)).into())
                });
        }
    }

    pub fn get_results(&self) -> Option<impl Deref<Target = SearchResults> + '_> {
        self.app_model
            .map_state_opt(|s| Some(&s.browser.search_state()?.results))
    }

    /// Sets of ids that live in the logged-in user's own library, used to float
    /// matching search results to the top (like Spotify). Returns
    /// `(owned_playlist_ids, saved_album_ids, followed_artist_ids)`.
    ///
    /// - Owned playlists come from the authoritative login-state index
    ///   (`playlist_ids`, populated from the user's own playlists).
    /// - Saved albums / followed artists come from the home stores the Library
    ///   screen already primes; if those haven't loaded yet the sets are simply
    ///   empty (results still show, just unsorted for that type).
    pub fn library_ids(&self) -> (HashSet<String>, HashSet<String>, HashSet<String>) {
        let state = self.app_model.get_state();
        let owned_playlists = state.logged_user.playlist_ids.clone();
        let (saved_albums, followed_artists) = match state.browser.home_state() {
            Some(home) => (
                home.albums.iter().map(|c| c.id()).collect(),
                home.artists.iter().map(|c| c.id()).collect(),
            ),
            None => (HashSet::new(), HashSet::new()),
        };
        (owned_playlists, saved_albums, followed_artists)
    }
    pub fn open_track(&self, song: SongDescription) {
        self.queued_song.borrow_mut().replace(song.clone());
        self.dispatcher
            .dispatch(AppAction::ViewAlbum(song.album.id.clone()));
    }
    pub fn on_album_loaded(&self, id: &str) {
        if let Some(song) = self.queued_song.borrow_mut().take() {
            if song.album.id == id {
                self.dispatcher
                    .dispatch(BrowserAction::PlaySong(song.id.clone()).into())
            }
        }
    }

    pub fn open_album(&self, id: String) {
        self.dispatcher.dispatch(AppAction::ViewAlbum(id));
    }

    pub fn open_artist(&self, id: String) {
        self.dispatcher.dispatch(AppAction::ViewArtist(id));
    }

    pub fn open_playlist(&self, id: String) {
        self.dispatcher.dispatch(AppAction::ViewPlaylist(id));
    }
}
