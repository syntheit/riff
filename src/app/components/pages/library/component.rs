use std::cell::Cell;
use std::rc::Rc;

use gettextrs::gettext;
use gtk::prelude::*;

use super::model::{LibraryFilter, LibraryModel};
use crate::app::components::{
    display_add_css_provider, CardLayout, CardList, CardListModel, CardSize, Component,
    EventListener, SortOrder,
};
use crate::app::dispatch::Worker;
use crate::app::state::LoginEvent;
use crate::app::{ActionDispatcher, AppEvent, BrowserAction, BrowserEvent};
use crate::settings::StateTracker;

/// Number of columns in grid mode on a phone-width screen.
const GRID_COLUMNS: u32 = 3;

/// Margin around the card list content.
const CONTENT_MARGIN: i32 = 12;

/// The library screen's own page id, used for sort persistence (`sort-library`).
const PAGE_ID: &str = "library";

/// The three sort orders offered on this screen.
const SORT_ORDERS: [SortOrder; 3] = [
    SortOrder::RecentlyAdded,
    SortOrder::Alphabetic,
    SortOrder::Size,
];

/// The unified "Library" screen: a large title, a filter-pill row, a toolbar
/// (sort on the left, grid/list toggle on the right) and a card list that shows
/// a compact list or a 3-column grid over the user's saved content.
pub struct LibraryScreen {
    root: gtk::Box,
    model: Rc<LibraryModel>,
    card_list: Rc<CardList>,
    worker: Worker,
    status_page: libadwaita::StatusPage,
    scrolled_window: gtk::ScrolledWindow,
    layout: Rc<Cell<CardLayout>>,
    size: Rc<Cell<CardSize>>,
    current_sort: Rc<Cell<SortOrder>>,
    toggle_button: gtk::Button,
}

impl LibraryScreen {
    pub fn new(
        model: Rc<LibraryModel>,
        worker: Worker,
        layout: Rc<Cell<CardLayout>>,
        size: Rc<Cell<CardSize>>,
        dispatcher: Rc<dyn ActionDispatcher>,
    ) -> Self {
        display_add_css_provider(resource!("/components/library.css"));

        let tracker = StateTracker::new_from_gsettings();
        let current_sort = Rc::new(Cell::new(tracker.load_sort_order("library")));

        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.set_vexpand(true);

        // (a) Big in-content title
        let title = gtk::Label::new(Some(&gettext("Library")));
        title.set_halign(gtk::Align::Start);
        title.set_margin_start(CONTENT_MARGIN);
        title.set_margin_end(CONTENT_MARGIN);
        title.set_margin_top(CONTENT_MARGIN);
        title.add_css_class("library-title");
        root.append(&title);

        // (b) Filter-pill row
        let (pill_row, clear_button, pills) = Self::build_pill_row();
        root.append(&pill_row);

        // (c) Toolbar: sort left, grid/list toggle right
        let card_list = Rc::new(CardList::new());
        let (toolbar, toggle_button) = Self::build_toolbar(
            &current_sort,
            &layout,
            Rc::clone(&card_list),
            Rc::clone(&dispatcher),
        );
        root.append(&toolbar);

        // (d) Content: scrolled card list with an empty-state overlay
        card_list.widget().set_margin_start(CONTENT_MARGIN);
        card_list.widget().set_margin_end(CONTENT_MARGIN);
        card_list.widget().set_margin_bottom(CONTENT_MARGIN);

        let status_page = libadwaita::StatusPage::new();
        status_page.set_icon_name(Some("library-music-symbolic"));
        status_page.set_visible(false);

        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(card_list.widget()));
        overlay.add_overlay(&status_page);

        let scrolled_window = gtk::ScrolledWindow::new();
        scrolled_window.set_vexpand(true);
        scrolled_window.set_hscrollbar_policy(gtk::PolicyType::Never);
        scrolled_window.set_child(Some(&overlay));
        root.append(&scrolled_window);

        let screen = Self {
            root,
            model,
            card_list,
            worker,
            status_page,
            scrolled_window,
            layout,
            size,
            current_sort,
            toggle_button,
        };

        screen.apply_grid_columns();
        screen.rebind();
        screen.card_list.show_placeholders();
        screen.connect_infinite_scroll();
        screen.connect_pills(&pills, &clear_button);

