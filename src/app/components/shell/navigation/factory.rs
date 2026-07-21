use std::cell::Cell;
use std::rc::Rc;

use crate::app::components::sidebar::{Sidebar, SidebarModel};
use crate::app::components::*;
use crate::app::{ActionDispatcher, AppModel, Worker};
use crate::settings::StateTracker;

pub struct ScreenFactory {
    app_model: Rc<AppModel>,
    dispatcher: Rc<dyn ActionDispatcher>,
    worker: Worker,
    shared_layout: Rc<Cell<CardLayout>>,
    shared_size: Rc<Cell<CardSize>>,
    pins: PinnedStore,
}

impl ScreenFactory {
    pub fn new(
        app_model: Rc<AppModel>,
        dispatcher: Box<dyn ActionDispatcher>,
        worker: Worker,
        pins: PinnedStore,
    ) -> Self {
        let tracker = StateTracker::new_from_gsettings();
        Self {
            app_model,
            dispatcher: Rc::from(dispatcher),
            worker,
            shared_layout: Rc::new(Cell::new(tracker.load_card_layout())),
            shared_size: Rc::new(Cell::new(tracker.load_card_size())),
            pins,
        }
    }

    /// Wrap a `CardListComponent` in a `StandardScreen`, packing the view button into the headerbar.
    fn make_card_page<M: CardListPageModel + 'static>(
        page: CardListComponent<M>,
        screen_model: DefaultHeaderBarModel,
    ) -> StandardScreen<DefaultHeaderBarModel> {
        let view_btn = page.view_button().clone();
        let screen = StandardScreen::new(page, Rc::new(screen_model));
        screen.headerbar().pack_end(&view_btn);
        screen
    }

    /// The "Home" tab: a Spotify-style personal feed of horizontal card shelves
    /// (recently played, top artists/tracks, a "Because you listen to <artist>"
    /// mix, and saved-library albums).
    pub fn make_home(&self) -> impl ListenerComponent {
        let model = Rc::new(HomeFeedModel::new(
            Rc::clone(&self.app_model),
            self.dispatcher.box_clone(),
        ));
        HomeScreen::new(model, self.worker.clone())
    }

    /// The unified Spotify-style "Your Library" screen (filter pills + sort +
    /// grid/list toggle over saved albums/playlists/artists).
    pub fn make_library_screen(&self) -> impl ListenerComponent {
        let model = Rc::new(LibraryModel::new(
            Rc::clone(&self.app_model),
            self.dispatcher.box_clone(),
        ));
        LibraryScreen::new(
            model,
            self.worker.clone(),
            Rc::clone(&self.shared_layout),
            Rc::clone(&self.shared_size),
            Rc::clone(&self.dispatcher),
            self.pins.clone(),
        )
    }

    pub fn make_sidebar(&self, listbox: gtk::ListBox) -> impl ListenerComponent {
        let model = SidebarModel::new(Rc::clone(&self.app_model), self.dispatcher.box_clone());
        Sidebar::new(listbox, Rc::new(model))
    }

    // The standalone saved-albums / saved-playlists / saved-artists grids are
    // superseded by the home feed and the unified library screen's filter pills,
    // but kept as ready-to-mount detail targets (and to preserve their page
    // modules).
    #[allow(dead_code)]
    pub fn make_saved_albums(&self) -> impl ListenerComponent {
        let model = SavedAlbumsModel::new(Rc::clone(&self.app_model), self.dispatcher.box_clone());
        let screen_model = DefaultHeaderBarModel::new(
            Some(gettext("Albums")),
            None,
            Rc::clone(&self.app_model),
            self.dispatcher.box_clone(),
        );
        let page = make_saved_albums(
            self.worker.clone(),
            model,
            Rc::clone(&self.shared_layout),
            Rc::clone(&self.shared_size),
            Rc::clone(&self.dispatcher),
        );
        Self::make_card_page(page, screen_model)
    }

    #[allow(dead_code)]
    pub fn make_saved_playlists(&self) -> impl ListenerComponent {
        let model =
            SavedPlaylistsModel::new(Rc::clone(&self.app_model), self.dispatcher.box_clone());
        let screen_model = DefaultHeaderBarModel::new(
            Some(gettext("Playlists")),
            None,
            Rc::clone(&self.app_model),
            self.dispatcher.box_clone(),
        );
        let page = make_saved_playlists(
            self.worker.clone(),
            model,
            Rc::clone(&self.shared_layout),
            Rc::clone(&self.shared_size),
            Rc::clone(&self.dispatcher),
        );
        Self::make_card_page(page, screen_model)
    }

    #[allow(dead_code)]
    pub fn make_saved_artists(&self) -> impl ListenerComponent {
        let model = SavedArtistsModel::new(Rc::clone(&self.app_model), self.dispatcher.box_clone());
        let screen_model = DefaultHeaderBarModel::new(
            Some(gettext("Artists")),
            None,
            Rc::clone(&self.app_model),
            self.dispatcher.box_clone(),
        );
        let page = make_saved_artists(
            self.worker.clone(),
            model,
            Rc::clone(&self.shared_layout),
            Rc::clone(&self.shared_size),
            Rc::clone(&self.dispatcher),
        );
        Self::make_card_page(page, screen_model)
    }

    pub fn make_saved_tracks(&self) -> impl ListenerComponent {
        let model = Rc::new(SavedTracksModel::new(
            Rc::clone(&self.app_model),
            self.dispatcher.box_clone(),
        ));
        SavedTracks::new(model, self.worker.clone())
    }

    pub fn make_album_details(&self, id: String) -> impl ListenerComponent {
        let model = Rc::new(DetailsModel::new(
            id,
            Rc::clone(&self.app_model),
            self.dispatcher.box_clone(),
        ));
        Details::new(model, self.worker.clone())
    }

    pub fn make_radio(&self, seed_id: String, seed_name: String) -> impl ListenerComponent {
        let model = Rc::new(RadioModel::new(
            seed_id,
            seed_name,
            Rc::clone(&self.app_model),
            self.dispatcher.box_clone(),
        ));
        Radio::new(model, self.worker.clone())
    }

    pub fn make_search_results(&self) -> impl ListenerComponent {
        let model =
            SearchResultsModel::new(Rc::clone(&self.app_model), self.dispatcher.box_clone());
        SearchResults::new(model, self.worker.clone())
    }

    pub fn make_artist_details(&self, id: String) -> impl ListenerComponent {
        let model = Rc::new(ArtistDetailsModel::new(
            id,
            Rc::clone(&self.app_model),
            self.dispatcher.box_clone(),
        ));
        ArtistDetails::new(
            model,
            self.worker.clone(),
            Rc::clone(&self.shared_layout),
            Rc::clone(&self.shared_size),
            Rc::clone(&self.dispatcher),
        )
    }

    pub fn make_playlist_details(&self, id: String) -> impl ListenerComponent {
        let model = Rc::new(PlaylistDetailsModel::new(
            id,
            Rc::clone(&self.app_model),
            self.dispatcher.box_clone(),
        ));
        PlaylistDetails::new(model, self.worker.clone())
    }

    pub fn make_user_details(&self, id: String) -> impl ListenerComponent {
        let model =
            UserDetailsModel::new(id, Rc::clone(&self.app_model), self.dispatcher.box_clone());
        UserDetails::new(
            model,
            self.worker.clone(),
            Rc::clone(&self.shared_layout),
            Rc::clone(&self.shared_size),
            Rc::clone(&self.dispatcher),
        )
    }

    pub fn make_settings(&self) -> impl ListenerComponent {
        let model = SettingsModel::new(Rc::clone(&self.app_model), self.dispatcher.box_clone());
        SettingsPage::new(model, self.dispatcher.box_clone())
    }
}
