use futures::channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};
use librespot::core::SpotifyUri;
use tokio::task;
use url::Url;

use crate::app::models::SongDescription;
use crate::app::state::{LoginAction, PlaybackAction};
use crate::app::AppAction;
use crate::auth::TokenStore;
#[allow(clippy::module_inception)]
mod player;
pub use player::*;

#[derive(Debug, Clone)]
pub enum Command {
    Restore,
    InitLogin,
    CompleteLogin,
    RefreshToken,
    Logout,
    PlayerLoad { track: SpotifyUri, resume: bool },
    PlayerResume,
    PlayerPause,
    PlayerStop,
    PlayerSeek(u32),
    PlayerSetVolume(f64),
    PlayerPreload(SpotifyUri),
    // Resolve a "song radio" station seeded from a track (base62 id) entirely
    // through the librespot session: get station track ids via the internal
    // radio-apollo endpoint, then hydrate each track's metadata via the internal
    // metadata API, and report the finished songs back to the app. The Web API is
    // never touched (it 403s for this dev-mode app).
    StartRadio { seed_id: String },
    ReloadSettings,
    SetEqualizer { bands: [f64; 10] },
    SetMono { enabled: bool },
    SetPan { pan: f64 },
    SetPitch { cents: f64 },
    // --- Spotify Connect RECEIVER (Spirc) ---------------------------------
    // Transport commands routed to the librespot Spirc handle when riff is the
    // active Connect device (a remote app transferred playback here). These make
    // riff's own transport buttons drive the Spirc-owned playback, so Spirc keeps
    // its connect-state coherent and reports it back to other devices. No-ops when
    // Spirc isn't running / not the active device.
    SpircPlay,
    SpircPause,
    SpircNext,
    SpircPrev,
    SpircSeek(u32),
    /// Volume as a 0.0..=1.0 fraction; scaled to librespot's u16 range.
    SpircSetVolume(f64),
    // --- LOCAL play announced THROUGH Spirc (Half-B: announce) ------------
    // The user initiated playback IN riff and riff is the intended output. We
    // drive it THROUGH Spirc (activate + load) so Spirc owns the play_request_id
    // and reports riff as the active device to other Spotify apps — while still
    // loading into the SHARED Player so riff HEARS the audio. If Spirc isn't
    // running the player thread FALLS BACK to the bare Player using `fallback`,
    // so local audio never depends on Spirc being online.
    //
    // A playlist/album (has a Spotify context) loads via `context_uri` + offset.
    SpircLoadContext {
        context_uri: String,
        /// Offset of the tapped track within the context.
        offset: usize,
        /// Track uri to seek to inside the context (robust to reordering); the
        /// player uses this as the `playing_track`, with `offset` as fallback.
        playing_track_uri: Option<String>,
        /// Whether to start playing immediately (vs. load paused).
        start_playing: bool,
        /// Bare-Player fallback: the tapped track to load directly if Spirc is
        /// offline, so local audio still works.
        fallback: SpotifyUri,
    },
    // An ad-hoc list (radio / Liked Songs / search / arbitrary queue — no context
    // uri) loads via an explicit `uris` track list + offset.
    SpircLoadTracks {
        uris: Vec<String>,
        offset: usize,
        start_playing: bool,
        /// Bare-Player fallback (the tapped track) if Spirc is offline.
        fallback: SpotifyUri,
    },
    /// Tell the player thread whether riff currently owns a LOCAL playback
    /// session (user played something in riff's own queue). The Player-event
    /// delegate reads this to decide, for an incoming librespot event, whether it
    /// is riff's own local playback (drive riff's queue: fire Next at end-of-track)
    /// or Spirc-driven receiver playback (mirror only, let Spirc advance).
    SetLocalOwnsPlayer(bool),
    /// WATCHDOG for a Spirc-routed local play (see `spirc_load_local`). Emitted by
    /// a timer the player arms after handing a local load to Spirc: `Spirc::load()`
    /// returns `Ok` as soon as the command is QUEUED, and the real load can still
    /// fail later inside the Spirc task (network / context resolve / bad track),
    /// which librespot only debug-logs — leaving riff silent. If no confirming
    /// Player event arrived within the timeout, this fires the bare-Player
    /// `fallback` so audio always happens. `generation` guards against races: it
    /// only fires if it is still the pending load (not confirmed, not superseded).
    SpircLoadWatchdog {
        generation: u64,
        fallback: SpotifyUri,
        start_playing: bool,
    },
}

#[derive(Clone)]
pub(crate) struct AppPlayerDelegate {
    sender: UnboundedSender<AppAction>,
}

