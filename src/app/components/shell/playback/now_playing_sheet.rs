use std::ops::Deref;
use std::rc::Rc;

use gettextrs::gettext;
use gtk::prelude::*;

use crate::app::components::EventListener;
use crate::app::models::{RepeatMode, SongDescription};
use crate::app::state::{BrowserAction, BrowserEvent, PlaybackAction, PlaybackEvent, ScreenName};
use crate::app::{ActionDispatcher, AppAction, AppEvent, AppModel, AppState, SongsSource, Worker};

use super::now_playing_full::NowPlayingFullWidget;

// The libadwaita 0.7 Rust binding predates AdwBottomSheet (1.6), so the sheet is
// created in the blueprint and driven here through the generic `open` property.
fn set_sheet_open(sheet: &gtk::Widget, open: bool) {
    sheet.set_property("open", open);
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

    fn toggle_playback(&self) {
        self.dispatcher.dispatch(PlaybackAction::TogglePlay.into());
    }

    fn play_next(&self) {
        self.dispatcher.dispatch(PlaybackAction::Next.into());
    }

    fn play_prev(&self) {
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

    // Name of the device to show in "Playing on X": the mirrored remote device
    // (controller direction) or the switched-to Connect device; None when local.
    fn current_device_name(&self) -> Option<String> {
        self.state().playback.displayed_device_name()
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

    /// The "PLAYING FROM <TYPE>" / "<name>" pair for the current playback source,
    /// or None when there is no meaningful navigable source (nothing playing, or
    /// an ad-hoc queue with no context). The type label is translated here; the
    /// name is the source's own name where it carries one (Liked Songs, Radio),
    /// otherwise resolved from the matching detail screen in browser state, with
    /// the type label as a final fallback.
    fn source_display(&self) -> Option<(String, String)> {
        let state = self.state();
        let source = state.playback.current_source()?;

        // Only show the header while something is actually playing.
        if state.playback.current_song().is_none() {
            return None;
        }

        let type_label = translate_source_type(source);

        let name = source.intrinsic_name().or_else(|| match source {
            SongsSource::Album(id) => state
                .browser
                .details_state(id)
                .and_then(|s| s.content.as_ref())
                .map(|c| c.description.title.clone()),
            SongsSource::Playlist(id) => state
                .browser
                .playlist_details_state(id)
                .and_then(|s| s.playlist.as_ref())
                .map(|p| p.title.clone()),
            SongsSource::Artist(id) => state
                .browser
                .artist_state(id)
                .and_then(|s| s.artist.clone()),
            _ => None,
        });

        // Fall back to the (title-cased-ish) type label when the name is unknown.
        let name = name.unwrap_or_else(|| type_label.clone());
        // Temporary on-device confirmation of source-header resolution (issue #4).
        eprintln!(
            "RIFF_SRC: source_display -> type={type_label:?} name={name:?} source={source:?}"
        );
        Some((type_label, name))
    }

    /// Navigate to the current playback source (open its playlist/album/artist/
    /// Liked Songs/Radio screen), mirroring Spotify's tappable "Playing from X".
    /// Returns true when a navigation was dispatched (the caller then closes the
    /// now-playing sheet).
    fn navigate_to_source(&self) -> bool {
        let action = {
            let state = self.state();
            let Some(source) = state.playback.current_source() else {
                return false;
            };
            match source {
                SongsSource::Album(id) => AppAction::ViewAlbum(id.clone()),
                SongsSource::Playlist(id) => AppAction::ViewPlaylist(id.clone()),
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

/// Translate the source's stable English type key for the "PLAYING FROM <TYPE>"
/// caption.
fn translate_source_type(source: &SongsSource) -> String {
    match source {
        SongsSource::Playlist(_) => gettext("PLAYLIST"),
        SongsSource::Album(_) => gettext("ALBUM"),
        SongsSource::Artist(_) => gettext("ARTIST"),
        SongsSource::SavedTracks => gettext("LIKED SONGS"),
        SongsSource::Radio { .. } => gettext("RADIO"),
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
        widget.connect_close(clone!(
            #[weak]
            sheet,
            move || set_sheet_open(&sheet, false)
        ));
        // Tapping "Playing from <source>" navigates to that source and closes the
        // now-playing sheet — exactly like Spotify.
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
        self.update_playing_on();
        self.update_source();
    }

    // Reflect the current playback source in the tappable "Playing from" header,
    // hiding it when there's no navigable source.
    fn update_source(&self) {
        match self.model.source_display() {
            Some((type_label, name)) => {
                eprintln!("RIFF_SRC: update_source SHOW type={type_label:?} name={name:?}");
                self.widget
                    .set_source(Some((type_label.as_str(), name.as_str())))
            }
            None => {
                eprintln!("RIFF_SRC: update_source HIDE (no navigable source)");
                self.widget.set_source(None)
            }
        }
    }

    // Reflect the active device in the "Playing on <device>" label — the remote
    // device name when a Connect device is active, hidden when playing locally.
    fn update_playing_on(&self) {
        self.widget
            .set_playing_on(self.model.current_device_name().as_deref());
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
            AppEvent::PlaybackEvent(PlaybackEvent::SourceChanged) => {
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
            AppEvent::PlaybackEvent(PlaybackEvent::SwitchedDevice(_))
            | AppEvent::PlaybackEvent(PlaybackEvent::AvailableDevicesChanged) => {
                self.update_playing_on();
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
