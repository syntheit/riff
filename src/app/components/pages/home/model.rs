use std::cell::Ref;
use std::rc::Rc;

use crate::app::models::CardKind;
use crate::app::state::{HomeState, ScreenName};
use crate::app::{ActionDispatcher, AppAction, AppModel, BrowserAction};

/// Number of items to request for each personal-data shelf.
const RECENTLY_PLAYED_LIMIT: usize = 20;
const TOP_ARTISTS_LIMIT: usize = 20;
const TOP_TRACKS_LIMIT: usize = 20;
/// How many albums to pull for the "Because you listen to <artist>" shelf.
/// Dev-mode client ids have a hard cap of 10 on /v1/artists/{id}/albums.
const MADE_FOR_YOU_LIMIT: usize = 10;

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
    ///
    /// Errors from any shelf are swallowed silently (logged at error level but no
    /// user-facing toast). `call_spotify_and_dispatch_many` is used with closures
    /// that map `Err` to `Ok(vec![])` so the notification path is never reached.
    pub fn refresh_feed(&self) {
        eprintln!("RIFF_HOME: refresh_feed triggered");

        // Recently played → track strip + "Jump back in" contexts (one fetch,
        // sliced two ways in the reducer).
        let api = self.app_model.get_spotify();
        self.dispatcher
            .call_spotify_and_dispatch_many(move || async move {
                match api.recently_played(RECENTLY_PLAYED_LIMIT).await {
                    Ok((songs, contexts)) => {
                        eprintln!(
                            "RIFF_HOME: fetch recently_played -> {} songs, {} contexts",
                            songs.len(),
                            contexts.len()
                        );
                        Ok(vec![
                            BrowserAction::SetRecentlyPlayed(songs, contexts).into()
                        ])
                    }
                    Err(e) => {
                        error!("Home: recently_played failed (shelf hidden): {}", e);
                        Ok(vec![])
                    }
                }
            });

        // Top artists (round cards).
        let api = self.app_model.get_spotify();
        self.dispatcher
            .call_spotify_and_dispatch_many(move || async move {
                match api.get_top_artists(TOP_ARTISTS_LIMIT).await {
                    Ok(artists) => {
                        eprintln!("RIFF_HOME: fetch top_artists -> {} items", artists.len());
                        Ok(vec![BrowserAction::SetTopArtists(artists).into()])
                    }
                    Err(e) => {
                        error!("Home: get_top_artists failed (shelf hidden): {}", e);
                        Ok(vec![])
                    }
                }
            });

        // Top tracks.
        let api = self.app_model.get_spotify();
        self.dispatcher
            .call_spotify_and_dispatch_many(move || async move {
                match api.get_top_tracks(TOP_TRACKS_LIMIT).await {
                    Ok(songs) => {
                        eprintln!("RIFF_HOME: fetch top_tracks -> {} items", songs.len());
                        Ok(vec![BrowserAction::SetTopTracks(songs).into()])
                    }
                    Err(e) => {
                        error!("Home: get_top_tracks failed (shelf hidden): {}", e);
                        Ok(vec![])
                    }
                }
            });

        // "From your library" reuses the shared saved-albums store.
        let api = self.app_model.get_spotify();
        self.dispatcher
            .call_spotify_and_dispatch_many(move || async move {
                match api.get_saved_albums(0, 20).await {
                    Ok(albums) => {
                        eprintln!("RIFF_HOME: fetch saved_albums -> {} items", albums.len());
                        Ok(vec![BrowserAction::SetLibraryContent(albums).into()])
                    }
                    Err(e) => {
                        error!("Home: get_saved_albums failed (shelf hidden): {}", e);
                        Ok(vec![])
                    }
                }
            });

        self.refresh_made_for_you();
    }

    /// Seed the "Because you listen to <artist>" shelf from a top artist: fetch
    /// that artist's albums. Personal data end to end (top artist → their
    /// releases), so it survives every API-restriction wave.
    fn refresh_made_for_you(&self) {
        let api = self.app_model.get_spotify();
        self.dispatcher
            .call_spotify_and_dispatch_many(move || async move {
                let artists = match api.get_top_artists(TOP_ARTISTS_LIMIT).await {
                    Ok(a) => a,
                    Err(e) => {
                        error!(
                            "Home: made-for-you top_artists failed (shelf hidden): {}",
                            e
                        );
                        return Ok(vec![]);
                    }
                };
                let Some(seed) = artists.into_iter().next() else {
                    return Ok(vec![
                        BrowserAction::SetMadeForYou(String::new(), vec![]).into()
                    ]);
                };
                let name = seed.name.clone();
                let albums = match api.get_artist_albums(&seed.id, 0, MADE_FOR_YOU_LIMIT).await {
                    Ok(a) => a,
                    Err(e) => {
                        error!(
                            "Home: made-for-you get_artist_albums failed (shelf hidden): {}",
                            e
                        );
                        return Ok(vec![]);
                    }
                };
                eprintln!(
                    "RIFF_HOME: fetch made_for_you (seed='{}') -> {} items",
                    name,
                    albums.len()
                );
                Ok(vec![BrowserAction::SetMadeForYou(name, albums).into()])
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
