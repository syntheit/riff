use std::cell::Ref;
use std::rc::Rc;

use crate::app::models::CardKind;
use crate::app::state::{HomeState, ScreenName};
use crate::app::{ActionDispatcher, AppAction, AppModel, BrowserAction};

/// Number of items to request for each personal-data shelf.
const RECENTLY_PLAYED_LIMIT: usize = 50;
const TOP_ARTISTS_LIMIT: usize = 20;
const TOP_TRACKS_LIMIT: usize = 20;
/// How many albums to pull for the "Because you listen to <artist>" shelf.
const MADE_FOR_YOU_LIMIT: usize = 12;

/// Sentinel id for the pinned "Liked Songs" shortcut tile. Opening it pushes the
/// saved-tracks page rather than resolving against a Spotify item id.
pub const LIKED_SONGS_ID: &str = "__riff_liked_songs__";

/// Read-model / command surface for the Spotify-style home feed. Owns the async
/// fetches that fill the feed shelves and resolves card taps to navigation.
pub struct HomeFeedModel {
    app_model: Rc<AppModel>,
    dispatcher: Box<dyn ActionDispatcher>,
}

impl HomeFeedModel {
    pub fn new(app_model: Rc<AppModel>, dispatcher: Box<dyn ActionDispatcher>) -> Self {
        Self {
            app_model,
            dispatcher,
        }
    }

    pub fn state(&self) -> Option<Ref<'_, HomeState>> {
        self.app_model.map_state_opt(|s| s.browser.home_state())
    }

    /// Kick off every feed fetch. Each call is independent so a failing shelf
    /// (empty history, a 403 on an endpoint) just leaves its store empty and the
    /// shelf hides itself — the rest of the feed still loads.
    pub fn refresh_feed(&self) {
        // Recently played → track strip + "Jump back in" contexts (one fetch,
        // sliced two ways in the reducer).
        let api = self.app_model.get_spotify();
        self.dispatcher
            .call_spotify_and_dispatch(move || async move {
                api.recently_played(RECENTLY_PLAYED_LIMIT)
                    .await
                    .map(|(songs, contexts)| {
                        BrowserAction::SetRecentlyPlayed(songs, contexts).into()
                    })
            });

        // Top artists (round cards).
        let api = self.app_model.get_spotify();
        self.dispatcher
            .call_spotify_and_dispatch(move || async move {
                api.get_top_artists(TOP_ARTISTS_LIMIT)
                    .await
                    .map(|artists| BrowserAction::SetTopArtists(artists).into())
            });

        // Top tracks.
        let api = self.app_model.get_spotify();
        self.dispatcher
            .call_spotify_and_dispatch(move || async move {
                api.get_top_tracks(TOP_TRACKS_LIMIT)
                    .await
                    .map(|songs| BrowserAction::SetTopTracks(songs).into())
            });

        // "From your library" reuses the shared saved-albums store.
        let api = self.app_model.get_spotify();
        self.dispatcher
            .call_spotify_and_dispatch(move || async move {
                api.get_saved_albums(0, 20)
                    .await
                    .map(|albums| BrowserAction::SetLibraryContent(albums).into())
            });

        self.refresh_made_for_you();
    }

    /// Seed the "Because you listen to <artist>" shelf from a top artist: fetch
    /// that artist's albums. Personal data end to end (top artist → their
    /// releases), so it survives every API-restriction wave.
    fn refresh_made_for_you(&self) {
        let api = self.app_model.get_spotify();
        self.dispatcher
            .call_spotify_and_dispatch(move || async move {
                let artists = api.get_top_artists(TOP_ARTISTS_LIMIT).await?;
                let Some(seed) = artists.into_iter().next() else {
                    return Ok(BrowserAction::SetMadeForYou(String::new(), vec![]).into());
                };
                let name = seed.name.clone();
                let albums = api
                    .get_artist_albums(&seed.id, 0, MADE_FOR_YOU_LIMIT)
                    .await?;
                Ok(BrowserAction::SetMadeForYou(name, albums).into())
            });
    }

    /// Resolve a shortcut/shelf card tap to a navigation action.
    pub fn open_item(&self, id: String, kind: CardKind) {
        if id == LIKED_SONGS_ID {
            self.dispatcher
                .dispatch(BrowserAction::NavigationPush(ScreenName::SavedTracks).into());
            return;
        }
        match kind {
            CardKind::Artist => self.dispatcher.dispatch(AppAction::ViewArtist(id)),
            CardKind::Playlist => self.dispatcher.dispatch(AppAction::ViewPlaylist(id)),
            // Albums and bare track cards both resolve to an album detail page.
            _ => self.dispatcher.dispatch(AppAction::ViewAlbum(id)),
        }
    }
}
