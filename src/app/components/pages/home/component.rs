use std::rc::Rc;

use gettextrs::*;
use gtk::prelude::*;

use super::model::{HomeFeedModel, LIKED_SONGS_ID};
use crate::app::components::{
    display_add_css_provider, CardWidget, Component, EventListener, ImageShape,
};
use crate::app::dispatch::Worker;
use crate::app::models::{CardKind, CardLayout, CardModel, CardSize};
use crate::app::state::LoginEvent;
use crate::app::{AppEvent, BrowserEvent};

/// Card size for the horizontal shelf strips (square album/playlist art).
const SHELF_CARD_SIZE: CardSize = CardSize::Medium;

/// Card size for the round artist strip.
const ARTIST_CARD_SIZE: CardSize = CardSize::Medium;

/// Left/right margin for section content, matching the Library screen.
const CONTENT_MARGIN: i32 = 12;

/// Top margin for the big "Home" title so it clears the floating ⋯ menu button
/// (window.blp: margin-top 4 + ~40 px button height), same as LibraryScreen.
const TITLE_TOP_MARGIN: i32 = 48;

/// Which feed shelf a section renders, so a single event handler can rebuild the
/// one store that changed.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Shelf {
    JumpBackIn,
    RecentlyPlayed,
    TopArtists,
    TopTracks,
    MadeForYou,
    Library,
}

/// The Spotify-style home feed: a vertical scroll of horizontal card shelves
/// over the user's personal data. Every shelf self-hides while its store is
/// empty, so fresh accounts and failed/unavailable endpoints degrade silently.
pub struct HomeScreen {
    root: gtk::Box,
    model: Rc<HomeFeedModel>,
    worker: Worker,
    /// The shortcuts grid, rebuilt from recently-played + the pinned Liked tile.
    shortcuts: gtk::FlowBox,
    /// One container per shelf; each is emptied and refilled on its update event,
    /// and hidden while it has no cards.
    sections: Vec<(Shelf, gtk::Box)>,
}

impl HomeScreen {
    pub fn new(model: Rc<HomeFeedModel>, worker: Worker) -> Self {
        display_add_css_provider(resource!("/components/home.css"));

        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.set_vexpand(true);

        let title = gtk::Label::new(Some(&gettext("Home")));
        title.set_halign(gtk::Align::Start);
        title.set_margin_start(CONTENT_MARGIN);
        title.set_margin_end(CONTENT_MARGIN);
        title.set_margin_top(TITLE_TOP_MARGIN);
        title.add_css_class("home-title");
        root.append(&title);

        // Chip row (All · Music). Music has no effect yet — riff has no podcast
        // support — but it keeps the Spotify visual pattern.
        root.append(&Self::build_chip_row());

        // Shortcuts grid: fixed 2-column block of compact tiles.
        let shortcuts = gtk::FlowBox::new();
        shortcuts.set_selection_mode(gtk::SelectionMode::None);
        shortcuts.set_min_children_per_line(1);
        shortcuts.set_max_children_per_line(2);
        shortcuts.set_column_spacing(8);
        shortcuts.set_row_spacing(8);
        shortcuts.set_homogeneous(true);
        shortcuts.set_margin_start(CONTENT_MARGIN);
        shortcuts.set_margin_end(CONTENT_MARGIN);
        shortcuts.set_margin_bottom(6);

        // Scrolled column of shelves.
        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.append(&shortcuts);

        // Shelves, in display order.
        let shelf_specs = [
            (Shelf::JumpBackIn, gettext("Jump back in")),
            (Shelf::RecentlyPlayed, gettext("Recently played")),
            (Shelf::TopArtists, gettext("Your top artists")),
            (Shelf::TopTracks, gettext("Your top tracks")),
            (Shelf::MadeForYou, String::new()),
            (Shelf::Library, gettext("From your library")),
        ];
        let mut sections = Vec::with_capacity(shelf_specs.len());
        for (shelf, title) in shelf_specs {
            let section = Self::build_empty_shelf(&title);
            content.append(&section);
            sections.push((shelf, section));
        }

        let scrolled = gtk::ScrolledWindow::new();
        scrolled.set_vexpand(true);
        scrolled.set_hscrollbar_policy(gtk::PolicyType::Never);
        scrolled.set_child(Some(&content));
        root.append(&scrolled);

        let screen = Self {
            root,
            model,
            worker,
            shortcuts,
            sections,
        };
        screen.rebuild_all();
        screen
    }

