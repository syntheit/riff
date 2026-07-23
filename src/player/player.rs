use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender};
use futures::stream::StreamExt;

use librespot::core::authentication::Credentials;
use librespot::core::cache::Cache;
use librespot::core::config::{DeviceType, SessionConfig};
use librespot::core::session::Session;
use librespot::core::spotify_id::SpotifyId;
use librespot::core::SpotifyUri;
use librespot::metadata::audio::item::{AudioItem, UniqueFields};
use librespot::metadata::{Metadata, Track};

// Spotify Connect RECEIVER: make riff a controllable Connect device via
// librespot's Spirc, spawned on top of riff's existing authenticated Session +
// Player + Mixer. See riff-connect.md §4 (Half B).
use librespot::connect::{
    ConnectConfig, LoadRequest, LoadRequestOptions, PlayingTrack, Spirc,
};

use librespot::playback::mixer::softmixer::SoftMixer;
use librespot::playback::mixer::{Mixer, MixerConfig};

use librespot::playback::audio_backend;
use librespot::playback::audio_backend::Sink;
use librespot::playback::config::{
    AudioFormat, Bitrate, NormalisationMethod, NormalisationType, PlayerConfig, VolumeCtrl,
};
use librespot::playback::player::{Player, PlayerEvent, PlayerEventChannel};

use crate::app::models::{AlbumRef, ArtistRef, ImageSet, RepeatMode, SongDescription};
use crate::audio_engine::{
    CaptureSink, EqController, EqProcessor, MixController, MixProcessor, MonoController,
    MonoProcessor, PanController, PanProcessor, PitchController, PitchProcessor, ProcessorChain,
};
use crate::player::AppPlayerDelegate;

use crate::auth::{AuthcodeChallenge, OAuthError, RiffOauthClient, TokenStore};

use super::Command;
use crate::app::credentials;
use crate::settings::RiffSettings;
use std::env;
use std::error::Error;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Debug)]
pub enum SpotifyError {
    LoginFailed,
    LoggedOut,
    PlayerNotReady,
    TechnicalError,
}

impl Error for SpotifyError {}

