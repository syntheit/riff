//! Virtualized library content view.
//!
//! Replaces the old FlowBox (which built a widget per item) with a `gtk::ListView`
//! (list mode) and a `gtk::GridView` (grid mode), both backed by a shared
//! `FilterListModel` → `SortListModel` → `NoSelection` chain over the model's
//! source `gio::ListStore`. Only rows scrolled into view are ever realized; art is
//! loaded lazily per visible row and recycled on unbind. Sorting and pin-floating
//! are driven by the `SortListModel`'s sorter (stable + instant), never by rebuilds.

use std::cell::Cell;
use std::rc::Rc;

use gio::prelude::*;
use gtk::prelude::*;

use super::pinned_store::PinnedStore;
use super::row_widgets::{LibraryCard, LibraryRow};
use crate::app::components::SortOrder;
use crate::app::models::{CardLayout, CardModel};
use crate::app::Worker;

/// Number of columns in grid mode on a phone-width screen.
const GRID_COLUMNS: u32 = 3;

/// Callbacks the library screen hands to the list: what to do on tap (open) and
/// on long-press (context drawer). Both receive the item id.
pub struct LibraryListCallbacks {
    pub on_activate: Box<dyn Fn(String)>,
    pub on_long_press: Box<dyn Fn(String)>,
}

/// The swappable list/grid content widget for the library screen.
pub struct LibraryList {
    /// A `gtk::Stack` holding the list and grid views; only the active one is shown.
    stack: gtk::Stack,
    list_view: gtk::ListView,
    grid_view: gtk::GridView,
    /// The live model chain. `selection` feeds both views; `filter_model` root is
    /// re-pointed when the filter changes; `sorter` is invalidated on sort changes.
    filter_model: gtk::FilterListModel,
    /// Kept alive so the sorted view survives (holds the chain together); read via
    /// the views' selection model rather than directly.
    #[allow(dead_code)]
    sort_model: gtk::SortListModel,
    sorter: gtk::CustomSorter,
    pins: PinnedStore,
    sort: Rc<Cell<SortOrder>>,
    descending: Rc<Cell<bool>>,
}