    /// A pinned row of filter chips (All / Music), matching Spotify's top bar.
    fn build_chip_row() -> gtk::ScrolledWindow {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        row.set_margin_start(CONTENT_MARGIN);
        row.set_margin_end(CONTENT_MARGIN);
        row.set_margin_top(CONTENT_MARGIN);
        row.set_margin_bottom(6);

        let all = gtk::ToggleButton::with_label(&gettext("All"));
        all.add_css_class("pill");
        all.add_css_class("home-chip");
        all.set_active(true);
        let music = gtk::ToggleButton::with_label(&gettext("Music"));
        music.add_css_class("pill");
        music.add_css_class("home-chip");
        music.set_group(Some(&all));
        row.append(&all);
        row.append(&music);

        let scroller = gtk::ScrolledWindow::new();
        scroller.set_vscrollbar_policy(gtk::PolicyType::Never);
        scroller.set_propagate_natural_height(true);
        scroller.set_child(Some(&row));
        scroller
    }

    /// Build a shelf shell: a header label plus an (initially empty) horizontal
    /// strip inside a horizontally-scrolling window. Hidden until filled.
    fn build_empty_shelf(title: &str) -> gtk::Box {
        let section = gtk::Box::new(gtk::Orientation::Vertical, 0);
        section.set_visible(false);

        let header = gtk::Label::new(Some(title));
        header.set_halign(gtk::Align::Start);
        header.set_margin_start(CONTENT_MARGIN);
        header.set_margin_end(CONTENT_MARGIN);
        header.set_margin_top(10);
        header.set_margin_bottom(6);
        header.add_css_class("home-shelf-title");
        section.append(&header);

        let strip = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        strip.set_margin_start(CONTENT_MARGIN);
        strip.set_margin_end(CONTENT_MARGIN);
        strip.add_css_class("home-shelf-strip");

        let scroller = gtk::ScrolledWindow::new();
        scroller.set_vscrollbar_policy(gtk::PolicyType::Never);
        scroller.set_hscrollbar_policy(gtk::PolicyType::External);
        scroller.set_propagate_natural_height(true);
        scroller.set_child(Some(&strip));
        section.append(&scroller);

        section
    }

    /// Rebuild every shelf plus the shortcuts grid from the current state.
    fn rebuild_all(&self) {
        self.rebuild_shortcuts();
        for &(shelf, _) in &self.sections {
            self.rebuild_shelf(shelf);
        }
    }

    /// Fill the fixed shortcuts grid: a pinned "Liked Songs" tile plus the first
    /// few recently-played contexts as compact horizontal tiles.
    fn rebuild_shortcuts(&self) {
        while let Some(child) = self.shortcuts.first_child() {
            self.shortcuts.remove(&child);
        }

        let mut cards: Vec<CardModel> = vec![liked_songs_card()];
        if let Some(state) = self.model.state() {
            // Up to five recent contexts → six tiles including Liked Songs.
            cards.extend(state.jump_back_in.iter().take(5));
        }

        for card in &cards {
            let widget = self.build_card(card, CardLayout::Horizontal, CardSize::Small);
            self.shortcuts.append(&widget);
        }
        // The grid is always visible (Liked Songs is always present).
    }

    /// Empty and refill a single shelf from its store, hiding it when empty.
    fn rebuild_shelf(&self, shelf: Shelf) {
        let Some((_, section)) = self.sections.iter().find(|(s, _)| *s == shelf) else {
            return;
        };
        let Some(strip) = shelf_strip(section) else {
            return;
        };
        while let Some(child) = strip.first_child() {
            strip.remove(&child);
        }

        let Some(state) = self.model.state() else {
            section.set_visible(false);
            return;
        };

        let (cards, size): (Vec<CardModel>, CardSize) = match shelf {
            Shelf::JumpBackIn => (state.jump_back_in.iter().collect(), SHELF_CARD_SIZE),
            Shelf::RecentlyPlayed => (state.recently_played.iter().collect(), SHELF_CARD_SIZE),
            Shelf::TopArtists => (state.top_artists.iter().collect(), ARTIST_CARD_SIZE),
            Shelf::TopTracks => (state.top_tracks.iter().collect(), SHELF_CARD_SIZE),
            Shelf::MadeForYou => (state.made_for_you.iter().collect(), SHELF_CARD_SIZE),
            Shelf::Library => (state.albums.iter().collect(), SHELF_CARD_SIZE),
        };

        // The "Because you listen to <artist>" header follows the seed artist.
        if shelf == Shelf::MadeForYou {
            if let Some(header) = shelf_header(section) {
                let seed = state.made_for_you_seed.clone().unwrap_or_default();
                header.set_label(&made_for_you_title(&seed));
            }
        }

        if cards.is_empty() {
            section.set_visible(false);
            return;
        }
        for card in &cards {
            let widget = self.build_card(card, CardLayout::Vertical, size);
            strip.append(&widget);
        }
        section.set_visible(true);
    }

