use std::borrow::Cow;
use std::collections::VecDeque;
use std::time::Instant;

use crate::app::models::*;
use crate::app::state::{AppAction, AppEvent, UpdatableState};
use crate::app::{LazyRandomIndex, SongsSource};

#[derive(Debug)]
pub struct PlaybackState {
    available_devices: Vec<ConnectDevice>,
    current_device: Device,
    // A mapping of indices for shuffled playback
    index: LazyRandomIndex,
    // The actual list like thing backing the currently playing tracks
    songs: SongListModel,
    list_position: Option<usize>,
    seek_position: PositionMillis,
    source: Option<SongsSource>,
    repeat: RepeatMode,
    is_playing: bool,
    is_shuffled: bool,
    // Last volume that was applied (0.0..=1.0). Initialised to a sentinel
    // outside the valid range so the first `SetVolume` always propagates.
    volume: f64,
    // Spotify-style manual queue: tracks the user explicitly queued, played in
    // order right after the current track, ahead of the context. While a queued
    // track plays it's held in `current_override` so `list_position` stays parked
    // on the context track and playback resumes there when the queue drains.
    manual_queue: VecDeque<SongDescription>,
    current_override: Option<SongDescription>,
    // What's playing on the user's OTHER (remote Connect) devices, mirrored from
    // polling `GET /me/player`. This is display-only state, kept separate from the
    // fields above so surfacing remote playback never hijacks riff's local queue.
    // `Some` only while a remote device is the active player; `None` otherwise.
    remote_playback: Option<RemotePlayback>,
    // Whether riff currently OWNS an active local playback session. Set TRUE when
    // the user starts playing something locally (a Load / play), and it STAYS true
    // across local pause/resume — a PAUSE must NOT hand the display back to a
    // remote device. Cleared only when the user explicitly moves away: transfers
    // to a Connect device, or the local session/queue is stopped. This is what
    // gates the remote MIRROR (not raw play/pause), so pausing local playback
    // doesn't get the user yanked back to the desktop.
    local_session_active: bool,
    // Whether riff is currently being driven as a Spotify Connect RECEIVER: a
    // remote Spotify app transferred playback TO riff, and librespot's Spirc now
    // owns the local `Player` (issues its own loads / next / seek). While true,
    // riff must NOT double-drive the `Player` from its own queue — transport UI
    // routes to the Spirc handle instead, and the librespot Player events are
    // MIRRORED into the fields above for display only. Flipped by the player
    // thread via `SetRemoteControlled` as Spirc activates / deactivates. This is
    // the "who owns the Player" switch that keeps riff's queue and Spirc from
    // fighting (see riff-connect.md §4.3, receiver mode).
    is_remote_controlled: bool,
}

// Most mutatings methods shouldn't be pub
// If they are, they probably are only used by the app state
impl PlaybackState {
    pub fn songs(&self) -> &SongListModel {
        &self.songs
    }

    pub fn is_playing(&self) -> bool {
        self.is_playing && (self.list_position.is_some() || self.current_override.is_some())
    }

    pub fn is_shuffled(&self) -> bool {
        self.is_shuffled
    }

    pub fn repeat_mode(&self) -> RepeatMode {
        self.repeat
    }

    fn index(&self, i: usize) -> Option<SongDescription> {
        let song = if self.is_shuffled {
            self.songs.index(self.index.get(i)?)
        } else {
            self.songs.index(i)
        };
        Some(song?.into_description())
    }

    pub fn current_source(&self) -> Option<&SongsSource> {
        self.source.as_ref()
    }

    pub fn current_song_index(&self) -> Option<usize> {
        self.list_position
    }

    pub fn current_song_id(&self) -> Option<String> {
        if let Some(song) = &self.current_override {
            return Some(song.id.clone());
        }
        Some(self.index(self.list_position?)?.id)
    }

    pub fn current_song(&self) -> Option<SongDescription> {
        if let Some(song) = &self.current_override {
            return Some(song.clone());
        }
        self.index(self.list_position?)
    }

    pub fn next_id(&self) -> Option<String> {
        if let Some(song) = self.manual_queue.front() {
            return Some(song.id.clone());
        }
        self.next_index()
            .and_then(|i| Some(self.songs().index(i)?.description().id.clone()))
    }

    fn clear(&mut self, source: Option<SongsSource>) -> SongListModelPending {
        self.source = source;
        self.index = Default::default();
        self.list_position = None;
        self.manual_queue.clear();
        self.current_override = None;
        self.songs.clear()
    }

    // Replaces (!) the current playlist with the contents of a song batch
    fn set_batch(&mut self, source: Option<SongsSource>, song_batch: SongBatch) -> bool {
        let ok = self.clear(source).and(|s| s.add(song_batch)).commit();
        self.index.resize(self.songs.partial_len());
        ok
    }

    fn add_batch(&mut self, song_batch: SongBatch) -> bool {
        let ok = self.songs.add(song_batch).commit();
        self.index.resize(self.songs.partial_len());
        ok
    }

    // Replaces (!) the current playlist with a bunch of songs (not batched, not expected to grow)
    fn set_queue(&mut self, tracks: Vec<SongDescription>) {
        self.clear(None).and(|s| s.append(tracks)).commit();
        self.index.grow(self.songs.len());
    }

    // Test-only helper to build up a context playlist (append tracks, grow the
    // shuffle index). Real "add to queue" goes through `queue_next`.
    #[cfg(test)]
    pub fn queue(&mut self, tracks: Vec<SongDescription>) {
        self.source = None;
        self.songs.append(tracks).commit();
        self.index.grow(self.songs.len());
    }

    // Spotify "add to queue": plays after the current track (and any earlier
    // queued tracks), then playback returns to the context.
    pub fn queue_next(&mut self, tracks: Vec<SongDescription>) {
        self.manual_queue.extend(tracks);
    }

