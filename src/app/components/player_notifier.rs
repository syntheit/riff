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

    // Whether riff is currently being driven as a Spotify Connect RECEIVER (a
    // remote app transferred playback here; Spirc owns the local Player). While
    // true, transport UI must route to the Spirc handle, NOT the bare local
    // player, so riff and Spirc don't double-drive the Player.
    fn is_remote_controlled(&self) -> bool {
        self.app_model.get_state().playback.is_remote_controlled()
    }

    // The remote device that is the active OUTPUT right now (a remote is playing
    // and riff isn't the chosen local output), so a play the user just triggered
    // should be ROUTED there rather than played locally. `None` -> play locally.
    fn active_remote_device(&self) -> Option<String> {
        self.app_model.get_state().playback.active_remote_device()
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
            // A (re)load initiated locally: route it THROUGH Spirc so riff is
            // announced as the active device, with a bare-Player fallback baked in
            // (the player thread uses that if Spirc is offline). `resume: true` —
            // a local TrackChanged is a user-initiated play.
            PlaybackEvent::TrackChanged(id) => {
                info!("track changed: {}", id);
                self.local_load_command(true)
                    // Extremely defensive: if we somehow can't build the announced
                    // command (no current song), fall back to the bare load so
                    // audio never silently drops.
                    .or_else(|| {
                        SpotifyId::from_base62(id).ok().map(|track| {
                            Command::PlayerLoad {
                                track: SpotifyUri::Track { id: track },
                                resume: true,
                            }
                        })
                    })
            }
            PlaybackEvent::SourceChanged => {
                let resume = self.is_playing();
                self.local_load_command(resume).or_else(|| {
                    self.currently_playing()
                        .and_then(|c| SpotifyId::from_base62(c.song_id()).ok())
                        .map(|track| Command::PlayerLoad {
                            track: SpotifyUri::Track { id: track },
                            resume,
                        })
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

    // Build the LOCAL-play command that announces riff as the active device via
    // Spirc, from what riff just loaded. Mirrors the source mapping of
    // `route_play_to_remote`, but targets riff's OWN Spirc handle (activate + load
    // into the shared Player) instead of the Web API:
    //   - a playlist/album source (has a Spotify context uri) -> `context_uri` +
    //     offset to the tapped track (`SpircLoadContext`);
    //   - anything else (radio / Liked Songs / search / ad-hoc queue) -> an
    //     explicit `uris` list + offset (`SpircLoadTracks`).
    // Each carries a bare-Player `fallback` (the tapped track) so the player thread
    // can preserve local audio when Spirc is offline. Returns `None` only when
    // there's nothing to play, or the tapped song id isn't a valid base62 track id
    // (so the fallback can't be built) — the caller then bare-loads.
    fn local_load_command(&self, start_playing: bool) -> Option<Command> {
        // Spotify caps the explicit `uris` list; window a large list around the
        // offset before capping so the tapped track stays in range. Same bound as
        // `route_play_to_remote`.
        const MAX_URIS: usize = 500;
        let playing = self.currently_playing()?;
        // The tapped track id -> bare-Player fallback uri. If it isn't a valid
        // track id we can't build a safe fallback, so bail to the caller's path.
        let fallback_id = SpotifyId::from_base62(playing.song_id()).ok()?;
        let fallback = SpotifyUri::Track { id: fallback_id };

        let command = match playing {
            CurrentlyPlaying::WithSource {
                source,
                offset,
                song,
            } => {
                // has_spotify_uri() guaranteed the uri exists for these sources.
                let context_uri = source.spotify_uri()?;
                Command::SpircLoadContext {
                    context_uri,
                    offset,
                    playing_track_uri: Some(format!("spotify:track:{song}")),
                    start_playing,
                    fallback,
                }
            }
            CurrentlyPlaying::Songs { songs, offset } => {
                let (window, offset) = if songs.len() > MAX_URIS {
                    let start = offset.min(songs.len().saturating_sub(MAX_URIS));
                    let end = (start + MAX_URIS).min(songs.len());
                    (songs[start..end].to_vec(), offset - start)
                } else {
                    (songs, offset)
                };
                let uris = window
                    .into_iter()
                    .map(|id| format!("spotify:track:{id}"))
                    .collect::<Vec<_>>();
                Command::SpircLoadTracks {
                    uris,
                    offset,
                    start_playing,
                    fallback,
                }
            }
        };
        Some(command)
    }

    // RECEIVER mode: riff is the active Connect device (a remote app transferred
    // playback here). Route riff's own transport actions to the Spirc handle
    // (via the local-player Command channel) instead of the bare Player, so Spirc
    // stays the single Player owner and keeps its connect-state coherent + reported
    // back to other devices. Track/source changes are NOT forwarded: Spirc owns the
    // queue while receiving, so riff must not issue loads (that would double-drive).
    fn notify_spirc_player(&self, event: &PlaybackEvent) {
        let command = match event {
            PlaybackEvent::PlaybackResumed => Some(Command::SpircPlay),
            PlaybackEvent::PlaybackPaused => Some(Command::SpircPause),
            PlaybackEvent::PlaybackStopped => Some(Command::SpircPause),
            PlaybackEvent::TrackSeeked(position) => Some(Command::SpircSeek(*position)),
            PlaybackEvent::VolumeSet(volume) => Some(Command::SpircSetVolume(*volume)),
            PlaybackEvent::RemoteNextRequested => Some(Command::SpircNext),
            PlaybackEvent::RemotePrevRequested => Some(Command::SpircPrev),
            _ => None,
        };
        if let Some(command) = command {
            self.send_command_to_local_player(command);
        }
    }

    // "Tap plays on the active device": a play was triggered in riff while a
    // REMOTE device is the active output. Instead of playing locally, START that
    // playback ON the remote device via the Web API (`PUT /me/player/play?
    // device_id=…`) and let the existing mirror surface it. Builds the request
    // from what riff just loaded:
    //   - a playlist/album source (has a Spotify context uri) -> `context_uri`
    //     + `offset` to the tapped track;
    //   - anything else (radio station, ad-hoc/liked/search track list) -> an
    //     explicit `uris` list (spotify:track:…) + `offset` to the tapped track.
    // We do NOT also start local playback, and (because the play was routed while
    // a remote was active) `local_session_active` was left false, so riff stays a
    // remote and keeps mirroring. Fire-and-forget; a failure is logged and the
    // mirror poll reconciles. Only fired on TrackChanged / SourceChanged (an
    // actual (re)load), not on transport-only events.
    fn route_play_to_remote(&self, device_id: String) {
        let Some(playing) = self.currently_playing() else {
            return;
        };
        // Spotify caps the explicit `uris` list; keep the tapped track in range by
        // windowing a large list around the offset before capping.
        const MAX_URIS: usize = 500;
        let (context_uri, uris, offset) = match playing {
            CurrentlyPlaying::WithSource { source, offset, .. } => {
                (source.spotify_uri(), None, offset)
            }
            CurrentlyPlaying::Songs { songs, offset } => {
                let (window, offset) = if songs.len() > MAX_URIS {
                    let start = offset.min(songs.len().saturating_sub(MAX_URIS));
                    let end = (start + MAX_URIS).min(songs.len());
                    (songs[start..end].to_vec(), offset - start)
                } else {
                    (songs, offset)
                };
                let uris = window
                    .into_iter()
                    .map(|id| format!("spotify:track:{id}"))
                    .collect::<Vec<_>>();
                (None, Some(uris), offset)
            }
        };

        let api = self.app_model.get_spotify();
        self.dispatcher.dispatch_async(Box::pin(async move {
            if let Err(err) = api
                .player_play_context(device_id, context_uri, uris, Some(offset), None)
                .await
            {
                error!("failed to route play to remote device: {err}");
            }
            // Re-poll the mirror so the UI reflects the remote starting right away
            // (whether the play succeeded or the device vanished and we should
            // clear). This runs regardless so a stale mirror doesn't linger.
            Some(AppAction::RepollRemoteMirror)
        }));
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
        self.send_command_to_connect_player(ConnectCommand::SetRemoteMirrorActive(active));
    }

    // Turn the low-rate TAKEOVER-watch poll on/off. It runs while riff is on its
    // local device AND holds a local session — exactly when the mirror is idle —
    // to notice another device taking over (safety net for the Spirc push edge).
    fn set_takeover_watch(&self, active: bool) {
        self.send_command_to_connect_player(ConnectCommand::SetTakeoverWatchActive(active));
    }

    // Decide + push the mirror state from current app state: active only when the
    // active device is Local and riff does NOT own a local session. Gating on the
    // sticky `local_session_active` (rather than raw play-state) is what stops a
    // local PAUSE from re-enabling the mirror and yanking the user back to the
    // desktop. When a local session is active the mirror poll idles (battery) and
    // the TAKEOVER-watch takes over instead — it's the exact inverse: it polls at
    // a low rate WHILE riff is the sticky local output, purely to catch another
    // device becoming active (so riff yields). Mirror and watch are never both on.
    fn refresh_remote_mirror(&self) {
        let is_local = matches!(&*self.device(), Device::Local);
        let owns_local = self.local_session_active();
        let mirror = is_local && !owns_local;
        self.set_remote_mirror(mirror);
        // Watch for a takeover only while we're the sticky local output on Local.
        self.set_takeover_watch(is_local && owns_local);
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

    // Push the app-side `local_session_active` down to the player thread as the
    // "riff owns the Player" flag, so its Player-event delegate distinguishes
    // riff's OWN local playback (advance riff's queue) from Spirc-driven receiver
    // playback (mirror only). Cheap (an atomic store); called on local playback
    // transitions.
    fn sync_local_ownership(&self) {
        let owns = self.local_session_active();
        self.send_command_to_local_player(Command::SetLocalOwnsPlayer(owns));
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
            // Entered / left Connect RECEIVER mode (a remote transferred playback
            // to riff via Spirc). Tell the player thread whether riff owns local
            // playback so its Player-event delegate knows to mirror (receiving) vs
            // advance riff's own queue (local). Also refresh the OTHER-device
            // mirror: it must stay off while receiving.
            (_, AppEvent::PlaybackEvent(PlaybackEvent::RemoteControlChanged(controlled))) => {
                // While receiving, riff (via Spirc) owns local playback, so
                // local_owns_player = false only when NOT receiving.
                self.send_command_to_local_player(Command::SetLocalOwnsPlayer(!controlled));
                self.refresh_remote_mirror();
            }
            // riff yielded active-device status because ANOTHER Connect device took
            // over. Both sticky flags were just cleared, so: tell the player thread
            // riff no longer owns the local Player (any further librespot events are
            // not riff's own queue), then re-enable the OTHER-device mirror. Because
            // `refresh_remote_mirror` pushes `SetRemoteMirrorActive(true)` — which
            // polls `/me/player` ONCE immediately in the connect handler — this is
            // the immediate re-poll that switches the UI to the device that took
            // over without waiting for the next ~4s tick.
            (_, AppEvent::PlaybackEvent(PlaybackEvent::YieldedActiveDevice)) => {
                self.send_command_to_local_player(Command::SetLocalOwnsPlayer(false));
                self.refresh_remote_mirror();
            }
            // While riff is a Connect RECEIVER, route ALL transport to the Spirc
            // handle (Spirc owns the Player). This branch must precede the
            // Local/Connect arms so we never double-drive the local Player.
            (_, AppEvent::PlaybackEvent(event)) if self.is_remote_controlled() => {
                self.notify_spirc_player(event);
            }
            // Startup + whenever the now-playing sheet opens: (re)evaluate whether
            // to mirror remote playback, and poll it immediately.
            (_, AppEvent::Started) | (_, AppEvent::NowPlayingSheetShown) => {
                self.refresh_remote_mirror();
            }
            // "Tap plays on the ACTIVE device": the active device is Local, but a
            // REMOTE device is the active OUTPUT (mirroring) and riff isn't the
            // chosen local output. A play the user just triggered (an actual
            // (re)load — TrackChanged / SourceChanged) must START on that remote
            // device, not locally. Transport-only events (pause/resume/seek/vol)
            // never reach here for a mirrored device: the mini-player drives those
            // straight at the remote. This precedes the local arm so we never also
            // start local playback. The reducer left `local_session_active` false
            // for this play, so riff stays a remote and keeps mirroring.
            (Device::Local, AppEvent::PlaybackEvent(event))
                if matches!(
                    event,
                    PlaybackEvent::TrackChanged(_) | PlaybackEvent::SourceChanged
                ) && self.active_remote_device().is_some() =>
            {
                // active_remote_device() was just checked as Some.
                let device_id = self.active_remote_device().unwrap();
                self.route_play_to_remote(device_id);
            }
            (Device::Local, AppEvent::PlaybackEvent(event)) => {
                // Reached only when riff itself is the active output (no active
                // remote device): play LOCALLY. This is the unchanged local path;
                // the reducer set `local_session_active` for this play, so riff
                // becomes/stays the active output and the mirror yields.
                // Keep the player thread's "riff owns the Player" flag in sync with
                // the app-side local session for TRANSPORT-only events (pause /
                // resume / stop). For a (re)LOAD (TrackChanged / SourceChanged) we
                // do NOT sync ownership here: those now route THROUGH Spirc (see
                // `local_load_command`), and the SpircLoad* command on the player
                // thread is the SOLE authority on `local_owns_player` — it clears it
                // when Spirc drives the shared Player (so the event delegate mirrors
                // it, no double Next) or sets it when it falls back to the bare
                // Player. Sending SetLocalOwnsPlayer(true) here would fight that.
                if matches!(
                    event,
                    PlaybackEvent::PlaybackResumed
                        | PlaybackEvent::PlaybackPaused
                        | PlaybackEvent::PlaybackStopped
                ) {
                    self.sync_local_ownership();
                }
                self.notify_local_player(event);
                // Local play-state changes flip whether mirroring remote playback
                // is useful (mirror while idle, yield once riff plays locally).
                if matches!(
                    event,
                    PlaybackEvent::PlaybackResumed
                        | PlaybackEvent::PlaybackPaused
                        | PlaybackEvent::PlaybackStopped
                        | PlaybackEvent::TrackChanged(_)
                        | PlaybackEvent::SourceChanged
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
