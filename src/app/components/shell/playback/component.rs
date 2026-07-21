use std::ops::Deref;
use std::rc::Rc;

use crate::app::components::EventListener;
use crate::app::models::*;
use crate::app::state::{BrowserEvent, PlaybackAction, PlaybackEvent, SelectionEvent};
use crate::app::{ActionDispatcher, AppAction, AppEvent, AppModel, AppState, Worker};

use super::playback_widget::PlaybackWidget;

pub struct PlaybackModel {
    app_model: Rc<AppModel>,
    dispatcher: Box<dyn ActionDispatcher>,
}

impl PlaybackModel {
    pub fn new(app_model: Rc<AppModel>, dispatcher: Box<dyn ActionDispatcher>) -> Self {
        Self {
            app_model,
            dispatcher,
        }
    }

    fn state(&self) -> impl Deref<Target = AppState> + '_ {
        self.app_model.get_state()
    }

    fn open_now_playing_sheet(&self) {
        self.dispatcher.dispatch(AppAction::ShowNowPlayingSheet);
    }

    fn is_playing(&self) -> bool {
        // Reflect remote play-state while mirroring a remote device, else local.
        self.state().playback.displayed_is_playing()
    }

    fn is_shuffled(&self) -> bool {
        self.state().playback.is_shuffled()
    }

    fn current_song(&self) -> Option<SongDescription> {
        // The track to show: remote snapshot's track while mirroring, else local.
        self.app_model.get_state().playback.displayed_song()
    }

    // Whether the mini-player is currently mirroring a remote device's playback.
    fn is_mirroring_remote(&self) -> bool {
        self.state().playback.is_mirroring_remote()
    }

    // Current progress (ms) of the displayed track: from the remote snapshot when
    // mirroring (so the mini-bar shows the right spot), else 0 (local seek events
    // drive the local case).
    fn remote_progress_ms(&self) -> Option<u32> {
        self.state()
            .playback
            .remote_playback()
            .filter(|_| self.is_mirroring_remote())
            .map(|r| r.progress_ms)
    }

    // The remote device we're currently mirroring (id + snapshot), if the mini-
    // player is in remote-mirror mode. Transport presses drive THIS device via
    // the Web API directly (no local queue involved).
    fn mirrored_remote(&self) -> Option<RemotePlayback> {
        let state = self.state();
        if !state.playback.is_mirroring_remote() {
            return None;
        }
        state.playback.remote_playback().cloned()
    }

    // Run a transport call against the mirrored remote device via the Web API,
    // then optimistically update the mirrored snapshot so the UI reacts instantly
    // (the ~4s poll will reconcile). `update` mutates the local snapshot copy.
    fn remote_control<F>(&self, device_id: String, call: F, updated: Option<RemotePlayback>)
    where
        F: std::future::Future<Output = crate::api::SpotifyResult<()>> + Send + 'static,
    {
        eprintln!("RIFF_CONNECT: mini-player driving remote device={device_id}");
        if let Some(snapshot) = updated {
            self.dispatcher
                .dispatch(PlaybackAction::SetRemotePlayback(Some(snapshot)).into());
        }
        self.dispatcher.dispatch_async(Box::pin(async move {
            if let Err(err) = call.await {
                error!("remote transport failed: {}", err);
            }
            None
        }));
    }

    fn play_next_song(&self) {
        if let Some(remote) = self.mirrored_remote() {
            let api = self.app_model.get_spotify();
            let id = remote.device.id.clone();
            let call = { let id = id.clone(); async move { api.player_next(id).await } };
            self.remote_control(id, call, None);
            return;
        }
        self.dispatcher.dispatch(PlaybackAction::Next.into());
    }

    fn play_prev_song(&self) {
        if let Some(remote) = self.mirrored_remote() {
            let api = self.app_model.get_spotify();
            let id = remote.device.id.clone();
            let call = { let id = id.clone(); async move { api.player_previous(id).await } };
            self.remote_control(id, call, None);
            return;
        }
        self.dispatcher.dispatch(PlaybackAction::Previous.into());
    }

    fn toggle_playback(&self) {
        if let Some(mut remote) = self.mirrored_remote() {
            let api = self.app_model.get_spotify();
            let id = remote.device.id.clone();
            let was_playing = remote.is_playing;
            remote.is_playing = !was_playing; // optimistic
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
            self.remote_control(id, call, Some(remote));
            return;
        }
        self.dispatcher.dispatch(PlaybackAction::TogglePlay.into());
    }

    fn toggle_shuffle(&self) {
        // Shuffle isn't exposed on the mirrored bar path; drive locally as before
        // (only reachable when not mirroring).
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
            self.remote_control(id, call, Some(remote));
            return;
        }
        self.dispatcher
            .dispatch(PlaybackAction::Seek(position).into());
    }

    fn set_volume(&self, value: f64) {
        if let Some(remote) = self.mirrored_remote() {
            let api = self.app_model.get_spotify();
            let id = remote.device.id.clone();
            let vol = (value * 100f64).trunc() as u8;
            let call = { let id = id.clone(); async move { api.player_volume(id, vol).await } };
            self.remote_control(id, call, None);
            return;
        }
        self.dispatcher
            .dispatch(PlaybackAction::SetVolume(value).into())
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
}

