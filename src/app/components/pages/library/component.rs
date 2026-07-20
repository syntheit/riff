use std::cell::Cell;
use std::rc::Rc;

use gettextrs::gettext;
use gtk::prelude::*;

use super::list::{LibraryList, LibraryListCallbacks};
use super::model::{LibraryFilter, LibraryModel};
use super::pinned_store::PinnedStore;
use crate::app::components::{CardLayout, CardListModel, Component, EventListener, SortOrder};
use crate::app::dispatch::Worker;
use crate::app::models::LibraryItem;
use crate::app::state::LoginEvent;
use crate::app::{ActionDispatcher, AppAction, AppEvent, BrowserAction, BrowserEvent};
use crate::settings::StateTracker;

/// Margin around the card list content.
const CONTENT_MARGIN: i32 = 12;

/// Top margin (px) added to the "Library" title so it clears the floating ⋯ menu
/// button that now lives at the top-RIGHT of the header (window.blp).
const TITLE_TOP_MARGIN: i32 = 48;

/// The library screen's own page id, used for sort persistence (`sort-library`).
const PAGE_ID: &str = "library";

/// The three sort orders offered on this screen.
const SORT_ORDERS: [SortOrder; 3] = [
    SortOrder::RecentlyAdded,
    SortOrder::Alphabetic,
    SortOrder::Size,
];

/// The unified "Library" screen: a large title, a filter-pill row, a toolbar
/// (toggleable sort on the left, grid/list toggle on the right) and a virtualized
/// list/grid over the user's saved content. Backed by `gtk::ListView`/`gtk::GridView`
/// so scrolling stays smooth with hundreds of items.
pub struct LibraryScreen {
    root: gtk::Box,
    model: Rc<LibraryModel>,
    list: Rc<LibraryList>,
    status_page: libadwaita::StatusPage,
    layout: Rc<Cell<CardLayout>>,
    current_sort: Rc<Cell<SortOrder>>,
    descending: Rc<Cell<bool>>,
}

impl LibraryScreen {
    pub fn new(
        model: Rc<LibraryModel>,
        worker: Worker,
        layout: Rc<Cell<CardLayout>>,
        _size: Rc<Cell<crate::app::components::CardSize>>,
        dispatcher: Rc<dyn ActionDispatcher>,
        pins: PinnedStore,
    ) -> Self {
        crate::app::components::display_add_css_provider(resource!("/components/library.css"));

        let tracker = StateTracker::new_from_gsettings();
        let current_sort = Rc::new(Cell::new(tracker.load_sort_order(PAGE_ID)));
        let descending = Rc::new(Cell::new(tracker.load_sort_descending(PAGE_ID)));

        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.set_vexpand(true);

        // (a) Big in-content title.
        let title = gtk::Label::new(Some(&gettext("Library")));
        title.set_halign(gtk::Align::Start);
        title.set_margin_start(CONTENT_MARGIN);
        title.set_margin_end(CONTENT_MARGIN);
        title.set_margin_top(TITLE_TOP_MARGIN);
        title.add_css_class("library-title");
        root.append(&title);

        // (b) Filter-pill row.
        let (pill_row, clear_button, pills) = Self::build_pill_row();
        root.append(&pill_row);

        // (c) The virtualized list/grid. Built before the toolbar so the toolbar
        // handlers can capture it.
        let callbacks = LibraryListCallbacks {
            on_activate: {
                let model = Rc::downgrade(&model);
                Box::new(move |id| {
                    if let Some(model) = model.upgrade() {
                        model.open_item(id);
                    }
                })
            },
            on_long_press: {
                let model = Rc::downgrade(&model);
                let pins = pins.clone();
                let dispatcher = Rc::clone(&dispatcher);
                Box::new(move |id| {
                    if let Some(item) = build_library_item(model.upgrade().as_deref(), &pins, &id) {
                        dispatcher.dispatch(AppAction::ShowLibraryItemMenu(item));
                    }
                })
            },
        };
        let list = LibraryList::new(
            worker.clone(),
            pins.clone(),
            current_sort.get(),
            descending.get(),
            callbacks,
        );

        let (sort_button, toggle_button) = Self::build_toolbar(
            &current_sort,
            &descending,
            &layout,
            Rc::clone(&list),
            Rc::clone(&dispatcher),
            &tracker,
        );

        // Toolbar box (sort left, grid/list toggle right).
        let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        toolbar.set_margin_start(CONTENT_MARGIN);
        toolbar.set_margin_end(CONTENT_MARGIN);
        toolbar.set_margin_top(2);
        toolbar.set_margin_bottom(2);
        toolbar.append(&sort_button);
        toolbar.append(&toggle_button);
        root.append(&toolbar);

        // (d) Content: the list/grid with an empty-state overlay.
        list.widget().set_margin_start(CONTENT_MARGIN);
        list.widget().set_margin_end(CONTENT_MARGIN);
        list.widget().set_margin_bottom(CONTENT_MARGIN);

        let status_page = libadwaita::StatusPage::new();
        status_page.set_icon_name(Some("library-music-symbolic"));
        status_page.set_visible(false);

        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(list.widget()));
        overlay.add_overlay(&status_page);
        overlay.set_vexpand(true);
        root.append(&overlay);