        screen
    }

    /// Build the horizontally-scrollable pill row plus a leading "✕" clear pill.
    /// Returns the row, the clear button, and the three filter toggles.
    fn build_pill_row() -> (gtk::ScrolledWindow, gtk::Button, [gtk::ToggleButton; 3]) {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        row.set_margin_start(CONTENT_MARGIN);
        row.set_margin_end(CONTENT_MARGIN);
        row.set_margin_top(CONTENT_MARGIN);
        row.set_margin_bottom(6);

        // Leading clear pill, shown only when a filter is active.
        let clear_button = gtk::Button::from_icon_name("window-close-symbolic");
        clear_button.add_css_class("circular");
        clear_button.add_css_class("library-pill-clear");
        clear_button.set_visible(false);
        clear_button.set_tooltip_text(Some(&gettext("Clear filter")));
        row.append(&clear_button);

        let playlists = Self::make_pill(&gettext("Playlists"));
        let albums = Self::make_pill(&gettext("Albums"));
        let artists = Self::make_pill(&gettext("Artists"));
        // Group for mutual exclusion (only one active at a time).
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

    fn build_toolbar(
        current_sort: &Rc<Cell<SortOrder>>,
        layout: &Rc<Cell<CardLayout>>,
        card_list: Rc<CardList>,
        dispatcher: Rc<dyn ActionDispatcher>,
    ) -> (gtk::Box, gtk::Button) {
        let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        toolbar.set_margin_start(CONTENT_MARGIN);
        toolbar.set_margin_end(CONTENT_MARGIN);
        toolbar.set_margin_top(2);
        toolbar.set_margin_bottom(2);

        // LEFT: sort menu button labelled with the current sort.
        let sort_button = gtk::MenuButton::new();
        sort_button.add_css_class("flat");
        sort_button.set_halign(gtk::Align::Start);
        sort_button.set_hexpand(true);

        let sort_content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let sort_icon = gtk::Image::from_icon_name("view-sort-descending-symbolic");
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
            &card_list,
            &dispatcher,
            &sort_label,
        );
        popover.set_child(Some(&sort_box));
        sort_button.set_popover(Some(&popover));
        toolbar.append(&sort_button);

        // RIGHT: grid/list toggle.
        let toggle_button = gtk::Button::new();
        toggle_button.add_css_class("flat");
        toggle_button.set_icon_name(toggle_icon(layout.get()));
        toggle_button.set_tooltip_text(Some(&gettext("Toggle grid or list view")));

        let layout_ref = Rc::clone(layout);
        let card_list_ref = Rc::clone(&card_list);
        toggle_button.connect_clicked(move |btn| {
            // Two-state toggle between grid (vertical) and list (horizontal).
            let next = match layout_ref.get() {
                CardLayout::Horizontal => CardLayout::Vertical,
                _ => CardLayout::Horizontal,
            };
            layout_ref.set(next);
            btn.set_icon_name(toggle_icon(next));
            card_list_ref.update_layout(next);
            dispatcher.dispatch(BrowserAction::ChangeCardLayout(next).into());
        });
        toolbar.append(&toggle_button);

        (toolbar, toggle_button)
    }

    /// Fill the sort popover with the three radio options for this screen.
    /// Selecting one re-sorts the list, updates the button label and persists it.
    fn populate_sort_options(
        sort_box: &gtk::Box,
        current_sort: &Rc<Cell<SortOrder>>,
        card_list: &Rc<CardList>,
        dispatcher: &Rc<dyn ActionDispatcher>,
        sort_label: &gtk::Label,
    ) {
        let mut group: Option<gtk::CheckButton> = None;
        for order in SORT_ORDERS {
            let btn = gtk::CheckButton::with_label(&order.label());
            if let Some(ref g) = group {
                btn.set_group(Some(g));
            } else {
                group = Some(btn.clone());
            }
            btn.set_active(order == current_sort.get());

            let sort_ref = Rc::clone(current_sort);
            let card_list_ref = Rc::clone(card_list);
            let dispatch = Rc::clone(dispatcher);
            let label = sort_label.clone();
            btn.connect_toggled(move |b| {
                if b.is_active() {
                    sort_ref.set(order);
                    card_list_ref.set_sort(order);
                    label.set_label(&order.label());
                    dispatch.dispatch(
                        BrowserAction::ChangeSortOrder(PAGE_ID.to_string(), order).into(),
                    );
                }
            });
            sort_box.append(&btn);
        }
    }

    /// Bind the card list to the store for the current filter and apply the
    /// current sort. Called on creation and on every filter change.
    fn rebind(&self) {
        if self.model.filter() == LibraryFilter::All {
            self.model.reconcile_combined();
        }
        self.card_list.bind(
            &self.model,
            self.worker.clone(),
            self.layout.get(),
            self.size.get(),
        );
        if self.current_sort.get() != SortOrder::RecentlyAdded {
            self.card_list.set_sort(self.current_sort.get());
        }
        self.update_empty_state();
    }

    /// Grid uses a fixed 3-column layout on phones; list is single-column.
    fn apply_grid_columns(&self) {
        let cols = if self.layout.get() == CardLayout::Horizontal {
            1
        } else {
            GRID_COLUMNS
        };
        self.card_list.widget().set_min_children_per_line(cols);
        self.card_list.widget().set_max_children_per_line(cols);
    }

    fn connect_infinite_scroll(&self) {
        let card_list_weak = Rc::downgrade(&self.card_list);
        let model_weak = Rc::downgrade(&self.model);
        self.scrolled_window.connect_edge_reached(move |_, pos| {
            if pos != gtk::PositionType::Bottom {
                return;
            }
            let (Some(model), Some(card_list)) = (model_weak.upgrade(), card_list_weak.upgrade())
            else {
                return;
            };
            if CardListModel::has_more(&*model) {
                card_list.append_placeholders();
                CardListModel::load_more(&*model);
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
            let clear = clear_button.clone();
            let this = self.weak_rebind_closure();
            pill.connect_toggled(move |btn| {
                // Grouped toggles behave like radios: only the "activated" edge
                // is a real filter change; deactivation happens when another pill
                // or the ✕ clear button takes over.
                if !btn.is_active() {
                    return;
                }
                let Some(model) = model.upgrade() else {
                    return;
                };
                model.set_filter(filter);
                clear.set_visible(true);
                this();
            });
        }

        // Clear pill resets every toggle, dropping back to All.
        let pills = pills.clone();
        let model = Rc::downgrade(&self.model);
        let this = self.weak_rebind_closure();
        clear_button.connect_clicked(move |btn| {
            for pill in pills.iter() {
                pill.set_active(false);
            }
            btn.set_visible(false);
            if let Some(model) = model.upgrade() {
                model.set_filter(LibraryFilter::All);
            }
            this();
        });
    }

    /// Produce a callback that re-binds the list and scrolls to the top, used by
    /// the pill handlers (which can't borrow `self`).
    fn weak_rebind_closure(&self) -> impl Fn() {
        let model = Rc::downgrade(&self.model);
        let card_list = Rc::downgrade(&self.card_list);
        let status_page = self.status_page.clone();
        let scrolled = self.scrolled_window.clone();
        let worker = self.worker.clone();
        let layout = Rc::clone(&self.layout);
        let size = Rc::clone(&self.size);
        let sort = Rc::clone(&self.current_sort);
        move || {
            let (Some(model), Some(card_list)) = (model.upgrade(), card_list.upgrade()) else {
                return;
            };
            if model.filter() == LibraryFilter::All {
                model.reconcile_combined();
            } else {
                model.clear_combined();
            }
            card_list.bind(&model, worker.clone(), layout.get(), size.get());
            if sort.get() != SortOrder::RecentlyAdded {
                card_list.set_sort(sort.get());
            }
            update_empty(&model, &status_page);
            scrolled.vadjustment().set_value(0.0);
        }
    }

    fn update_empty_state(&self) {
        update_empty(&self.model, &self.status_page);
    }
}

fn toggle_icon(layout: CardLayout) -> &'static str {
    match layout {
        CardLayout::Horizontal => "view-grid-symbolic",
        _ => "view-list-symbolic",
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
                self.card_list.show_placeholders();
                self.model.refresh_all();
            }
            AppEvent::LoginEvent(LoginEvent::LogoutCompleted) => {
                self.model.clear_combined();
                self.card_list.widget().remove_all();
                self.status_page.set_visible(false);
            }
            AppEvent::BrowserEvent(
                BrowserEvent::LibraryUpdated
                | BrowserEvent::SavedPlaylistsUpdated
                | BrowserEvent::SavedArtistsUpdated,
            ) => {
                self.card_list.remove_placeholders();
                if self.model.filter() == LibraryFilter::All {
                    // Append newly-arrived items to the combined store in place;
                    // the CardList picks them up via items-changed (no rebind).
                    self.model.reconcile_combined();
                }
                self.update_empty_state();
                // Keep filling the viewport while there's more and no scrollbar.
                let adj = self.scrolled_window.vadjustment();
                if adj.upper() <= adj.page_size() && CardListModel::has_more(&*self.model) {
                    self.card_list.append_placeholders();
                    CardListModel::load_more(&*self.model);
                }
                let sort = self.current_sort.get();
                if sort != SortOrder::RecentlyAdded {
                    self.card_list.set_sort(sort);
                }
            }
            AppEvent::BrowserEvent(
                BrowserEvent::CardLayoutChanged(_) | BrowserEvent::CardSizeChanged(_),
            ) => {
                self.card_list.update_layout(self.layout.get());
                self.card_list.update_size(self.size.get());
                self.apply_grid_columns();
                self.toggle_button
                    .set_icon_name(toggle_icon(self.layout.get()));
            }
            _ => {}
        }
    }
}
