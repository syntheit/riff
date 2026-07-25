use std::ops::Deref;
use std::rc::Rc;

use gtk::prelude::*;

use crate::app::components::EventListener;
use crate::app::models::{RemotePlayback, RepeatMode, SongDescription};
use crate::app::state::{BrowserAction, BrowserEvent, PlaybackAction, PlaybackEvent, ScreenName};
use crate::app::{ActionDispatcher, AppAction, AppEvent, AppModel, AppState, SongsSource, Worker};

use super::now_playing_full::NowPlayingFullWidget;

// The libadwaita 0.7 Rust binding predates AdwBottomSheet (1.6), so the sheet is
// created in the blueprint and driven here through the generic `open` property.
fn set_sheet_open(sheet: &gtk::Widget, open: bool) {
    sheet.set_property("open", open);
}

/// Header metadata for the current playback context. A mirrored remote track
/// only carries its album name, not a Spotify context URI, so it must never be
/// presented as a navigable local source.
struct SourceDisplay {
    name: String,
    navigable: bool,
}

pub struct NowPlayingSheetModel {
    app_model: Rc<AppModel>,
    dispatcher: Box<dyn ActionDispatcher>,
}

impl NowPlayingSheetModel {
    pub fn new(app_model: Rc<AppModel>, dispatcher: Box<dyn ActionDispatcher>) -> Self {
        Self {
            app_model,
            dispatcher,
        }
    }

