use gettextrs::gettext;
use gio::prelude::*;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::CompositeTemplate;
use std::cell::Cell;
use std::rc::Rc;

use crate::app::components::utils::Debouncer;
use crate::app::components::{Component, EventListener};
use crate::app::dispatch::Worker;
use crate::app::models::{CardKind, CardModel, SongDescription};
use crate::app::state::{AppEvent, BrowserEvent};

use super::search_row::SearchRow;
use super::SearchResultsModel;

/// Which slice of the mixed result list the active filter pill shows. `All`
/// keeps every type mixed (library matches first); the others narrow to one type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchFilter {
    All,
    Songs,
    Artists,
    Albums,
    Playlists,
}

impl SearchFilter {
    /// The `CardKind` this filter admits, or `None` for `All`.
    fn kind(self) -> Option<CardKind> {
        match self {
            SearchFilter::All => None,
            SearchFilter::Songs => Some(CardKind::None), // tracks carry CardKind::None
            SearchFilter::Artists => Some(CardKind::Artist),
            SearchFilter::Albums => Some(CardKind::Album),
            SearchFilter::Playlists => Some(CardKind::Playlist),
        }
    }
}

mod imp {

    use super::*;

    #[derive(Debug, Default, CompositeTemplate)]
    #[template(resource = "/dev/diegovsky/Riff/components/search.ui")]
    pub struct SearchResultsWidget {
        #[template_child]
        pub search_entry: TemplateChild<gtk::SearchEntry>,

        #[template_child]
        pub status_page: TemplateChild<libadwaita::StatusPage>,

        /// The scrolled container that fills the space below the pinned search
        /// field + pills. Its child (the ListView) is built in Rust.
        #[template_child]
        pub results_scroll: TemplateChild<gtk::ScrolledWindow>,

        /// Horizontally-scrollable pill row (its inner box is filled in Rust).
        #[template_child]
        pub pill_row: TemplateChild<gtk::Box>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for SearchResultsWidget {
        const NAME: &'static str = "SearchResultsWidget";
        type Type = super::SearchResultsWidget;
        type ParentType = gtk::Box;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for SearchResultsWidget {}
    impl BoxImpl for SearchResultsWidget {}

    impl WidgetImpl for SearchResultsWidget {
        fn grab_focus(&self) -> bool {
            self.search_entry.grab_focus()
        }
    }
}

glib::wrapper! {
    pub struct SearchResultsWidget(ObjectSubclass<imp::SearchResultsWidget>) @extends gtk::Widget, gtk::Box;
}

impl Default for SearchResultsWidget {
    fn default() -> Self {
        Self::new()
    }
}

impl SearchResultsWidget {
    pub fn new() -> Self {
        glib::Object::new()
    }

