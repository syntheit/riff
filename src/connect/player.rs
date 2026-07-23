use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use futures::channel::mpsc::UnboundedSender;
use gettextrs::gettext;

use crate::api::{SpotifyApiClient, SpotifyApiError, SpotifyResult};
use crate::app::models::{ConnectPlayerState, RemotePlayback, RepeatMode, SongDescription};
use crate::app::state::{Device, PlaybackAction};
use crate::app::{AppAction, SongsSource};

#[derive(Debug)]
pub enum ConnectCommand {
    SetDevice(String),
    PlayerLoadInContext {
        source: SongsSource,
        offset: usize,
        song: String,
    },
    PlayerLoad {
        songs: Vec<String>,
        offset: usize,
    },
    PlayerResume,
    PlayerPause,
    PlayerStop,
    PlayerSeek(usize),
    PlayerRepeat(RepeatMode),
    PlayerShuffle(bool),
    PlayerSetVolume(u8),
    /// Enable/disable the remote-playback MIRROR poll (controller direction:
    /// showing what's playing on the user's OTHER devices). Toggled by the app
    /// based on visibility + whether riff itself is playing locally, so we only
    /// poll `GET /me/player` when it's worth it (battery hygiene).
    SetRemoteMirrorActive(bool),
    /// Enable/disable the low-rate TAKEOVER-watch poll (safety net for the Spirc
    /// push signal). Runs `GET /me/player` at a low cadence WHILE riff holds a
    /// local session, purely to notice when ANOTHER device becomes the active
    /// player — so riff can yield even if the Spirc deactivation edge is missed
    /// (e.g. the bare-Player fallback path with no Spirc events). Toggled ON while
    /// a local session is active, OFF once riff yields / stops.
    SetTakeoverWatchActive(bool),
}

pub struct ConnectPlayer {
    api: Arc<dyn SpotifyApiClient + Send + Sync>,
    action_sender: UnboundedSender<AppAction>,
    device_id: RwLock<Option<String>>,
    last_queue: RwLock<u64>,
    last_state: RwLock<ConnectPlayerState>,
    // Whether the remote-playback mirror poll should currently run. Driven by
    // `ConnectCommand::SetRemoteMirrorActive` from the app (visibility + local
    // playback). We also keep the poll alive for one extra tick after it goes
    // false so the snapshot gets cleared once.
    mirror_active: AtomicBool,
    // Whether a mirrored snapshot is currently published to the UI, so we only
    // dispatch a clearing `SetRemotePlayback(None)` once (not every idle tick).
    mirror_published: AtomicBool,
    // Whether the low-rate TAKEOVER-watch poll should run. Set while riff holds a
    // local session so we can still notice another device taking over even when
    // the mirror poll is idle (mirror + takeover-watch are mutually exclusive:
    // the mirror runs when riff is idle, the watch when riff is active).
    takeover_watch_active: AtomicBool,
    // riff's OWN Connect device name (e.g. "riff (fajita)"), used to recognize
    // riff itself in `GET /me/player` so "riff is active, just paused" is not
    // mistaken for another device taking over. Computed once at startup.
    own_device_name: String,
}

impl ConnectPlayer {
    pub fn new(
        api: Arc<dyn SpotifyApiClient + Send + Sync>,
        action_sender: UnboundedSender<AppAction>,
    ) -> Self {
        Self {
            api: api.clone(),
            action_sender,
            device_id: Default::default(),
            last_queue: Default::default(),
            last_state: Default::default(),
            mirror_active: AtomicBool::new(false),
            mirror_published: AtomicBool::new(false),
            takeover_watch_active: AtomicBool::new(false),
            own_device_name: crate::player::connect_device_name(),
        }
    }

    pub fn mirror_active(&self) -> bool {
        self.mirror_active.load(Ordering::Relaxed)
    }

    fn set_mirror_active(&self, active: bool) {
        self.mirror_active.store(active, Ordering::Relaxed);
    }

