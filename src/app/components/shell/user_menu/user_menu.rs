use gettextrs::*;
use gio::{prelude::ActionMapExt, SimpleAction, SimpleActionGroup};
use gtk::prelude::*;
use libadwaita::prelude::AdwDialogExt;
use std::rc::Rc;

use super::UserMenuModel;
use crate::app::components::EventListener;
use crate::app::state::{BrowserAction, BrowserEvent, LoginEvent, ScreenName};
use crate::app::{ActionDispatcher, AppEvent};

pub struct UserMenu {
    user_button: gtk::MenuButton,
    model: Rc<UserMenuModel>,
    /// Number of detail pages currently pushed on top of the tab shell.
    /// The ⋯ button is hidden while this is > 0.
    nav_depth: usize,
}

impl UserMenu {
    pub fn new(
        user_button: gtk::MenuButton,
        about: libadwaita::AboutDialog,
        parent: gtk::Window,
        model: UserMenuModel,
        dispatcher: Box<dyn ActionDispatcher>,
    ) -> Self {
        let model = Rc::new(model);

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
            model,
            nav_depth: 0,
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

        if let Some(username) = self.model.username() {
            let user_menu = gio::Menu::new();
            // translators: This is a menu entry.
            user_menu.append(Some(&gettext("Log out")), Some("menu.logout"));
            menu.insert_section(0, Some(&username), &user_menu);
        }

        self.user_button.set_menu_model(Some(&menu));
    }
}

impl EventListener for UserMenu {
    fn on_event(&mut self, event: &AppEvent) {
        match event {
            AppEvent::LoginEvent(LoginEvent::LoginCompleted) | AppEvent::Started => {
                self.update_menu();
                self.model.fetch_user_playlists();
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
