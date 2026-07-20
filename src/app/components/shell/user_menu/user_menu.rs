use gettextrs::*;
use gio::{prelude::ActionMapExt, SimpleAction, SimpleActionGroup};
use gtk::prelude::*;
use libadwaita::prelude::*;
use std::rc::Rc;

use super::UserMenuModel;
use crate::app::components::EventListener;
use crate::app::loader::ImageLoader;
use crate::app::state::{BrowserAction, BrowserEvent, LoginEvent, ScreenName};
use crate::app::{ActionDispatcher, AppEvent, Worker};

/// Diameter (px) of the round profile avatar shown in place of the menu icon.
const AVATAR_SIZE: i32 = 28;

pub struct UserMenu {
    user_button: gtk::MenuButton,
    /// The round profile avatar shown as the button's face (Spotify-style). Falls
    /// back to the user's initials, then a generic icon, until the image loads.
    avatar: libadwaita::Avatar,
    model: Rc<UserMenuModel>,
    worker: Worker,
    /// Number of detail pages currently pushed on top of the tab shell.
    /// The avatar button is hidden while this is > 0.
    nav_depth: usize,
}

impl UserMenu {
    pub fn new(
        user_button: gtk::MenuButton,
        about: libadwaita::AboutDialog,
        parent: gtk::Window,
        model: UserMenuModel,
        dispatcher: Box<dyn ActionDispatcher>,
        worker: Worker,
    ) -> Self {
        let model = Rc::new(model);

        // Replace the hamburger icon with a round avatar. The MenuButton still opens
        // the same popover menu on tap; only its visible face changes. Setting a
        // child suppresses the default icon; drop the dropdown arrow so it reads as
        // a plain circular avatar.
        let avatar = libadwaita::Avatar::new(AVATAR_SIZE, None, true);
        avatar.set_icon_name(Some("open-menu-symbolic"));
        user_button.set_child(Some(&avatar));
        user_button.set_always_show_arrow(false);
        user_button.add_css_class("flat");
        user_button.add_css_class("circular");

        let action_group = SimpleActionGroup::new();

        action_group.add_action(&{
            let logout = SimpleAction::new("logout", None);
            logout.connect_activate(clone!(
                #[weak]
                model,
                move |_, _| {
                    model.logout();
                }
            ));
            logout
        });

        action_group.add_action(&{
            let settings_action = SimpleAction::new("settings", None);
            settings_action.connect_activate(move |_, _| {
                dispatcher.dispatch(BrowserAction::NavigationPush(ScreenName::Settings).into());
            });
            settings_action
        });

        action_group.add_action(&{
            let about_action = SimpleAction::new("about", None);
            about_action.connect_activate(clone!(
                #[weak]
                about,
                #[weak]
                parent,
                move |_, _| {
                    about.present(Some(&parent));
                }
            ));
            about_action
        });

        user_button.insert_action_group("menu", Some(&action_group));

        Self {
            user_button,
            avatar,
            model,
            worker,
            nav_depth: 0,
        }
    }

    /// Refresh the avatar's face from the current login state: set the initials
    /// text (fallback) and, if an image URL is present, load it into the avatar as
    /// a round custom image. Loading is late-arriving and guarded — a failed or
    /// empty load simply leaves the initials/icon fallback in place.
    fn update_avatar(&self) {
        let name = self.model.display_name().unwrap_or_default();
        // Non-empty text makes Adw.Avatar show initials instead of the icon.
        self.avatar.set_text(Some(&name));
        self.avatar.set_show_initials(!name.is_empty());

        match self.model.user_image_url() {
            Some(url) if !url.is_empty() => {
                let avatar = self.avatar.clone();
                self.worker.send_local_task(async move {
                    let loader = ImageLoader::new();
                    if let Some(pixbuf) = loader
                        .load_remote(&url, "jpg", AVATAR_SIZE, AVATAR_SIZE)
                        .await
                    {
                        let texture = gdk::Texture::for_pixbuf(&pixbuf);
                        avatar.set_custom_image(Some(&texture));
                    }
                });
            }
            // No picture: clear any stale custom image so initials/icon show.
            _ => self
                .avatar
                .set_custom_image(None::<&gdk::Paintable>),
        }
    }

    fn update_menu(&self) {
        let menu = gio::Menu::new();
        // translators: This is a menu entry.
        menu.append(Some(&gettext("Preferences")), Some("menu.settings"));
        // translators: This is a menu entry.
        menu.append(Some(&gettext("About")), Some("menu.about"));
        // translators: This is a menu entry.
        menu.append(Some(&gettext("Quit")), Some("app.quit"));

        if let Some(name) = self.model.display_name() {
            let user_menu = gio::Menu::new();
            // translators: This is a menu entry.
            user_menu.append(Some(&gettext("Log out")), Some("menu.logout"));
            menu.insert_section(0, Some(&name), &user_menu);
        }

        self.user_button.set_menu_model(Some(&menu));
    }
}

impl EventListener for UserMenu {
    fn on_event(&mut self, event: &AppEvent) {
        match event {
            AppEvent::LoginEvent(LoginEvent::LoginCompleted) | AppEvent::Started => {
                self.update_menu();
                self.update_avatar();
                self.model.fetch_user_playlists();
                self.model.fetch_user_details();
            }
            // /me landed: swap in the real display name + profile picture.
            AppEvent::LoginEvent(LoginEvent::UserDetailsLoaded) => {
                self.update_menu();
                self.update_avatar();
            }
            // Hide the ⋯ button while any detail page is on top of the tab shell
            // so it doesn't float over the detail page's own back button.
            AppEvent::BrowserEvent(BrowserEvent::NavigationPushed(_)) => {
                self.nav_depth += 1;
                self.user_button.set_visible(false);
            }
            AppEvent::BrowserEvent(BrowserEvent::NavigationPopped) => {
                self.nav_depth = self.nav_depth.saturating_sub(1);
                if self.nav_depth == 0 {
                    self.user_button.set_visible(true);
                }
            }
            AppEvent::BrowserEvent(BrowserEvent::NavigationPoppedTo(_)) => {
                self.nav_depth = 0;
                self.user_button.set_visible(true);
            }
            _ => {}
        }
    }
}
