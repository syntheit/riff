use gettextrs::*;
use std::borrow::Cow;
use std::collections::HashSet;
use url::Url;

use crate::app::models::{PlaylistDescription, PlaylistSummary, SongBatch, UserRef};
use crate::app::state::{AppAction, AppEvent, UpdatableState};

#[derive(Clone, Debug)]
pub enum TryLoginAction {
    Restore,
    InitLogin,
    CompleteLogin,
}

#[derive(Clone, Debug)]
pub enum LoginAction {
    ShowLogin,
    OpenLoginUrl(Url),
    TryLogin(TryLoginAction),
    SetLoginSuccess(String),
    /// The logged-in user's profile details from `/me`: display name and avatar
    /// image URL (None when the account has no picture). Drives the top-right
    /// profile button.
    SetUserDetails {
        display_name: String,
        image_url: Option<String>,
    },
    /// The user's own playlists as full descriptions (name + cover art + id),
    /// fetched on login. The reducer derives the lightweight `playlists`/
    /// `playlist_ids` indices from these and keeps the full list for the search
    /// screen's local owned-playlist matching (which needs covers to render).
    SetUserPlaylists(Vec<PlaylistDescription>),
    UpdateUserPlaylist(PlaylistSummary),
    PrependUserPlaylist(Vec<PlaylistSummary>),
    RemoveUserPlaylist(String),
    SetLoginFailure,
    RefreshToken,
    TokenRefreshed,
    Logout,
}

impl From<LoginAction> for AppAction {
    fn from(login_action: LoginAction) -> Self {
        Self::LoginAction(login_action)
    }
}

#[derive(Clone, Debug)]
pub enum LoginStartedEvent {
    Restore,
    InitLogin,
    CompleteLogin,
    OpenUrl(Url),
}

#[derive(Clone, Debug)]
pub enum LoginEvent {
    LoginShown,
    LoginStarted(LoginStartedEvent),
    LoginCompleted,
    UserDetailsLoaded,
    UserPlaylistsLoaded,
    LoginFailed,
    FreshTokenRequested,
    RefreshTokenCompleted,
    LogoutCompleted,
}

impl From<LoginEvent> for AppEvent {
    fn from(login_event: LoginEvent) -> Self {
        Self::LoginEvent(login_event)
    }
}

#[derive(Default)]
pub struct LoginState {
    // Username
    pub user: Option<String>,
    // Display name from /me (falls back to the username in the UI when None)
    pub user_display_name: Option<String>,
    // Avatar image URL from /me, for the top-right profile button (None = no picture)
    pub user_image_url: Option<String>,
    // Playlists owned by the logged in user
    pub playlists: Vec<PlaylistSummary>,
    // Playlist IDs for O(1) ownership checks
    pub playlist_ids: HashSet<String>,
    // Full descriptions of the user's own playlists (name + cover + id), kept so
    // the search screen can locally match & render owned playlists (with covers)
    // even for 1-2 char queries the Spotify API doesn't return them for.
    pub owned_playlists: Vec<PlaylistDescription>,
}

impl UpdatableState for LoginState {
    type Action = LoginAction;
    type Event = AppEvent;

    // The login state has a lot of actions that just translate to events
    fn update_with(&mut self, action: Cow<Self::Action>) -> Vec<Self::Event> {
        info!("update_with({:?})", action);
        match action.into_owned() {
            LoginAction::ShowLogin => vec![LoginEvent::LoginShown.into()],
            LoginAction::OpenLoginUrl(url) => {
                vec![LoginEvent::LoginStarted(LoginStartedEvent::OpenUrl(url)).into()]
            }
            LoginAction::TryLogin(TryLoginAction::Restore) => {
                vec![LoginEvent::LoginStarted(LoginStartedEvent::Restore).into()]
            }
            LoginAction::TryLogin(TryLoginAction::CompleteLogin) => {
                vec![LoginEvent::LoginStarted(LoginStartedEvent::CompleteLogin).into()]
            }
            LoginAction::SetLoginSuccess(username) => {
                self.user = Some(username);
                vec![LoginEvent::LoginCompleted.into()]
            }
            LoginAction::SetUserDetails {
                display_name,
                image_url,
            } => {
                self.user_display_name = Some(display_name);
                self.user_image_url = image_url;
                vec![LoginEvent::UserDetailsLoaded.into()]
            }
            LoginAction::SetLoginFailure => vec![LoginEvent::LoginFailed.into()],
            LoginAction::RefreshToken => vec![LoginEvent::FreshTokenRequested.into()],
            LoginAction::TokenRefreshed => {
                // translators: This notification is shown when, after some inactivity, the session is successfully restored. The user might have to repeat its last action.
                vec![
                    AppEvent::NotificationShown(gettext("Connection restored")),
                    LoginEvent::RefreshTokenCompleted.into(),
                ]
            }
            LoginAction::Logout => {
                self.user = None;
                self.user_display_name = None;
                self.user_image_url = None;
                self.playlists.clear();
                self.playlist_ids.clear();
                self.owned_playlists.clear();
                vec![LoginEvent::LogoutCompleted.into()]
            }
            LoginAction::SetUserPlaylists(playlists) => {
                self.playlist_ids = playlists.iter().map(|p| p.id.clone()).collect();
                self.playlists = playlists.iter().cloned().map(Into::into).collect();
                self.owned_playlists = playlists;
                vec![LoginEvent::UserPlaylistsLoaded.into()]
            }
            LoginAction::UpdateUserPlaylist(PlaylistSummary { id, title }) => {
                if let Some(p) = self.playlists.iter_mut().find(|p| p.id == id) {
                    p.title = title.clone();
                }
                if let Some(p) = self.owned_playlists.iter_mut().find(|p| p.id == id) {
                    p.title = title;
                }
                vec![LoginEvent::UserPlaylistsLoaded.into()]
            }
            LoginAction::PrependUserPlaylist(mut summaries) => {
                // A just-created playlist arrives as a summary (id + title, no cover).
                // Keep the owned-playlists list in sync — with a cover-less synthetic
                // description — so it's locally searchable immediately, before the next
                // login refresh replaces it with the full (art-bearing) description.
                for s in &summaries {
                    if !self.playlist_ids.contains(&s.id) {
                        self.owned_playlists.insert(
                            0,
                            PlaylistDescription {
                                id: s.id.clone(),
                                title: s.title.clone(),
                                art: None,
                                songs: SongBatch::empty(),
                                owner: UserRef {
                                    id: String::new(),
                                    display_name: String::new(),
                                },
                                snapshot_id: None,
                            },
                        );
                    }
                    self.playlist_ids.insert(s.id.clone());
                }
                summaries.append(&mut self.playlists);
                self.playlists = summaries;
                vec![LoginEvent::UserPlaylistsLoaded.into()]
            }
            LoginAction::RemoveUserPlaylist(id) => {
                self.playlists.retain(|p| p.id != id);
                self.owned_playlists.retain(|p| p.id != id);
                self.playlist_ids.remove(&id);
                vec![LoginEvent::UserPlaylistsLoaded.into()]
            }
            LoginAction::TryLogin(TryLoginAction::InitLogin) => {
                vec![LoginEvent::LoginStarted(LoginStartedEvent::InitLogin).into()]
            }
        }
    }
}