    fn connect_search_updated<F>(&self, f: F)
    where
        F: Fn(String) + 'static,
    {
        self.imp().search_entry.connect_changed(clone!(
            #[weak(rename_to = _self)]
            self,
            move |s| {
                let query = s.text();
                let query = query.as_str();
                _self.imp().status_page.set_visible(query.is_empty());
                _self.imp().results_scroll.set_visible(!query.is_empty());
                if !query.is_empty() {
                    f(query.to_string());
                }
            }
        ));
    }
}

pub struct SearchResults {
    widget: SearchResultsWidget,
    model: Rc<SearchResultsModel>,
    /// The single flat store holding every result (mixed types). Rebuilt on each
    /// results update. The view's model chain filters/sorts a live view over it.
    results_store: gio::ListStore,
    /// Root of the model chain — re-filtered when the active pill changes.
    filter_model: gtk::FilterListModel,
    filter: Rc<Cell<SearchFilter>>,
    debouncer: Debouncer,
}

impl SearchResults {
    pub fn new(model: SearchResultsModel, worker: Worker) -> Self {
        crate::app::components::display_add_css_provider(resource!("/components/search.css"));

        let model = Rc::new(model);
        let widget = SearchResultsWidget::new();

        let results_store = gio::ListStore::new::<CardModel>();
        let filter = Rc::new(Cell::new(SearchFilter::All));

        // ── Model chain: store → filter (by pill) → sort (library first) ─────────
        let filter_fn = gtk::CustomFilter::new({
            let filter = Rc::clone(&filter);
            move |obj| {
                let Some(card) = obj.downcast_ref::<CardModel>() else {
                    return true;
                };
                match filter.get().kind() {
                    None => true,
                    Some(kind) => card.card_kind() == kind,
                }
            }
        });
        let filter_model =
            gtk::FilterListModel::new(Some(results_store.clone()), Some(filter_fn));

        // Sorter: library matches first (pinned flag reused as "in your library"),
        // then original result order (insertion position).
        let sorter = gtk::CustomSorter::new(|a, b| {
            let a = a.downcast_ref::<CardModel>().unwrap();
            let b = b.downcast_ref::<CardModel>().unwrap();
            use std::cmp::Ordering;
            let ord = match (a.is_pinned(), b.is_pinned()) {
                (true, false) => Ordering::Less,
                (false, true) => Ordering::Greater,
                _ => a.insertion_position().cmp(&b.insertion_position()),
            };
            ord.into()
        });
        let sort_model =
            gtk::SortListModel::new(Some(filter_model.clone()), Some(sorter));
        let selection = gtk::NoSelection::new(Some(sort_model));

        // ── The virtualized ListView (compact rows) ─────────────────────────────
        let factory = gtk::SignalListItemFactory::new();
        factory.connect_setup(|_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().unwrap();
            item.set_child(Some(&SearchRow::new()));
        });
        {
            let worker = worker.clone();
            factory.connect_bind(move |_, item| {
                let item = item.downcast_ref::<gtk::ListItem>().unwrap();
                let model = item.item().unwrap().downcast::<CardModel>().unwrap();
                let row = item.child().unwrap().downcast::<SearchRow>().unwrap();
                row.bind(&model, &worker);
            });
        }
        factory.connect_unbind(|_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().unwrap();
            let row = item.child().unwrap().downcast::<SearchRow>().unwrap();
            row.unbind();
        });

        let list_view = gtk::ListView::builder()
            .model(&selection)
            .factory(&factory)
            .single_click_activate(true)
            .css_classes(["search-list"])
            .build();

        // Activation (tap) → open the item by kind.
        {
            let selection = selection.clone();
            let model = Rc::downgrade(&model);
            list_view.connect_activate(move |_, position| {
                let (Some(item), Some(model)) = (selection.item(position), model.upgrade()) else {
                    return;
                };
                let Some(card) = item.downcast_ref::<CardModel>() else {
                    return;
                };
                open_card(&model, card);
            });
        }

        widget.imp().results_scroll.set_child(Some(&list_view));

        widget.connect_search_updated(clone!(
            #[weak]
            model,
            move |q| {
                model.search(q);
            }
        ));

        let this = Self {
            widget,
            model,
            results_store,
            filter_model,
            filter,
            debouncer: Debouncer::new(),
        };

        this.build_pills();
        this
    }

    /// Populate the pill row with All / Songs / Artists / Albums / Playlists.
    /// Reuses the library filter-pill look (`ToggleButton` + `pill` css class in a
    /// horizontally-scrollable row). Selecting a pill re-filters the single list.
    fn build_pills(&self) {
        let row = &self.widget.imp().pill_row;
        let specs = [
            (SearchFilter::All, gettext("All")),
            (SearchFilter::Songs, gettext("Songs")),
            (SearchFilter::Artists, gettext("Artists")),
            (SearchFilter::Albums, gettext("Albums")),
            (SearchFilter::Playlists, gettext("Playlists")),
        ];

        let mut first: Option<gtk::ToggleButton> = None;
        for (which, label) in specs {
            let pill = gtk::ToggleButton::with_label(&label);
            pill.add_css_class("pill");
            pill.add_css_class("search-pill");
            if let Some(ref group) = first {
                pill.set_group(Some(group));
            } else {
                pill.set_active(true); // "All" selected by default
                first = Some(pill.clone());
            }

            let filter = Rc::clone(&self.filter);
            let filter_model = self.filter_model.clone();
            pill.connect_toggled(move |btn| {
                if !btn.is_active() {
                    return;
                }
                filter.set(which);
                // Re-run the filter (and thus the sort) over the store.
                if let Some(f) = filter_model.filter() {
                    f.changed(gtk::FilterChange::Different);
                }
            });
            row.append(&pill);
        }
    }

