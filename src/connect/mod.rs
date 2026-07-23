use std::sync::Arc;
use std::time::Duration;

use futures::channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};
use futures::StreamExt;
use tokio::{task, time};

use crate::api::SpotifyApiClient;
use crate::app::AppAction;

mod player;
pub use player::ConnectCommand;

#[tokio::main]
async fn connect_server(
    api: Arc<dyn SpotifyApiClient + Send + Sync>,
    action_sender: UnboundedSender<AppAction>,
    receiver: UnboundedReceiver<ConnectCommand>,
) {
    let player = Arc::new(player::ConnectPlayer::new(api, action_sender));

    // Poll a device we're actively CONTROLLING (user switched to it): drives the
    // main queue display. Runs every 5 s while a device is set.
    let player_clone = Arc::clone(&player);
    task::spawn(async move {
        let mut interval = time::interval(Duration::from_secs(5));
        loop {
            interval.tick().await;
            if player_clone.has_device() {
                player_clone.sync_state().await;
            }
        }
    });

    // MIRROR poll (controller direction): surface what's playing on the user's
    // OTHER devices. Only actually hits the API while the mirror is active (mini-
    // player / now-playing visible AND riff isn't itself the active local player).
    // ~4 s cadence for battery; when inactive the tick just re-checks the flag and
    // makes no request.
    let player_clone = Arc::clone(&player);
    task::spawn(async move {
        let mut interval = time::interval(Duration::from_secs(4));
        loop {
            interval.tick().await;
            if player_clone.mirror_active() && !player_clone.has_device() {
                player_clone.poll_remote_snapshot().await;
            }
        }
    });

    // TAKEOVER-watch poll (safety net for the Spirc push signal): while riff holds
    // a LOCAL session, poll `GET /me/player` at a LOW cadence (7 s — battery-
    // conscious) purely to notice when ANOTHER device becomes the active player,
    // so riff yields even if the immediate Spirc deactivation edge is missed
    // (e.g. the bare-Player fallback with no Spirc events). Only hits the API
    // while the watch is active AND riff isn't directly controlling a device;
    // otherwise the tick just re-checks the flag and makes no request.
    let player_clone = Arc::clone(&player);
    task::spawn(async move {
        let mut interval = time::interval(Duration::from_secs(7));
        loop {
            interval.tick().await;
            if player_clone.takeover_watch_active() && !player_clone.has_device() {
                player_clone.poll_takeover().await;
            }
        }
    });

    receiver
        .for_each(|command| async { player.handle_command(command).await.unwrap() })
        .await;
}

pub fn start_connect_server(
    api: Arc<dyn SpotifyApiClient + Send + Sync>,
    action_sender: UnboundedSender<AppAction>,
) -> UnboundedSender<ConnectCommand> {
    let (sender, receiver) = unbounded();

    std::thread::spawn(move || connect_server(api, action_sender, receiver));

    sender
}