    pub fn manual_queue(&self) -> impl Iterator<Item = &SongDescription> + '_ {
        self.manual_queue.iter()
    }

    // Move the manual-queue item with `id` to position `to` (0-based).
    // Adjusts `to` for the removal shift. Clamps `to` to the valid range; no-ops
    // when `id` is not found in the manual queue.
    pub fn move_in_queue(&mut self, id: &str, to: usize) {
        let from = match self.manual_queue.iter().position(|s| s.id == id) {
            Some(i) => i,
            None => return,
        };
        let song = self.manual_queue.remove(from).unwrap();
        // After removal the VecDeque is one shorter; clamp to the new length.
        let insert_at = to.min(self.manual_queue.len());
        self.manual_queue.insert(insert_at, song);
    }

    // The upcoming context tracks after the current one (for the queue view).
    pub fn next_context_tracks(&self, limit: usize) -> Vec<SongDescription> {
        let start = self.list_position.map(|p| p + 1).unwrap_or(0);
        (start..start.saturating_add(limit))
            .filter_map(|i| self.index(i))
            .collect()
    }

    pub fn dequeue(&mut self, ids: &[String]) {
        self.manual_queue.retain(|s| !ids.contains(&s.id));
        // Keep list_position parked on the same context track (ignore any override).
        let context_id = self.list_position.and_then(|p| self.index(p)).map(|s| s.id);
        self.songs.remove(ids).commit();
        self.list_position = context_id.and_then(|id| self.songs.find_index(&id));
        self.index.shrink(self.songs.len());
    }

    // Update the current playing track (identified by a position in the list) if we're swapping songs
    fn swap_pos(&mut self, index: usize, other_index: usize) {
        let len = self.songs.len();
        self.list_position = self
            .list_position
            .map(|position| match position {
                i if i == index => other_index,
                i if i == other_index => index,
                _ => position,
            })
            .map(|p| usize::min(p, len - 1))
    }

    pub fn move_down(&mut self, id: &str) -> Option<usize> {
        let index = self.songs.find_index(id)?;
        self.songs.move_down(index).commit();
        self.swap_pos(index + 1, index);
        Some(index)
    }

    pub fn move_up(&mut self, id: &str) -> Option<usize> {
        let index = self.songs.find_index(id).filter(|&index| index > 0)?;
        self.songs.move_up(index).commit();
        self.swap_pos(index - 1, index);
        Some(index)
    }

    fn play(&mut self, id: &str) -> bool {
        if self.current_song_id().map(|cur| cur == id).unwrap_or(false) {
            return false;
        }
        debug!("Playing {id}");

        let found_index = self.songs.find_index(id);

        if let Some(index) = found_index {
            // If shufflings songs, we make sure the track we just picked is the first to come up
            if self.is_shuffled {
                self.index.reset_picking_first(index);
                self.play_index(0);
            } else {
                self.play_index(index);
            }
            true
        } else {
            debug!("Song not found");
            false
        }
    }

    fn stop(&mut self) {
        self.list_position = None;
        self.manual_queue.clear();
        self.current_override = None;
        self.is_playing = false;
        self.seek_position.set(0, false);
        // The local session is over: the mirror may take back over.
        self.local_session_active = false;
    }

    fn play_index(&mut self, index: usize) -> Option<String> {
        // If a REMOTE device is the active output (and riff isn't the chosen local
        // output), this play is being ROUTED to that device — riff stays a remote
        // and keeps mirroring it, so we must NOT open a local session. Capture the
        // decision BEFORE mutating so it reflects the state at play-initiation.
        let routes_remote = self.active_remote_device().is_some();
        self.current_override = None;
        self.is_playing = true;
        self.list_position.replace(index);
        self.seek_position.set(0, true);
        self.index.next_until(index + 1);
        // Playing a LOCAL track opens (or keeps) a local session — this "wins"
        // over any mirrored remote device until the user explicitly leaves. When
        // routing to a remote device we deliberately leave `local_session_active`
        // untouched so the mirror keeps surfacing that device.
        if !routes_remote {
            self.local_session_active = true;
        }
        self.current_song_id()
    }

    fn play_next(&mut self) -> Option<String> {
        // Manual queue plays first, ahead of the context.
        if let Some(song) = self.manual_queue.pop_front() {
            let id = song.id.clone();
            let routes_remote = self.active_remote_device().is_some();
            self.current_override = Some(song);
            self.is_playing = true;
            self.seek_position.set(0, true);
            if !routes_remote {
                self.local_session_active = true;
            }
            return Some(id);
        }
        self.next_index().and_then(|i| {
            self.seek_position.set(0, true);
            self.play_index(i)
        })
    }

    pub fn next_index(&self) -> Option<usize> {
        // When shuffled, we can only play songs that are actually loaded
        let len = if self.is_shuffled {
            self.songs.partial_len()
        } else {
            self.songs.len()
        };
        self.list_position.and_then(|p| match self.repeat {
            RepeatMode::Song => Some(p),
            RepeatMode::Playlist if len != 0 => Some((p + 1) % len),
            RepeatMode::None => Some(p + 1).filter(|&i| i < len),
            _ => None,
        })
    }

    fn play_prev(&mut self) -> Option<String> {
        // Can't navigate backwards into the manual queue; restart the queued track.
        if self.current_override.is_some() {
            self.seek_position.set(0, true);
            return None;
        }
        self.prev_index().and_then(|i| {
            // Only jump to the previous track if we aren't more than 2 seconds (2,000 ms) into the current track.
            // Otherwise, seek to the start of the current track.
            // (This replicates the behavior of official Spotify clients.)
            if self.seek_position.current() <= 2000 {
                self.seek_position.set(0, true);
                self.play_index(i)
            } else {
                self.seek_position.set(0, true);
                None
            }
        })
    }

    pub fn prev_index(&self) -> Option<usize> {
        let len = if self.is_shuffled {
            self.songs.partial_len()
        } else {
            self.songs.len()
        };
        self.list_position.and_then(|p| match self.repeat {
            RepeatMode::Song => Some(p),
            RepeatMode::Playlist if len != 0 => Some((if p == 0 { len } else { p }) - 1),
            RepeatMode::None => Some(p).filter(|&i| i > 0).map(|i| i - 1),
            _ => None,
        })
    }

    fn toggle_play(&mut self) -> Option<bool> {
        if self.list_position.is_some() || self.current_override.is_some() {
            self.is_playing = !self.is_playing;

            match self.is_playing {
                false => self.seek_position.pause(),
                true => self.seek_position.resume(),
            };

            Some(self.is_playing)
        } else {
            None
        }
    }

    fn set_shuffled(&mut self, shuffled: bool) {
        self.is_shuffled = shuffled;
        let old = self.list_position.replace(0).unwrap_or(0);
        self.index.reset_picking_first(old);
    }

    pub fn available_devices(&self) -> &Vec<ConnectDevice> {
        &self.available_devices
    }

    pub fn current_device(&self) -> &Device {
        &self.current_device
    }

    /// The remote-playback snapshot (what's playing on another device), if any.
    pub fn remote_playback(&self) -> Option<&RemotePlayback> {
        self.remote_playback.as_ref()
    }

    /// Whether riff currently owns an active LOCAL playback session. True from the
    /// moment the user plays something locally, staying true across local
    /// pause/resume, until the session is stopped or handed to a remote device.
    /// This — NOT raw play/pause — is what makes local playback "sticky" so a
    /// pause doesn't hand the display back to the desktop.
    pub fn local_session_active(&self) -> bool {
        self.local_session_active
    }

    /// Whether riff is currently being driven as a Spotify Connect RECEIVER
    /// (a remote app transferred playback here and Spirc owns the local Player).
    /// While true, riff's own queue must not drive the Player and transport
    /// controls route to the Spirc handle.
    pub fn is_remote_controlled(&self) -> bool {
        self.is_remote_controlled
    }

    /// Whether the UI should MIRROR remote playback right now: a remote snapshot
    /// exists, the active device is Local (we haven't switched to control a
    /// Connect device directly), AND riff doesn't own a local session. Gating on
    /// `local_session_active` (not `is_playing`) is what keeps local playback
    /// sticky: once the user plays in riff, a local PAUSE no longer re-enables the
    /// mirror. This is the single "prefer remote vs local" decision the mini-
    /// player / now-playing read. (We mirror whether the remote is playing or
    /// paused, so the user can still see a paused remote track and resume it.)
    pub fn is_mirroring_remote(&self) -> bool {
        self.remote_playback.is_some()
            && matches!(self.current_device, Device::Local)
            && !self.local_session_active
    }

    /// The remote device that is currently the ACTIVE OUTPUT, when riff should
    /// route a newly-triggered play there instead of playing locally (the "tap
    /// plays on the active device" behavior). This is `Some(device_id)` iff:
    ///   - a remote-playback snapshot exists (some other device is the active
    ///     Spotify player), AND
    ///   - riff is NOT the chosen local output — i.e. we don't own a local
    ///     session (`!local_session_active`), the active device is still `Local`
    ///     (the user hasn't switched to directly control a Connect device, which
    ///     already routes through the Connect path), and riff isn't itself a
    ///     Connect RECEIVER (a remote transferred playback TO us).
    /// Otherwise `None` → play locally (making riff the active output). Once the
    /// user plays locally (`local_session_active`) or transfers OUTPUT to riff,
    /// this returns `None` and taps play locally again.
    pub fn active_remote_device(&self) -> Option<String> {
        if self.local_session_active || self.is_remote_controlled {
            return None;
        }
        if !matches!(self.current_device, Device::Local) {
            return None;
        }
        self.remote_playback.as_ref().map(|r| r.device.id.clone())
    }

    /// The track to display: the remote snapshot's track while mirroring a remote
    /// device, otherwise riff's own current (local / switched-Connect) track.
    pub fn displayed_song(&self) -> Option<SongDescription> {
        if self.is_mirroring_remote() {
            self.remote_playback.as_ref().map(|r| r.song.clone())
        } else {
            self.current_song()
        }
    }

    /// The play/pause state to display (remote while mirroring, else local).
    pub fn displayed_is_playing(&self) -> bool {
        if self.is_mirroring_remote() {
            self.remote_playback
                .as_ref()
                .map(|r| r.is_playing)
                .unwrap_or(false)
        } else {
            self.is_playing()
        }
    }

    /// The name of the device to show in the "Playing on X" indicator: the remote
    /// snapshot's device while mirroring, else the switched-to Connect device.
    pub fn displayed_device_name(&self) -> Option<String> {
        if self.is_mirroring_remote() {
            return self.remote_playback.as_ref().map(|r| r.device.label.clone());
        }
        match &self.current_device {
            Device::Connect(device) => Some(device.label.clone()),
            Device::Local => None,
        }
    }
}