    /// Rebuild the flat result store from the domain results, tagging each card
    /// with its kind and whether it lives in the user's own library (so it floats
    /// to the top). Everything lands in one store; the pill filter narrows the view.
    fn update_results(&self) {
        let Some(results) = self.model.get_results() else {
            return;
        };

        // O(1) library-membership lookups from the logged-in user's owned/saved
        // content (playlists via login state; albums/artists via the home stores).
        let (owned_playlists, saved_albums, followed_artists) = self.model.library_ids();

        self.results_store.remove_all();
        let mut position: u32 = 0;

        // Songs (tracks). These carry CardKind::None; open → play in album context.
        for track in results.tracks.songs.iter() {
            let card = CardModel::from(track).with_data(track.clone());
            card.set_insertion_position(position);
            self.results_store.append(&card);
            position += 1;
        }

        // Artists (round art). Float followed artists to the top.
        for artist in results.artists.iter() {
            let card = CardModel::from(artist);
            card.set_pinned(followed_artists.contains(&artist.id));
            card.set_insertion_position(position);
            self.results_store.append(&card);
            position += 1;
        }

        // Albums. Float saved albums to the top.
        for album in results.albums.iter() {
            let card = CardModel::from(album);
            card.set_pinned(saved_albums.contains(&album.id));
            card.set_insertion_position(position);
            self.results_store.append(&card);
            position += 1;
        }

        // Playlists. Float the user's own playlists to the top.
        for playlist in results.playlists.iter() {
            let card = CardModel::from(playlist);
            card.set_pinned(owned_playlists.contains(&playlist.id));
            card.set_insertion_position(position);
            self.results_store.append(&card);
            position += 1;
        }
    }

    fn update_search_query(&self) {
        self.debouncer.debounce(
            600,
            clone!(
                #[weak(rename_to = model)]
                self.model,
                move || model.fetch_results()
            ),
        );
    }
}

/// Open the item behind a result card by its kind: songs play (in album context),
/// artists/albums/playlists push their detail page.
fn open_card(model: &SearchResultsModel, card: &CardModel) {
    match card.card_kind() {
        CardKind::Artist => model.open_artist(card.id()),
        CardKind::Album => model.open_album(card.id()),
        CardKind::Playlist => model.open_playlist(card.id()),
        // A track: reuse the stashed SongDescription so playback opens the album
        // and starts the song (mirrors the old track section behaviour).
        CardKind::None => {
            if let Some(data) = card.data() {
                if let Some(song) = data.downcast_ref::<SongDescription>() {
                    model.open_track(song.clone());
                }
            }
        }
    }
}

impl Component for SearchResults {
    fn get_root_widget(&self) -> &gtk::Widget {
        self.widget.as_ref()
    }
}

impl EventListener for SearchResults {
    fn on_event(&mut self, app_event: &AppEvent) {
        match app_event {
            AppEvent::BrowserEvent(BrowserEvent::SearchUpdated) => {
                self.get_root_widget().grab_focus();
                self.update_search_query();
            }
            AppEvent::BrowserEvent(BrowserEvent::SearchResultsUpdated) => {
                self.update_results();
            }
            AppEvent::BrowserEvent(BrowserEvent::AlbumDetailsLoaded(id)) => {
                self.model.on_album_loaded(id);
            }
            _ => {}
        }
    }
}