        list.set_layout(layout.get());

        let screen = Self {
            root,
            model,
            list,
            status_page,
            layout,
            current_sort,
            descending,
        };

        screen.rebind();
        screen.connect_infinite_scroll();
        screen.connect_pills(&pills, &clear_button);

        screen
    }

    /// Build the horizontally-scrollable pill row plus a leading "✕" clear pill.
    fn build_pill_row() -> (gtk::ScrolledWindow, gtk::Button, [gtk::ToggleButton; 3]) {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        row.set_margin_start(CONTENT_MARGIN);
        row.set_margin_end(CONTENT_MARGIN);
        row.set_margin_top(CONTENT_MARGIN);
        row.set_margin_bottom(6);

        let clear_button = gtk::Button::from_icon_name("window-close-symbolic");
        clear_button.add_css_class("circular");
        clear_button.add_css_class("library-pill-clear");
        clear_button.set_visible(false);
        clear_button.set_tooltip_text(Some(&gettext("Clear filter")));
        row.append(&clear_button);

        let playlists = Self::make_pill(&gettext("Playlists"));
        let albums = Self::make_pill(&gettext("Albums"));
        let artists = Self::make_pill(&gettext("Artists"));
        albums.set_group(Some(&playlists));
        artists.set_group(Some(&playlists));
        row.append(&playlists);
        row.append(&albums);
        row.append(&artists);

        let scroller = gtk::ScrolledWindow::new();
        scroller.set_vscrollbar_policy(gtk::PolicyType::Never);
        scroller.set_propagate_natural_height(true);
        scroller.set_child(Some(&row));

        (scroller, clear_button, [playlists, albums, artists])
    }

    fn make_pill(label: &str) -> gtk::ToggleButton {
        let btn = gtk::ToggleButton::with_label(label);
        btn.add_css_class("pill");
        btn.add_css_class("library-pill");
        btn
    }

    /// Build the toolbar's sort button (with popover) and grid/list toggle button.
    /// Returns `(sort_button, toggle_button)`.
    fn build_toolbar(
        current_sort: &Rc<Cell<SortOrder>>,
        descending: &Rc<Cell<bool>>,
        layout: &Rc<Cell<CardLayout>>,
        list: Rc<LibraryList>,
        dispatcher: Rc<dyn ActionDispatcher>,
        tracker: &StateTracker,
    ) -> (gtk::MenuButton, gtk::Button) {
        // LEFT: sort menu button showing the current sort + a direction arrow.
        let sort_button = gtk::MenuButton::new();
        sort_button.add_css_class("flat");
        sort_button.set_halign(gtk::Align::Start);
        sort_button.set_hexpand(true);

        let sort_content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let sort_icon = gtk::Image::from_icon_name(direction_icon(descending.get()));
        let sort_label = gtk::Label::new(Some(&current_sort.get().label()));
        sort_content.append(&sort_icon);
        sort_content.append(&sort_label);
        sort_button.set_child(Some(&sort_content));

        let popover = gtk::Popover::new();
        let sort_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
        sort_box.set_margin_top(6);
        sort_box.set_margin_bottom(6);
        sort_box.set_margin_start(6);
        sort_box.set_margin_end(6);
        Self::populate_sort_options(
            &sort_box,
            current_sort,
            descending,
            &list,
            &dispatcher,
            &sort_label,
            &sort_icon,
            &popover,
            tracker,
        );
        popover.set_child(Some(&sort_box));
        sort_button.set_popover(Some(&popover));

        // RIGHT: grid/list toggle.
        let toggle_button = gtk::Button::new();
        toggle_button.add_css_class("flat");
        toggle_button.set_icon_name(toggle_icon(layout.get()));
        toggle_button.set_tooltip_text(Some(&gettext("Toggle grid or list view")));

        let layout_ref = Rc::clone(layout);
        let list_ref = Rc::clone(&list);
        toggle_button.connect_clicked(move |btn| {
            let next = match layout_ref.get() {
                CardLayout::Horizontal => CardLayout::Vertical,
                _ => CardLayout::Horizontal,
            };
            layout_ref.set(next);
            btn.set_icon_name(toggle_icon(next));
            list_ref.set_layout(next);
            dispatcher.dispatch(BrowserAction::ChangeCardLayout(next).into());
        });

        (sort_button, toggle_button)
    }

    /// Fill the sort popover with three radio options. Selecting the ALREADY-active
    /// option flips ascending/descending; selecting a different one switches to it
    /// (keeping its default direction). Both update the button label + arrow and
    /// re-sort the list instantly via the SortListModel.
    #[allow(clippy::too_many_arguments)]
    fn populate_sort_options(
        sort_box: &gtk::Box,
        current_sort: &Rc<Cell<SortOrder>>,
        descending: &Rc<Cell<bool>>,
        list: &Rc<LibraryList>,
        dispatcher: &Rc<dyn ActionDispatcher>,
        sort_label: &gtk::Label,
        sort_icon: &gtk::Image,
        popover: &gtk::Popover,
        tracker: &StateTracker,
    ) {
        for order in SORT_ORDERS {
            let btn = gtk::Button::new();
            btn.add_css_class("flat");
            let content = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            let check = gtk::Image::from_icon_name("object-select-symbolic");
            check.set_visible(order == current_sort.get());
            let label = gtk::Label::new(Some(&order.label()));
            label.set_hexpand(true);
            label.set_xalign(0.0);
            let arrow = gtk::Image::from_icon_name(direction_icon(descending.get()));
            arrow.set_visible(order == current_sort.get());
            content.append(&check);
            content.append(&label);
            content.append(&arrow);
            btn.set_child(Some(&content));

            let sort_ref = Rc::clone(current_sort);
            let desc_ref = Rc::clone(descending);
            let list_ref = Rc::clone(list);
            let dispatch = Rc::clone(dispatcher);
            let button_label = sort_label.clone();
            let button_icon = sort_icon.clone();
            let popover = popover.clone();
            let tracker = tracker.clone();
            btn.connect_clicked(move |_| {
                if order == sort_ref.get() {
                    // Tapping the active sort flips its direction.
                    desc_ref.set(!desc_ref.get());
                } else {
                    sort_ref.set(order);
                    // A fresh order starts in its natural direction (ascending flag
                    // = false → the sort's default: A→Z, largest-first, recent-first).
                    desc_ref.set(false);
                }
                list_ref.set_sort(sort_ref.get(), desc_ref.get());
                button_label.set_label(&sort_ref.get().label());
                button_icon.set_icon_name(Some(direction_icon(desc_ref.get())));
                dispatch.dispatch(
                    BrowserAction::ChangeSortOrder(PAGE_ID.to_string(), sort_ref.get()).into(),
                );
                tracker.save_sort_descending(PAGE_ID, desc_ref.get());
                popover.popdown();
            });
            sort_box.append(&btn);
        }
    }

    /// Point the list at the current filter's store and apply sort + empty state.
    fn rebind(&self) {
        if self.model.filter() == LibraryFilter::All {
            self.model.reconcile_combined();
        }
        if let Some(store) = self.model.current_source_store() {
            self.list.set_source(&store);
        }
        self.list.set_sort(self.current_sort.get(), self.descending.get());
        self.update_empty_state();
    }

    fn connect_infinite_scroll(&self) {
        let model_weak = Rc::downgrade(&self.model);
        self.list.connect_edge_reached(move || {
            if let Some(model) = model_weak.upgrade() {
                if CardListModel::has_more(&*model) {
                    CardListModel::load_more(&*model);
                }
            }
        });
    }

    fn connect_pills(&self, pills: &[gtk::ToggleButton; 3], clear_button: &gtk::Button) {
        let filters = [
            LibraryFilter::Playlists,
            LibraryFilter::Albums,
            LibraryFilter::Artists,
        ];
        for (pill, filter) in pills.iter().zip(filters) {
            let model = Rc::downgrade(&self.model);
            let list = Rc::downgrade(&self.list);
            let status_page = self.status_page.clone();
            let clear = clear_button.clone();
            pill.connect_toggled(move |btn| {
                if !btn.is_active() {
                    return;
                }
                let (Some(model), Some(list)) = (model.upgrade(), list.upgrade()) else {
                    return;
                };
                model.set_filter(filter);
                model.clear_combined();
                clear.set_visible(true);
                if let Some(store) = model.current_source_store() {
                    list.set_source(&store);
                }
                list.scroll_to_top();
                update_empty(&model, &status_page);
            });
        }

        let pills = pills.clone();
        let model = Rc::downgrade(&self.model);
        let list = Rc::downgrade(&self.list);
        let status_page = self.status_page.clone();
        clear_button.connect_clicked(move |btn| {
            for pill in pills.iter() {
                pill.set_active(false);
            }
            btn.set_visible(false);
            let (Some(model), Some(list)) = (model.upgrade(), list.upgrade()) else {
                return;
            };
            model.set_filter(LibraryFilter::All);
            model.reconcile_combined();
            if let Some(store) = model.current_source_store() {
                list.set_source(&store);
            }
            list.scroll_to_top();
            update_empty(&model, &status_page);
        });
    }

    fn update_empty_state(&self) {
        update_empty(&self.model, &self.status_page);
    }
}

