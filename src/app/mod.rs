use crate::auth::TokenStore;
use crate::settings::{RiffSettings, StateTracker};
use crate::{api::CachedSpotifyClient, feature_flags};
use futures::channel::mpsc::UnboundedSender;
use gtk::prelude::*;
use libadwaita::prelude::BinExt;
use std::rc::Rc;
use std::sync::Arc;

pub mod dispatch;
pub use dispatch::{ActionDispatcher, ActionDispatcherImpl, DispatchLoop, Worker};

pub mod components;
use components::*;

pub mod models;

mod list_store;
pub use list_store::*;

pub mod state;
pub use state::{
    AppAction, AppEvent, AppModel, AppState, BrowserAction, BrowserEvent, PaginationTarget,
};

mod batch_loader;
pub use batch_loader::*;

pub mod credentials;
pub mod loader;

pub mod rng;
pub use rng::LazyRandomIndex;

// Where all the app logic happens
pub struct App {
    settings: RiffSettings,
    // The builder instance used to properly configure all the widgets created at startup
    builder: gtk::Builder,
    // All the "components" that will be notified of things happening throughout the app
    components: Vec<Box<dyn EventListener>>,
    // Holds the app state
    model: Rc<AppModel>,
    // Allows sending actions that are handled by the model above
    sender: UnboundedSender<AppAction>,
    worker: Worker,
}

impl App {
    pub fn new(
        settings: RiffSettings,
        builder: gtk::Builder,
        sender: UnboundedSender<AppAction>,
        worker: Worker,
    ) -> Self {
        let state = AppState::new();
        let token_store = TokenStore::new();
        let spotify_client = Arc::new(CachedSpotifyClient::new(token_store.clone()));
        let model = Rc::new(AppModel::new(state, spotify_client));

        // Non widget components
        let components: Vec<Box<dyn EventListener>> = vec![
            App::make_player_notifier(
                Rc::clone(&model),
                &settings,
                Box::new(ActionDispatcherImpl::new(sender.clone(), worker.clone())),
                sender.clone(),
                token_store,
            ),
            Box::new(StateTracker::new_from_gsettings()),
            App::make_dbus(Rc::clone(&model), sender.clone()),
        ];

        Self {
            settings,
            builder,
            components,
            model,
            sender,
            worker,
        }
    }

    fn add_ui_components(&mut self) {
        // Most components will need some or all of these to work
        // ie some way to retrieve widgets
        let builder = &self.builder;
        // ...some way to read the app state
        let model = &self.model;
        // ...some way to handle various asynchronous tasks
        let worker = &self.worker;
        // ...some (basic) way to send actions that will change the app state
        let sender = &self.sender;
        // ...ALSO some way to send actions, but more conveniently
        let dispatcher = Box::new(ActionDispatcherImpl::new(sender.clone(), worker.clone()));

        // Send gsettings updates for saved settings like repeat mode, shuffle, etc.
        // has to be done after the UI loads, otherwise visual glitches occour.
        for action in self.settings.player_settings.actions() {
            sender.unbounded_send(action).unwrap();
        }

        // All components that will be available initially
        let mut components: Vec<Box<dyn EventListener>> = vec![
            App::make_window(&self.settings, builder, Rc::clone(model)),
            App::make_selection_toolbar(builder, Rc::clone(model), dispatcher.box_clone()),
            App::make_playback(
                builder,
                Rc::clone(model),
                dispatcher.box_clone(),
                worker.clone(),
            ),
            App::make_now_playing_sheet(
                builder,
                Rc::clone(model),
                dispatcher.box_clone(),
                worker.clone(),
            ),
            App::make_queue(
                builder,
                Rc::clone(model),
                dispatcher.box_clone(),
                worker.clone(),
            ),
            App::make_add_to_playlist(
                builder,
                Rc::clone(model),
                dispatcher.box_clone(),
                worker.clone(),
            ),
            App::make_song_menu(
                builder,
                Rc::clone(model),
                dispatcher.box_clone(),
                worker.clone(),
            ),
            App::make_login(builder, dispatcher.box_clone()),
            App::make_navigation(
                builder,
                Rc::clone(model),
                dispatcher.box_clone(),
                worker.clone(),
            ),
            App::make_user_menu(builder, Rc::clone(model), dispatcher),
            App::make_notification(builder),
        ];

        self.components.append(&mut components);
    }

