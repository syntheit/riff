use gettextrs::gettext;
use gtk::prelude::*;

use crate::app::components::{Component, EventListener, ScreenFactory};
use crate::app::AppEvent;

// The Library tab: the user's saved content split into filter tabs
// (Playlists / Albums / Artists / Liked), switched by a header AdwViewSwitcher.
// Replaces the old sidebar-driven master/detail.
pub struct LibraryPane {
    container: gtk::Box,
    components: Vec<Box<dyn EventListener>>,
}

impl LibraryPane {
    pub fn new(screen_factory: &ScreenFactory) -> Self {
        let playlists = screen_factory.make_saved_playlists();
        let albums = screen_factory.make_library();
        let artists = screen_factory.make_saved_artists();
        let liked = screen_factory.make_saved_tracks();

        let stack = libadwaita::ViewStack::new();
        stack.set_vexpand(true);
        stack.add_titled_with_icon(
            playlists.get_root_widget(),
            Some("playlists"),
            &gettext("Playlists"),
            "view-app-grid-symbolic",
        );
        stack.add_titled_with_icon(
            albums.get_root_widget(),
            Some("albums"),
            &gettext("Albums"),
            "library-music-symbolic",
        );
        stack.add_titled_with_icon(
            artists.get_root_widget(),
            Some("artists"),
            &gettext("Artists"),
            "avatar-default-symbolic",
        );
        stack.add_titled_with_icon(
            liked.get_root_widget(),
            Some("liked"),
            &gettext("Liked Songs"),
            "starred-symbolic",
        );

        // A filter-chip row (not a full header) above the section, so the tab
        // doesn't stack multiple headerbars.
        let switcher = libadwaita::ViewSwitcher::builder()
            .stack(&stack)
            .policy(libadwaita::ViewSwitcherPolicy::Wide)
            .halign(gtk::Align::Center)
            .margin_top(6)
            .margin_bottom(6)
            .build();

        let container = gtk::Box::new(gtk::Orientation::Vertical, 0);
        container.append(&switcher);
        container.append(&stack);

        Self {
            container,
            components: vec![
                Box::new(playlists),
                Box::new(albums),
                Box::new(artists),
                Box::new(liked),
            ],
        }
    }
}

impl Component for LibraryPane {
    fn get_root_widget(&self) -> &gtk::Widget {
        self.container.upcast_ref()
    }

    fn get_children(&mut self) -> Option<&mut Vec<Box<dyn EventListener>>> {
        Some(&mut self.components)
    }
}

impl EventListener for LibraryPane {
    fn on_event(&mut self, event: &AppEvent) {
        self.broadcast_event(event);
    }
}