/// Build the drawer payload for the item under a long-press. Resolves title/art/
/// kind from the model (or the synthetic Liked Songs row) plus its current pin
/// state. Returns None if the id can't be resolved.
fn build_library_item(
    model: Option<&LibraryModel>,
    pins: &PinnedStore,
    id: &str,
) -> Option<LibraryItem> {
    // The Liked Songs row is synthetic; no library actions apply, so skip it.
    if id.starts_with("__riff_liked") {
        return None;
    }
    let card = model?.card_for(id)?;
    Some(LibraryItem {
        id: id.to_string(),
        title: card.title(),
        subtitle: card.subtitle(),
        art: card.image(),
        kind: card.card_kind(),
        pinned: pins.is_pinned(id),
    })
}

fn toggle_icon(layout: CardLayout) -> &'static str {
    match layout {
        CardLayout::Horizontal => "view-grid-symbolic",
        _ => "view-list-symbolic",
    }
}

/// Arrow shown next to the sort label: down = descending, up = ascending.
fn direction_icon(descending: bool) -> &'static str {
    if descending {
        "view-sort-descending-symbolic"
    } else {
        "view-sort-ascending-symbolic"
    }
}

fn update_empty(model: &LibraryModel, status_page: &libadwaita::StatusPage) {
    let has_items = CardListModel::has_items(model);
    status_page.set_title(&model.empty_title());
    status_page.set_description(Some(&model.empty_description()));
    status_page.set_visible(!has_items);
}

