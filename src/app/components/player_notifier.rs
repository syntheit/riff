use std::ops::Deref;
use std::rc::Rc;

use futures::channel::mpsc::UnboundedSender;
use gio::prelude::*;
use librespot::core::spotify_id::SpotifyId;
use librespot::core::SpotifyUri;

use crate::app::components::EventListener;
use crate::app::state::{
    Device, LoginAction, LoginEvent, LoginStartedEvent, PlaybackEvent, SettingsEvent,
};
use crate::app::{ActionDispatcher, AppAction, AppEvent, AppModel, SongsSource};
use crate::connect::ConnectCommand;
use crate::player::Command;

const SETTINGS: &str = "dev.diegovsky.Riff";

enum CurrentlyPlaying {
    WithSource {
        source: SongsSource,
        offset: usize,
        song: String,
    },
    Songs {
        songs: Vec<String>,
        offset: usize,
    },
}

impl CurrentlyPlaying {
    fn song_id(&self) -> &String {
        match self {
            Self::WithSource { song, .. } => song,
            Self::Songs { songs, offset } => &songs[*offset],
        }
    }
}

pub struct PlayerNotifier {
    app_model: Rc<AppModel>,
    dispatcher: Box<dyn ActionDispatcher>,
    command_sender: UnboundedSender<Command>,
    connect_command_sender: UnboundedSender<ConnectCommand>,
    // Kept alive so its `changed` signals keep firing for live DSP updates.
    _dsp_settings: gio::Settings,
}

impl PlayerNotifier {
    pub fn new(
        app_model: Rc<AppModel>,
        dispatcher: Box<dyn ActionDispatcher>,
        command_sender: UnboundedSender<Command>,
        connect_command_sender: UnboundedSender<ConnectCommand>,
    ) -> Self {
        let dsp_settings = Self::watch_dsp_settings(command_sender.clone());
        Self {
            app_model,
            dispatcher,
            command_sender,
            connect_command_sender,
            _dsp_settings: dsp_settings,
        }
    }

    /// Watch the equalizer, mono-audio, pan, and pitch GSettings keys and push
    /// changes to the local player immediately, so adjustments apply live with
    /// no player reload and no playback interruption.
    fn watch_dsp_settings(sender: UnboundedSender<Command>) -> gio::Settings {
        let settings = gio::Settings::new(SETTINGS);

        // Mono audio toggle.
        let s = sender.clone();
        settings.connect_changed(Some("mono-audio"), move |settings, _| {
            let enabled = settings.boolean("mono-audio");
            let _ = s.unbounded_send(Command::SetMono { enabled });
        });

        // Stereo pan / balance.
        let s = sender.clone();
        settings.connect_changed(Some("pan"), move |settings, _| {
            let pan = settings.double("pan");
            let _ = s.unbounded_send(Command::SetPan { pan });
        });

        // Pitch shift in cents.
        let s = sender.clone();
        settings.connect_changed(Some("pitch-cents"), move |settings, _| {
            let cents = settings.double("pitch-cents");
            let _ = s.unbounded_send(Command::SetPitch { cents });
        });

        // 10-band EQ: any band change sends the full band array.
        for band_key in &[
            "eq-band-0",
            "eq-band-1",
            "eq-band-2",
            "eq-band-3",
            "eq-band-4",
            "eq-band-5",
            "eq-band-6",
            "eq-band-7",
            "eq-band-8",
            "eq-band-9",
        ] {
            let s = sender.clone();
            settings.connect_changed(Some(band_key), move |settings, _| {
                let bands = [
                    settings.double("eq-band-0"),
                    settings.double("eq-band-1"),
                    settings.double("eq-band-2"),
                    settings.double("eq-band-3"),
                    settings.double("eq-band-4"),
                    settings.double("eq-band-5"),
                    settings.double("eq-band-6"),
                    settings.double("eq-band-7"),
                    settings.double("eq-band-8"),
                    settings.double("eq-band-9"),
                ];
                let _ = s.unbounded_send(Command::SetEqualizer { bands });
            });
        }

        settings
    }

    fn is_playing(&self) -> bool {
        self.app_model.get_state().playback.is_playing()
    }

    // Whether riff owns an active LOCAL session (played something locally, still
    // "sticky" across pause/resume). Gates the mirror instead of raw play-state.
    fn local_session_active(&self) -> bool {
        self.app_model
            .get_state()
            .playback
            .local_session_active()
    }

    fn currently_playing(&self) -> Option<CurrentlyPlaying> {
        let state = self.app_model.get_state();
        let song = state.playback.current_song_id()?;
        let offset = state.playback.current_song_index()?;
        let source = state.playback.current_source().cloned();
        let result = match source {
            Some(source) if source.has_spotify_uri() => CurrentlyPlaying::WithSource {
                source,
                offset,
                song,
            },
            _ => CurrentlyPlaying::Songs {
                songs: state.playback.songs().map_collect(|s| s.id),
                offset,
            },
        };
        Some(result)
    }

