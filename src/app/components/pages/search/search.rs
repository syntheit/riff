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
use crate::app::models::{CardKind, CardModel, PlaylistDescription, SongDescription};
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
                // Fire on every change (including down to a single character and back
                // to empty) so the local owned-playlist matches can update instantly.
                f(query.to_string());
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
            #[weak]
            results_store,
            #[weak]
            filter_model,
            move |q| {
                // Record the query in state and kick the (debounced) API search.
                // An empty query clears the store; local matches need a query too.
                model.search(q.clone());
                // Rebuild the local owned-playlist matches immediately so a partial
                // query (even a single character) surfaces a matching own playlist
                // without waiting for — or depending on — the API round-trip.
                rebuild_store(&model, &results_store, &filter_model, &q);
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

    /// Rebuild the store when fresh API results land. Reads the current query from
    /// state so the local owned-playlist matches stay in sync with the API results.
    fn update_results(&self) {
        let query = self.model.current_query();
        rebuild_store(&self.model, &self.results_store, &self.filter_model, &query);
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

/// Base insertion position offset applied to every API result. Local owned-playlist
/// matches take positions `0..N` (below this base), so the "library first" sorter —
/// which orders pinned items by insertion position — always ranks them above every
/// API result (even a pinned/saved API album or the user's own playlist as returned
/// by the API).
const API_POSITION_BASE: u32 = 1_000;

/// Rank of an owned-playlist name against a lowercased query. Lower is better;
/// `None` means no match. Prefix of the whole name ranks first, then a match at a
/// word boundary (a word in the name starts with the query), then any substring.
fn owned_match_rank(name_lower: &str, query_lower: &str) -> Option<u32> {
    if query_lower.is_empty() {
        return None;
    }
    if name_lower.starts_with(query_lower) {
        return Some(0);
    }
    // Word-boundary prefix: some word in the name starts with the query.
    let word_prefix = name_lower
        .split(|c: char| c.is_whitespace() || c == '-' || c == '_' || c == '/')
        .any(|w| w.starts_with(query_lower));
    if word_prefix {
        return Some(1);
    }
    if name_lower.contains(query_lower) {
        return Some(2);
    }
    None
}

/// The user's own playlists whose name matches `query` (case-insensitive contains),
/// ranked prefix-first then by title, as (rank, playlist) pairs sorted best-first.
fn matching_owned_playlists(
    owned: &[PlaylistDescription],
    query: &str,
) -> Vec<PlaylistDescription> {
    let query_lower = query.trim().to_lowercase();
    if query_lower.is_empty() {
        return Vec::new();
    }
    let mut matches: Vec<(u32, &PlaylistDescription)> = owned
        .iter()
        .filter_map(|p| {
            owned_match_rank(&p.title.to_lowercase(), &query_lower).map(|rank| (rank, p))
        })
        .collect();
    // Best rank first; within a rank, alphabetical by title for stable ordering.
    matches.sort_by(|(ra, a), (rb, b)| {
        ra.cmp(rb)
            .then_with(|| a.title.to_lowercase().cmp(&b.title.to_lowercase()))
    });
    matches.into_iter().map(|(_, p)| p.clone()).collect()
}

/// Rebuild the flat result store as: the user's own matching playlists (matched
/// locally, prepended at the very top) followed by the API results (songs, artists,
/// albums, playlists). Local matches are deduped against the API playlists by id so
/// a playlist the API *did* return isn't shown twice. Cards are tagged with kind and
/// a "pinned / in your library" flag; the model chain's sorter floats pinned items
/// (local matches first, via their low insertion position) to the top.
fn rebuild_store(
    model: &SearchResultsModel,
    store: &gio::ListStore,
    filter_model: &gtk::FilterListModel,
    query: &str,
) {
    store.remove_all();

    // ── Local owned-playlist matches (instant, API-independent) ──────────────
    let owned = model.owned_playlists();
    let local_matches = matching_owned_playlists(&owned, query);
    let local_ids: std::collections::HashSet<String> =
        local_matches.iter().map(|p| p.id.clone()).collect();

    let mut position: u32 = 0;
    for playlist in local_matches.iter() {
        let card = CardModel::from(playlist);
        card.set_pinned(true); // float above every API result
        card.set_insertion_position(position); // 0..N: best match first
        store.append(&card);
        position += 1;
    }

    // O(1) library-membership lookups from the logged-in user's owned/saved
    // content (playlists via login state; albums/artists via the home stores).
    let (owned_playlist_ids, saved_albums, followed_artists) = model.library_ids();

    // ── API results (below the local matches) ────────────────────────────────
    let Some(results) = model.get_results() else {
        // No API results yet (e.g. still typing / before the debounced fetch):
        // the local matches above are already visible. Re-run the pill filter so
        // the view reflects the new store, then return.
        if let Some(f) = filter_model.filter() {
            f.changed(gtk::FilterChange::Different);
        }
        return;
    };

    let mut position: u32 = API_POSITION_BASE;

    // Songs (tracks). These carry CardKind::None; open → play in album context.
    for track in results.tracks.songs.iter() {
        let card = CardModel::from(track).with_data(track.clone());
        card.set_insertion_position(position);
        store.append(&card);
        position += 1;
    }

    // Artists (round art). Float followed artists to the top.
    for artist in results.artists.iter() {
        let card = CardModel::from(artist);
        card.set_pinned(followed_artists.contains(&artist.id));
        card.set_insertion_position(position);
        store.append(&card);
        position += 1;
    }

    // Albums. Float saved albums to the top.
    for album in results.albums.iter() {
        let card = CardModel::from(album);
        card.set_pinned(saved_albums.contains(&album.id));
        card.set_insertion_position(position);
        store.append(&card);
        position += 1;
    }

    // Playlists. Skip any the local pass already injected (dedupe by id), and
    // float the user's own playlists to the top of the API block.
    for playlist in results.playlists.iter() {
        if local_ids.contains(&playlist.id) {
            continue;
        }
        let card = CardModel::from(playlist);
        card.set_pinned(owned_playlist_ids.contains(&playlist.id));
        card.set_insertion_position(position);
        store.append(&card);
        position += 1;
    }

    // Re-run the active pill filter (and thus the sort) over the rebuilt store.
    if let Some(f) = filter_model.filter() {
        f.changed(gtk::FilterChange::Different);
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