impl Default for PlaybackState {
    fn default() -> Self {
        Self {
            available_devices: vec![],
            current_device: Device::Local,
            index: LazyRandomIndex::default(),
            songs: SongListModel::new(50),
            list_position: None,
            seek_position: PositionMillis::new(1.0),
            source: None,
            repeat: RepeatMode::None,
            is_playing: false,
            is_shuffled: false,
            volume: -1.0,
            manual_queue: VecDeque::new(),
            current_override: None,
            remote_playback: None,
            local_session_active: false,
            is_remote_controlled: false,
        }
    }
}

#[derive(Clone, Debug)]
pub enum PlaybackAction {
    TogglePlay,
    Play,
    Pause,
    Stop,
    SetRepeatMode(RepeatMode),
    SetShuffled(bool),
    ToggleRepeat,
    ToggleShuffle,
    Seek(u32),
    // I can't remember the diff betweek Seek and SyncSeek right now. Probably the source of the action
    SyncSeek(u32),
    Load(String),
    #[deprecated]
    LoadSongs(Vec<SongDescription>),
    LoadPagedSongs(SongsSource, SongBatch),
    SetVolume(f64),
    Next,
    Previous,
    PreloadNext,
    Queue(Vec<SongDescription>),
    Dequeue(String),
    MoveInQueue {
        id: String,
        to: usize,
    },
    SwitchDevice(Device),
    SetAvailableDevices(Vec<ConnectDevice>),
    /// Set (or clear) the mirrored remote-playback snapshot from a poll of
    /// `GET /me/player`. `None` clears it (remote stopped / became local).
    SetRemotePlayback(Option<RemotePlayback>),
    /// Enter / leave Spotify Connect RECEIVER mode: `true` when a remote app
    /// transferred playback to riff and librespot's Spirc now owns the local
    /// Player; `false` when it releases. Emitted by the player thread. While
    /// receiving, riff's own queue must not drive the Player (transport routes
    /// to Spirc), and Player events are mirrored in for display.
    SetRemoteControlled(bool),
    /// ANOTHER Connect device took over as the active player (riff lost active
    /// status). Clears BOTH sticky flags — `local_session_active` and
    /// `is_remote_controlled` — so the desktop→riff mirror is un-gated and riff
    /// starts mirroring + controlling the device that took over. Dispatched by
    /// the player thread on the Spirc deactivation edge (`PlayerEvent::Stopped`
    /// / `SessionDisconnected` while riff was the active Spirc device), and by
    /// the low-rate takeover poll when it sees a different device become the
    /// active player. This is the "yield active-device status" signal. Unlike a
    /// user PAUSE of riff-as-active (which keeps riff sticky), a takeover means
    /// riff is no longer the active output at all.
    YieldToRemote,
}

impl From<PlaybackAction> for AppAction {
    fn from(playback_action: PlaybackAction) -> Self {
        Self::PlaybackAction(playback_action)
    }
}

#[derive(Clone, Debug)]
pub enum Device {
    Local,
    Connect(ConnectDevice),
}

#[derive(Clone, Debug)]
pub enum PlaybackEvent {
    PlaybackPaused,
    PlaybackResumed,
    RepeatModeChanged(RepeatMode),
    TrackSeeked(u32),
    SeekSynced(u32),
    VolumeSet(f64),
    TrackChanged(String),
    SourceChanged,
    Preload(String),
    ShuffleChanged(bool),
    PlaylistChanged,
    PlaybackStopped,
    SwitchedDevice(Device),
    AvailableDevicesChanged,
    /// The mirrored remote-playback snapshot changed (track/state/progress on
    /// another device, or it appeared/disappeared). UI re-renders from it.
    RemotePlaybackChanged,
    /// Spotify Connect receiver mode toggled: `true` when a remote app took over
    /// riff's local Player via Spirc, `false` when it released back to riff's own
    /// queue. `PlayerNotifier` uses this to route transport to Spirc vs the local
    /// player.
    RemoteControlChanged(bool),
    /// The user pressed Next/Previous while riff is a Connect receiver; route the
    /// skip to the Spirc handle (Spirc owns the queue) instead of riff's own.
    RemoteNextRequested,
    RemotePrevRequested,
    /// riff yielded active-device status to another Connect device that took
    /// over (both sticky flags cleared). The notifier re-enables the OTHER-device
    /// mirror and forces an immediate `/me/player` re-poll so the UI switches to
    /// mirroring + controlling the new active device right away.
    YieldedActiveDevice,
}

impl From<PlaybackEvent> for AppEvent {
    fn from(playback_event: PlaybackEvent) -> Self {
        Self::PlaybackEvent(playback_event)
    }
}

impl UpdatableState for PlaybackState {
    type Action = PlaybackAction;
    type Event = PlaybackEvent;

