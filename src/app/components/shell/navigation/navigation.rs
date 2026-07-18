use gettextrs::gettext;
use gtk::prelude::WidgetExt;

use crate::app::components::{Component, EventListener, ListenerComponent};
use crate::app::state::ScreenName;
use crate::app::{AppEvent, BrowserEvent};

use super::factory::ScreenFactory;

// The shell is a single AdwNavigationView (`root_nav`) whose root page is the tab
// shell: an AdwViewStack (`tab_stack`) with Home / Search / Library, switched by a
// bottom AdwViewSwitcherBar. Album/artist/playlist/user detail pages — and the
// queue — push on top of the whole tab shell. Pushed pages are `can-pop: false`
// so navigation stays purely state-driven (back button / Alt+Left → NavigationPop),
// exactly like the previous gtk::Stack model.
pub struct Navigation {
    root_nav: libadwaita::NavigationView,
    tab_stack: libadwaita::ViewStack,
    screen_factory: ScreenFactory,
    tab_roots: Vec<Box<dyn ListenerComponent>>,
    children: Vec<Box<dyn ListenerComponent>>,
}

impl Navigation {
    pub fn new(
        root_nav: libadwaita::NavigationView,
        tab_stack: libadwaita::ViewStack,
        screen_factory: ScreenFactory,
    ) -> Self {
        Self {
            root_nav,
            tab_stack,
            screen_factory,
            tab_roots: vec![],
            children: vec![],
        }
    }

    fn setup_tabs(&mut self) {
        if !self.tab_roots.is_empty() {
            return;
        }
        let home = Box::new(self.screen_factory.make_library());
        let search = Box::new(self.screen_factory.make_search_results());
        let library = Box::new(super::home::LibraryPane::new(&self.screen_factory));

        self.tab_stack.add_titled_with_icon(
            home.get_root_widget(),
            Some("home"),
            &gettext("Home"),
            "go-home-symbolic",
        );
        self.tab_stack.add_titled_with_icon(
            search.get_root_widget(),
            Some("search"),
            &gettext("Search"),
            "system-search-symbolic",
        );
        self.tab_stack.add_titled_with_icon(
            library.get_root_widget(),
            Some("library"),
            &gettext("Library"),
            "library-music-symbolic",
        );
        self.tab_stack.set_visible_child_name("home");

        self.tab_roots = vec![home, search, library];
    }

    fn push_screen(&mut self, name: &ScreenName) {
        let component: Box<dyn ListenerComponent> = match name {
            ScreenName::AlbumDetails(id) => {
                Box::new(self.screen_factory.make_album_details(id.to_owned()))
            }
            ScreenName::Artist(id) => {
                Box::new(self.screen_factory.make_artist_details(id.to_owned()))
            }
            ScreenName::PlaylistDetails(id) => {
                Box::new(self.screen_factory.make_playlist_details(id.to_owned()))
            }
            ScreenName::User(id) => Box::new(self.screen_factory.make_user_details(id.to_owned())),
            // Home is the tab shell (never pushed); Search is a persistent tab.
            ScreenName::Home | ScreenName::Search => return,
        };
        self.push_component(component, name.identifier().as_ref());
    }

    fn push_component(&mut self, component: Box<dyn ListenerComponent>, tag: &str) {
        let widget = component.get_root_widget().clone();
        let page = libadwaita::NavigationPage::builder()
            .child(&widget)
            .title(tag)
            .tag(tag)
            .can_pop(false)
            .build();
        self.root_nav.push(&page);
        self.children.push(component);
    }

    fn pop(&mut self) {
        self.root_nav.pop();
        self.children.pop();
    }

    fn pop_to_root(&mut self) {
        self.root_nav.pop_to_tag("tabs");
        self.children.clear();
    }

    fn show_queue(&mut self) {
        // Don't stack duplicate queue pages if one is already open.
        if self.root_nav.find_page("now-playing").is_some() {
            return;
        }
        let component = Box::new(self.screen_factory.make_now_playing());
        self.push_component(component, "now-playing");
    }
}

impl EventListener for Navigation {
    fn on_event(&mut self, event: &AppEvent) {
        match event {
            AppEvent::Started => self.setup_tabs(),
            AppEvent::BrowserEvent(BrowserEvent::NavigationPushed(name)) => self.push_screen(name),
            AppEvent::BrowserEvent(BrowserEvent::NavigationPopped) => self.pop(),
            AppEvent::BrowserEvent(BrowserEvent::NavigationPoppedTo(_)) => self.pop_to_root(),
            AppEvent::SearchTabShown => self.tab_stack.set_visible_child_name("search"),
            AppEvent::NowPlayingShown => self.show_queue(),
            _ => {}
        };
        for child in self.tab_roots.iter_mut() {
            child.on_event(event);
        }
        for child in self.children.iter_mut() {
            child.on_event(event);
        }
    }
}