    // A component that listens to what's happening in the app, and translates it for the actual player
    fn make_player_notifier(
        app_model: Rc<AppModel>,
        settings: &RiffSettings,
        dispatcher: Box<dyn ActionDispatcher>,
        sender: UnboundedSender<AppAction>,
        token_store: TokenStore,
    ) -> Box<impl EventListener> {
        let api = app_model.get_spotify();
        Box::new(PlayerNotifier::new(
            app_model,
            dispatcher,
            // Either communications with the librespot player
            crate::player::start_player_service(
                settings.player_settings.clone(),
                sender.clone(),
                token_store,
            ),
            // or with a Spotify Connect device
            crate::connect::start_connect_server(api, sender),
        ))
    }

    // A component to handle anything DBUS related
    fn make_dbus(
        app_model: Rc<AppModel>,
        sender: UnboundedSender<AppAction>,
    ) -> Box<impl EventListener> {
        Box::new(crate::dbus::start_dbus_server(app_model, sender))
    }

    fn make_window(
        settings: &RiffSettings,
        builder: &gtk::Builder,
        app_model: Rc<AppModel>,
    ) -> Box<impl EventListener> {
        let window: libadwaita::ApplicationWindow = builder.object("window").unwrap();
        Box::new(MainWindow::new(settings.window.clone(), app_model, window))
    }

    fn make_navigation(
        builder: &gtk::Builder,
        app_model: Rc<AppModel>,
        dispatcher: Box<dyn ActionDispatcher>,
        worker: Worker,
    ) -> Box<Navigation> {
        let root_nav: libadwaita::NavigationView = builder.object("root_nav").unwrap();
        let tab_stack: libadwaita::ViewStack = builder.object("tab_stack").unwrap();
        // This is where components that are not created initially will be assembled
        let screen_factory = ScreenFactory::new(app_model, dispatcher.box_clone(), worker);
        Box::new(Navigation::new(
            root_nav,
            tab_stack,
            screen_factory,
            dispatcher,
        ))
    }

    fn make_login(builder: &gtk::Builder, dispatcher: Box<dyn ActionDispatcher>) -> Box<Login> {
        let parent: gtk::Window = builder.object("window").unwrap();
        let model = LoginModel::new(dispatcher);
        Box::new(Login::new(parent, model))
    }

    fn make_selection_toolbar(
        builder: &gtk::Builder,
        app_model: Rc<AppModel>,
        dispatcher: Box<dyn ActionDispatcher>,
    ) -> Box<impl EventListener> {
        Box::new(SelectionToolbar::new(
            SelectionToolbarModel::new(app_model, dispatcher),
            builder.object("selection_toolbar").unwrap(),
        ))
    }

    fn make_playback(
        builder: &gtk::Builder,
        app_model: Rc<AppModel>,
        dispatcher: Box<dyn ActionDispatcher>,
        worker: Worker,
    ) -> Box<impl EventListener> {
        let model = PlaybackModel::new(app_model, dispatcher);
        Box::new(PlaybackControl::new(
            model,
            builder.object("playback").unwrap(),
            worker,
        ))
    }

    fn make_now_playing_sheet(
        builder: &gtk::Builder,
        app_model: Rc<AppModel>,
        dispatcher: Box<dyn ActionDispatcher>,
        worker: Worker,
    ) -> Box<impl EventListener> {
        let sheet: gtk::Widget = builder.object("now_playing_sheet").unwrap();
        let queue_sheet: gtk::Widget = builder.object("queue_sheet").unwrap();
        let widget: NowPlayingFullWidget = builder.object("now_playing_full").unwrap();
        let model = NowPlayingSheetModel::new(app_model, dispatcher);
        Box::new(NowPlayingSheet::new(
            model,
            sheet,
            queue_sheet,
            widget,
            worker,
        ))
    }