    fn state(&self) -> impl Deref<Target = AppState> + '_ {
        self.app_model.get_state()
    }

    // The remote device we're currently mirroring, if the full player is in
    // remote-mirror mode. Transport presses drive THIS device via the Web API
    // directly (same path as the mini-player), instead of the local queue.
    fn mirrored_remote(&self) -> Option<RemotePlayback> {
        let state = self.state();
        if !state.playback.is_mirroring_remote() {
            return None;
        }
        state.playback.remote_playback().cloned()
    }

    // Run a transport call against the mirrored remote device via the Web API,
    // optimistically update the mirrored snapshot for instant UI feedback, then
    // request an immediate re-poll to reconcile. `endpoint` labels the call.
    fn remote_control<F>(
        &self,
        endpoint: &'static str,
        device_id: String,
        call: F,
        updated: Option<RemotePlayback>,
    ) where
        F: std::future::Future<Output = crate::api::SpotifyResult<()>> + Send + 'static,
    {
        if let Some(snapshot) = updated {
            self.dispatcher
                .dispatch(PlaybackAction::SetRemotePlayback(Some(snapshot)).into());
        }
        self.dispatcher.dispatch_async(Box::pin(async move {
            if let Err(err) = call.await {
                error!("remote transport failed: {}", err);
            }
            Some(AppAction::RepollRemoteMirror)
        }));
    }

    fn toggle_playback(&self) {
        if let Some(mut remote) = self.mirrored_remote() {
            let api = self.app_model.get_spotify();
            let id = remote.device.id.clone();
            let was_playing = remote.is_playing;
            remote.is_playing = !was_playing; // optimistic
            let endpoint = if was_playing { "pause" } else { "play" };
            let call = {
                let id = id.clone();
                async move {
                    if was_playing {
                        api.player_pause(id).await
                    } else {
                        api.player_resume(id).await
                    }
                }
            };
            self.remote_control(endpoint, id, call, Some(remote));
            return;
        }
        self.dispatcher.dispatch(PlaybackAction::TogglePlay.into());
    }

    fn play_next(&self) {
        if let Some(remote) = self.mirrored_remote() {
            let api = self.app_model.get_spotify();
            let id = remote.device.id.clone();
            let call = { let id = id.clone(); async move { api.player_next(id).await } };
            self.remote_control("next", id, call, None);
            return;
        }
        self.dispatcher.dispatch(PlaybackAction::Next.into());
    }

    fn play_prev(&self) {
        if let Some(remote) = self.mirrored_remote() {
            let api = self.app_model.get_spotify();
            let id = remote.device.id.clone();
            let call = { let id = id.clone(); async move { api.player_previous(id).await } };
            self.remote_control("previous", id, call, None);
            return;
        }
        self.dispatcher.dispatch(PlaybackAction::Previous.into());
    }

    fn toggle_shuffle(&self) {
        self.dispatcher
            .dispatch(PlaybackAction::ToggleShuffle.into());
    }

    fn toggle_repeat(&self) {
        self.dispatcher
            .dispatch(PlaybackAction::ToggleRepeat.into());
    }

    fn seek_to(&self, position: u32) {
        if let Some(mut remote) = self.mirrored_remote() {
            let api = self.app_model.get_spotify();
            let id = remote.device.id.clone();
            let pos = position as usize;
            remote.progress_ms = position; // optimistic
            let call = { let id = id.clone(); async move { api.player_seek(id, pos).await } };
            self.remote_control("seek", id, call, Some(remote));
            return;
        }
        self.dispatcher
            .dispatch(PlaybackAction::Seek(position).into());
    }

    fn is_playing(&self) -> bool {
        // Remote play-state while mirroring a remote device, else local.
        self.state().playback.displayed_is_playing()
    }

    fn is_shuffled(&self) -> bool {
        self.state().playback.is_shuffled()
    }

    fn repeat_mode(&self) -> RepeatMode {
        self.state().playback.repeat_mode()
    }

    fn current_song(&self) -> Option<SongDescription> {
        // Remote snapshot's track while mirroring a remote device, else local.
        self.state().playback.displayed_song()
    }

    fn is_current_song_liked(&self) -> bool {
        let state = self.state();
        let Some(song) = state.playback.current_song() else {
            return false;
        };
        state
            .browser
            .home_state()
            .map(|h| h.saved_tracks.get(&song.id).is_some())
            .unwrap_or(false)
    }

    fn show_add_to_playlist(&self) {
        if let Some(song) = self.current_song() {
            self.dispatcher.dispatch(AppAction::ShowAddToPlaylist(song));
        }
    }

    fn show_song_menu(&self) {
        if let Some(song) = self.current_song() {
            self.dispatcher.dispatch(AppAction::ShowSongMenu(song));
        }
    }

    fn view_album(&self) {
        if let Some(song) = self.current_song() {
            self.dispatcher.dispatch(AppAction::ViewAlbum(song.album.id.clone()));
        }
    }

    fn view_artist(&self) {
        if let Some(song) = self.current_song() {
            if let Some(artist) = song.artists.first() {
                self.dispatcher.dispatch(AppAction::ViewArtist(artist.id.clone()));
            }
        }
    }

    /// Header metadata for the currently displayed playback context. A remote
    /// snapshot only reuses a local playlist title after its context URI exactly
    /// matches; otherwise it shows its track's album and disables navigation. A
    /// local playlist carries its name in `SongsSource` when playback starts,
    /// rather than depending on an optional browser detail page that may not be
    /// open.
    fn source_display(&self) -> Option<SourceDisplay> {
        let state = self.state();
        let playback = &state.playback;

        if playback.is_mirroring_remote() {
            let remote = playback.remote_playback()?;
            // A remote snapshot may identify its context URI. Reuse a local
            // playlist title only when that URI exactly proves it is the same
            // playlist; an absent or different URI safely falls back to the
            // remote track's album and never exposes a stale navigation target.
            let name = playback
                .current_source()
                .filter(|source| source.matches_spotify_context(remote.context_uri.as_deref()))
                .and_then(|source| match source {
                    SongsSource::Playlist { title, .. } if !title.is_empty() => {
                        Some(title.clone())
                    }
                    _ => None,
                })
                .unwrap_or_else(|| remote.song.album.name.clone());
            return (!name.is_empty()).then(|| SourceDisplay {
                name,
                navigable: false,
            });
        }

        let song = playback.current_song()?;

        let Some(source) = playback.current_source() else {
            return (!song.album.name.is_empty()).then(|| SourceDisplay {
                name: song.album.name,
                navigable: false,
            });
        };

        let name = match source {
            SongsSource::Playlist { title, .. } => title.clone(),
            SongsSource::Album(_) => song.album.name,
            SongsSource::Artist(_) => song.artists_name(),
            SongsSource::SavedTracks | SongsSource::Radio { .. } => source
                .intrinsic_name()
                .expect("intrinsic playback sources always have a display name"),
        };

        (!name.is_empty()).then(|| SourceDisplay {
            name,
            navigable: true,
        })
    }

    /// Navigate to the current playback source (open its playlist/album/artist/
    /// Liked Songs/Radio screen), mirroring Spotify's tappable "Playing from X".
    /// Returns true when a navigation was dispatched (the caller then closes the
    /// now-playing sheet).
    fn navigate_to_source(&self) -> bool {
        let action = {
            let state = self.state();
            // A remote snapshot may carry a context URI for safe header display,
            // but it is never a navigation target from this player sheet.
            if state.playback.is_mirroring_remote() {
                return false;
            }
            let Some(source) = state.playback.current_source() else {
                return false;
            };
            match source {
                SongsSource::Album(id) => AppAction::ViewAlbum(id.clone()),
                SongsSource::Playlist { id, .. } => AppAction::ViewPlaylist(id.clone()),
                SongsSource::Artist(id) => AppAction::ViewArtist(id.clone()),
                SongsSource::SavedTracks => {
                    BrowserAction::NavigationPush(ScreenName::SavedTracks).into()
                }
                SongsSource::Radio { seed_id, seed_name } => {
                    BrowserAction::NavigationPush(ScreenName::Radio {
                        seed_id: seed_id.clone(),
                        seed_name: seed_name.clone(),
                    })
                    .into()
                }
            }
        };
        self.dispatcher.dispatch(action);
        true
    }
}