    pub fn takeover_watch_active(&self) -> bool {
        self.takeover_watch_active.load(Ordering::Relaxed)
    }

    fn set_takeover_watch_active(&self, active: bool) {
        self.takeover_watch_active.store(active, Ordering::Relaxed);
    }

    fn send_actions(&self, actions: impl IntoIterator<Item = AppAction>) {
        for action in actions.into_iter() {
            self.action_sender.unbounded_send(action).unwrap();
        }
    }

    fn device_lost(&self) {
        let _ = self.device_id.write().unwrap().take();
        self.send_actions([
            AppAction::ShowNotification(gettext("Connection to device lost!")),
            PlaybackAction::SwitchDevice(Device::Local).into(),
            PlaybackAction::SetAvailableDevices(vec![]).into(),
        ]);
    }

    async fn get_queue_if_changed(&self) -> Option<Vec<SongDescription>> {
        let last_queue = *self.last_queue.read().ok().as_deref().unwrap_or(&0u64);
        let songs = self.api.get_player_queue().await.ok();
        songs.filter(|songs| {
            let hash = {
                let mut hasher = DefaultHasher::new();
                songs.hash(&mut hasher);
                hasher.finish()
            };
            if let Some(last_queue) = self.last_queue.try_write().ok().as_deref_mut() {
                *last_queue = hash;
            }
            hash != last_queue
        })
    }

    async fn apply_remote_state(&self, state: &ConnectPlayerState) {
        if let Some(songs) = self.get_queue_if_changed().await {
            self.send_actions([PlaybackAction::LoadSongs(songs).into()]);
        }

        let play_pause = if state.is_playing {
            PlaybackAction::Load(state.current_song_id.clone().unwrap())
        } else {
            PlaybackAction::Pause
        };

        self.send_actions([
            play_pause.into(),
            PlaybackAction::SetRepeatMode(state.repeat).into(),
            PlaybackAction::SetShuffled(state.shuffle).into(),
            PlaybackAction::SyncSeek(state.progress_ms).into(),
        ]);
    }

    pub fn has_device(&self) -> bool {
        self.device_id
            .read()
            .map(|it| it.is_some())
            .unwrap_or(false)
    }

    pub async fn sync_state(&self) {
        debug!("polling connect device...");
        let player_state = self.api.player_state().await;
        let Ok(state) = player_state else {
            self.device_lost();
            return;
        };
        self.apply_remote_state(&state).await;
        if let Ok(mut last_state) = self.last_state.write() {
            *last_state = state;
        }
    }

    // Poll `GET /me/player` and mirror whatever is playing on the user's OTHER
    // devices into riff's display state. Controller direction only. No-op while
    // the user has explicitly SWITCHED to a Connect device (then `sync_state`
    // owns the display through the main queue) — this only surfaces remote
    // playback the user hasn't taken over.
    pub async fn poll_remote_snapshot(&self) {
        // If we're actively controlling a switched-to device, don't also mirror.
        if self.has_device() {
            self.clear_mirror_if_published();
            return;
        }

        match self.api.get_player_snapshot().await {
            Ok(Some(snapshot)) => {
                let remote: RemotePlayback = snapshot.into();
                self.mirror_published.store(true, Ordering::Relaxed);
                self.send_actions([PlaybackAction::SetRemotePlayback(Some(remote)).into()]);
            }
            Ok(None) => {
                // Nothing playing anywhere remote.
                self.clear_mirror_if_published();
            }
            Err(SpotifyApiError::TooManyRequests) => {
                debug!("mirror poll rate-limited; backing off");
            }
            Err(err) => {
                debug!("mirror poll failed: {err}");
                self.clear_mirror_if_published();
            }
        }
    }