    /// Build one tappable card widget bound to `card`, dispatching the right
    /// navigation on activation. Round shape is used for artist cards.
    fn build_card(&self, card: &CardModel, layout: CardLayout, size: CardSize) -> gtk::Widget {
        let shape = if card.card_kind() == CardKind::Artist {
            ImageShape::Round
        } else {
            ImageShape::Square
        };
        let widget = CardWidget::for_model(card, self.worker.clone(), shape, layout, size);

        let button = gtk::Button::new();
        button.add_css_class("flat");
        button.add_css_class("home-card-button");
        button.set_child(Some(&widget));

        let model = Rc::downgrade(&self.model);
        let id = card.id();
        let kind = card.card_kind();
        button.connect_clicked(move |_| {
            if let Some(model) = model.upgrade() {
                model.open_item(id.clone(), kind);
            }
        });
        button.upcast()
    }
}

/// Build the pinned "Liked Songs" shortcut tile.
fn liked_songs_card() -> CardModel {
    CardModel::new(
        LIKED_SONGS_ID,
        None,
        &gettext("Liked Songs"),
        "",
        None,
        None,
        None,
    )
    .with_kind(CardKind::Playlist)
}

/// Section title for the pseudo-mix shelf: "Because you listen to <artist>"
/// (falls back to a generic title when there's no seed artist yet).
fn made_for_you_title(seed: &str) -> String {
    if seed.is_empty() {
        gettext("Made for you")
    } else {
        // Translators: home-feed shelf title; {} is an artist name.
        gettext!("Because you listen to {}", seed)
    }
}

/// The header label of a shelf section (its first child).
fn shelf_header(section: &gtk::Box) -> Option<gtk::Label> {
    section.first_child().and_downcast::<gtk::Label>()
}

/// The horizontal card strip inside a shelf section (child of its scroller).
///
/// GTK4's ScrolledWindow always wraps its set_child() target in a GtkViewport,
/// so `scroller.child()` returns a Viewport, not the Box we set.  We handle
/// both cases (Viewport-wrapped and bare Box) so this is robust against any
/// future GTK version that might change the wrapping behaviour.
fn shelf_strip(section: &gtk::Box) -> Option<gtk::Box> {
    let scroller = section.last_child().and_downcast::<gtk::ScrolledWindow>()?;
    let child = scroller.child()?;
    // GTK4 wraps the child in a GtkViewport; descend through it.
    if let Some(viewport) = child.downcast_ref::<gtk::Viewport>() {
        viewport.child().and_downcast::<gtk::Box>()
    } else {
        child.downcast::<gtk::Box>().ok()
    }
}

impl Component for HomeScreen {
    fn get_root_widget(&self) -> &gtk::Widget {
        self.root.upcast_ref()
    }
}

impl EventListener for HomeScreen {
    fn on_event(&mut self, event: &AppEvent) {
        match event {
            AppEvent::Started => {
                self.model.refresh_feed();
            }
            AppEvent::LoginEvent(LoginEvent::LoginCompleted) => {
                self.model.refresh_feed();
            }
            AppEvent::LoginEvent(LoginEvent::LogoutCompleted) => {
                self.rebuild_all();
            }
            AppEvent::BrowserEvent(BrowserEvent::RecentlyPlayedUpdated) => {
                self.rebuild_shortcuts();
                self.rebuild_shelf(Shelf::JumpBackIn);
                self.rebuild_shelf(Shelf::RecentlyPlayed);
            }
            AppEvent::BrowserEvent(BrowserEvent::TopArtistsUpdated) => {
                self.rebuild_shelf(Shelf::TopArtists);
            }
            AppEvent::BrowserEvent(BrowserEvent::TopTracksUpdated) => {
                self.rebuild_shelf(Shelf::TopTracks);
            }
            AppEvent::BrowserEvent(BrowserEvent::MadeForYouUpdated) => {
                self.rebuild_shelf(Shelf::MadeForYou);
            }
            AppEvent::BrowserEvent(BrowserEvent::LibraryUpdated) => {
                self.rebuild_shelf(Shelf::Library);
            }
            _ => {}
        }
    }
}
