use crate::api::clear_user_cache;
use crate::app::state::{LoginAction, PlaybackAction};
use crate::app::{ActionDispatcher, AppModel};
use std::ops::Deref;
use std::rc::Rc;

pub struct UserMenuModel {
    app_model: Rc<AppModel>,
    dispatcher: Box<dyn ActionDispatcher>,
}

impl UserMenuModel {
    pub fn new(app_model: Rc<AppModel>, dispatcher: Box<dyn ActionDispatcher>) -> Self {
        Self {
            app_model,
            dispatcher,
        }
    }

    pub fn username(&self) -> Option<impl Deref<Target = String> + '_> {
        self.app_model
            .map_state_opt(|s| s.logged_user.user.as_ref())
    }

    /// The user's display name from `/me` if loaded, else the librespot username.
    /// Used both for the menu header and the avatar's initials fallback.
    pub fn display_name(&self) -> Option<String> {
        let state = self.app_model.get_state();
        state
            .logged_user
            .user_display_name
            .clone()
            .or_else(|| state.logged_user.user.clone())
    }

    /// The logged-in user's avatar URL from `/me`, if loaded and non-empty.
    pub fn user_image_url(&self) -> Option<String> {
        self.app_model
            .get_state()
            .logged_user
            .user_image_url
            .clone()
    }

    /// Fetch the logged-in user's profile (`/me`) to populate the display name and
    /// avatar image for the top-right profile button. Cheap and cached.
    pub fn fetch_user_details(&self) {
        if self.username().is_none() {
            return;
        }
        let api = self.app_model.get_spotify();
        self.dispatcher
            .call_spotify_and_dispatch(move || async move {
                api.get_current_user().await.map(|user| {
                    LoginAction::SetUserDetails {
                        display_name: user.display_name,
                        image_url: user.image_url,
                    }
                    .into()
                })
            });
    }

    pub fn logout(&self) {
        self.dispatcher.dispatch(PlaybackAction::Stop.into());
        self.dispatcher.dispatch_async(Box::pin(async {
            // let _ = self.app_model.key.await;
            let _ = clear_user_cache().await;
            Some(LoginAction::Logout.into())
        }));
    }

    pub fn fetch_user_playlists(&self) {
        let api = self.app_model.get_spotify();
        if self.username().is_some() {
            self.dispatcher
                .call_spotify_and_dispatch(move || async move {
                    api.get_saved_playlists(0, 30).await.map(|playlists| {
                        let summaries = playlists.into_iter().map(|p| p.into()).collect();
                        LoginAction::SetUserPlaylists(summaries).into()
                    })
                });
        }
    }
}