impl LibraryList {
    pub fn new(
        worker: Worker,
        pins: PinnedStore,
        initial_sort: SortOrder,
        initial_descending: bool,
        callbacks: LibraryListCallbacks,
    ) -> Rc<Self> {
        let sort = Rc::new(Cell::new(initial_sort));
        let descending = Rc::new(Cell::new(initial_descending));

        // Sorter: pinned items first (by pin rank), then the active sort order.
        let sorter = gtk::CustomSorter::new({
            let sort = Rc::clone(&sort);
            let descending = Rc::clone(&descending);
            let pins = pins.clone();
            move |a, b| {
                let a = a.downcast_ref::<CardModel>().unwrap();
                let b = b.downcast_ref::<CardModel>().unwrap();
                compare_cards(&pins, sort.get(), descending.get(), a, b).into()
            }
        });

        // No source model yet — set_source() points it at the current filter's store.
        // A passthrough (no filter): filtering is done model-side by store-swap, so
        // the FilterListModel just adapts whatever source we point it at.
        let filter_model =
            gtk::FilterListModel::new(None::<gio::ListStore>, None::<gtk::CustomFilter>);
        let sort_model =
            gtk::SortListModel::new(Some(filter_model.clone()), Some(sorter.clone()));
        let selection = gtk::NoSelection::new(Some(sort_model.clone()));

        let callbacks = Rc::new(callbacks);

        // ── List view (compact rows) ──────────────────────────────────────────
        let list_factory = gtk::SignalListItemFactory::new();
        list_factory.connect_setup(|_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().unwrap();
            item.set_child(Some(&LibraryRow::new()));
        });
        {
            let worker = worker.clone();
            list_factory.connect_bind(move |_, item| {
                let item = item.downcast_ref::<gtk::ListItem>().unwrap();
                let model = item.item().unwrap().downcast::<CardModel>().unwrap();
                let row = item.child().unwrap().downcast::<LibraryRow>().unwrap();
                row.bind(&model, &worker);
            });
        }
        list_factory.connect_unbind(|_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().unwrap();
            let row = item.child().unwrap().downcast::<LibraryRow>().unwrap();
            row.unbind();
        });

        let list_view = gtk::ListView::builder()
            .model(&selection)
            .factory(&list_factory)
            // Tap-to-open on touch: a single click/tap activates the row. A held
            // press is intercepted by the long-press gesture below (which claims the
            // sequence), so it opens the context drawer instead of activating.
            .single_click_activate(true)
            .css_classes(["library-list"])
            .build();

        // ── Grid view (tiles) ─────────────────────────────────────────────────
        let grid_factory = gtk::SignalListItemFactory::new();
        grid_factory.connect_setup(|_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().unwrap();
            item.set_child(Some(&LibraryCard::new()));
        });
        {
            let worker = worker.clone();
            grid_factory.connect_bind(move |_, item| {
                let item = item.downcast_ref::<gtk::ListItem>().unwrap();
                let model = item.item().unwrap().downcast::<CardModel>().unwrap();
                let card = item.child().unwrap().downcast::<LibraryCard>().unwrap();
                card.bind(&model, &worker);
            });
        }
        grid_factory.connect_unbind(|_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().unwrap();
            let card = item.child().unwrap().downcast::<LibraryCard>().unwrap();
            card.unbind();
        });

        let grid_view = gtk::GridView::builder()
            .model(&selection)
            .factory(&grid_factory)
            .min_columns(1)
            .max_columns(GRID_COLUMNS)
            // Tap-to-open (see the ListView note above).
            .single_click_activate(true)
            .css_classes(["library-grid"])
            .build();

        // Activation (tap) → open the item. Both views expose the same selection
        // model, so `position` indexes the sorted/filtered view.
        {
            let selection = selection.clone();
            let cb = callbacks.clone();
            list_view.connect_activate(move |_, position| {
                if let Some(id) = id_at(&selection, position) {
                    (cb.on_activate)(id);
                }
            });
        }
        {
            let selection = selection.clone();
            let cb = callbacks.clone();
            grid_view.connect_activate(move |_, position| {
                if let Some(id) = id_at(&selection, position) {
                    (cb.on_activate)(id);
                }
            });
        }

        // Long-press → context drawer. GtkGestureLongPress fires with the press
        // coordinates; `pick` finds the child widget under them, and we walk up to
        // the row/card whose bound id we resolve back through the model.
        Self::attach_long_press(list_view.upcast_ref(), callbacks.clone());
        Self::attach_long_press(grid_view.upcast_ref(), callbacks.clone());

        let list_scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&list_view)
            .build();
        let grid_scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&grid_view)
            .build();

        let stack = gtk::Stack::new();
        stack.set_vexpand(true);
        stack.add_named(&list_scroll, Some("list"));
        stack.add_named(&grid_scroll, Some("grid"));

        Rc::new(Self {
            stack,
            list_view,
            grid_view,
            filter_model,
            sort_model,
            sorter,
            pins,
            sort,
            descending,
        })
    }

    /// Attach a long-press gesture to a view that resolves the pressed item id and
    /// invokes the context-drawer callback.
    fn attach_long_press(view: &gtk::Widget, callbacks: Rc<LibraryListCallbacks>) {
        let gesture = gtk::GestureLongPress::new();
        gesture.set_touch_only(false);
        let view_weak = view.downgrade();
        gesture.connect_pressed(move |_, x, y| {
            let Some(view) = view_weak.upgrade() else {
                return;
            };
            // `pick` gives the deepest widget under the press (e.g. a label inside a
            // row). Walk UP to the enclosing LibraryRow/LibraryCard and read the id
            // it is currently bound to.
            let Some(mut node) = view.pick(x, y, gtk::PickFlags::DEFAULT) else {
                return;
            };
            loop {
                if let Some(id) = bound_id_of_self(&node) {
                    (callbacks.on_long_press)(id);
                    return;
                }
                match node.parent() {
                    Some(parent) if parent == view => return,
                    Some(parent) => node = parent,
                    None => return,
                }
            }
        });
        view.add_controller(gesture);
    }

    pub fn widget(&self) -> &gtk::Widget {
        self.stack.upcast_ref()
    }

    /// Point the model chain at a new source store (on filter change). Applies pins
    /// and stable positions to the new store's items and re-sorts.
    pub fn set_source(&self, source: &gio::ListStore) {
        self.filter_model.set_model(Some(source));
        self.sync_pins();
    }

    /// Set which view (list vs grid) is visible.
    pub fn set_layout(&self, layout: CardLayout) {
        let name = if layout == CardLayout::Horizontal {
            "list"
        } else {
            "grid"
        };
        self.stack.set_visible_child_name(name);
    }

    /// Change the sort order and re-sort instantly via the SortListModel.
    pub fn set_sort(&self, sort: SortOrder, descending: bool) {
        self.sort.set(sort);
        self.descending.set(descending);
        self.sorter.changed(gtk::SorterChange::Different);
    }

    /// Re-apply the pinned flag and a stable source-order position to every item in
    /// the current source, then re-sort. Cheap: the store holds a few hundred
    /// lightweight GObjects. The position stamp gives "Recently added" a stable
    /// ordinal (the API fetch order) so the sort is deterministic in both
    /// directions rather than relying on SortListModel's incidental stability.
    pub fn sync_pins(&self) {
        if let Some(model) = self.filter_model.model() {
            for i in 0..model.n_items() {
                if let Some(card) = model.item(i).and_then(|o| o.downcast::<CardModel>().ok()) {
                    card.set_pinned(self.pins.is_pinned(&card.id()));
                    card.set_insertion_position(i + 1);
                }
            }
        }
        self.sorter.changed(gtk::SorterChange::Different);
    }

    /// Scroll both views back to the top (used on filter change).
    pub fn scroll_to_top(&self) {
        if let Some(sw) = self
            .list_view
            .ancestor(gtk::ScrolledWindow::static_type())
            .and_then(|w| w.downcast::<gtk::ScrolledWindow>().ok())
        {
            sw.vadjustment().set_value(0.0);
        }
        if let Some(sw) = self
            .grid_view
            .ancestor(gtk::ScrolledWindow::static_type())
            .and_then(|w| w.downcast::<gtk::ScrolledWindow>().ok())
        {
            sw.vadjustment().set_value(0.0);
        }
    }

    /// Connect a callback fired when either view is scrolled to the bottom edge
    /// (for infinite paging). Fires once per edge reach.
    pub fn connect_edge_reached(&self, callback: impl Fn() + 'static) {
        let callback = Rc::new(callback);
        for view in [
            self.list_view.clone().upcast::<gtk::Widget>(),
            self.grid_view.clone().upcast::<gtk::Widget>(),
        ] {
            if let Some(sw) = view
                .ancestor(gtk::ScrolledWindow::static_type())
                .and_then(|w| w.downcast::<gtk::ScrolledWindow>().ok())
            {
                let cb = callback.clone();
                sw.connect_edge_reached(move |_, pos| {
                    if pos == gtk::PositionType::Bottom {
                        cb();
                    }
                });
            }
        }
    }
}

