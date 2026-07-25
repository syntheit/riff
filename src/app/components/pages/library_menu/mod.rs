//! Long-press context drawer for the library screen.
//!
//! A bottom-sheet drawer (mirrors the song-menu pattern) opened by long-pressing a
//! library row/card. It shows the item at the top and a list of actions: Pin/Unpin,
//! Play, Go to (open the item), Remove from library (unfollow/unsave) and Share
//! (copy link). Actions dispatch the existing engine `BrowserAction`s; Pin/Unpin
//! toggles the local `PinnedStore`.

use gettextrs::gettext;
use gtk::prelude::*;
use libadwaita::prelude::BinExt;
use std::cell::RefCell;
use std::rc::Rc;

use super::library::PinnedStore;
use crate::app::components::{Component, EventListener};
use crate::app::loader::ImageLoader;
use crate::app::models::{CardKind, LibraryItem};
use crate::app::state::{BrowserAction, PlaybackAction};
use crate::app::{ActionDispatcher, AppAction, AppEvent, AppModel, SongsSource, Worker};

fn set_sheet_open(sheet: &gtk::Widget, open: bool) {
    sheet.set_property("open", open);
}

pub struct LibraryMenuModel {
    app_model: Rc<AppModel>,
    dispatcher: Box<dyn ActionDispatcher>,
    pins: PinnedStore,
}

impl LibraryMenuModel {
    pub fn new(
        app_model: Rc<AppModel>,
        dispatcher: Box<dyn ActionDispatcher>,
        pins: PinnedStore,
    ) -> Self {
        Self {
            app_model,
            dispatcher,
            pins,
        }
    }

    /// Toggle the local pin for an item and notify the library screen so it
    /// re-floats the list.
    fn toggle_pin(&self, id: &str) {
        self.pins.toggle(id);
        self.dispatcher.dispatch(AppAction::LibraryPinsChanged);
    }

    /// Remove an item from the library: unfollow artists, unsave albums/playlists.
    /// Dispatches the same engine actions the detail pages use.
    fn remove_from_library(&self, item: &LibraryItem) {
        let api = self.app_model.get_spotify();
        let id = item.id.clone();
        match item.kind {
            CardKind::Album => {
                self.dispatcher.call_spotify_and_dispatch(move || async move {
                    api.remove_saved_album(&id)
                        .await
                        .map(|_| BrowserAction::UnsaveAlbum(id).into())
                });
            }
            CardKind::Artist => {
                self.dispatcher.call_spotify_and_dispatch(move || async move {
                    api.unfollow_artist(&id).await?;
                    Ok(BrowserAction::UnfollowArtist(id).into())
                });
            }
            CardKind::Playlist => {
                self.dispatcher.call_spotify_and_dispatch(move || async move {
                    api.unfollow_playlist(&id).await?;
                    Ok(BrowserAction::UnsavePlaylist(id).into())
                });
            }
            CardKind::None => {}
        }
        // A removed item can't stay pinned.
        if self.pins.unpin(&item.id) {
            self.dispatcher.dispatch(AppAction::LibraryPinsChanged);
        }
    }

    /// Start playback of an album or playlist: fetch its first track batch and load
    /// it as the playback source, then start at the first track. Artists have no
    /// single obvious track list here, so "Play" is only offered for albums/playlists.
    fn play_item(&self, item: &LibraryItem) {
        let api = self.app_model.get_spotify();
        let id = item.id.clone();
        match item.kind {
            CardKind::Album => {
                self.dispatcher.dispatch_many_async(Box::pin(async move {
                    match api.get_album(&id).await {
                        Ok(album) => {
                            let batch = album.description.songs.clone();
                            let source = SongsSource::Album(id.clone());
                            match batch.songs.first().map(|s| s.id.clone()) {
                                Some(first) => vec![
                                    PlaybackAction::LoadPagedSongs(source, batch).into(),
                                    PlaybackAction::Load(first).into(),
                                ],
                                None => vec![],
                            }
                        }
                        Err(_) => vec![],
                    }
                }));
            }
            CardKind::Playlist => {
                self.dispatcher.dispatch_many_async(Box::pin(async move {
                    match api.get_playlist(&id).await {
                        Ok(playlist) => {
                            let batch = playlist.songs.clone();
                            let source = SongsSource::Playlist {
                                id: playlist.id.clone(),
                                title: playlist.title.clone(),
                            };
                            match batch.songs.first().map(|s| s.id.clone()) {
                                Some(first) => vec![
                                    PlaybackAction::LoadPagedSongs(source, batch).into(),
                                    PlaybackAction::Load(first).into(),
                                ],
                                None => vec![],
                            }
                        }
                        Err(_) => vec![],
                    }
                }));
            }
            _ => {}
        }
    }

    fn open_item(&self, item: &LibraryItem) {
        match item.kind {
            CardKind::Album => self.dispatcher.dispatch(AppAction::ViewAlbum(item.id.clone())),
            CardKind::Artist => self.dispatcher.dispatch(AppAction::ViewArtist(item.id.clone())),
            _ => self.dispatcher.dispatch(AppAction::ViewPlaylist(item.id.clone())),
        }
    }

    fn share_link(&self, item: &LibraryItem) {
        let kind = match item.kind {
            CardKind::Album => "album",
            CardKind::Artist => "artist",
            CardKind::Playlist => "playlist",
            CardKind::None => return,
        };
        let link = format!("https://open.spotify.com/{kind}/{}", item.id);
        if let Some(display) = gdk::Display::default() {
            display
                .clipboard()
                .set_content(Some(&gdk::ContentProvider::for_value(&link.to_value())))
                .ok();
        }
        self.dispatcher
            .dispatch(AppAction::ShowNotification(gettext("Link copied")));
    }
}