    // Main "reducer" :)
    fn update_with(&mut self, action: Cow<Self::Action>) -> Vec<Self::Event> {
        match action.into_owned() {
            PlaybackAction::TogglePlay => {
                if let Some(playing) = self.toggle_play() {
                    if playing {
                        vec![PlaybackEvent::PlaybackResumed]
                    } else {
                        vec![PlaybackEvent::PlaybackPaused]
                    }
                } else {
                    vec![]
                }
            }
            PlaybackAction::Play => {
                if !self.is_playing() && self.toggle_play() == Some(true) {
                    vec![PlaybackEvent::PlaybackResumed]
                } else {
                    vec![]
                }
            }
            PlaybackAction::Pause => {
                if self.is_playing() && self.toggle_play() == Some(false) {
                    vec![PlaybackEvent::PlaybackPaused]
                } else {
                    vec![]
                }
            }
            PlaybackAction::ToggleRepeat => {
                self.repeat = match self.repeat {
                    RepeatMode::Song => RepeatMode::None,
                    RepeatMode::Playlist => RepeatMode::Song,
                    RepeatMode::None => RepeatMode::Playlist,
                };
                vec![PlaybackEvent::RepeatModeChanged(self.repeat)]
            }
            PlaybackAction::SetRepeatMode(mode) if self.repeat != mode => {
                self.repeat = mode;
                vec![PlaybackEvent::RepeatModeChanged(self.repeat)]
            }
            PlaybackAction::SetShuffled(shuffled) if self.is_shuffled != shuffled => {
                self.set_shuffled(shuffled);
                vec![PlaybackEvent::ShuffleChanged(shuffled)]
            }
            PlaybackAction::ToggleShuffle => {
                self.set_shuffled(!self.is_shuffled);
                vec![PlaybackEvent::ShuffleChanged(self.is_shuffled)]
            }
            PlaybackAction::Next => {
                // In receiver mode Spirc owns the queue: don't advance riff's own
                // (single mirrored) queue — ask Spirc to skip and let the mirror
                // reflect the new track.
                if self.is_remote_controlled {
                    return vec![PlaybackEvent::RemoteNextRequested];
                }
                if let Some(id) = self.play_next() {
                    vec![PlaybackEvent::TrackChanged(id)]
                } else {
                    self.stop();
                    vec![PlaybackEvent::PlaybackStopped]
                }
            }
            PlaybackAction::Stop => {
                self.stop();
                vec![PlaybackEvent::PlaybackStopped]
            }
            PlaybackAction::Previous => {
                if self.is_remote_controlled {
                    return vec![PlaybackEvent::RemotePrevRequested];
                }
                if let Some(id) = self.play_prev() {
                    vec![PlaybackEvent::TrackChanged(id)]
                } else {
                    vec![PlaybackEvent::TrackSeeked(0)]
                }
            }
            PlaybackAction::Load(id) => {
                if self.play(&id) {
                    vec![PlaybackEvent::TrackChanged(id)]
                } else {
                    vec![]
                }
            }
            PlaybackAction::PreloadNext => {
                if let Some(id) = self.next_id() {
                    vec![PlaybackEvent::Preload(id)]
                } else {
                    vec![]
                }
            }
            PlaybackAction::LoadPagedSongs(source, batch)
                if Some(&source) == self.source.as_ref() =>
            {
                if self.add_batch(batch) {
                    vec![PlaybackEvent::PlaylistChanged]
                } else {
                    vec![]
                }
            }
            PlaybackAction::LoadPagedSongs(source, batch)
                if Some(&source) != self.source.as_ref() =>
            {
                debug!("new source: {:?}", &source);
                self.set_batch(Some(source), batch);
                vec![PlaybackEvent::PlaylistChanged, PlaybackEvent::SourceChanged]
            }
            PlaybackAction::LoadSongs(tracks) => {
                self.set_queue(tracks);
                vec![PlaybackEvent::PlaylistChanged, PlaybackEvent::SourceChanged]
            }
            PlaybackAction::Queue(tracks) => {
                self.queue_next(tracks);
                vec![PlaybackEvent::PlaylistChanged]
            }
            PlaybackAction::Dequeue(id) => {
                self.dequeue(&[id]);
                vec![PlaybackEvent::PlaylistChanged]
            }
            PlaybackAction::MoveInQueue { id, to } => {
                self.move_in_queue(&id, to);
                vec![PlaybackEvent::PlaylistChanged]
            }
            PlaybackAction::Seek(pos) => {
                self.seek_position.set(pos as u64 * 1000, true);
                vec![PlaybackEvent::TrackSeeked(pos)]
            }
            PlaybackAction::SyncSeek(pos) => {
                self.seek_position.set(pos as u64 * 1000, true);
                vec![PlaybackEvent::SeekSynced(pos)]
            }
            PlaybackAction::SetVolume(volume) => {
                // Idempotency guard: only emit (and thus touch dconf, the
                // mixer, the Web API and MPRIS/D-Bus) when the volume actually
                // changes. Rapid volume input (e.g. mouse-wheel scrolling the
                // slider) would otherwise fan out a storm of `VolumeSet` events
                // and MPRIS `PropertiesChanged` signals.
                if self.volume == volume {
                    vec![]
                } else {
                    self.volume = volume;
                    vec![PlaybackEvent::VolumeSet(volume)]
                }
            }

            PlaybackAction::SetAvailableDevices(list) => {
                self.available_devices = list;
                vec![PlaybackEvent::AvailableDevicesChanged]
            }
            PlaybackAction::SetRemotePlayback(snapshot) => {
                // Only emit when something the UI cares about actually changed
                // (device / track / play-state / a >1s progress jump), so the
                // ~4s poll doesn't fan out a re-render + MPRIS churn every tick.
                let changed = match (&self.remote_playback, &snapshot) {
                    (None, None) => false,
                    (Some(a), Some(b)) => {
                        a.device.id != b.device.id
                            || a.song.id != b.song.id
                            || a.is_playing != b.is_playing
                            || a.progress_ms.abs_diff(b.progress_ms) > 1500
                    }
                    _ => true,
                };
                self.remote_playback = snapshot;
                if changed {
                    vec![PlaybackEvent::RemotePlaybackChanged]
                } else {
                    vec![]
                }
            }
            PlaybackAction::SetRemoteControlled(controlled) => {
                if self.is_remote_controlled == controlled {
                    return vec![];
                }
                self.is_remote_controlled = controlled;
                if controlled {
                    // A remote app transferred playback to riff: riff is now the
                    // active local player (Spirc-driven), so the OTHER-device
                    // mirror must yield and stay yielded until release.
                    self.remote_playback = None;
                    self.local_session_active = true;
                } else {
                    // Spirc released control. Whatever Spirc left loaded is riff's
                    // current queue/track; keep the local session so we don't snap
                    // back to a stale remote mirror on release.
                    self.local_session_active = self.list_position.is_some()
                        || self.current_override.is_some();
                }
                vec![PlaybackEvent::RemoteControlChanged(controlled)]
            }
            PlaybackAction::YieldToRemote => {
                // Another device became the active player: riff is no longer the
                // active output. Drop BOTH sticky flags so `is_mirroring_remote`
                // (which gates on `!local_session_active`) is un-gated and the
                // desktop→riff mirror can surface the device that took over. We do
                // NOT touch the local queue/track here — the mirror snapshot from
                // the next poll drives the display; keeping the queue lets a later
                // local play resume cleanly. No-op (no event) when we already hold
                // neither flag, so a redundant poll/edge doesn't churn the UI.
                if !self.local_session_active && !self.is_remote_controlled {
                    return vec![];
                }
                self.local_session_active = false;
                self.is_remote_controlled = false;
                vec![PlaybackEvent::YieldedActiveDevice]
            }
            PlaybackAction::SwitchDevice(new_device) => {
                // Explicitly picking a remote Connect device to control means the
                // user is LEAVING their local session — end it so the mirror is
                // free to surface that device again. Switching back to Local does
                // NOT start a session (that only happens on an actual local play).
                if matches!(new_device, Device::Connect(_)) {
                    self.local_session_active = false;
                }
                self.current_device = new_device.clone();
                vec![PlaybackEvent::SwitchedDevice(new_device)]
            }
            _ => vec![],
        }
    }
}