impl AppPlayerDelegate {
    fn new(sender: UnboundedSender<AppAction>) -> Self {
        Self { sender }
    }

    fn send(&self, action: AppAction) {
        self.sender.unbounded_send(action).unwrap();
    }

    fn end_of_track_reached(&self) {
        self.send(PlaybackAction::Next.into())
    }

    // Report a resolved radio station back to the app: the seed track id plus the
    // fully-hydrated station songs (seed first, then the similar tracks). Metadata
    // is fetched on the player thread via librespot's internal metadata API,
    // because the Web API /v1/tracks endpoint 403s for this dev-mode app. The app
    // side then just loads these songs into the queue.
    fn radio_resolved(&self, seed_id: String, songs: Vec<SongDescription>) {
        self.send(AppAction::StartRadioResolved { seed_id, songs })
    }

    fn token_login_successful(&self, username: String) {
        self.send(LoginAction::SetLoginSuccess(username).into())
    }

    fn refresh_successful(&self) {
        self.send(LoginAction::TokenRefreshed.into())
    }

    fn report_error(&self, error: SpotifyError) {
        self.send(match error {
            SpotifyError::LoginFailed => LoginAction::SetLoginFailure.into(),
            SpotifyError::LoggedOut => LoginAction::Logout.into(),
            _ => AppAction::ShowNotification(format!("{error}")),
        })
    }

    fn notify_playback_state(&self, position: u32) {
        self.send(PlaybackAction::SyncSeek(position).into())
    }

    fn preload_next_track(&self) {
        self.send(PlaybackAction::PreloadNext.into())
    }

    fn login_challenge_started(&self, url: Url) {
        self.send(LoginAction::OpenLoginUrl(url).into())
    }

    // --- Spotify Connect RECEIVER (Spirc) mirroring -----------------------
    // The librespot Player is a MULTI-subscriber event source. Riff keeps its own
    // subscription (via `player_setup_delegate`); when Spirc — not riff's queue —
    // is driving the Player (a remote app transferred playback here), these bridge
    // the librespot Player events into riff's `PlaybackState` so the mini-player /
    // now-playing reflect what Spirc is playing. Display-only: riff does NOT issue
    // its own loads while receiving (see `is_remote_controlled`).

    /// Enter / leave receiver mode (Spirc took over / released the local Player).
    fn set_remote_controlled(&self, controlled: bool) {
        self.send(PlaybackAction::SetRemoteControlled(controlled).into())
    }

    /// riff lost active-device status: ANOTHER Connect device took over (Spirc
    /// deactivated riff — `PlayerEvent::Stopped` / `SessionDisconnected` while
    /// riff was the active device). Yield: clear both sticky flags so the
    /// desktop→riff mirror re-enables and riff switches to mirroring +
    /// controlling the device that took over. The app side also forces an
    /// immediate `/me/player` re-poll off this.
    fn yield_active_device(&self) {
        self.send(PlaybackAction::YieldToRemote.into())
    }

    /// Mirror the Spirc-driven current track into riff's display queue. We load a
    /// one-song queue and select it, so the existing now-playing UI renders it
    /// with no UI changes. Called on librespot `TrackChanged` while receiving.
    #[allow(deprecated)]
    fn mirror_remote_track(&self, song: SongDescription) {
        let id = song.id.clone();
        self.send(PlaybackAction::LoadSongs(vec![song]).into());
        self.send(PlaybackAction::Load(id).into());
    }

    /// Mirror the Spirc-driven play/pause state.
    fn mirror_remote_playing(&self, playing: bool) {
        if playing {
            self.send(PlaybackAction::Play.into())
        } else {
            self.send(PlaybackAction::Pause.into())
        }
    }
}

#[tokio::main]
async fn player_main(
    player_settings: SpotifyPlayerSettings,
    appaction_sender: UnboundedSender<AppAction>,
    token_store: TokenStore,
    sender: UnboundedSender<Command>,
    receiver: UnboundedReceiver<Command>,
) {
    task::spawn(async move {
        let delegate = AppPlayerDelegate::new(appaction_sender.clone());
        let player = SpotifyPlayer::new(player_settings, delegate, token_store, sender);
        player.start(receiver).await.unwrap();
    })
    .await
    .unwrap();
}

pub fn start_player_service(
    player_settings: SpotifyPlayerSettings,
    appaction_sender: UnboundedSender<AppAction>,
    token_store: TokenStore,
) -> UnboundedSender<Command> {
    let (sender, receiver) = unbounded::<Command>();
    let sender_clone = sender.clone();
    std::thread::spawn(move || {
        player_main(
            player_settings,
            appaction_sender,
            token_store,
            sender_clone,
            receiver,
        )
    });
    sender
}