    // TAKEOVER-watch safety net: while riff holds a local session, poll
    // `GET /me/player` at a low cadence purely to notice when ANOTHER device
    // becomes the active player. This backstops the Spirc push signal (which is
    // immediate but absent on the bare-Player fallback path). If the active
    // device reported by Spotify is present, PLAYING, and is NOT riff itself
    // (matched by device id when we control one, else by device NAME against our
    // own Connect name) → riff has lost active status → dispatch YieldToRemote so
    // the app clears the sticky flags and re-enables the mirror. When the active
    // device IS riff (or nothing else is actively playing) we do nothing: riff
    // stays the sticky local output (e.g. it's just paused).
    pub async fn poll_takeover(&self) {
        // If the user explicitly switched to control a device, the mirror/sync
        // path owns the display; the takeover watch is only for the local-session
        // case, so bail.
        if self.has_device() {
            return;
        }
        match self.api.get_player_snapshot().await {
            Ok(Some(snapshot)) => {
                // Only a device that is actively PLAYING counts as a takeover —
                // a paused snapshot of riff itself (or a stale idle device) must
                // not yank the user away from their sticky local session.
                let is_self = snapshot.device_name == self.own_device_name;
                if snapshot.is_playing && !is_self {
                    // Publish the snapshot alongside the yield so the mirror has
                    // something to show the instant the flags clear (the yield
                    // handler's refresh_remote_mirror will re-poll too).
                    let remote: RemotePlayback = snapshot.into();
                    self.mirror_published.store(true, Ordering::Relaxed);
                    self.send_actions([
                        PlaybackAction::YieldToRemote.into(),
                        PlaybackAction::SetRemotePlayback(Some(remote)).into(),
                    ]);
                }
            }
            // Nothing playing / no active device / a transient error: riff keeps
            // its sticky local session; the next tick re-checks.
            Ok(None) => {}
            Err(SpotifyApiError::TooManyRequests) => {
                debug!("takeover-watch poll rate-limited; backing off");
            }
            Err(err) => {
                debug!("takeover-watch poll failed: {err}");
            }
        }
    }

    // Clear any published remote snapshot (exactly once). Called when remote
    // playback stops, when we switch to controlling a device, or when the mirror
    // is disabled — so the UI falls back to local display.
    fn clear_mirror_if_published(&self) {
        if self.mirror_published.swap(false, Ordering::Relaxed) {
            self.send_actions([PlaybackAction::SetRemotePlayback(None).into()]);
        }
    }

    async fn handle_player_load_in_context(
        &self,
        device_id: String,
        current_state: &ConnectPlayerState,
        command: ConnectCommand,
    ) -> SpotifyResult<()> {
        let ConnectCommand::PlayerLoadInContext {
            source,
            offset,
            song,
        } = command
        else {
            panic!("Illegal call");
        };
        let is_diff_song = current_state
            .current_song_id
            .as_ref()
            .map(|it| it != &song)
            .unwrap_or(true);
        let is_paused = !current_state.is_playing;
        if is_diff_song {
            let context = source.spotify_uri().unwrap();
            self.api
                .player_play_in_context(device_id, context, offset)
                .await
        } else if is_paused {
            self.api.player_resume(device_id).await
        } else {
            Ok(())
        }
    }

    async fn handle_player_load(
        &self,
        device_id: String,
        current_state: &ConnectPlayerState,
        command: ConnectCommand,
    ) -> SpotifyResult<()> {
        let ConnectCommand::PlayerLoad { songs, offset } = command else {
            panic!("Illegal call");
        };
        let is_diff_song = current_state
            .current_song_id
            .as_ref()
            .map(|it| it != &songs[offset])
            .unwrap_or(true);
        let is_paused = !current_state.is_playing;
        if is_diff_song {
            self.api
                .player_play_no_context(
                    device_id,
                    songs
                        .into_iter()
                        .map(|s| format!("spotify:track:{}", s))
                        .collect(),
                    offset,
                )
                .await
        } else if is_paused {
            self.api.player_resume(device_id).await
        } else {
            Ok(())
        }
    }