/// Resolve the item id at a sorted-view `position` from the selection model.
fn id_at(selection: &gtk::NoSelection, position: u32) -> Option<String> {
    selection
        .item(position)
        .and_then(|o| o.downcast::<CardModel>().ok())
        .map(|c| c.id())
}

/// If `widget` is itself a bound `LibraryRow`/`LibraryCard`, return its currently
/// bound item id (None when the widget is something else or is recycled/unbound).
fn bound_id_of_self(widget: &gtk::Widget) -> Option<String> {
    if let Some(row) = widget.downcast_ref::<LibraryRow>() {
        let id = row.bound_id();
        return (!id.is_empty()).then_some(id);
    }
    if let Some(card) = widget.downcast_ref::<LibraryCard>() {
        let id = card.bound_id();
        return (!id.is_empty()).then_some(id);
    }
    None
}

/// Compare two cards: pinned first (by pin rank), then by the active sort order.
/// `descending` flips the within-order comparison (pins always stay on top).
fn compare_cards(
    pins: &PinnedStore,
    sort: SortOrder,
    descending: bool,
    a: &CardModel,
    b: &CardModel,
) -> std::cmp::Ordering {
    use std::cmp::Ordering;

    // Pinned items always float above unpinned, regardless of sort/direction.
    let (a_pinned, b_pinned) = (a.is_pinned(), b.is_pinned());
    match (a_pinned, b_pinned) {
        (true, false) => return Ordering::Less,
        (false, true) => return Ordering::Greater,
        (true, true) => {
            // Both pinned: keep a stable pin order (pin rank ascending).
            return pins.rank(&a.id()).cmp(&pins.rank(&b.id()));
        }
        (false, false) => {}
    }

    base_compare(sort, descending, a, b)
}

/// The comparison for a sort order, honouring `descending`. Only the primary key
/// is reversed by `descending`; the stable tiebreaker (insertion order, then id)
/// always stays ascending so the result is deterministic in both directions, and
/// unknown track counts (0 — e.g. artists) always sort last regardless of
/// direction rather than jumping to the top when the "Largest" order is flipped.
fn base_compare(
    sort: SortOrder,
    descending: bool,
    a: &CardModel,
    b: &CardModel,
) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let primary = match sort {
        SortOrder::Alphabetic => a.title().to_lowercase().cmp(&b.title().to_lowercase()),
        SortOrder::Size => {
            // Largest first by default; unknown counts (0) always sort last, in both
            // directions (they represent "no count", not "the smallest count").
            let (a_count, b_count) = (a.track_count(), b.track_count());
            match (a_count, b_count) {
                (0, 0) => Ordering::Equal,
                (0, _) => return Ordering::Greater,
                (_, 0) => return Ordering::Less,
                // b vs a → largest first as the natural (ascending-flag) direction.
                _ => b_count.cmp(&a_count),
            }
        }
        // RecentlyAdded (and any other order not offered on this screen) falls back
        // to insertion order, which mirrors the API fetch order.
        _ => a.insertion_position().cmp(&b.insertion_position()),
    };
    let primary = if descending { primary.reverse() } else { primary };
    primary.then_with(|| stable_tiebreak(a, b))
}

/// A deterministic tiebreaker used when a sort key compares equal (same track
/// count, same title). Falls back to the source insertion order, then the id, so
/// equal-keyed items keep a stable, repeatable position instead of shuffling.
fn stable_tiebreak(a: &CardModel, b: &CardModel) -> std::cmp::Ordering {
    a.insertion_position()
        .cmp(&b.insertion_position())
        .then_with(|| a.id().cmp(&b.id()))
}