// A struct to keep track of the playback position
// Caller must call pause/play at the right time
#[derive(Debug)]
struct PositionMillis {
    // Last recorded position in the track (in milliseconds)
    last_known_position: u64,
    // Last time we resumed playback
    last_resume_instant: Option<Instant>,
    // Playback rate (1)
    rate: f32,
}

impl PositionMillis {
    fn new(rate: f32) -> Self {
        Self {
            last_known_position: 0,
            last_resume_instant: None,
            rate,
        }
    }

    // Read the current pos by adding elapsed time since the last time we resumed playback to the last know position
    fn current(&self) -> u64 {
        let current_progress = self.last_resume_instant.map(|ri| {
            let elapsed = ri.elapsed().as_millis() as f32;
            let real_elapsed = self.rate * elapsed;
            real_elapsed.ceil() as u64
        });
        self.last_known_position + current_progress.unwrap_or(0)
    }

    fn set(&mut self, position: u64, playing: bool) {
        self.last_known_position = position;
        self.last_resume_instant = if playing { Some(Instant::now()) } else { None }
    }

    fn pause(&mut self) {
        self.last_known_position = self.current();
        self.last_resume_instant = None;
    }

    fn resume(&mut self) {
        self.last_resume_instant = Some(Instant::now());
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::app::models::AlbumRef;

    fn song(id: &str) -> SongDescription {
        SongDescription {
            id: id.to_string(),
            uri: "".to_string(),
            title: "Title".to_string(),
            artists: vec![],
            album: AlbumRef {
                id: "".to_string(),
                name: "".to_string(),
            },
            duration_ms: 1000,
            art: None,
            track_number: None,
        }
    }

    impl PlaybackState {
        fn current_position(&self) -> Option<usize> {
            self.list_position
        }

        fn prev_id(&self) -> Option<String> {
            self.prev_index()
                .and_then(|i| Some(self.songs().index(i)?.description().id.clone()))
        }

        fn song_ids(&self) -> Vec<String> {
            self.songs()
                .collect()
                .iter()
                .map(|s| s.id.clone())
                .collect()
        }
    }

    fn remote(id: &str, song_id: &str, playing: bool, progress: u32) -> RemotePlayback {
        RemotePlayback {
            device: ConnectDevice {
                id: id.to_string(),
                label: "Desktop".to_string(),
                kind: ConnectDeviceKind::Computer,
            },
            song: song(song_id),
            is_playing: playing,
            progress_ms: progress,
            duration_ms: 200_000,
        }
    }

    #[test]
    fn test_initial_state() {
        let state = PlaybackState::default();
        assert!(!state.is_playing());
        assert!(!state.is_shuffled());
        assert!(state.current_song().is_none());
        assert!(state.prev_index().is_none());
        assert!(state.next_index().is_none());
    }

    #[test]
    fn test_remote_mirror_displays_remote_track_when_idle() {
        let mut state = PlaybackState::default();
        // Idle locally on the local device: a remote snapshot should mirror.
        let events = state.update_with(Cow::Owned(PlaybackAction::SetRemotePlayback(Some(
            remote("dev1", "remote-song", true, 5000),
        ))));
        assert!(events
            .iter()
            .any(|e| matches!(e, PlaybackEvent::RemotePlaybackChanged)));
        assert!(state.is_mirroring_remote());
        assert!(state.displayed_is_playing());
        assert_eq!(
            state.displayed_song().map(|s| s.id),
            Some("remote-song".to_string())
        );
        assert_eq!(state.displayed_device_name(), Some("Desktop".to_string()));
    }

    #[test]
    fn test_local_playback_wins_over_later_remote_snapshot() {
        let mut state = PlaybackState::default();
        // No remote is active, so playing locally makes riff the active output.
        state.queue(vec![song("local-song")]);
        state.play("local-song");
        assert!(state.is_playing());
        assert!(state.local_session_active());
        // A later remote-playback poll must NOT hijack riff's local session.
        state.update_with(Cow::Owned(PlaybackAction::SetRemotePlayback(Some(remote(
            "dev1", "remote-song", true, 5000,
        )))));
        assert!(!state.is_mirroring_remote());
        assert_eq!(
            state.displayed_song().map(|s| s.id),
            Some("local-song".to_string())
        );
        // ...and taps stay local while the local session is active.
        assert!(state.active_remote_device().is_none());
    }

    // A play triggered while a REMOTE device is the active output is ROUTED to
    // that device: riff must NOT open a local session and must keep mirroring
    // (the "tap plays on the active device" behavior).
    #[test]
    fn test_play_routes_to_active_remote_and_keeps_mirroring() {
        let mut state = PlaybackState::default();
        state.update_with(Cow::Owned(PlaybackAction::SetRemotePlayback(Some(remote(
            "dev1", "remote-song", true, 5000,
        )))));
        assert!(state.is_mirroring_remote());
        // The active output is the remote device.
        assert_eq!(state.active_remote_device(), Some("dev1".to_string()));

        // User taps a song in riff: the state loads it but, because a remote is
        // the active output, no local session opens and the mirror stays.
        state.queue(vec![song("local-song")]);
        state.update_with(Cow::Owned(PlaybackAction::Load("local-song".to_string())));
        assert!(!state.local_session_active());
        assert!(state.is_mirroring_remote());
        assert_eq!(
            state.displayed_song().map(|s| s.id),
            Some("remote-song".to_string())
        );
    }

    // Once riff is the chosen local output (local session active), taps play
    // LOCALLY again even though a remote snapshot is still mirrored/present.
    #[test]
    fn test_no_active_remote_when_local_session_owns() {
        let mut state = PlaybackState::default();
        // First establish a local session (no remote active).
        state.queue(vec![song("a"), song("b")]);
        state.update_with(Cow::Owned(PlaybackAction::Load("a".to_string())));
        assert!(state.local_session_active());
        // A remote snapshot arrives but we own the local session.
        state.update_with(Cow::Owned(PlaybackAction::SetRemotePlayback(Some(remote(
            "dev1", "remote-song", true, 5000,
        )))));
        assert!(state.active_remote_device().is_none());
        // A second tap plays locally too.
        state.update_with(Cow::Owned(PlaybackAction::Load("b".to_string())));
        assert!(state.local_session_active());
        assert!(!state.is_mirroring_remote());
    }

    // After transferring OUTPUT to riff (Spirc receiver), taps play local: there
    // is no "active remote" to route to.
    #[test]
    fn test_no_active_remote_while_receiver() {
        let mut state = PlaybackState::default();
        state.update_with(Cow::Owned(PlaybackAction::SetRemotePlayback(Some(remote(
            "dev1", "remote-song", true, 5000,
        )))));
        state.update_with(Cow::Owned(PlaybackAction::SetRemoteControlled(true)));
        assert!(state.active_remote_device().is_none());
    }

    #[test]
    fn test_local_pause_stays_sticky_and_does_not_remirror() {
        let mut state = PlaybackState::default();
        // User plays something locally first (no remote active): riff owns the
        // local session and becomes the active output.
        state.queue(vec![song("local-song")]);
        state.update_with(Cow::Owned(PlaybackAction::Load("local-song".to_string())));
        assert!(state.local_session_active());
        assert!(!state.is_mirroring_remote());

        // A desktop later reports playback, but the sticky local session ignores it.
        state.update_with(Cow::Owned(PlaybackAction::SetRemotePlayback(Some(remote(
            "dev1", "remote-song", true, 5000,
        )))));
        assert!(!state.is_mirroring_remote());

        // Pausing locally must NOT hand the display back to the desktop.
        state.update_with(Cow::Owned(PlaybackAction::Pause));
        assert!(!state.is_playing());
        assert!(state.local_session_active());
        assert!(!state.is_mirroring_remote());
        assert_eq!(
            state.displayed_song().map(|s| s.id),
            Some("local-song".to_string())
        );

        // Resuming keeps the local session too.
        state.update_with(Cow::Owned(PlaybackAction::Play));
        assert!(state.is_playing());
        assert!(!state.is_mirroring_remote());
    }

    #[test]
    fn test_local_session_cleared_on_switch_to_connect_remirrors() {
        let mut state = PlaybackState::default();
        // Establish a local session first (no remote active).
        state.queue(vec![song("local-song")]);
        state.update_with(Cow::Owned(PlaybackAction::Load("local-song".to_string())));
        assert!(state.local_session_active());
        // A remote snapshot exists (so the mirror can resume once we leave local).
        state.update_with(Cow::Owned(PlaybackAction::SetRemotePlayback(Some(remote(
            "dev1", "remote-song", true, 5000,
        )))));

        // Explicitly transfer to the Connect device: the local session ends.
        state.update_with(Cow::Owned(PlaybackAction::SwitchDevice(Device::Connect(
            ConnectDevice {
                id: "dev1".to_string(),
                label: "Desktop".to_string(),
                kind: ConnectDeviceKind::Computer,
            },
        ))));
        assert!(!state.local_session_active());
        // (Now controlling a Connect device directly, so still not mirroring.)
        assert!(!state.is_mirroring_remote());

        // Coming back to Local with no session -> mirror resumes.
        state.update_with(Cow::Owned(PlaybackAction::SwitchDevice(Device::Local)));
        assert!(!state.local_session_active());
        assert!(state.is_mirroring_remote());
    }

    #[test]
    fn test_stop_clears_local_session() {
        let mut state = PlaybackState::default();
        state.queue(vec![song("local-song")]);
        state.update_with(Cow::Owned(PlaybackAction::Load("local-song".to_string())));
        assert!(state.local_session_active());
        state.update_with(Cow::Owned(PlaybackAction::Stop));
        assert!(!state.local_session_active());
    }

    #[test]
    fn test_switched_connect_device_does_not_mirror() {
        let mut state = PlaybackState::default();
        state.update_with(Cow::Owned(PlaybackAction::SetRemotePlayback(Some(remote(
            "dev1", "remote-song", true, 5000,
        )))));
        // Explicitly switched to controlling a Connect device: no mirror (the main
        // queue drives the display instead).
        state.update_with(Cow::Owned(PlaybackAction::SwitchDevice(Device::Connect(
            ConnectDevice {
                id: "dev1".to_string(),
                label: "Desktop".to_string(),
                kind: ConnectDeviceKind::Computer,
            },
        ))));
        assert!(!state.is_mirroring_remote());
    }

    #[test]
    fn test_remote_snapshot_change_detection() {
        let mut state = PlaybackState::default();
        // First set -> change.
        assert!(!state
            .update_with(Cow::Owned(PlaybackAction::SetRemotePlayback(Some(remote(
                "dev1", "s1", true, 1000
            )))))
            .is_empty());
        // Same track/state, tiny progress drift -> no event.
        assert!(state
            .update_with(Cow::Owned(PlaybackAction::SetRemotePlayback(Some(remote(
                "dev1", "s1", true, 1200
            )))))
            .is_empty());
        // Big progress jump (seek) -> event.
        assert!(!state
            .update_with(Cow::Owned(PlaybackAction::SetRemotePlayback(Some(remote(
                "dev1", "s1", true, 60000
            )))))
            .is_empty());
        // Clearing -> event.
        assert!(!state
            .update_with(Cow::Owned(PlaybackAction::SetRemotePlayback(None)))
            .is_empty());
    }

    #[test]
    fn test_remote_controlled_yields_other_device_mirror() {
        let mut state = PlaybackState::default();
        // Desktop is playing: mirror it while idle.
        state.update_with(Cow::Owned(PlaybackAction::SetRemotePlayback(Some(remote(
            "dev1", "remote-song", true, 5000,
        )))));
        assert!(state.is_mirroring_remote());

        // A remote app transfers playback TO riff (Spirc becomes active).
        let events = state.update_with(Cow::Owned(PlaybackAction::SetRemoteControlled(true)));
        assert!(events
            .iter()
            .any(|e| matches!(e, PlaybackEvent::RemoteControlChanged(true))));
        assert!(state.is_remote_controlled());
        // The other-device mirror must yield; riff is the active player now.
        assert!(!state.is_mirroring_remote());
        assert!(state.local_session_active());

        // Idempotent: setting the same value emits nothing.
        assert!(state
            .update_with(Cow::Owned(PlaybackAction::SetRemoteControlled(true)))
            .is_empty());

        // Release: back to riff's own queue ownership.
        let events = state.update_with(Cow::Owned(PlaybackAction::SetRemoteControlled(false)));
        assert!(events
            .iter()
            .any(|e| matches!(e, PlaybackEvent::RemoteControlChanged(false))));
        assert!(!state.is_remote_controlled());
    }

    // Another device takes over while riff was the active (Spirc-driven) player:
    // YieldToRemote must clear BOTH sticky flags so the desktop mirror re-enables
    // and, once a remote snapshot is present, riff mirrors the device that took
    // over. This is the core "yield on takeover" behavior.
    #[test]
    fn test_yield_to_remote_clears_sticky_and_remirrors() {
        let mut state = PlaybackState::default();
        // Simulate riff being the active local (Spirc) player after a takeover TO
        // riff: receiver mode set both flags true and parked a mirrored track.
        state.update_with(Cow::Owned(PlaybackAction::SetRemoteControlled(true)));
        state.queue(vec![song("mirrored")]);
        state.play("mirrored");
        assert!(state.local_session_active());
        assert!(state.is_remote_controlled());
        assert!(!state.is_mirroring_remote());

        // The desktop takes over: riff yields active-device status.
        let events = state.update_with(Cow::Owned(PlaybackAction::YieldToRemote));
        assert!(events
            .iter()
            .any(|e| matches!(e, PlaybackEvent::YieldedActiveDevice)));
        assert!(!state.local_session_active());
        assert!(!state.is_remote_controlled());

        // A snapshot of the device that took over now mirrors (the gate is open).
        state.update_with(Cow::Owned(PlaybackAction::SetRemotePlayback(Some(remote(
            "desktop", "desktop-song", true, 3000,
        )))));
        assert!(state.is_mirroring_remote());
        assert_eq!(
            state.displayed_song().map(|s| s.id),
            Some("desktop-song".to_string())
        );
        assert_eq!(state.active_remote_device(), Some("desktop".to_string()));
    }

    // A user PAUSE of riff-as-active must NOT look like a takeover: the sticky
    // local session is preserved and the mirror stays gated OFF. (The yield path
    // is only ever driven by an actual deactivation edge / takeover poll, never by
    // a pause — this asserts the state layer keeps stickiness through a pause so a
    // spurious YieldToRemote is the only thing that could break it, and there
    // isn't one on pause.)
    #[test]
    fn test_pause_of_active_riff_is_not_a_yield() {
        let mut state = PlaybackState::default();
        state.queue(vec![song("local")]);
        state.update_with(Cow::Owned(PlaybackAction::Load("local".to_string())));
        assert!(state.local_session_active());
        // Desktop reports playback; sticky session ignores it.
        state.update_with(Cow::Owned(PlaybackAction::SetRemotePlayback(Some(remote(
            "desktop", "desktop-song", true, 3000,
        )))));
        // User pauses locally: still sticky, still not mirroring.
        state.update_with(Cow::Owned(PlaybackAction::Pause));
        assert!(state.local_session_active());
        assert!(!state.is_mirroring_remote());
    }

    // YieldToRemote is a no-op (no event) when riff already holds neither sticky
    // flag, so a redundant takeover poll / duplicate edge doesn't churn the UI.
    #[test]
    fn test_yield_to_remote_noop_when_already_yielded() {
        let mut state = PlaybackState::default();
        assert!(state
            .update_with(Cow::Owned(PlaybackAction::YieldToRemote))
            .is_empty());
    }

    #[test]
    fn test_play_one() {
        let mut state = PlaybackState::default();
        state.queue(vec![song("foo")]);

        state.play("foo");
        assert!(state.is_playing());

        assert_eq!(state.current_song_id(), Some("foo".to_string()));
        assert!(state.prev_index().is_none());
        assert!(state.next_index().is_none());

        state.toggle_play();
        assert!(!state.is_playing());
    }

    #[test]
    fn test_queue() {
        let mut state = PlaybackState::default();
        state.queue(vec![song("1"), song("2"), song("3")]);

        assert_eq!(state.songs().len(), 3);

        state.play("2");

        state.queue(vec![song("4")]);
        assert_eq!(state.songs().len(), 4);
    }

    #[test]
    fn test_manual_queue_plays_next_then_resumes_context() {
        let mut state = PlaybackState::default();
        state.queue(vec![song("1"), song("2"), song("3")]);
        state.play("1");
        assert_eq!(state.current_song_id(), Some("1".to_string()));

        // Queue two tracks: they play right after the current one, before context.
        state.queue_next(vec![song("q1"), song("q2")]);
        assert_eq!(state.next_id(), Some("q1".to_string()));

        state.play_next();
        assert_eq!(state.current_song_id(), Some("q1".to_string()));
        // list_position stays parked on the context track "1".
        assert_eq!(state.current_position(), Some(0));

        state.play_next();
        assert_eq!(state.current_song_id(), Some("q2".to_string()));

        // Queue drained -> context resumes at the track after "1".
        state.play_next();
        assert_eq!(state.current_song_id(), Some("2".to_string()));
        assert_eq!(state.current_position(), Some(1));
    }

    #[test]
    fn test_dequeue_removes_from_manual_queue() {
        let mut state = PlaybackState::default();
        state.queue(vec![song("1"), song("2")]);
        state.play("1");
        state.queue_next(vec![song("q1"), song("q2")]);

        state.dequeue(&["q1".to_string()]);
        assert_eq!(state.next_id(), Some("q2".to_string()));
        // Context is untouched.
        assert_eq!(state.current_song_id(), Some("1".to_string()));
        assert_eq!(state.songs().len(), 2);
    }

    #[test]
    fn test_move_in_queue() {
        let mut state = PlaybackState::default();
        state.queue(vec![song("ctx")]);
        state.play("ctx");
        state.queue_next(vec![song("a"), song("b"), song("c")]);

        // Move "c" (index 2) to the front (index 0).
        state.move_in_queue("c", 0);
        let ids: Vec<_> = state.manual_queue().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["c", "a", "b"]);

        // Move "a" (now at index 1) to the end (index 2).
        state.move_in_queue("a", 2);
        let ids: Vec<_> = state.manual_queue().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["c", "b", "a"]);