pub struct PlaybackControl {
    model: Rc<PlaybackModel>,
    widget: PlaybackWidget,
    worker: Worker,
}

impl PlaybackControl {
    pub fn new(model: PlaybackModel, widget: PlaybackWidget, worker: Worker) -> Self {
        let model = Rc::new(model);

        widget.connect_play_pause(clone!(
            #[weak]
            model,
            move || model.toggle_playback()
        ));
        widget.connect_next(clone!(
            #[weak]
            model,
            move || model.play_next_song()
        ));
        widget.connect_prev(clone!(
            #[weak]
            model,
            move || model.play_prev_song()
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
        widget.connect_now_playing_clicked(clone!(
            #[weak]
            model,
            move || model.open_now_playing_sheet()
        ));
        widget.connect_add_to_playlist(clone!(
            #[weak]
            model,
            move || model.show_add_to_playlist()
        ));
        widget.connect_volume_changed(clone!(
            #[weak]
            model,
            move |value| model.set_volume(value)
        ));

        let control = Self {
            model,
            widget,
            worker,
        };
        // Start hidden until a track loads.
        control.update_current_info();
        control
    }

    fn update_repeat(&self, mode: &RepeatMode) {
        self.widget.set_repeat_mode(*mode);
    }

    fn update_shuffled(&self) {
        self.widget.set_shuffled(self.model.is_shuffled());
    }

    fn update_playing(&self) {
        let is_playing = self.model.is_playing();
        self.widget.set_playing(is_playing);
    }

    fn update_current_info(&self) {
        if let Some(song) = self.model.current_song() {
            self.widget.set_mini_player_visible(true);
            self.widget
                .set_title_and_artist(&song.title, &song.artists_name());
            self.widget.set_song_duration(Some(song.duration_ms as f64));
            if let Some(url) = song.art.as_ref().and_then(|s| s.best_for_width(120)) {
                self.widget
                    .set_artwork_from_url(url.to_owned(), &self.worker);
            }
        } else {
            self.widget.set_mini_player_visible(false);
            self.widget.reset_info();
        }
    }

    fn sync_seek(&self, pos: u32) {
        self.widget.set_seek_position(pos as f64);
    }

    // Re-render the mini-player from the mirrored remote-playback snapshot: track
    // info, play/pause, and the current progress spot. Also handles the snapshot
    // being cleared (falls back to the local current song, or hides the bar).
    fn update_remote(&self) {
        self.update_current_info();
        self.update_playing();
        self.widget.set_liked(self.model.is_current_song_liked());
        if let Some(pos) = self.model.remote_progress_ms() {
            self.widget.set_seek_position(pos as f64);
        }
    }
}

impl EventListener for PlaybackControl {
    fn on_event(&mut self, event: &AppEvent) {
        match event {
            AppEvent::PlaybackEvent(PlaybackEvent::PlaybackPaused)
            | AppEvent::PlaybackEvent(PlaybackEvent::PlaybackResumed) => {
                self.update_playing();
            }
            AppEvent::PlaybackEvent(PlaybackEvent::RepeatModeChanged(mode)) => {
                self.update_repeat(mode);
            }
            AppEvent::PlaybackEvent(PlaybackEvent::ShuffleChanged(_)) => {
                self.update_shuffled();
            }
            AppEvent::PlaybackEvent(PlaybackEvent::TrackChanged(_)) => {
                self.update_playing();
                self.update_current_info();
                self.widget.set_liked(self.model.is_current_song_liked());
            }
            AppEvent::PlaybackEvent(PlaybackEvent::PlaybackStopped) => {
                self.update_playing();
                self.update_current_info();
            }
            AppEvent::PlaybackEvent(PlaybackEvent::SeekSynced(pos))
            | AppEvent::PlaybackEvent(PlaybackEvent::TrackSeeked(pos)) => {
                self.sync_seek(*pos);
            }
            AppEvent::SelectionEvent(SelectionEvent::SelectionModeChanged(active)) => {
                self.widget.set_seekbar_visible(!active);
            }
            AppEvent::PlaybackEvent(PlaybackEvent::VolumeSet(value)) => {
                self.widget.set_volume(*value)
            }
            AppEvent::BrowserEvent(BrowserEvent::SavedTracksUpdated) => {
                self.widget.set_liked(self.model.is_current_song_liked());
            }
            // Remote playback appeared / changed / cleared — mirror it (or fall
            // back to local) in the mini-player bar.
            AppEvent::PlaybackEvent(PlaybackEvent::RemotePlaybackChanged) => {
                self.update_remote();
            }
            _ => {}
        }
    }
}