pub struct NowPlayingSheet {
    model: Rc<NowPlayingSheetModel>,
    widget: NowPlayingFullWidget,
    sheet: gtk::Widget,
    worker: Worker,
    // Last reported playback position, so the scrubber shows the right spot when
    // the sheet is opened mid-track (its clock/seek events fill in from there).
    last_position: u32,
}

impl NowPlayingSheet {
    pub fn new(
        model: NowPlayingSheetModel,
        sheet: gtk::Widget,
        queue_sheet: gtk::Widget,
        widget: NowPlayingFullWidget,
        worker: Worker,
    ) -> Self {
        let model = Rc::new(model);

        widget.connect_play_pause(clone!(
            #[weak]
            model,
            move || model.toggle_playback()
        ));
        widget.connect_next(clone!(
            #[weak]
            model,
            move || model.play_next()
        ));
        widget.connect_prev(clone!(
            #[weak]
            model,
            move || model.play_prev()
        ));
        widget.connect_shuffle(clone!(
            #[weak]
            model,
            move || model.toggle_shuffle()
        ));
        widget.connect_repeat(clone!(
            #[weak]
            model,
            move || model.toggle_repeat()
        ));
        widget.connect_seek(clone!(
            #[weak]
            model,
            move |position| model.seek_to(position)
        ));
        // Open the queue card over the player (player stays open behind it).
        widget.connect_queue(clone!(
            #[weak]
            queue_sheet,
            move || set_sheet_open(&queue_sheet, true)
        ));
        widget.connect_add_to_playlist(clone!(
            #[weak]
            model,
            move || model.show_add_to_playlist()
        ));
        widget.connect_show_menu(clone!(
            #[weak]
            model,
            move || {
                model.show_song_menu();
            }
        ));
        widget.connect_close(clone!(
            #[weak]
            sheet,
            move || set_sheet_open(&sheet, false)
        ));
        widget.connect_view_album(clone!(
            #[weak]
            model,
            #[weak]
            sheet,
            move || {
                model.view_album();
                set_sheet_open(&sheet, false);
            }
        ));
        widget.connect_view_artist(clone!(
            #[weak]
            model,
            #[weak]
            sheet,
            move || {
                model.view_artist();
                set_sheet_open(&sheet, false);
            }
        ));
        // Tapping the playback source navigates to it and closes the sheet.
        widget.connect_source(clone!(
            #[weak]
            model,
            #[weak]
            sheet,
            move || {
                if model.navigate_to_source() {
                    set_sheet_open(&sheet, false);
                }
            }
        ));

        Self {
            model,
            widget,
            sheet,
            worker,
            last_position: 0,
        }
    }

