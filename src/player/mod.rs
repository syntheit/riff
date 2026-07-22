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
    /// Tell the player thread whether riff currently owns a LOCAL playback
    /// session (user played something in riff's own queue). The Player-event
    /// delegate reads this to decide, for an incoming librespot event, whether it
    /// is riff's own local playback (drive riff's queue: fire Next at end-of-track)
    /// or Spirc-driven receiver playback (mirror only, let Spirc advance).
    SetLocalOwnsPlayer(bool),
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
        eprintln!("RIFF_SPIRC: mirror set_remote_controlled({controlled})");
        self.send(PlaybackAction::SetRemoteControlled(controlled).into())
    }

    /// Mirror the Spirc-driven current track into riff's display queue. We load a
    /// one-song queue and select it, so the existing now-playing UI renders it
    /// with no UI changes. Called on librespot `TrackChanged` while receiving.
    #[allow(deprecated)]
    fn mirror_remote_track(&self, song: SongDescription) {
        eprintln!(
            "RIFF_SPIRC: mirror track '{}' — {}",
            song.title,
            song.artists_name()
        );
        let id = song.id.clone();
        self.send(PlaybackAction::LoadSongs(vec![song]).into());
        self.send(PlaybackAction::Load(id).into());
    }

    /// Mirror the Spirc-driven play/pause state.
    fn mirror_remote_playing(&self, playing: bool) {
        eprintln!("RIFF_SPIRC: mirror playing={playing}");
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
