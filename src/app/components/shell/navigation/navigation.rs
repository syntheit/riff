use std::cell::Cell;
use std::rc::Rc;

use gettextrs::gettext;
use gtk::prelude::WidgetExt;

use crate::app::components::{Component, EventListener, ListenerComponent};
use crate::app::state::ScreenName;
use crate::app::{ActionDispatcher, AppEvent, BrowserAction, BrowserEvent};

use super::factory::ScreenFactory;

// The shell is a single AdwNavigationView (`root_nav`) whose root page is the tab
// shell: an AdwViewStack (`tab_stack`) with Home / Search / Library, switched by a
// bottom AdwViewSwitcherBar. Album/artist/playlist/user detail pages — and the
// queue — push on top of the whole tab shell. Pushed pages have `can-pop: true` so
// the native swipe-back gesture works; the `popped` signal keeps the state machine
// in sync. A `programmatic_pop` flag prevents double-dispatching when we pop
// programmatically in response to a `NavigationPopped` event.
pub struct Navigation {
    root_nav: libadwaita::NavigationView,
    tab_stack: libadwaita::ViewStack,
    screen_factory: ScreenFactory,
    dispatcher: Box<dyn ActionDispatcher>,
    tab_roots: Vec<Box<dyn ListenerComponent>>,
    children: Vec<Box<dyn ListenerComponent>>,
    /// Incremented before each programmatic pop and decremented inside
    /// `connect_popped` so the callback knows to skip re-dispatching `NavigationPop`.
    /// Using a count (not a bool) correctly handles `pop_to_root` which fires
    /// `popped` once per removed page.
    programmatic_pop_count: Rc<Cell<u32>>,
}

impl Navigation {
    pub fn new(
        root_nav: libadwaita::NavigationView,
        tab_stack: libadwaita::ViewStack,
        screen_factory: ScreenFactory,
        dispatcher: Box<dyn ActionDispatcher>,
    ) -> Self {
        let programmatic_pop_count = Rc::new(Cell::new(0u32));

        // Wire swipe-back: when the NavigationView pops a page by gesture (or its
        // own back button), dispatch NavigationPop to keep the state machine in sync.
        // Guard with `programmatic_pop_count` so we don't dispatch when we initiated
        // the pop ourselves (which would cause a double-pop). A count (not a bool)
        // handles `pop_to_root` which fires `popped` once per removed page.
        {
            let count = Rc::clone(&programmatic_pop_count);
            let dispatcher = dispatcher.box_clone();
            root_nav.connect_popped(move |_nav, _page| {
                let c = count.get();
                if c > 0 {
                    // We triggered this pop ourselves — state machine already knows.
                    count.set(c - 1);
                    return;
                }
                // User gesture / native back button — tell the state machine.
                dispatcher.dispatch(BrowserAction::NavigationPop.into());
            });
        }

        Self {
            root_nav,
            tab_stack,
            screen_factory,
            dispatcher,
            tab_roots: vec![],
            children: vec![],
            programmatic_pop_count,
        }
    }

    fn setup_tabs(&mut self) {
        if !self.tab_roots.is_empty() {
            return;
        }
        let home = Box::new(self.screen_factory.make_library());
        let search = Box::new(self.screen_factory.make_search_results());
        let library = Box::new(self.screen_factory.make_library_screen());

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

        // Belt-and-suspenders: force the title + icon on each AdwViewStackPage
        // directly, bypassing the gettext catalog entirely. The bottom
        // AdwViewSwitcherBar renders the *page* title, and a bad `en` translation
        // (`msgid "Library"` → `msgstr "Albums"`) made the Library tab read
        // "Albums" no matter how many rebuilds we did. Using literal titles here
        // means the switcher label can never be poisoned by the .po again.
        // "library-music-symbolic" ships in riff's own gresource (see
        // riff.gresource.xml), so it always resolves regardless of the device's
        // system icon theme; "go-home-symbolic"/"system-search-symbolic" are stock
        // GTK/Adwaita symbolics.
        let home_page = self.tab_stack.page(home.get_root_widget());
        home_page.set_title(Some(&gettext("Home")));
        home_page.set_icon_name(Some("go-home-symbolic"));

        let search_page = self.tab_stack.page(search.get_root_widget());
        search_page.set_title(Some(&gettext("Search")));
        search_page.set_icon_name(Some("system-search-symbolic"));

        let library_page = self.tab_stack.page(library.get_root_widget());
        library_page.set_title(Some("Library"));
        library_page.set_icon_name(Some("library-music-symbolic"));

        // Diagnostic: print the ACTUAL titles/icons the switcher will render for
        // each tab, so a device run confirms the Library tab reads "Library".
        // (Strip once verified on-device.)
        for page in [&home_page, &search_page, &library_page] {
            error!(
                "TABDBG name={:?} title={:?} icon={:?}",
                page.name(),
                page.title(),
                page.icon_name(),
            );
        }

        self.tab_stack.set_visible_child_name("home");

        // Tapping a bottom tab while a detail page is open returns to the tab shell.
        let dispatcher = self.dispatcher.box_clone();
        self.tab_stack.connect_visible_child_notify(move |_| {
            dispatcher.dispatch(BrowserAction::NavigationPopTo(ScreenName::Home).into());
        });

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
            ScreenName::SavedTracks => Box::new(self.screen_factory.make_saved_tracks()),
            ScreenName::Settings => Box::new(self.screen_factory.make_settings()),
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
            .can_pop(true)
            .build();
        self.root_nav.push(&page);
        self.children.push(component);
    }

    fn pop(&mut self) {
        // Increment the count so the `connect_popped` callback knows this pop
        // was programmatic and must not re-dispatch NavigationPop.
        self.programmatic_pop_count
            .set(self.programmatic_pop_count.get() + 1);
        self.root_nav.pop();
        self.children.pop();
    }

    fn pop_to_root(&mut self) {
        // pop_to_tag fires `popped` once per removed page; bump the count by
        // the number of pages we're removing so every callback is suppressed.
        let pages_to_remove = self.children.len() as u32;
        if pages_to_remove > 0 {
            self.programmatic_pop_count
                .set(self.programmatic_pop_count.get() + pages_to_remove);
        }
        self.root_nav.pop_to_tag("tabs");
        self.children.clear();
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