pub struct LibraryMenu {
    model: Rc<LibraryMenuModel>,
    worker: Worker,
    sheet: gtk::Widget,
    host: libadwaita::Bin,
    current: RefCell<Option<LibraryItem>>,
}

impl LibraryMenu {
    pub fn new(
        model: LibraryMenuModel,
        host: libadwaita::Bin,
        sheet: gtk::Widget,
        worker: Worker,
    ) -> Self {
        Self {
            model: Rc::new(model),
            worker,
            sheet,
            host,
            current: RefCell::new(None),
        }
    }

    fn build_for(&self, item: &LibraryItem) {
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();

        // Item card at the top.
        let card = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .margin_top(28)
            .margin_bottom(12)
            .margin_start(16)
            .margin_end(16)
            .build();
        card.append(&art_thumbnail(item.art.as_deref(), item.kind, &self.worker));
        card.append(&item_text_box(&item.title, &compose_subtitle(item)));
        root.append(&card);

        root.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

        let actions = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .margin_bottom(16)
            .build();

        // Pin / Unpin.
        {
            let (icon, label) = if item.pinned {
                ("view-pin-symbolic", gettext("Unpin"))
            } else {
                ("view-pin-symbolic", gettext("Pin"))
            };
            let model = self.model.clone();
            let sheet = self.sheet.clone();
            let id = item.id.clone();
            actions.append(&action_row(icon, &label, move || {
                set_sheet_open(&sheet, false);
                model.toggle_pin(&id);
            }));
        }

        // Play (albums and playlists only).
        if matches!(item.kind, CardKind::Album | CardKind::Playlist) {
            let model = self.model.clone();
            let sheet = self.sheet.clone();
            let item = item.clone();
            actions.append(&action_row(
                "media-playback-start-symbolic",
                &gettext("Play"),
                move || {
                    set_sheet_open(&sheet, false);
                    model.play_item(&item);
                },
            ));
        }

        // Go to (open the item).
        {
            let model = self.model.clone();
            let sheet = self.sheet.clone();
            let item = item.clone();
            actions.append(&action_row(
                "go-next-symbolic",
                &gettext("Go to"),
                move || {
                    set_sheet_open(&sheet, false);
                    model.open_item(&item);
                },
            ));
        }

        // Remove from library.
        {
            let label = match item.kind {
                CardKind::Artist => gettext("Unfollow"),
                _ => gettext("Remove from library"),
            };
            let model = self.model.clone();
            let sheet = self.sheet.clone();
            let item = item.clone();
            actions.append(&action_row("user-trash-symbolic", &label, move || {
                set_sheet_open(&sheet, false);
                model.remove_from_library(&item);
            }));
        }

        // Share (copy link).
        {
            let model = self.model.clone();
            let sheet = self.sheet.clone();
            let item = item.clone();
            actions.append(&action_row(
                "edit-copy-symbolic",
                &gettext("Share"),
                move || {
                    set_sheet_open(&sheet, false);
                    model.share_link(&item);
                },
            ));
        }

        root.append(&actions);
        self.host.set_child(Some(&root));
    }
}

fn compose_subtitle(item: &LibraryItem) -> String {
    match item.kind.label() {
        Some(kind) if item.subtitle.is_empty() => kind,
        Some(kind) => format!("{kind} • {}", item.subtitle),
        None => item.subtitle.clone(),
    }
}

fn art_thumbnail(url: Option<&str>, kind: CardKind, worker: &Worker) -> gtk::Image {
    let image = gtk::Image::builder()
        .pixel_size(48)
        .valign(gtk::Align::Center)
        .build();
    if kind == CardKind::Artist {
        image.add_css_class("library-cover-round");
    }
    if let Some(url) = url.map(str::to_owned) {
        let weak = image.downgrade();
        worker.send_local_task(async move {
            if let Some(img) = weak.upgrade() {
                let loader = ImageLoader::new();
                if let Some(pixbuf) = loader.load_remote(&url, "jpg", 48, 48).await {
                    img.set_paintable(Some(&gdk::Texture::for_pixbuf(&pixbuf)));
                }
            }
        });
    } else {
        image.set_icon_name(Some("library-music-symbolic"));
    }
    image
}

fn item_text_box(title: &str, subtitle: &str) -> gtk::Box {
    let b = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .hexpand(true)
        .valign(gtk::Align::Center)
        .build();
    b.append(
        &gtk::Label::builder()
            .label(title)
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build(),
    );
    b.append(
        &gtk::Label::builder()
            .label(subtitle)
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["caption", "dim-label"])
            .build(),
    );
    b
}

fn action_row(icon_name: &str, label: &str, on_click: impl Fn() + 'static) -> gtk::Button {
    let inner = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .margin_top(4)
        .margin_bottom(4)
        .margin_start(12)
        .margin_end(12)
        .build();
    inner.append(
        &gtk::Image::builder()
            .icon_name(icon_name)
            .pixel_size(20)
            .valign(gtk::Align::Center)
            .build(),
    );
    inner.append(
        &gtk::Label::builder()
            .label(label)
            .hexpand(true)
            .xalign(0.0)
            .build(),
    );
    let btn = gtk::Button::builder()
        .child(&inner)
        .css_classes(["flat"])
        .build();
    btn.connect_clicked(move |_| on_click());
    btn
}

impl Component for LibraryMenu {
    fn get_root_widget(&self) -> &gtk::Widget {
        self.host.upcast_ref()
    }
}

impl EventListener for LibraryMenu {
    fn on_event(&mut self, event: &AppEvent) {
        if let AppEvent::LibraryItemMenuShown(item) = event {
            *self.current.borrow_mut() = Some(item.clone());
            self.build_for(item);
            set_sheet_open(&self.sheet, true);
        }
    }
}