    async fn handle_other_command(
        &self,
        device_id: String,
        command: ConnectCommand,
    ) -> SpotifyResult<()> {
        let state = self.last_state.read();
        let Ok(state) = state.as_deref() else {
            return Ok(());
        };
        match command {
            ConnectCommand::PlayerLoadInContext { .. } => {
                self.handle_player_load_in_context(device_id, state, command)
                    .await
            }
            ConnectCommand::PlayerLoad { .. } => {
                self.handle_player_load(device_id, state, command).await
            }
            ConnectCommand::PlayerResume if !state.is_playing => {
                self.api.player_resume(device_id).await
            }
            ConnectCommand::PlayerPause if state.is_playing => {
                self.api.player_pause(device_id).await
            }
            ConnectCommand::PlayerSeek(offset) => self.api.player_seek(device_id, offset).await,
            ConnectCommand::PlayerRepeat(mode) => self.api.player_repeat(device_id, mode).await,
            ConnectCommand::PlayerShuffle(shuffle) => {
                self.api.player_shuffle(device_id, shuffle).await
            }
            ConnectCommand::PlayerSetVolume(volume) => {
                self.api.player_volume(device_id, volume).await
            }
            _ => Ok(()),
        }
    }

    pub async fn handle_command(&self, command: ConnectCommand) -> Option<()> {
        let device_lost = match command {
            ConnectCommand::SetDevice(new_device_id) => {
                self.device_id.write().ok()?.replace(new_device_id);
                // We now control this device directly; the mirror must yield so
                // the two displays don't fight (sync_state drives the main queue).
                self.clear_mirror_if_published();
                self.sync_state().await;
                false
            }
            ConnectCommand::PlayerStop => {
                let device_id = self.device_id.write().ok()?.take();
                if let Some(old_id) = device_id {
                    let _ = self.api.player_pause(old_id).await;
                }
                false
            }
            ConnectCommand::SetRemoteMirrorActive(active) => {
                self.set_mirror_active(active);
                if active {
                    // Mirroring means riff is NOT the active local player, so the
                    // takeover watch is unnecessary; disable it to avoid a
                    // double-poll while both could be on across a transition.
                    self.set_takeover_watch_active(false);
                    // Poll immediately so the UI reflects remote playback without
                    // waiting for the next tick (startup / becoming visible).
                    self.poll_remote_snapshot().await;
                } else {
                    // Stopped mirroring (riff took over locally / went hidden):
                    // drop the snapshot so the UI falls back to local display.
                    self.clear_mirror_if_published();
                }
                false
            }
            ConnectCommand::SetTakeoverWatchActive(active) => {
                // Only act on an actual on/off EDGE: `refresh_remote_mirror` re-
                // sends this on every local play-state transition, and we don't
                // want to fire an immediate `/me/player` poll on each one.
                let was_active = self.takeover_watch_active();
                self.set_takeover_watch_active(active);
                if active && !was_active {
                    // Just turned on: check once right away so a takeover that
                    // already happened (e.g. between the local play and this
                    // enable) is caught without waiting a full watch interval.
                    self.poll_takeover().await;
                }
                false
            }
            _ => {
                let device_id = self.device_id.read().ok()?.clone();
                if let Some(device_id) = device_id {
                    let result = self.handle_other_command(device_id, command).await;
                    if matches!(result, Err(SpotifyApiError::TooManyRequests)) {
                        self.send_actions([AppAction::ShowNotification(gettext(
                            // translators: This notification is shown when Spotify throttles requests.
                            "Rate limited by Spotify. Please wait a moment and try again.",
                        ))]);
                    }
                    matches!(result, Err(SpotifyApiError::BadStatus(404, _)))
                } else {
                    true
                }
            }
        };

        if device_lost {
            self.device_lost();
        }

        Some(())
    }
}