        // No-op: unknown id.
        state.move_in_queue("z", 0);
        let ids: Vec<_> = state.manual_queue().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["c", "b", "a"]);

        // Clamp: to index way past end.
        state.move_in_queue("c", 999);
        let ids: Vec<_> = state.manual_queue().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["b", "a", "c"]);
    }

    #[test]
    fn test_play_multiple() {
        let mut state = PlaybackState::default();
        state.queue(vec![song("1"), song("2"), song("3")]);
        assert_eq!(state.songs().len(), 3);

        state.play("2");
        assert!(state.is_playing());

        assert_eq!(state.current_position(), Some(1));
        assert_eq!(state.prev_id(), Some("1".to_string()));
        assert_eq!(state.current_song_id(), Some("2".to_string()));
        assert_eq!(state.next_id(), Some("3".to_string()));

        state.toggle_play();
        assert!(!state.is_playing());

        state.play_next();
        assert!(state.is_playing());
        assert_eq!(state.current_position(), Some(2));
        assert_eq!(state.prev_id(), Some("2".to_string()));
        assert_eq!(state.current_song_id(), Some("3".to_string()));
        assert!(state.next_index().is_none());

        state.play_next();
        assert!(state.is_playing());
        assert_eq!(state.current_position(), Some(2));
        assert_eq!(state.current_song_id(), Some("3".to_string()));

        state.play_prev();
        state.play_prev();
        assert!(state.is_playing());
        assert_eq!(state.current_position(), Some(0));
        assert!(state.prev_index().is_none());
        assert_eq!(state.current_song_id(), Some("1".to_string()));
        assert_eq!(state.next_id(), Some("2".to_string()));

        state.play_prev();
        assert!(state.is_playing());
        assert_eq!(state.current_position(), Some(0));
        assert_eq!(state.current_song_id(), Some("1".to_string()));
    }

    #[test]
    fn test_shuffle() {
        let mut state = PlaybackState::default();
        state.queue(vec![song("1"), song("2"), song("3"), song("4")]);

        assert_eq!(state.songs().len(), 4);

        state.play("2");
        assert_eq!(state.current_position(), Some(1));

        state.set_shuffled(true);
        assert!(state.is_shuffled());
        assert_eq!(state.current_position(), Some(0));

        state.play_next();
        assert_eq!(state.current_position(), Some(1));

        state.set_shuffled(false);
        assert!(!state.is_shuffled());

        let ids = state.song_ids();
        assert_eq!(
            ids,
            vec![
                "1".to_string(),
                "2".to_string(),
                "3".to_string(),
                "4".to_string()
            ]
        );
    }

    #[test]
    fn test_shuffle_queue() {
        let mut state = PlaybackState::default();
        state.queue(vec![song("1"), song("2"), song("3")]);

        state.set_shuffled(true);
        assert!(state.is_shuffled());

        state.queue(vec![song("4")]);

        state.set_shuffled(false);
        assert!(!state.is_shuffled());

        let ids = state.song_ids();
        assert_eq!(
            ids,
            vec![
                "1".to_string(),
                "2".to_string(),
                "3".to_string(),
                "4".to_string()
            ]
        );
    }

    #[test]
    fn test_move() {
        let mut state = PlaybackState::default();
        state.queue(vec![song("1"), song("2"), song("3")]);

        state.play("2");
        assert!(state.is_playing());

        state.move_down("1");
        assert_eq!(state.current_song_id(), Some("2".to_string()));
        let ids = state.song_ids();
        assert_eq!(ids, vec!["2".to_string(), "1".to_string(), "3".to_string()]);

        state.move_down("2");
        state.move_down("2");
        assert_eq!(state.current_song_id(), Some("2".to_string()));
        let ids = state.song_ids();
        assert_eq!(ids, vec!["1".to_string(), "3".to_string(), "2".to_string()]);

        state.move_down("2");
        assert_eq!(state.current_song_id(), Some("2".to_string()));
        let ids = state.song_ids();
        assert_eq!(ids, vec!["1".to_string(), "3".to_string(), "2".to_string()]);

        state.move_up("2");

        assert_eq!(state.current_song_id(), Some("2".to_string()));
        let ids = state.song_ids();
        assert_eq!(ids, vec!["1".to_string(), "2".to_string(), "3".to_string()]);
    }

    #[test]
    fn test_dequeue_last() {
        let mut state = PlaybackState::default();
        state.queue(vec![song("1"), song("2"), song("3")]);

        state.play("3");
        assert!(state.is_playing());

        state.dequeue(&["3".to_string()]);
        assert_eq!(state.current_song_id(), None);
    }

    #[test]
    fn test_dequeue_a_few_songs() {
        let mut state = PlaybackState::default();
        state.queue(vec![
            song("1"),
            song("2"),
            song("3"),
            song("4"),
            song("5"),
            song("6"),
        ]);

        state.play("5");
        assert!(state.is_playing());

        state.dequeue(&["1".to_string(), "2".to_string(), "3".to_string()]);
        assert_eq!(state.current_song_id(), Some("5".to_string()));
    }

    #[test]
    fn test_dequeue_all() {
        let mut state = PlaybackState::default();
        state.queue(vec![song("3")]);

        state.play("3");
        assert!(state.is_playing());

        state.dequeue(&["3".to_string()]);
        assert_eq!(state.current_song_id(), None);
    }

    /// Reproduces the exact dispatch sequence from the details page shuffle button:
    /// 1. ToggleShuffle (enables shuffle)
    /// 2. LoadPagedSongs (loads a new source with songs)
    /// 3. Load(first_song_id) (starts playing the first song)
    /// Then pressing Next should select the next shuffled song, not stop playback.
    #[test]
    fn test_details_page_shuffle_play() {
        let mut state = PlaybackState::default();
        let songs = vec![song("1"), song("2"), song("3"), song("4"), song("5")];
        let batch = SongBatch {
            songs: songs.clone(),
            batch: Batch {
                offset: 0,
                batch_size: 50,
                total: 5,
            },
        };

        // Step 1: ToggleShuffle (no songs loaded yet)
        state.update_with(Cow::Owned(PlaybackAction::ToggleShuffle));
        assert!(state.is_shuffled());

        // Step 2: LoadPagedSongs (new source)
        state.update_with(Cow::Owned(PlaybackAction::LoadPagedSongs(
            SongsSource::Album("album1".to_string()),
            batch,
        )));
        assert_eq!(state.songs().len(), 5);

        // Step 3: Load first song
        state.update_with(Cow::Owned(PlaybackAction::Load("1".to_string())));
        assert!(state.is_playing());
        assert!(state.current_song_id().is_some());

        // Now press Next — this should NOT stop playback
        let events = state.update_with(Cow::Owned(PlaybackAction::Next));
        assert!(
            state.is_playing(),
            "Playback stopped after Next! current_song_id={:?}, list_position={:?}",
            state.current_song_id(),
            state.current_position(),
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, PlaybackEvent::TrackChanged(_))),
            "Expected TrackChanged event, got: {:?}",
            events,
        );
        assert!(state.current_song_id().is_some());

        // Press Next again — should still work
        let events = state.update_with(Cow::Owned(PlaybackAction::Next));
        assert!(state.is_playing());
        assert!(
            events
                .iter()
                .any(|e| matches!(e, PlaybackEvent::TrackChanged(_))),
            "Second Next failed, got: {:?}",
            events,
        );
    }

    /// Reproduces the artist page shuffle bug: Batch::first_of_size(50) sets total=0,
    /// causing the playback state to think there are 0 songs.
    #[test]
    fn test_details_page_shuffle_play_artist_bug() {
        let mut state = PlaybackState::default();
        let songs = vec![song("1"), song("2"), song("3"), song("4"), song("5")];
        // Artist page uses Batch::first_of_size(50) which has total=0
        let batch = SongBatch {
            songs: songs.clone(),
            batch: Batch::first_of_size(50),
        };

        // Step 1: ToggleShuffle
        state.update_with(Cow::Owned(PlaybackAction::ToggleShuffle));
        assert!(state.is_shuffled());

        // Step 2: LoadPagedSongs with total=0 batch
        state.update_with(Cow::Owned(PlaybackAction::LoadPagedSongs(
            SongsSource::Artist("artist1".to_string()),
            batch,
        )));

        // Step 3: Load first song
        state.update_with(Cow::Owned(PlaybackAction::Load("1".to_string())));
        assert!(state.is_playing(), "Song should be playing after Load");
        assert_eq!(state.current_song_id(), Some("1".to_string()));

        // Now press Next — this SHOULD work but currently fails
        let events = state.update_with(Cow::Owned(PlaybackAction::Next));
        assert!(
            state.is_playing(),
            "Playback stopped after Next! current_song_id={:?}, list_position={:?}",
            state.current_song_id(),
            state.current_position(),
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, PlaybackEvent::TrackChanged(_))),
            "Expected TrackChanged event, got: {:?}",
            events,
        );
    }

    /// Reproduces the playlist shuffle bug: total > loaded songs means shuffle
    /// can pick indices that have no songs loaded.
    #[test]
    fn test_details_page_shuffle_play_playlist_bug() {
        let mut state = PlaybackState::default();
        // Playlist has 200 total songs but only first 100 are loaded
        let songs: Vec<_> = (1..=100).map(|i| song(&i.to_string())).collect();
        let batch = SongBatch {
            songs,
            batch: Batch {
                offset: 0,
                batch_size: 100,
                total: 200,
            },
        };

        // Step 1: ToggleShuffle
        state.update_with(Cow::Owned(PlaybackAction::ToggleShuffle));

        // Step 2: LoadPagedSongs
        state.update_with(Cow::Owned(PlaybackAction::LoadPagedSongs(
            SongsSource::Playlist("pl1".to_string()),
            batch,
        )));
        // songs.len() returns total=200, but only 100 are actually loaded
        assert_eq!(state.songs().len(), 200);

        // Step 3: Load first song
        state.update_with(Cow::Owned(PlaybackAction::Load("1".to_string())));
        assert!(state.is_playing());

        // Press Next multiple times — eventually shuffle will pick an index >= 100
        // which has no song loaded, causing playback to stop
        let mut failed = false;
        for _ in 0..99 {
            let events = state.update_with(Cow::Owned(PlaybackAction::Next));
            if !state.is_playing() || state.current_song_id().is_none() {
                failed = true;
                break;
            }
        }
        assert!(
            !failed,
            "Playback stopped because shuffle picked an unloaded song index"
        );
    }
}