    fn device(&self) -> impl Deref<Target = Device> + '_ {
        self.app_model.map_state(|s| s.playback.current_device())
    }

    fn notify_login(&self, event: &LoginEvent) {
        info!("notify_login: {:?}", event);
        let command = match event {
            LoginEvent::LoginStarted(LoginStartedEvent::Restore) => Some(Command::Restore),
            LoginEvent::LoginStarted(LoginStartedEvent::InitLogin) => Some(Command::InitLogin),
            LoginEvent::LoginStarted(LoginStartedEvent::CompleteLogin) => {
                Some(Command::CompleteLogin)
            }
            LoginEvent::FreshTokenRequested => Some(Command::RefreshToken),
            LoginEvent::LogoutCompleted => Some(Command::Logout),
            _ => None,
        };

        if let Some(command) = command {
            self.send_command_to_local_player(command);
        }

        // Once logged in, start mirroring remote playback (if nothing is playing
        // locally) so "what's playing on your other devices" shows up right away.
        if matches!(event, LoginEvent::LoginCompleted) {
            self.refresh_remote_mirror();
        }
    }

    fn notify_connect_player(&self, event: &PlaybackEvent) {
        let event = event.clone();
        let currently_playing = self.currently_playing();
        let command = match event {
            PlaybackEvent::TrackChanged(_) | PlaybackEvent::SourceChanged => {
                match currently_playing {
                    Some(CurrentlyPlaying::WithSource {
                        source,
                        offset,
                        song,
                    }) => Some(ConnectCommand::PlayerLoadInContext {
                        source,
                        offset,
                        song,
                    }),
                    Some(CurrentlyPlaying::Songs { songs, offset }) => {
                        Some(ConnectCommand::PlayerLoad { songs, offset })
                    }
                    None => None,
                }
            }
            PlaybackEvent::TrackSeeked(position) => {
                Some(ConnectCommand::PlayerSeek(position as usize))
            }
            PlaybackEvent::PlaybackPaused => Some(ConnectCommand::PlayerPause),
            PlaybackEvent::PlaybackResumed => Some(ConnectCommand::PlayerResume),
            PlaybackEvent::VolumeSet(volume) => Some(ConnectCommand::PlayerSetVolume(
                (volume * 100f64).trunc() as u8,
            )),
            PlaybackEvent::RepeatModeChanged(mode) => Some(ConnectCommand::PlayerRepeat(mode)),
            PlaybackEvent::ShuffleChanged(shuffled) => {
                Some(ConnectCommand::PlayerShuffle(shuffled))
            }
            _ => None,
        };

        if let Some(command) = command {
            self.send_command_to_connect_player(command);
        }
    }

    fn notify_local_player(&self, event: &PlaybackEvent) {
        let command = match event {
            PlaybackEvent::PlaybackPaused => Some(Command::PlayerPause),
            PlaybackEvent::PlaybackResumed => Some(Command::PlayerResume),
            PlaybackEvent::PlaybackStopped => Some(Command::PlayerStop),
            PlaybackEvent::VolumeSet(volume) => Some(Command::PlayerSetVolume(*volume)),
            PlaybackEvent::TrackChanged(id) => {
                info!("track changed: {}", id);
                SpotifyId::from_base62(id)
                    .ok()
                    .map(|track| Command::PlayerLoad {
                        track: SpotifyUri::Track { id: track },
                        resume: true,
                    })
            }
            PlaybackEvent::SourceChanged => {
                let resume = self.is_playing();
                self.currently_playing()
                    .and_then(|c| SpotifyId::from_base62(c.song_id()).ok())
                    .map(|track| Command::PlayerLoad {
                        track: SpotifyUri::Track { id: track },
                        resume,
                    })
            }
            PlaybackEvent::TrackSeeked(position) => Some(Command::PlayerSeek(*position)),
            PlaybackEvent::Preload(id) => SpotifyId::from_base62(id)
                .ok()
                .map(|track| SpotifyUri::Track { id: track })
                .map(Command::PlayerPreload),
            _ => None,
        };

        if let Some(command) = command {
            self.send_command_to_local_player(command);
        }
    }

    fn send_command_to_connect_player(&self, command: ConnectCommand) {
        self.connect_command_sender.unbounded_send(command).unwrap();
    }

    // Turn the remote-playback MIRROR poll on/off (controller direction: show
    // what's playing on the user's OTHER devices). We mirror while riff is on its
    // local device but NOT itself playing — that's when surfacing remote playback
    // is useful. Once riff plays locally, or the user switches to a Connect device
    // we control directly, the mirror yields.
    fn set_remote_mirror(&self, active: bool) {
        eprintln!("RIFF_CONNECT: set_remote_mirror({active})");
        self.send_command_to_connect_player(ConnectCommand::SetRemoteMirrorActive(active));
    }

    // Decide + push the mirror state from current app state: active only when the
    // active device is Local and riff does NOT own a local session. Gating on the
    // sticky `local_session_active` (rather than raw play-state) is what stops a
    // local PAUSE from re-enabling the mirror and yanking the user back to the
    // desktop. When a local session is active the mirror poll idles (battery).
    fn refresh_remote_mirror(&self) {
        let is_local = matches!(&*self.device(), Device::Local);
        let active = is_local && !self.local_session_active();
        self.set_remote_mirror(active);
    }

    // Force an immediate re-poll of remote playback (without changing the mirror
    // decision), so a remote CONTROL press (play/pause/next/seek/volume issued at
    // a mirrored device) is reflected in the UI right away instead of waiting for
    // the next poll tick. Re-uses the `SetRemoteMirrorActive(true)` nudge, which
    // polls once immediately. Only meaningful while actually mirroring.
    fn repoll_remote_mirror(&self) {
        if self.app_model.get_state().playback.is_mirroring_remote() {
            self.set_remote_mirror(true);
        }
    }

    fn send_command_to_local_player(&self, command: Command) {
        let dispatcher = &self.dispatcher;
        self.command_sender
            .unbounded_send(command)
            .unwrap_or_else(|_| {
                dispatcher.dispatch(AppAction::LoginAction(LoginAction::SetLoginFailure));
            });
    }

    fn switch_device(&mut self, device: &Device) {
        match device {
            Device::Connect(device) => {
                self.send_command_to_local_player(Command::PlayerStop);
                self.send_command_to_connect_player(ConnectCommand::SetDevice(device.id.clone()));
                // Actually MOVE the current session to the chosen device (rather
                // than only routing future commands at it). PUT /me/player.
                // When taking over the device we were already MIRRORING, preserve
                // its current play-state so a paused remote isn't force-resumed
                // (avoids a resume-then-pause flicker when the user taps pause).
                let play = self.remote_play_state_for(&device.id).unwrap_or(true);
                self.transfer_playback_to(device.id.clone(), play);
                self.notify_connect_player(&PlaybackEvent::SourceChanged);
            }
            Device::Local => {
                self.send_command_to_connect_player(ConnectCommand::PlayerStop);
                self.notify_local_player(&PlaybackEvent::SourceChanged);
                // Back on the local device: resume mirroring remote playback if
                // riff isn't itself playing.
                self.refresh_remote_mirror();
            }
        }
    }

    // Transfer the active Spotify session to `device_id`. `play` controls whether
    // playback resumes on the target or is transferred paused.
    // Fire-and-forget: on error the connect poller reconciles / drops the device.
    fn transfer_playback_to(&self, device_id: String, play: bool) {
        let api = self.app_model.get_spotify();
        self.dispatcher.dispatch_async(Box::pin(async move {
            if let Err(err) = api.player_transfer(device_id, play).await {
                error!("failed to transfer playback: {}", err);
            }
            None
        }));
    }

    // If a remote-playback snapshot for `device_id` is currently being mirrored,
    // return its play-state (so a takeover preserves it); otherwise `None`.
    fn remote_play_state_for(&self, device_id: &str) -> Option<bool> {
        let state = self.app_model.get_state();
        let remote = state.playback.remote_playback()?;
        (remote.device.id == device_id).then_some(remote.is_playing)
    }
}