impl fmt::Display for SpotifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LoginFailed => write!(f, "Login failed!"),
            Self::LoggedOut => write!(f, "You are logged out!"),
            Self::PlayerNotReady => write!(f, "Player is not responding."),
            Self::TechnicalError => {
                write!(f, "A technical error occured. Check your connectivity.")
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioBackend {
    GStreamer(String),
    PulseAudio,
    Alsa(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VolumeCurveType {
    Log,
    Linear,
    Cubic,
}

impl Default for VolumeCurveType {
    fn default() -> Self {
        Self::Log
    }
}

#[derive(Debug, Clone)]
pub struct SpotifyPlayerSettings {
    pub bitrate: Bitrate,
    pub backend: AudioBackend,
    pub gapless: bool,
    pub ap_port: Option<u16>,

    pub shuffle: bool,
    pub repeat: RepeatMode,
    pub volume: f64,

    // Volume curve
    pub volume_curve: VolumeCurveType,

    // Normalization
    pub normalisation: bool,
    pub normalisation_type: NormalisationType,
    pub normalisation_method: NormalisationMethod,
    pub normalisation_pregain_db: f64,
    pub normalisation_threshold_dbfs: f64,
    pub normalisation_attack_ms: f64,
    pub normalisation_release_ms: f64,
    pub normalisation_knee_db: f64,

    // Audio format
    pub audio_format: AudioFormat,

    // Mono audio
    pub mono_audio: bool,

    // Stereo pan / balance (-1.0 = full left, 0.0 = center, 1.0 = full right).
    // Always enabled; centered has no effect.
    pub pan: f64,

    // Pitch shift in cents (1 cent = 1/100 semitone). 0.0 = no shift.
    pub pitch_cents: f64,

    // Equalizer. Active whenever any band is non-zero; flat = passthrough.
    pub eq_bands: [f64; 10],
}

impl Default for SpotifyPlayerSettings {
    fn default() -> Self {
        Self {
            volume: 0.7,
            repeat: RepeatMode::None,
            shuffle: false,

            bitrate: Bitrate::Bitrate160,
            gapless: true,
            backend: AudioBackend::PulseAudio,
            ap_port: None,

            volume_curve: VolumeCurveType::Log,

            normalisation: false,
            normalisation_type: NormalisationType::Auto,
            normalisation_method: NormalisationMethod::Dynamic,
            normalisation_pregain_db: 0.0,
            normalisation_threshold_dbfs: -2.0,
            normalisation_attack_ms: 5.0,
            normalisation_release_ms: 100.0,
            normalisation_knee_db: 5.0,

            audio_format: AudioFormat::default(),

            mono_audio: false,

            pan: 0.0,

            pitch_cents: 0.0,

            eq_bands: [0.0; 10],
        }
    }
}

impl SpotifyPlayerSettings {
    /// Whether a change from `self` to `other` requires recreating the librespot
    /// player (which interrupts playback). Equalizer, mono, pan, and pitch
    /// settings are excluded: they are applied live via their controllers and
    /// never require a reload.
    pub fn requires_reload(&self, other: &Self) -> bool {
        /// Epsilon for comparing normalisation parameters. Values come from
        /// GtkSpinRow adjustments (step = 0.5), so a threshold well below the
        /// smallest meaningful step avoids spurious reloads from FP rounding.
        const EPS: f64 = 1.0e-9;

        #[inline]
        fn f64_changed(a: f64, b: f64) -> bool {
            (a - b).abs() > EPS
        }

        self.bitrate != other.bitrate
            || self.backend != other.backend
            || self.gapless != other.gapless
            || self.ap_port != other.ap_port
            || self.volume_curve != other.volume_curve
            || self.normalisation != other.normalisation
            || self.normalisation_type != other.normalisation_type
            || self.normalisation_method != other.normalisation_method
            || f64_changed(
                self.normalisation_pregain_db,
                other.normalisation_pregain_db,
            )
            || f64_changed(
                self.normalisation_threshold_dbfs,
                other.normalisation_threshold_dbfs,
            )
            || f64_changed(self.normalisation_attack_ms, other.normalisation_attack_ms)
            || f64_changed(
                self.normalisation_release_ms,
                other.normalisation_release_ms,
            )
            || f64_changed(self.normalisation_knee_db, other.normalisation_knee_db)
            || self.audio_format != other.audio_format
    }
}

pub struct SpotifyPlayer {
    settings: SpotifyPlayerSettings,
    player: Option<Arc<Player>>,
    // `Arc<dyn Mixer>` (was `Box`) so the SAME mixer can be shared with Spirc:
    // Spirc::new wants `Arc<dyn Mixer>`, and volume set from either side must hit
    // the same mixer. `Mixer::set_volume` takes `&self`, so no `&mut` is needed.
    mixer: Option<Arc<dyn Mixer>>,
    session: Option<Session>,

    // --- Spotify Connect RECEIVER (Spirc) ---------------------------------
    // The Spirc control handle (Some once login has spawned it) + the JoinHandle
    // of its driving task (so we can abort it on logout / settings-reload and not
    // leak). Both are torn down together.
    spirc: Option<Spirc>,
    spirc_task: Option<tokio::task::JoinHandle<()>>,
    // Shared flag read by the Player-event delegate: TRUE while riff owns a LOCAL
    // playback session (its own queue drives the Player), FALSE otherwise. When a
    // librespot Player event arrives and this is FALSE, it must be Spirc-driven
    // (a remote transferred playback here), so the delegate MIRRORS it instead of
    // firing riff's own `Next` at end-of-track. Set by `Command::SetLocalOwnsPlayer`
    // from `PlayerNotifier`, mirroring the app-side `local_session_active`.
    local_owns_player: Arc<AtomicBool>,

    // Shared equalizer configuration, updated live without recreating the player.
    eq_controller: EqController,

    // Shared mono audio setting, updated live without recreating the player.
    mono_controller: MonoController,

    // Shared pan/balance configuration, updated live without recreating the player.
    pan_controller: PanController,

    // Shared pitch-shift setting (stub), updated live without recreating the player.
    pitch_controller: PitchController,

    // Shared mixer setting (stub), updated live without recreating the player.
    mix_controller: MixController,

    // Auth related stuff
    oauth_client: Arc<RiffOauthClient>,
    auth_challenge: Option<AuthcodeChallenge>,
    command_sender: UnboundedSender<Command>,

    // Receives feedback from commands or various events in the player
    delegate: AppPlayerDelegate,
}

impl SpotifyPlayer {
    pub fn new(
        settings: SpotifyPlayerSettings,
        delegate: AppPlayerDelegate,
        token_store: TokenStore,
        command_sender: UnboundedSender<Command>,
    ) -> Self {
        let eq_controller = EqController::new(settings.eq_bands);
        let mono_controller = MonoController::new(settings.mono_audio);
        let pan_controller = PanController::new(settings.pan);
        let pitch_controller = PitchController::new(settings.pitch_cents);
        let mix_controller = MixController::new(false);
        Self {
            settings,
            mixer: None,
            player: None,
            session: None,
            spirc: None,
            spirc_task: None,
            local_owns_player: Arc::new(AtomicBool::new(false)),
            eq_controller,
            mono_controller,
            pan_controller,
            pitch_controller,
            mix_controller,
            oauth_client: Arc::new(RiffOauthClient::new(token_store)),
            auth_challenge: None,
            command_sender,
            delegate,
        }
    }

    async fn handle_and_notify(&mut self, action: Command) {
        match self.handle(action).await {
            Ok(_) => {}
            Err(e) => self.delegate.report_error(e),
        }
    }

    fn get_player(&self) -> Result<&Arc<Player>, SpotifyError> {
        self.player.as_ref().ok_or(SpotifyError::PlayerNotReady)
    }

    fn get_player_mut(&mut self) -> Result<&mut Arc<Player>, SpotifyError> {
        self.player.as_mut().ok_or(SpotifyError::PlayerNotReady)
    }

    async fn handle(&mut self, action: Command) -> Result<(), SpotifyError> {
        match action {
            Command::PlayerSetVolume(volume) => {
                if let Some(mixer) = self.mixer.as_ref() {
                    mixer_set_volume(&**mixer, volume);
                }
                Ok(())
            }
            Command::SetEqualizer { bands } => {
                // Live update: no player/session recreation, no playback interruption.
                self.settings.eq_bands = bands;
                self.eq_controller.update(bands);
                Ok(())
            }
            Command::SetMono { enabled } => {
                // Live update: no player/session recreation, no playback interruption.
                self.settings.mono_audio = enabled;
                self.mono_controller.update(enabled);
                Ok(())
            }
            Command::SetPan { pan } => {
                // Live update: no player/session recreation, no playback interruption.
                self.settings.pan = pan;
                self.pan_controller.update(pan);
                Ok(())
            }
            Command::SetPitch { cents } => {
                // Live update: no player/session recreation, no playback interruption.
                self.settings.pitch_cents = cents;
                self.pitch_controller.update(cents);
                Ok(())
            }
            // --- Spotify Connect RECEIVER transport -----------------------
            // Route riff's own transport actions to the Spirc handle while riff is
            // the active Connect device. The handle methods are synchronous, only
            // queue a SpircCommand, and are no-ops when this device isn't active,
            // so an `Err` (channel closed = task gone) is logged, not surfaced.
            Command::SpircPlay => {
                self.spirc_do("play", |s| s.play());
                Ok(())
            }
            Command::SpircPause => {
                self.spirc_do("pause", |s| s.pause());
                Ok(())
            }
            Command::SpircNext => {
                self.spirc_do("next", |s| s.next());
                Ok(())
            }
            Command::SpircPrev => {
                self.spirc_do("prev", |s| s.prev());
                Ok(())
            }
            Command::SpircSeek(position_ms) => {
                self.spirc_do("seek", |s| s.set_position_ms(position_ms));
                Ok(())
            }
            Command::SpircSetVolume(fraction) => {
                let volume = (fraction.clamp(0.0, 1.0) * u16::MAX as f64) as u16;
                self.spirc_do("set_volume", |s| s.set_volume(volume));
                Ok(())
            }
            // --- LOCAL play announced THROUGH Spirc --------------------------
            // Playlist/album (Spotify context): load by `context_uri` + offset so
            // Spotify shows riff playing FROM that playlist/album. Prefer the track
            // uri as the `playing_track` (robust to context reordering), with the
            // numeric offset as a fallback index. If Spirc is offline the helper
            // falls back to the bare Player track so local audio still plays.
            Command::SpircLoadContext {
                context_uri,
                offset,
                playing_track_uri,
                start_playing,
                fallback,
            } => {
                let playing_track = playing_track_uri
                    .clone()
                    .map(PlayingTrack::Uri)
                    .or(Some(PlayingTrack::Index(offset as u32)));
                let options = LoadRequestOptions {
                    start_playing,
                    seek_to: 0,
                    context_options: None,
                    playing_track,
                };
                let request =
                    LoadRequest::from_context_uri(context_uri.clone(), options);
                let ctx = context_uri.clone();
                let track = playing_track_uri.clone();
                self.spirc_load_local(
                    "context",
                    request,
                    move || format!("context_uri={ctx} offset={offset} track={track:?}"),
                    fallback,
                    start_playing,
                );
                Ok(())
            }
            // Ad-hoc list (radio / Liked Songs / search / arbitrary queue — no
            // context uri): load an explicit `uris` track list + offset. librespot
            // 0.8 supports this via `LoadRequest::from_tracks`, so these sources ARE
            // announced too (not deferred). Offset picks the tapped track.
            Command::SpircLoadTracks {
                uris,
                offset,
                start_playing,
                fallback,
            } => {
                let playing_track = uris
                    .get(offset)
                    .cloned()
                    .map(PlayingTrack::Uri)
                    .or(Some(PlayingTrack::Index(offset as u32)));
                let options = LoadRequestOptions {
                    start_playing,
                    seek_to: 0,
                    context_options: None,
                    playing_track,
                };
                let len = uris.len();
                let request = LoadRequest::from_tracks(uris, options);
                self.spirc_load_local(
                    "tracks",
                    request,
                    move || format!("uris={len} offset={offset}"),
                    fallback,
                    start_playing,
                );
                Ok(())
            }
            Command::SetLocalOwnsPlayer(owns) => {
                // Mirror the app-side local-session flag onto the player thread so
                // the Player-event delegate can tell riff's own local playback
                // (fire Next at end-of-track) from Spirc-driven receiver playback
                // (mirror only, let Spirc advance).
                self.local_owns_player.store(owns, Ordering::Relaxed);
                Ok(())
            }
            Command::PlayerResume => {
                self.get_player()?.play();
                Ok(())
            }
            Command::PlayerPause => {
                self.get_player()?.pause();
                Ok(())
            }
            Command::PlayerStop => {
                self.get_player()?.stop();
                Ok(())
            }
            Command::PlayerSeek(position) => {
                self.get_player()?.seek(position);
                Ok(())
            }
            Command::PlayerLoad { track, resume } => {
                debug!("Player: playing track {track}");
                self.get_player_mut()?.load(track, resume, 0);
                Ok(())
            }
            Command::PlayerPreload(track) => {
                self.get_player_mut()?.preload(track);
                Ok(())
            }
            Command::StartRadio { seed_id } => {
                let session = self
                    .session
                    .as_ref()
                    .ok_or(SpotifyError::PlayerNotReady)?
                    .clone();

                // Resolve the station into concrete tracks entirely through the
                // librespot session. The Web API path is dead for this dev-mode
                // app: /v1/recommendations 404s, and both the editorial station
                // playlist and /v1/tracks?ids= 403. librespot's internal endpoints
                // use the account's full streaming access, so they work where the
                // Web API refuses.
                //
                // FAST PATH (issue #2): the apollo station response already carries
                // per-track metadata (title/artist/album/art), so we build the
                // `SongDescription`s straight from that JSON — a single request, no
                // 51× `Track::get` round-trips, making radio near-instant. Only the
                // fallback (get_context / apollo without metadata) still hydrates
                // ids via the metadata API.
                let seed_id_for_hydrate = seed_id.clone();
                let mut radio_songs = resolve_radio_songs(&session, &seed_id).await;
                eprintln!(
                    "RIFF_RADIO: resolved {} radio song(s) for seed {}",
                    radio_songs.len(),
                    seed_id
                );

                // Ensure the seed track leads the station and carries correct
                // metadata/art (used as the page cover). The apollo response may or
                // may not include the seed; hydrate it directly (1 metadata call)
                // and prepend it, then drop any later duplicate of the seed id.
                let seed_song = hydrate_single_song(&session, &seed_id_for_hydrate).await;

                let mut songs: Vec<SongDescription> =
                    Vec::with_capacity(radio_songs.len() + 1);
                if let Some(seed) = seed_song {
                    songs.push(seed);
                }
                radio_songs.retain(|s| s.id != seed_id_for_hydrate);
                songs.append(&mut radio_songs);

                if songs.is_empty() {
                    // Nothing resolved at all (even the seed failed): surface a
                    // gentle error rather than silently doing nothing.
                    return Err(SpotifyError::TechnicalError);
                }

                eprintln!("RIFF_RADIO: station has {} song(s)", songs.len());
                self.delegate.radio_resolved(seed_id, songs);
                Ok(())
            }
            Command::RefreshToken => {
                // Refresh ONLY the Web-API access token. Do NOT reconnect the
                // librespot session: librespot keeps streaming across Web-API token
                // refreshes, and its connection is single-use (OnceLock) — a second
                // `connect()` on the shared session would ERROR and break playback.
                // We still require an active session (i.e. logged in).
                if self.session.is_none() {
                    return Err(SpotifyError::PlayerNotReady);
                }
                self.oauth_client
                    .get_valid_token()
                    .await
                    .map_err(|_| SpotifyError::LoginFailed)?;
                self.delegate.refresh_successful();
                Ok(())
            }
            Command::Logout => {
                self.oauth_client.clear_credentials().await;
                // Tear down the Connect receiver first so it stops advertising the
                // device and doesn't outlive the Session.
                self.shutdown_spirc();
                if let Some(session) = self.session.take() {
                    session.shutdown();
                }
                let _ = self.player.take();
                Ok(())
            }
            Command::Restore => {
                let credentials =
                    self.oauth_client
                        .get_valid_token()
                        .await
                        .map_err(|e| match e {
                            OAuthError::LoggedOut => SpotifyError::LoggedOut,
                            _ => SpotifyError::LoginFailed,
                        })?;

                info!("Restoring session");
                self.initial_login(credentials).await
            }
            Command::InitLogin => {
                let auth_url = match self.auth_challenge.as_ref() {
                    Some(challenge) => challenge.auth_url.clone(),
                    None => {
                        let cmd = self.command_sender.clone();
                        let challenge = self
                            .oauth_client
                            .spawn_authcode_listener(move || {
                                cmd.unbounded_send(Command::CompleteLogin).unwrap();
                            })
                            .await
                            .map_err(|_| SpotifyError::LoginFailed)?;
                        let auth_url = challenge.auth_url.clone();
                        self.auth_challenge = Some(challenge);
                        auth_url
                    }
                };
                self.delegate.login_challenge_started(auth_url);
                Ok(())
            }
            Command::CompleteLogin => {
                let Some(challenge) = self.auth_challenge.take() else {
                    return Err(SpotifyError::LoginFailed);
                };

                let credentials = self
                    .oauth_client
                    .exchange_authcode(challenge)
                    .await
                    .map_err(|_| SpotifyError::LoginFailed)?;

                info!("Login with OAuth2");
                self.initial_login(credentials).await
            }
            Command::ReloadSettings => {
                let settings = RiffSettings::new_from_gsettings().unwrap_or_default();
                self.settings = settings.player_settings;

                // A live settings reload rebuilds the Player, which would orphan
                // the Spirc receiver (it holds the old Arc<Player>). It must ALSO
                // rebuild the Session: librespot 0.8 backs a Session's connection
                // with a `OnceLock`, so the already-connected old session can never
                // be re-`connect()`ed — Spirc (or a direct connect) needs a FRESH,
                // unconnected session. So: tear Spirc down, drop the old session,
                // build a fresh one, bind a new Player to it, and re-run the
                // connect-once logic (Spirc connects it when enabled, else direct).
                self.shutdown_spirc();
                if let Some(old_session) = self.session.take() {
                    // Shut the old connection down so it doesn't linger as an
                    // orphaned dealer/streaming session on the account.
                    old_session.shutdown();
                }

                // Clear the mixer so it gets recreated with updated volume curve/dB range
                self.mixer.take();

                // Need credentials to (re)connect the fresh session. If we can't
                // get a token, we're effectively logged out — surface it.
                let token = self
                    .oauth_client
                    .get_valid_token()
                    .await
                    .map_err(|_| SpotifyError::LoggedOut)?;
                let creds = Credentials::with_access_token(token.access_token);

                let new_session = build_unconnected_session(self.settings.ap_port);
                let new_player = self.create_player(new_session.clone());
                tokio::task::spawn(player_setup_delegate(
                    new_player.get_player_event_channel(),
                    self.delegate.clone(),
                    Arc::clone(&self.local_owns_player),
                ));
                self.player.replace(new_player);
                self.session.replace(new_session);

                // Connect the fresh session exactly once (Spirc when enabled, else
                // direct) and bring the receiver back up on the rebuilt Player.
                self.connect_shared_session(&creds).await?;

                Ok(())
            }
        }
    }

    async fn initial_login(
        &mut self,
        mut credentials: credentials::Credentials,
    ) -> Result<(), SpotifyError> {
        // Make the given (already-refreshed, by get_valid_token) credentials
        // usable by the Web API *immediately*, before we probe premium status or
        // touch librespot. This is in-memory only: we defer the keyring persist
        // until premium is confirmed (see save_credentials below) so a genuine
        // non-premium account is not saved and retried on next launch — but the
        // running session can still make Web API calls regardless.
        self.oauth_client.cache_credentials(&credentials);

        // Check if the account is premium before connecting to librespot.
        // librespot will crash the process for free accounts, so we must
        // catch this early and report a graceful error instead.
        //
        // Crucially, a *failed* probe (e.g. a 401 from an expired/revoked probe
        // token, or a network error) must NOT be treated as "not premium":
        // doing so would wrongly abort login for a genuine premium account on
        // every relaunch. We only abort when /me actually succeeds and reports a
        // non-premium account.
        let mut status = crate::api::check_premium(&credentials.access_token).await;

        // If the probe could not be completed, the access token we probed with
        // may be stale despite looking valid (clock skew, server-side
        // revocation just before expiry, etc.). Force one refresh and re-probe
        // with the fresh token before drawing any conclusion.
        if let crate::api::PremiumStatus::ProbeFailed(e) = &status {
            warn!("Premium probe failed ({e}); forcing a token refresh and retrying");
            match self.oauth_client.force_refresh().await {
                Ok(refreshed) => {
                    credentials = refreshed;
                    self.oauth_client.cache_credentials(&credentials);
                    status = crate::api::check_premium(&credentials.access_token).await;
                }
                Err(e) => {
                    // A genuine refresh failure means we are really logged out
                    // (the refresh token was rejected). force_refresh/refresh_token
                    // already cleared the store in that case.
                    warn!("Token refresh failed during login: {e}");
                    return Err(SpotifyError::LoggedOut);
                }
            }
        }

        match status {
            crate::api::PremiumStatus::NotPremium => {
                // /me succeeded and genuinely reports a non-premium account.
                warn!("Account is not premium, aborting login");
                return Err(SpotifyError::LoginFailed);
            }
            crate::api::PremiumStatus::ProbeFailed(e) => {
                // Still couldn't confirm premium status even after a refresh.
                // Do NOT conclude "not premium" and do NOT wipe the refresh
                // token — this is a transient/technical failure. The in-memory
                // token is populated, so surface a technical error and let the
                // user retry rather than forcing a re-login.
                warn!("Could not confirm premium status after refresh: {e}");
                return Err(SpotifyError::TechnicalError);
            }
            crate::api::PremiumStatus::Premium => {}
        }

        // Premium confirmed: now it is safe to persist the (possibly refreshed)
        // credentials to the keyring for the next launch.
        self.oauth_client.save_credentials(&credentials).await;

        let creds = Credentials::with_access_token(&credentials.access_token);

        // ONE session per login. Build it UNCONNECTED. The audio `Player` is bound
        // to it now (Player construction needs no live connection); the SINGLE
        // `connect()` happens below — performed BY Spirc when the receiver is
        // enabled, or by us directly when it is disabled. This is the whole fix:
        // Player and Spirc share ONE connected session, so Spotify never bumps one
        // for the other (the two-session bug that killed local playback).
        let new_session = build_unconnected_session(self.settings.ap_port);

        // Bind the Player to the (still unconnected) shared session and store both
        // BEFORE connecting, so the connect-once logic below drives the very
        // session the Player streams through.
        let new_player = self.create_player(new_session.clone());
        tokio::task::spawn(player_setup_delegate(
            new_player.get_player_event_channel(),
            self.delegate.clone(),
            Arc::clone(&self.local_owns_player),
        ));
        self.player.replace(new_player);
        self.session.replace(new_session);

        // CONNECT THE SHARED SESSION EXACTLY ONCE, then spawn the Connect receiver.
        // - Spirc enabled : `spawn_spirc()` hands the shared session to
        //   `Spirc::new`, which calls `session.connect()` once. On success both the
        //   receiver AND the Player are live on that one connection.
        // - Spirc disabled: no one else connects, so we connect it ourselves here.
        // Either way the session is CONNECTED before this returns, and thus before
        // the user can trigger a track load (playback is user-initiated afterward).
        self.connect_shared_session(&creds).await?;

        // Read the canonical username only AFTER the session is connected —
        // `Session::username()` is empty until `connect()` populates it (librespot
        // 0.8 sets it inside `connect`). This is the account id used for Web-API
        // calls (playlists, etc.), so it must be the real value, not "".
        let username = self
            .session
            .as_ref()
            .map(|s| s.username())
            .unwrap_or_default();

        // Keep the Web-API access token fresh for the lifetime of the login. This
        // does NOT touch the librespot session: librespot keeps streaming across
        // Web-API token refreshes, and its connection is single-use (OnceLock), so
        // reconnecting here would ERROR and is unnecessary.
        let oauth_client = Arc::clone(&self.oauth_client);
        tokio::task::spawn(async move {
            loop {
                if oauth_client.refresh_token_at_expiry().await.is_err() {
                    // Logged out / no refresh token: stop the loop rather than spin.
                    break;
                }
            }
        });

        self.delegate.token_login_successful(username);

        Ok(())
    }

    /// Connect riff's shared (currently UNCONNECTED) `self.session` EXACTLY ONCE,
    /// and bring up the Connect receiver on that same session.
    ///
    /// librespot 0.8 backs a `Session`'s connection with a `OnceLock`, so
    /// `connect()` may be called at most once per `Session`. To keep the audio
    /// `Player` and the Spirc receiver on ONE connection:
    ///   * Spirc ENABLED  — hand the shared session to `Spirc::new`, which performs
    ///     the single `connect()` internally. We do NOT connect it ourselves.
    ///   * Spirc DISABLED — connect the shared session directly so local playback
    ///     still works with no receiver.
    /// On return (Ok) `self.session` is connected. Requires `self.session` and
    /// `self.player` to already be set.
    async fn connect_shared_session(
        &mut self,
        creds: &Credentials,
    ) -> Result<(), SpotifyError> {
        if Self::spirc_enabled() {
            // Spirc performs the single `connect()` on the shared session.
            if self.spawn_spirc(creds).await {
                return Ok(());
            }
            // Spirc failed to start. It may have failed AFTER its internal
            // `session.connect()` already succeeded (e.g. a later `auth_token()`
            // step in `Spirc::new`). If so, the shared session is ALREADY
            // connected and we MUST NOT connect it again — librespot's `OnceLock`
            // makes a second `connect()` error. Detect that via the canonical
            // username, which `connect()` populates on success (empty otherwise):
            //   * username set  -> session connected by the partial Spirc run; the
            //     Player can stream, so we're done.
            //   * username empty -> Spirc failed BEFORE connecting; fall through to
            //     a direct connect so local playback still works.
            let already_connected = self
                .session
                .as_ref()
                .map(|s| !s.username().is_empty())
                .unwrap_or(false);
            if already_connected {
                eprintln!(
                    "RIFF_SPIRC: receiver failed to start but shared session is already connected — local playback OK, receiver absent"
                );
                return Ok(());
            }
            eprintln!(
                "RIFF_SPIRC: receiver did not start (session unconnected) — connecting shared session directly so local playback works"
            );
        }

        // Spirc disabled (or failed before connecting): connect the shared session
        // ourselves — exactly one connect on this session either way.
        let session = self.session.as_ref().ok_or(SpotifyError::PlayerNotReady)?;
        match session.connect(creds.clone(), true).await {
            Ok(_) => Ok(()),
            Err(err) => {
                warn!("Login failure (direct session connect): {err}");
                Err(SpotifyError::LoginFailed)
            }
        }
    }

    /// Whether the Spotify Connect receiver (Spirc) is enabled. On by default;
    /// set `RIFF_SPIRC_DISABLE=1` to opt out (e.g. to save the warm dealer
    /// websocket on battery). Kept as an env var to avoid a gschema change.
    fn spirc_enabled() -> bool {
        !matches!(env::var("RIFF_SPIRC_DISABLE").as_deref(), Ok("1") | Ok("true"))
    }

    /// Run a fire-and-forget action on the Spirc handle, logging (never
    /// surfacing) a closed-channel error. No-op when Spirc isn't running.
    fn spirc_do<F>(&self, what: &str, f: F)
    where
        F: FnOnce(&Spirc) -> Result<(), librespot::core::Error>,
    {
        match self.spirc.as_ref() {
            Some(spirc) => {
                eprintln!("RIFF_SPIRC: handle command {what}");
                if let Err(e) = f(spirc) {
                    eprintln!("RIFF_SPIRC: handle command {what} failed: {e}");
                }
            }
            None => eprintln!("RIFF_SPIRC: {what} ignored — no Spirc running"),
        }
    }

    /// Route a LOCAL play THROUGH Spirc so it announces riff as the active device.
    ///
    /// When Spirc is online: `activate()` (acquire control as the active Connect
    /// device) then `load(request)` (load into the SHARED Player, so riff still
    /// HEARS the audio, AND Spirc owns the play_request_id → reports riff active to
    /// Spotify). Both go through Spirc's single serialized command channel, so the
    /// activate is guaranteed to run before the load. Because Spirc now drives the
    /// shared Player, this play is a RECEIVER-mode play from the Player-event
    /// delegate's view — so we clear `local_owns_player` (mirror it, let Spirc
    /// advance the queue), exactly like a remote transfer. The existing Half-B
    /// mirror then reflects it in riff's own now-playing/mini-player.
    ///
    /// When Spirc is OFFLINE (disabled / failed to start): FALL BACK to the bare
    /// Player using `fallback`, and set `local_owns_player = true` so riff owns its
    /// own queue as before. This guarantees local audio never depends on Spirc.
    ///
    /// `what` is a short label for logging; `describe` renders the load target.
    fn spirc_load_local(
        &mut self,
        what: &str,
        request: LoadRequest,
        describe: impl FnOnce() -> String,
        fallback: SpotifyUri,
        start_playing: bool,
    ) {
        // Attempt the activate + load against the Spirc handle in a scope that
        // releases the `&self.spirc` borrow before any `&mut self` fallback call.
        // `None`  -> Spirc absent (fall back). `Some(true)` -> announced OK.
        // `Some(false)` -> Spirc present but a call errored (fall back).
        let announced = match self.spirc.as_ref() {
            Some(spirc) => {
                eprintln!(
                    "RIFF_SPIRC: local play routed THROUGH Spirc ({what}) {}",
                    describe()
                );
                if let Err(e) = spirc.activate() {
                    eprintln!("RIFF_SPIRC: activate failed ({what}): {e} — falling back to bare Player");
                    Some(false)
                } else {
                    eprintln!("RIFF_SPIRC: activate ({what})");
                    if let Err(e) = spirc.load(request) {
                        eprintln!("RIFF_SPIRC: load failed ({what}): {e} — falling back to bare Player");
                        Some(false)
                    } else {
                        Some(true)
                    }
                }
            }
            None => None,
        };

        match announced {
            Some(true) => {
                // Spirc now drives the shared Player: the event delegate must treat
                // this as receiver-mode (mirror only, no double Next), exactly like
                // a remote transfer. Clear ownership AFTER a successful load — the
                // first emitted event (Loading/Playing) is what flips the mirror on.
                self.local_owns_player.store(false, Ordering::Relaxed);
            }
            _ => {
                eprintln!(
                    "RIFF_SPIRC: FALLBACK to bare Player ({what}) — local audio preserved"
                );
                self.spirc_load_fallback(what, fallback, start_playing);
            }
        }
    }

    /// Bare-Player fallback for `spirc_load_local`: load `track` directly into the
    /// Player (the pre-Half-B-announce path), and mark riff as owning its own
    /// queue so the event delegate advances it normally. Used whenever Spirc is
    /// absent or an `activate`/`load` call errors — so local audio always plays.
    fn spirc_load_fallback(&mut self, what: &str, track: SpotifyUri, start_playing: bool) {
        self.local_owns_player.store(true, Ordering::Relaxed);
        match self.get_player_mut() {
            Ok(player) => {
                eprintln!("RIFF_SPIRC: bare-Player fallback load ({what}) {track}");
                player.load(track, start_playing, 0);
            }
            Err(_) => eprintln!("RIFF_SPIRC: fallback load ({what}) skipped — player not ready"),
        }
    }

    /// Spawn the Spirc Connect receiver on riff's SHARED (still UNCONNECTED)
    /// `self.session`, connecting that one session via `Spirc::new`. The audio
    /// `Player` is bound to the same session, so after this the receiver and local
    /// playback both ride ONE connection — no second session, no account bump.
    ///
    /// Returns `true` iff the receiver started, in which case the shared session
    /// is now CONNECTED (Spirc performed the single `connect()`). Returns `false`
    /// if Spirc is disabled or failed to start; in that case the shared session is
    /// still unconnected and the caller must connect it directly. Shuts down any
    /// prior Spirc first (respawn path).
    ///
    /// `creds` are the credentials Spirc uses for the single `session.connect()`.
    async fn spawn_spirc(&mut self, creds: &Credentials) -> bool {
        if !Self::spirc_enabled() {
            eprintln!("RIFF_SPIRC: receiver disabled via RIFF_SPIRC_DISABLE");
            return false;
        }

        // Always start from a clean slate (respawn path).
        self.shutdown_spirc();

        // Clone the shared handles into owned values up front so we don't hold any
        // borrow of `self` across the `Spirc::new` await, leaving `self` free for
        // the `self.spirc = ...` assignment after. Spirc is handed the SAME
        // `self.session` the Player streams through — that is the single-session
        // fix. `Spirc::new` calls `session.connect()` on it exactly once.
        let (Some(session), Some(player), Some(mixer)) = (
            self.session.clone(),
            self.player.clone(),
            self.mixer.clone(),
        ) else {
            eprintln!("RIFF_SPIRC: cannot spawn — session/player/mixer not ready");
            return false;
        };
        let initial_volume = (self.settings.volume.clamp(0.0, 1.0) * u16::MAX as f64) as u16;

        let credentials = creds.clone();

        let device_name = connect_device_name();
        let config = ConnectConfig {
            name: device_name.clone(),
            device_type: DeviceType::Smartphone,
            // librespot's initial_volume is a u16 across the full range.
            initial_volume,
            ..Default::default()
        };

        eprintln!("RIFF_SPIRC: spawning Connect receiver as '{device_name}' (connecting shared session)");
        match Spirc::new(config, session, credentials, player, mixer).await {
            Ok((spirc, spirc_task)) => {
                let task = tokio::task::spawn(spirc_task);
                self.spirc = Some(spirc);
                self.spirc_task = Some(task);
                eprintln!("RIFF_SPIRC: receiver online (shared session connected)");
                true
            }
            Err(e) => {
                eprintln!("RIFF_SPIRC: failed to start receiver: {e}");
                false
            }
        }
    }

    /// Shut down the Spirc receiver + abort its task, clearing the device from
    /// other apps' lists. Safe to call when nothing is running.
    fn shutdown_spirc(&mut self) {
        if let Some(spirc) = self.spirc.take() {
            eprintln!("RIFF_SPIRC: shutting down receiver");
            let _ = spirc.shutdown();
        }
        if let Some(task) = self.spirc_task.take() {
            task.abort();
        }
    }

    fn create_player(&mut self, session: Session) -> Arc<Player> {
        let backend = self.settings.backend.clone();
        let audio_format = self.settings.audio_format;

        // Convert attack/release from milliseconds to coefficients
        let normalisation_attack_cf = librespot::playback::player::duration_to_coefficient(
            std::time::Duration::from_secs_f64(self.settings.normalisation_attack_ms / 1000.0),
        );
        let normalisation_release_cf = librespot::playback::player::duration_to_coefficient(
            std::time::Duration::from_secs_f64(self.settings.normalisation_release_ms / 1000.0),
        );

        let player_config = PlayerConfig {
            gapless: self.settings.gapless,
            bitrate: self.settings.bitrate,
            normalisation: self.settings.normalisation,
            normalisation_type: self.settings.normalisation_type,
            normalisation_method: self.settings.normalisation_method,
            normalisation_pregain_db: self.settings.normalisation_pregain_db,
            normalisation_threshold_dbfs: self.settings.normalisation_threshold_dbfs,
            normalisation_attack_cf,
            normalisation_release_cf,
            normalisation_knee_db: self.settings.normalisation_knee_db,
            ..Default::default()
        };
        info!("bitrate: {:?}", &player_config.bitrate);
        info!(
            "volume curve: {:?}, dB range: {:.1}",
            self.settings.volume_curve,
            VolumeCtrl::DEFAULT_DB_RANGE
        );
        if player_config.normalisation {
            info!(
                "normalisation: type={:?}, method={:?}, pregain={:.1}dB",
                player_config.normalisation_type,
                player_config.normalisation_method,
                player_config.normalisation_pregain_db
            );
        }

        let volume = self.settings.volume;
        let volume_curve = self.settings.volume_curve;
        let soft_volume = self
            .mixer
            .get_or_insert_with(|| {
                let volume_ctrl = match volume_curve {
                    VolumeCurveType::Log => VolumeCtrl::Log(VolumeCtrl::DEFAULT_DB_RANGE),
                    VolumeCurveType::Linear => VolumeCtrl::Linear,
                    VolumeCurveType::Cubic => VolumeCtrl::Cubic(VolumeCtrl::DEFAULT_DB_RANGE),
                };
                // `Arc<dyn Mixer>` so the SAME mixer instance can be handed to
                // Spirc (Spirc::new wants `Arc<dyn Mixer>`).
                let mix: Arc<dyn Mixer> = Arc::new(
                    SoftMixer::open(MixerConfig {
                        volume_ctrl,
                        ..Default::default()
                    })
                    .expect("Failed to create soft mixer"),
                );
                mixer_set_volume(&*mix, volume);
                mix
            })
            .get_soft_volume();

        let eq_controller = self.eq_controller.clone();
        let mono_controller = self.mono_controller.clone();
        let pan_controller = self.pan_controller.clone();
        let pitch_controller = self.pitch_controller.clone();
        let mix_controller = self.mix_controller.clone();

        Player::new(player_config, session, soft_volume, move || {
            let sink: Box<dyn Sink> = match backend {
                AudioBackend::GStreamer(pipeline) => {
                    let backend = audio_backend::find(Some("gstreamer".to_string())).unwrap();
                    backend(Some(pipeline), audio_format)
                }
                AudioBackend::PulseAudio => {
                    info!("using pulseaudio");
                    env::set_var("PULSE_PROP_application.name", "Riff");
                    let backend = audio_backend::find(Some("pulseaudio".to_string())).unwrap();
                    backend(None, audio_format)
                }
                AudioBackend::Alsa(device) => {
                    info!("using alsa ({})", &device);
                    let backend = audio_backend::find(Some("alsa".to_string())).unwrap();
                    backend(Some(device), audio_format)
                }
            };

            // Route decoded audio through the audio engine pipeline before it
            // reaches the backend. The chain runs in a fixed order; each stage
            // passes audio through untouched while disabled and applies live
            // updates via its controller otherwise.
            let chain = ProcessorChain::new()
                .with(Box::new(EqProcessor::new(eq_controller)))
                .with(Box::new(MonoProcessor::new(mono_controller)))
                .with(Box::new(PanProcessor::new(pan_controller)))
                .with(Box::new(PitchProcessor::new(pitch_controller)))
                .with(Box::new(MixProcessor::new(mix_controller)));

            CaptureSink::wrap(sink, chain)
        })
    }

    pub async fn start(self, receiver: UnboundedReceiver<Command>) -> Result<(), ()> {
        receiver
            .fold(self, |mut player, action| async {
                player.handle_and_notify(action).await;
                player
            })
            .await;
        Ok(())
    }
}

/// Build a FRESH, NOT-yet-connected librespot `Session` sharing riff's cache.
///
/// There is exactly ONE `Session` object per login, shared by the audio `Player`
/// and the Spirc receiver. It is CONNECTED exactly once — by `Spirc::new` when the
/// receiver is enabled, or by a direct `session.connect()` when it is disabled
/// (see `connect_shared_session`). librespot 0.8 backs a `Session`'s connection
/// with a `OnceLock`, so `connect()` may run at most once per `Session`; that is
/// why this returns an UNCONNECTED session and callers must ensure a single
/// connect. On any event that would otherwise re-connect (settings reload,
/// re-login) a brand new session is built here instead.
fn build_unconnected_session(ap_port: Option<u16>) -> Session {
    let session_config = SessionConfig {
        ap_port,
        ..Default::default()
    };
    let root = glib::user_cache_dir().join("riff").join("librespot");
    let cache = Cache::new(
        Some(root.join("credentials")),
        Some(root.join("volume")),
        Some(root.join("audio")),
        None,
    )
    .map_err(|e| dbg!(e))
    .ok();
    Session::new(session_config, cache)
}

async fn player_setup_delegate(
    mut channel: PlayerEventChannel,
    delegate: AppPlayerDelegate,
    local_owns_player: Arc<AtomicBool>,
) {
    // Tracks whether we last told the app it was in receiver (Spirc-driven) mode,
    // so we only emit `set_remote_controlled` on an actual edge.
    let mut receiving = false;

    while let Some(event) = channel.recv().await {
        // Who owns the Player right now? If riff started a local session, these
        // events are riff's own local playback (drive the queue). Otherwise a
        // remote app transferred playback here and Spirc is driving the Player —
        // we MIRROR its events and must NOT fire riff's own `Next`.
        let owns_local = local_owns_player.load(Ordering::Relaxed);

        // Detect the receiver-mode edge from Player activity we didn't originate.
        let spirc_active_event = matches!(
            event,
            PlayerEvent::TrackChanged { .. }
                | PlayerEvent::Playing { .. }
                | PlayerEvent::Loading { .. }
        );
        if !owns_local && spirc_active_event && !receiving {
            receiving = true;
            delegate.set_remote_controlled(true);
        }
        // If riff owns local playback again, leave receiver mode.
        if owns_local && receiving {
            receiving = false;
            delegate.set_remote_controlled(false);
        }

        if receiving {
            // ---- RECEIVER MODE: mirror Spirc-driven playback (display only) ----
            match event {
                PlayerEvent::TrackChanged { audio_item } => {
                    if let Some(song) = song_from_audio_item(&audio_item) {
                        delegate.mirror_remote_track(song);
                    }
                }
                PlayerEvent::Playing { position_ms, .. } => {
                    delegate.mirror_remote_playing(true);
                    delegate.notify_playback_state(position_ms);
                }
                PlayerEvent::Paused { position_ms, .. } => {
                    delegate.mirror_remote_playing(false);
                    delegate.notify_playback_state(position_ms);
                }
                PlayerEvent::Seeked { position_ms, .. }
                | PlayerEvent::PositionCorrection { position_ms, .. } => {
                    delegate.notify_playback_state(position_ms);
                }
                // Spirc stopped driving riff's Player while we were the active
                // Connect device. This is the DEACTIVATION / TAKEOVER edge: another
                // device became the active player (librespot's Spirc calls
                // handle_disconnect()+handle_stop() → emits Stopped +
                // SessionDisconnected), or the remote that had transferred playback
                // here stopped entirely. Either way riff is no longer the active
                // output, so YIELD: clear both sticky flags app-side, re-enable the
                // OTHER-device mirror, and re-poll /me/player so the UI switches to
                // the device that took over. A user PAUSE of riff-as-active does NOT
                // reach here — that's a `Paused` event (mirrored above), so this is
                // never a spurious yield on pause.
                PlayerEvent::Stopped { .. } => {
                    eprintln!("RIFF_SPIRC: receiver deactivated (Stopped) — takeover/stop, yielding");
                    receiving = false;
                    delegate.yield_active_device();
                }
                PlayerEvent::SessionDisconnected { .. } => {
                    eprintln!(
                        "RIFF_SPIRC: receiver deactivated (SessionDisconnected) — takeover/stop, yielding"
                    );
                    receiving = false;
                    delegate.yield_active_device();
                }
                // Crucially, do NOT fire riff's own `Next` on EndOfTrack while
                // receiving — Spirc owns advancing the queue.
                _ => {}
            }
            continue;
        }

        // ---- LOCAL MODE: unchanged behavior (riff owns the queue) ----
        match event {
            PlayerEvent::EndOfTrack { .. } => {
                delegate.end_of_track_reached();
            }
            PlayerEvent::Playing { position_ms, .. } => {
                delegate.notify_playback_state(position_ms);
            }
            PlayerEvent::TimeToPreloadNextTrack { .. } => {
                debug!("Requesting next track to be preloaded...");
                delegate.preload_next_track();
            }
            _ => {}
        }
    }
}

/// Build a riff `SongDescription` from a librespot `AudioItem` (delivered by the
/// `PlayerEvent::TrackChanged` event while Spirc drives playback). Mirrors the
/// shape of `song_from_track` but sources fields from the already-resolved
/// `AudioItem` (no extra metadata fetch). Returns `None` for non-track items
/// (episodes / local files) we don't render in the now-playing bar.
fn song_from_audio_item(item: &AudioItem) -> Option<SongDescription> {
    let id = item.track_id.to_id().ok()?;

    let (artists, album_name, track_number) = match &item.unique_fields {
        UniqueFields::Track {
            artists,
            album,
            number,
            ..
        } => {
            let artists: Vec<ArtistRef> = artists
                .0
                .iter()
                .map(|a| ArtistRef {
                    id: a.id.to_id().unwrap_or_default(),
                    name: a.name.clone(),
                })
                .collect();
            let number = if *number > 0 { Some(*number) } else { None };
            (artists, album.clone(), number)
        }
        // Episodes / local files: no artist/album in the Track shape; render the
        // name only so the now-playing bar still updates.
        _ => (Vec::new(), String::new(), None),
    };

    // AudioItem covers already carry ready-to-use CDN urls (+ width).
    let art = ImageSet::from_images(item.covers.iter().map(|c| {
        let width = if c.width > 0 { Some(c.width as u32) } else { None };
        (width, c.url.clone())
    }));

    Some(SongDescription {
        id,
        track_number,
        uri: item.uri.clone(),
        title: item.name.clone(),
        artists,
        album: AlbumRef {
            id: String::new(),
            name: album_name,
        },
        duration_ms: item.duration_ms,
        art,
    })
}

/// The name riff advertises as a Spotify Connect device. Prefers `RIFF_SPIRC_NAME`,
/// then the system hostname as "riff (<host>)", falling back to plain "riff".
///
/// Also used by the takeover-watch poll to recognize riff's OWN device in
/// `GET /me/player` (so "riff is the active device, just paused" is NOT mistaken
/// for another device taking over).
pub fn connect_device_name() -> String {
    if let Ok(name) = env::var("RIFF_SPIRC_NAME") {
        if !name.trim().is_empty() {
            return name;
        }
    }
    let host = glib::host_name().to_string();
    if host.is_empty() {
        "riff".to_string()
    } else {
        format!("riff ({host})")
    }
}

/// Resolve a "song radio" station into concrete `SongDescription`s, entirely
/// through the librespot session (no Web API). The seed is NOT prepended here —
/// the caller hydrates + prepends the seed and dedups.
///
/// Strategy, in order:
///  1. `get_apollo_station("tracks", "spotify:track:{seed}", …)` — the
///     `/radio-apollo/v3/tracks/{ctx}` endpoint. The response already carries a
///     per-track `metadata` block (title / artist_name / artist_uri /
///     album_title / image_url), so we build `SongDescription`s DIRECTLY from
///     the JSON: one request, no per-track metadata round-trips. This is the
///     fast path that makes radio near-instant (issue #2).
///  2. If scope "tracks" yields nothing, retry with scope "stations".
///  3. If the apollo response carries track uris but NO usable metadata, fall
///     back to hydrating those ids via `Track::get`.
///  4. If the apollo response has no track uris at all but *does* reference a
///     playlist/station/album context uri, resolve that context into ids via
///     `spclient().get_context(uri)` and hydrate them via `Track::get`.
///
/// The raw response head is logged (RIFF_RADIO) so the on-device schema can be
/// confirmed and this can be tightened later.
async fn resolve_radio_songs(session: &Session, seed_id: &str) -> Vec<SongDescription> {
    let context_uri = format!("spotify:track:{seed_id}");

    // (1)/(2): try the apollo station endpoint, scope "tracks" then "stations".
    for scope in ["tracks", "stations"] {
        match session
            .spclient()
            .get_apollo_station(scope, &context_uri, Some(50), Vec::new(), true)
            .await
        {
            Ok(bytes) => {
                let preview: String =
                    String::from_utf8_lossy(&bytes).chars().take(800).collect();
                eprintln!(
                    "RIFF_RADIO: apollo scope={scope} raw head ({} bytes): {preview}",
                    bytes.len()
                );

                let value: serde_json::Value = match serde_json::from_slice(&bytes) {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!("RIFF_RADIO: apollo scope={scope} JSON parse failed: {e}");
                        continue;
                    }
                };

                // (a) FAST PATH: build songs straight from the per-track metadata
                // in the apollo payload (no Track::get).
                let songs = songs_from_apollo_json(&value);
                if !songs.is_empty() {
                    eprintln!(
                        "RIFF_RADIO: apollo scope={scope} built {} song(s) from JSON metadata",
                        songs.len()
                    );
                    return songs;
                }

                // (b) apollo had track uris but no usable metadata: hydrate ids.
                let ids = collect_uri_ids(&value, "spotify:track:");
                if !ids.is_empty() {
                    eprintln!(
                        "RIFF_RADIO: apollo scope={scope} yielded {} track uri(s) w/o metadata; hydrating",
                        ids.len()
                    );
                    let songs = hydrate_radio_songs(session, &ids).await;
                    if !songs.is_empty() {
                        return songs;
                    }
                }

                // (c) apollo returned only a context uri (playlist/station/album);
                // resolve that context into ids via the internal resolver, then
                // hydrate.
                if let Some(ctx_uri) = first_context_uri(&value) {
                    eprintln!(
                        "RIFF_RADIO: apollo scope={scope} returned context uri {ctx_uri}; resolving via get_context"
                    );
                    let ids = resolve_context_track_ids(session, &ctx_uri).await;
                    if !ids.is_empty() {
                        let songs = hydrate_radio_songs(session, &ids).await;
                        if !songs.is_empty() {
                            return songs;
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!("RIFF_RADIO: get_apollo_station scope={scope} failed: {e}");
            }
        }
    }

    eprintln!("RIFF_RADIO: no radio songs resolved for seed {seed_id}");
    Vec::new()
}

/// Build `SongDescription`s directly from the apollo station JSON's per-track
/// `metadata` blocks — the fast path (issue #2). Each station track looks like:
///
/// ```json
/// { "uri": "spotify:track:{id}",
///   "metadata": {
///     "title": "...", "artist_name": "...", "artist_uri": "spotify:artist:{id}",
///     "album_title": "...", "image_url": "spotify:image:{hex}" } }
/// ```
///
/// The image uri (`spotify:image:{hex}`) maps to the CDN url
/// `https://i.scdn.co/image/{hex}`, the same shape riff's ImageLoader fetches.
/// Tracks are walked in document order and de-duplicated by id. Any track
/// missing a usable `spotify:track:` uri or a title is skipped; if none survive
/// (e.g. the payload has no per-track metadata) the caller falls back to id
/// hydration.
fn songs_from_apollo_json(value: &serde_json::Value) -> Vec<SongDescription> {
    let mut songs: Vec<SongDescription> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    // The track array lives under a top-level key (observed: `tracks`); be
    // tolerant and search any array of objects whose items carry a
    // `spotify:track:` uri + a `metadata` object.
    fn walk(
        value: &serde_json::Value,
        songs: &mut Vec<SongDescription>,
        seen: &mut std::collections::HashSet<String>,
    ) {
        match value {
            serde_json::Value::Array(arr) => {
                for v in arr {
                    if let Some(song) = song_from_apollo_track(v) {
                        if seen.insert(song.id.clone()) {
                            songs.push(song);
                        }
                    } else {
                        walk(v, songs, seen);
                    }
                }
            }
            serde_json::Value::Object(map) => {
                for v in map.values() {
                    walk(v, songs, seen);
                }
            }
            _ => {}
        }
    }

    walk(value, &mut songs, &mut seen);
    songs
}

/// Try to build one `SongDescription` from a single apollo station track object.
/// Returns `None` when the object isn't a track (no `spotify:track:` uri) or
/// lacks a title — signalling the caller to keep walking / fall back.
fn song_from_apollo_track(v: &serde_json::Value) -> Option<SongDescription> {
    let obj = v.as_object()?;
    let uri = obj.get("uri")?.as_str()?;
    let id = uri.strip_prefix("spotify:track:")?;
    let id = id.split(':').next().unwrap_or(id);
    if id.is_empty() {
        return None;
    }

    let meta = obj.get("metadata").and_then(|m| m.as_object())?;
    let title = meta.get("title").and_then(|t| t.as_str())?;
    if title.is_empty() {
        return None;
    }

    let artist_name = meta
        .get("artist_name")
        .and_then(|a| a.as_str())
        .unwrap_or_default();
    let artist_id = meta
        .get("artist_uri")
        .and_then(|a| a.as_str())
        .and_then(|u| u.strip_prefix("spotify:artist:"))
        .map(|s| s.split(':').next().unwrap_or(s).to_string())
        .unwrap_or_default();
    let album_title = meta
        .get("album_title")
        .and_then(|a| a.as_str())
        .unwrap_or_default();

    // `spotify:image:{hex}` -> `https://i.scdn.co/image/{hex}`.
    let art = meta
        .get("image_url")
        .and_then(|i| i.as_str())
        .and_then(|u| u.strip_prefix("spotify:image:"))
        .filter(|hex| !hex.is_empty())
        .and_then(|hex| {
            ImageSet::from_images(std::iter::once((
                None,
                format!("https://i.scdn.co/image/{hex}"),
            )))
        });

    Some(SongDescription {
        id: id.to_string(),
        track_number: None,
        uri: uri.to_string(),
        title: title.to_string(),
        artists: vec![ArtistRef {
            id: artist_id,
            name: artist_name.to_string(),
        }],
        album: AlbumRef {
            id: String::new(),
            name: album_title.to_string(),
        },
        // Apollo metadata doesn't include duration; 0 renders as "0:00" and the
        // real duration is filled in by the player once the track loads.
        duration_ms: 0,
        art,
    })
}

/// Resolve a context uri (playlist/station/album/…) into base62 track ids using
/// librespot's internal context resolver (`/context-resolve/v1/{uri}`), which
/// returns a typed `Context` protobuf. This is the librespot analog of the
/// context_resolver in librespot-connect and works without the Web API.
async fn resolve_context_track_ids(session: &Session, context_uri: &str) -> Vec<String> {
    match session.spclient().get_context(context_uri).await {
        Ok(ctx) => {
            let mut ids: Vec<String> = Vec::new();
            let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
            for page in &ctx.pages {
                for track in &page.tracks {
                    // ContextTrack.uri is `Option<String>` (proto2 optional);
                    // when present it is a `spotify:track:{id}` uri. Match
                    // librespot's own field-access style (see connect's
                    // state/context.rs: `ctx_track.uri.as_ref()`).
                    let Some(uri) = track.uri.as_deref() else {
                        continue;
                    };
                    if let Some(id) = uri.strip_prefix("spotify:track:") {
                        let id = id.split(':').next().unwrap_or(id);
                        if !id.is_empty() && seen.insert(id.to_string()) {
                            ids.push(id.to_string());
                        }
                    }
                }
            }
            eprintln!(
                "RIFF_RADIO: get_context({context_uri}) yielded {} track id(s)",
                ids.len()
            );
            ids
        }
        Err(e) => {
            eprintln!("RIFF_RADIO: get_context({context_uri}) failed: {e}");
            Vec::new()
        }
    }
}

/// Hydrate full metadata for a single base62 track id via librespot's internal
/// metadata API (`Track::get`). Used for the seed track (always fetched so the
/// station cover + header are correct) and by the fallback batch hydrator.
async fn hydrate_single_song(session: &Session, id: &str) -> Option<SongDescription> {
    let spotify_id = match SpotifyId::from_base62(id) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("RIFF_RADIO: bad track id {id}: {e}");
            return None;
        }
    };
    let uri = SpotifyUri::Track { id: spotify_id };
    match Track::get(session, &uri).await {
        Ok(track) => match song_from_track(&track) {
            Some(song) => Some(song),
            None => {
                eprintln!("RIFF_RADIO: could not build SongDescription for {id}");
                None
            }
        },
        Err(e) => {
            eprintln!("RIFF_RADIO: metadata fetch failed for {id}: {e}");
            None
        }
    }
}

/// Hydrate full metadata for a list of base62 track ids via librespot's internal
/// metadata API (`Track::get`), converting each into a riff `SongDescription`.
/// Order is preserved. Tracks that fail to parse/fetch are skipped (logged).
/// Only used on the fallback paths — the apollo fast path builds songs straight
/// from JSON without any per-track fetch.
async fn hydrate_radio_songs(session: &Session, ids: &[String]) -> Vec<SongDescription> {
    let mut songs = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(song) = hydrate_single_song(session, id).await {
            songs.push(song);
        }
    }
    songs
}

/// Build a riff `SongDescription` from a librespot metadata `Track`.
///
/// All ids (track/album/artist) come back from librespot as `SpotifyUri`; riff
/// stores base62 id strings, so we convert via `to_base62`. Cover art file ids
/// are mapped to the Spotify image CDN (`https://i.scdn.co/image/{hex}`), the
/// same URL shape the Web API returns and that riff's ImageLoader already
/// fetches.
fn song_from_track(track: &Track) -> Option<SongDescription> {
    let id = track.id.to_id().ok()?;
    let uri = track.id.to_uri().ok()?;

    let artists: Vec<ArtistRef> = track
        .artists
        .iter()
        .filter_map(|a| {
            Some(ArtistRef {
                id: a.id.to_id().ok()?,
                name: a.name.clone(),
            })
        })
        .collect();

    let album = AlbumRef {
        id: track.album.id.to_id().unwrap_or_default(),
        name: track.album.name.clone(),
    };

    // Album cover images: librespot gives file ids + width/height. Prefer the
    // `covers` group (falling back to `cover_group`), map each to a CDN url.
    let cover_images = if !track.album.covers.is_empty() {
        &track.album.covers
    } else {
        &track.album.cover_group
    };
    let art = ImageSet::from_images(cover_images.iter().filter_map(|img| {
        let hex = img.id.to_base16().ok()?;
        let width = if img.width > 0 {
            Some(img.width as u32)
        } else {
            None
        };
        Some((width, format!("https://i.scdn.co/image/{hex}")))
    }));

    Some(SongDescription {
        id,
        track_number: if track.number > 0 {
            Some(track.number as u32)
        } else {
            None
        },
        uri,
        title: track.name.clone(),
        artists,
        album,
        duration_ms: track.duration.max(0) as u32,
        art,
    })
}

/// Recursively walk a JSON value collecting base62 ids from any string of the
/// form `{prefix}{id}` (e.g. `spotify:track:{id}`), in document order, dedup'd.
fn collect_uri_ids(value: &serde_json::Value, prefix: &str) -> Vec<String> {
    fn walk(
        value: &serde_json::Value,
        prefix: &str,
        ids: &mut Vec<String>,
        seen: &mut std::collections::HashSet<String>,
    ) {
        match value {
            serde_json::Value::String(s) => {
                if let Some(rest) = s.strip_prefix(prefix) {
                    // Guard against `{prefix}{id}:...` variants: take the id segment.
                    let id = rest.split(':').next().unwrap_or(rest);
                    if !id.is_empty() && seen.insert(id.to_string()) {
                        ids.push(id.to_string());
                    }
                }
            }
            serde_json::Value::Array(arr) => {
                for v in arr {
                    walk(v, prefix, ids, seen);
                }
            }
            serde_json::Value::Object(map) => {
                for v in map.values() {
                    walk(v, prefix, ids, seen);
                }
            }
            _ => {}
        }
    }

    let mut ids = Vec::new();
    let mut seen = std::collections::HashSet::new();
    walk(value, prefix, &mut ids, &mut seen);
    ids
}

/// Find the first playlist / station / album context uri referenced anywhere in a
/// JSON value. Used when the apollo station response contains no track uris but a
/// context uri to resolve (mirrors the observed `get_radio_for_track` behavior of
/// returning only `spotify:playlist:37i9…`).
fn first_context_uri(value: &serde_json::Value) -> Option<String> {
    for prefix in ["spotify:playlist:", "spotify:station:", "spotify:album:"] {
        let ids = collect_uri_ids(value, prefix);
        if let Some(id) = ids.into_iter().next() {
            // Reassemble the full uri from the matched prefix + id. `station:`
            // uris nest an inner type (e.g. spotify:station:track:{id}); those are
            // not stripped by collect_uri_ids beyond the first segment, so prefer
            // playlist/album which are flat. For station, hand the full original
            // string instead.
            if prefix == "spotify:station:" {
                // Recover the full station uri (its id part may itself be
                // `track:{id}`), by searching for a string with this prefix.
                if let Some(full) = find_first_string_with_prefix(value, prefix) {
                    return Some(full);
                }
            }
            return Some(format!("{prefix}{id}"));
        }
    }
    None
}

/// Return the first string value anywhere in `value` that begins with `prefix`.
fn find_first_string_with_prefix(value: &serde_json::Value, prefix: &str) -> Option<String> {
    match value {
        serde_json::Value::String(s) if s.starts_with(prefix) => Some(s.clone()),
        serde_json::Value::Array(arr) => arr
            .iter()
            .find_map(|v| find_first_string_with_prefix(v, prefix)),
        serde_json::Value::Object(map) => map
            .values()
            .find_map(|v| find_first_string_with_prefix(v, prefix)),
        _ => None,
    }
}

/// Maps a 0.0–1.0 volume slider value to the mixer's u16 volume.
///
/// The VolumeCtrl curve (configured in create_player) determines the dB mapping.
/// The curve's db_range parameter (derived from volume_min_db/volume_max_db settings)
/// controls how many dB of dynamic range the slider spans.
fn mixer_set_volume(mixer: &dyn Mixer, volume: f64) {
    mixer.set_volume((VolumeCtrl::MAX_VOLUME as f64 * volume) as u16);
}