    fn update_current_info(&self) {
        if let Some(song) = self.model.current_song() {
            self.widget
                .set_title_and_artist(&song.title, &song.artists_name());
            self.widget.set_song_duration(Some(song.duration_ms as f64));
            if let Some(url) = song.art.as_ref().and_then(|s| s.best_for_width(320)) {
                self.widget
                    .set_artwork_from_url(url.to_owned(), &self.worker);
            }
        }
    }

    // Populate the whole view from current state — used when opening the sheet,
    // since the playback events that fill it may have fired before it was shown.
    fn sync_all(&self) {
        self.widget.set_playing(self.model.is_playing());
        self.widget.set_shuffled(self.model.is_shuffled());
        self.widget.set_repeat_mode(self.model.repeat_mode());
        self.update_current_info();
        self.widget.set_seek_position(self.last_position as f64);
        self.update_source();
    }

    // Reflect the current playback source in the header, hiding it when there is
    // no source metadata available.
    fn update_source(&self) {
        let source = self.model.source_display();
        self.widget.set_source(
            source
                .as_ref()
                .map(|source| (source.name.as_str(), source.navigable)),
        );
    }

}

impl EventListener for NowPlayingSheet {
    fn on_event(&mut self, event: &AppEvent) {
        match event {
            AppEvent::PlaybackEvent(PlaybackEvent::PlaybackPaused)
            | AppEvent::PlaybackEvent(PlaybackEvent::PlaybackResumed) => {
                self.widget.set_playing(self.model.is_playing());
            }
            AppEvent::PlaybackEvent(PlaybackEvent::RepeatModeChanged(mode)) => {
                self.widget.set_repeat_mode(*mode);
            }
            AppEvent::PlaybackEvent(PlaybackEvent::ShuffleChanged(_)) => {
                self.widget.set_shuffled(self.model.is_shuffled());
            }
            AppEvent::PlaybackEvent(PlaybackEvent::TrackChanged(_)) => {
                self.last_position = 0;
                self.widget.set_playing(self.model.is_playing());
                self.widget.set_liked(self.model.is_current_song_liked());
                self.update_current_info();
                self.update_source();
            }
            AppEvent::PlaybackEvent(PlaybackEvent::SourceChanged)
            | AppEvent::PlaybackEvent(PlaybackEvent::RemoteContextChanged) => {
                self.update_source();
            }
            AppEvent::PlaybackEvent(PlaybackEvent::PlaybackStopped) => {
                self.widget.set_playing(self.model.is_playing());
                self.update_current_info();
                self.update_source();
            }
            AppEvent::PlaybackEvent(PlaybackEvent::SeekSynced(pos))
            | AppEvent::PlaybackEvent(PlaybackEvent::TrackSeeked(pos)) => {
                self.last_position = *pos;
                self.widget.set_seek_position(*pos as f64);
            }
            AppEvent::BrowserEvent(BrowserEvent::SavedTracksUpdated) => {
                self.widget.set_liked(self.model.is_current_song_liked());
            }
            // A local playlist can have been started from Library or its
            // long-press menu, where no PlaylistDetailsState was pushed. Refresh
            // the header when either the saved-library card or detail metadata
            // arrives so it picks up the actual playlist title.
            AppEvent::BrowserEvent(BrowserEvent::SavedPlaylistsUpdated)
            | AppEvent::BrowserEvent(BrowserEvent::PlaylistDetailsLoaded(_))
            | AppEvent::BrowserEvent(BrowserEvent::ArtistDetailsUpdated(_)) => {
                self.update_source();
            }
            // Remote playback (on another device) changed while the sheet is open:
            // re-render the whole view from the mirrored snapshot.
            AppEvent::PlaybackEvent(PlaybackEvent::RemotePlaybackChanged) => {
                let progress = self
                    .model
                    .state()
                    .playback
                    .remote_playback()
                    .map(|r| r.progress_ms);
                if let Some(progress) = progress {
                    self.last_position = progress;
                }
                self.sync_all();
                self.widget.set_liked(self.model.is_current_song_liked());
            }
            AppEvent::NowPlayingSheetShown => {
                self.sync_all();
                self.widget.set_liked(self.model.is_current_song_liked());
                set_sheet_open(&self.sheet, true);
            }
            _ => {}
        }
    }
}