impl Component for LibraryScreen {
    fn get_root_widget(&self) -> &gtk::Widget {
        self.root.upcast_ref()
    }
}

impl EventListener for LibraryScreen {
    fn on_event(&mut self, event: &AppEvent) {
        match event {
            AppEvent::Started => {
                self.model.refresh_all();
            }
            AppEvent::LoginEvent(LoginEvent::LoginCompleted) => {
                self.model.refresh_all();
            }
            AppEvent::LoginEvent(LoginEvent::LogoutCompleted) => {
                self.model.clear_combined();
                self.status_page.set_visible(false);
            }
            AppEvent::BrowserEvent(
                BrowserEvent::LibraryUpdated
                | BrowserEvent::SavedPlaylistsUpdated
                | BrowserEvent::SavedArtistsUpdated,
            ) => {
                if self.model.filter() == LibraryFilter::All {
                    // New items land in the sub-stores; fold them into the combined
                    // store. items-changed then flows through the model chain, so no
                    // view rebuild is needed.
                    self.model.reconcile_combined();
                }
                // The source store the view already points at is the live one, so we
                // only need to (re)apply pins/positions and empty state.
                self.list.sync_pins();
                self.update_empty_state();
            }
            AppEvent::BrowserEvent(BrowserEvent::CardLayoutChanged(_)) => {
                self.list.set_layout(self.layout.get());
            }
            // A pin toggle from the long-press drawer: re-float the list.
            AppEvent::LibraryPinsChanged => {
                self.list.sync_pins();
            }
            _ => {}
        }
    }
}