    // The queue card, hosted inside the queue_sheet overlay. It listens to
    // playback events and rebuilds itself, so it's always current when opened.
    fn make_queue(
        builder: &gtk::Builder,
        app_model: Rc<AppModel>,
        dispatcher: Box<dyn ActionDispatcher>,
        worker: Worker,
    ) -> Box<impl EventListener> {
        let host: libadwaita::Bin = builder.object("queue_host").unwrap();
        let model = QueueModel::new(app_model, dispatcher);
        let queue = Queue::new(model, worker);
        host.set_child(Some(queue.get_root_widget()));
        Box::new(queue)
    }

    fn make_add_to_playlist(
        builder: &gtk::Builder,
        app_model: Rc<AppModel>,
        dispatcher: Box<dyn ActionDispatcher>,
        worker: Worker,
    ) -> Box<impl EventListener> {
        let host: libadwaita::Bin = builder.object("add_to_playlist_host").unwrap();
        let sheet: gtk::Widget = builder.object("add_to_playlist_sheet").unwrap();
        let model = AddToPlaylistModel::new(app_model, dispatcher);
        Box::new(AddToPlaylist::new(model, host, sheet, worker))
    }

    fn make_song_menu(
        builder: &gtk::Builder,
        app_model: Rc<AppModel>,
        dispatcher: Box<dyn ActionDispatcher>,
        worker: Worker,
    ) -> Box<impl EventListener> {
        let host: libadwaita::Bin = builder.object("song_menu_host").unwrap();
        let sheet: gtk::Widget = builder.object("song_menu_sheet").unwrap();
        let add_to_playlist_sheet: gtk::Widget = builder.object("add_to_playlist_sheet").unwrap();
        let queue_sheet: gtk::Widget = builder.object("queue_sheet").unwrap();
        let now_playing_sheet: gtk::Widget = builder.object("now_playing_sheet").unwrap();
        let model = SongMenuModel::new(app_model, dispatcher);
        Box::new(SongMenu::new(
            model,
            host,
            sheet,
            add_to_playlist_sheet,
            queue_sheet,
            now_playing_sheet,
            worker,
        ))
    }

    fn make_user_menu(
        builder: &gtk::Builder,
        app_model: Rc<AppModel>,
        dispatcher: Box<dyn ActionDispatcher>,
    ) -> Box<UserMenu> {
        let parent: gtk::Window = builder.object("window").unwrap();
        let settings_model = SettingsModel::new(app_model.clone(), dispatcher.box_clone());
        let settings = Settings::new(parent.clone(), settings_model);

        let button: gtk::MenuButton = builder.object("user").unwrap();
        let about: libadwaita::AboutDialog = builder.object("about").unwrap();
        let model = UserMenuModel::new(app_model, dispatcher);
        let user_menu = UserMenu::new(button, settings, about, parent, model);
        Box::new(user_menu)
    }

    fn make_notification(builder: &gtk::Builder) -> Box<Notification> {
        let toast_overlay: libadwaita::ToastOverlay = builder.object("main").unwrap();
        Box::new(Notification::new(toast_overlay))
    }

    // Main handler called in a loop
    fn handle(&mut self, action: AppAction) {
        let starting = matches!(&action, &AppAction::Start);

        // Update the state based on an incoming action
        // and obtain events representing what that mutation entailed...
        let events = self.model.update_state(action);

        // (AppAction::Start is special and is used to setup the initial components)
        if !events.is_empty() && starting {
            self.add_ui_components();
        }

        // ...and notify every component that we know.
        // They'll be responsible for passing down these events, if they feel like it.
        for event in events.iter() {
            info!("Event: {event:?}");
            for component in self.components.iter_mut() {
                component.on_event(event);
            }
        }
    }

    // Here is the loop
    pub async fn attach(mut self, dispatch_loop: DispatchLoop) {
        let rt = tokio::runtime::Runtime::new().expect("Failed to acquire tokio runtime");
        let _guard = rt.enter();

        let app = &mut self;
        dispatch_loop
            .attach(move |action| {
                info!("Action: {action:?}");
                app.handle(action);
            })
            .await;
    }
}