impl EventListener for PlayerNotifier {
    fn on_event(&mut self, event: &AppEvent) {
        let device = self.device().clone();
        match (device, event) {
            (_, AppEvent::LoginEvent(event)) => self.notify_login(event),
            // Song radio always resolves through the local librespot session (it
            // owns the internal radio endpoint), regardless of the active device.
            (_, AppEvent::RadioRequested(seed_id)) => {
                self.send_command_to_local_player(Command::StartRadio {
                    seed_id: seed_id.clone(),
                });
            }
            (_, AppEvent::PlaybackEvent(PlaybackEvent::SwitchedDevice(d))) => self.switch_device(d),
            // A transport control was issued to a mirrored remote device: re-poll
            // now so the mirrored snapshot catches up immediately.
            (_, AppEvent::RemoteMirrorRepollRequested) => self.repoll_remote_mirror(),
            // Startup + whenever the now-playing sheet opens: (re)evaluate whether
            // to mirror remote playback, and poll it immediately.
            (_, AppEvent::Started) | (_, AppEvent::NowPlayingSheetShown) => {
                self.refresh_remote_mirror();
            }
            (Device::Local, AppEvent::PlaybackEvent(event)) => {
                self.notify_local_player(event);
                // Local play-state changes flip whether mirroring remote playback
                // is useful (mirror while idle, yield once riff plays locally).
                if matches!(
                    event,
                    PlaybackEvent::PlaybackResumed
                        | PlaybackEvent::PlaybackPaused
                        | PlaybackEvent::PlaybackStopped
                        | PlaybackEvent::TrackChanged(_)
                ) {
                    self.refresh_remote_mirror();
                }
            }
            (Device::Local, AppEvent::SettingsEvent(SettingsEvent::PlayerSettingsChanged)) => {
                self.send_command_to_local_player(Command::ReloadSettings)
            }
            (Device::Connect(_), AppEvent::PlaybackEvent(event)) => {
                self.notify_connect_player(event)
            }
            _ => {}
        }
    }
}
