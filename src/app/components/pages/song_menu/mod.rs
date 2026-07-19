use gettextrs::gettext;
use gtk::prelude::*;
use libadwaita::prelude::BinExt;
use std::cell::RefCell;
use std::rc::Rc;

use crate::app::components::{Component, EventListener};
use crate::app::loader::ImageLoader;
use crate::app::models::{ImageSet, SongDescription};
use crate::app::state::{BrowserAction, BrowserEvent, PlaybackAction};
use crate::app::{ActionDispatcher, AppAction, AppEvent, AppModel, Worker};

fn set_sheet_open(sheet: &gtk::Widget, open: bool) {
    sheet.set_property("open", open);
}

pub struct SongMenuModel {
    app_model: Rc<AppModel>,
    dispatcher: Box<dyn ActionDispatcher>,
}

impl SongMenuModel {
    pub fn new(app_model: Rc<AppModel>, dispatcher: Box<dyn ActionDispatcher>) -> Self {
        Self {
            app_model,
            dispatcher,
        }
    }

    fn is_song_liked(&self, id: &str) -> bool {
        self.app_model
            .get_state()
            .browser
            .home_state()
            .map(|h| h.saved_tracks.get(id).is_some())
            .unwrap_or(false)
    }

    fn save_track(&self, song: SongDescription) {
        let api = self.app_model.get_spotify();
        let id = song.id.clone();
        self.dispatcher
            .call_spotify_and_dispatch(move || async move {
                api.save_tracks(vec![id])
                    .await
                    .map(|_| AppAction::BrowserAction(BrowserAction::SaveTracks(vec![song])))
            });
    }

    fn remove_track(&self, id: String) {
        let api = self.app_model.get_spotify();
        self.dispatcher
            .call_spotify_and_dispatch(move || async move {
                api.remove_saved_tracks(vec![id.clone()])
                    .await
                    .map(|_| AppAction::BrowserAction(BrowserAction::RemoveSavedTracks(vec![id])))
            });
    }
}

pub struct SongMenu {
    model: Rc<SongMenuModel>,
    worker: Worker,
    sheet: gtk::Widget,
    host: libadwaita::Bin,
    add_to_playlist_sheet: gtk::Widget,
    queue_sheet: gtk::Widget,
    now_playing_sheet: gtk::Widget,
    current_song: RefCell<Option<SongDescription>>,
}

impl SongMenu {
    pub fn new(
        model: SongMenuModel,
        host: libadwaita::Bin,
        sheet: gtk::Widget,
        add_to_playlist_sheet: gtk::Widget,
        queue_sheet: gtk::Widget,
        now_playing_sheet: gtk::Widget,
        worker: Worker,
    ) -> Self {
        Self {
            model: Rc::new(model),
            worker,
            sheet,
            host,
            add_to_playlist_sheet,
            queue_sheet,
            now_playing_sheet,
            current_song: RefCell::new(None),
        }
    }

    fn build_for(&self, song: &SongDescription) {
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();

        // Song card at the top
        let card = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .margin_top(28)
            .margin_bottom(12)
            .margin_start(16)
            .margin_end(16)
            .build();
        card.append(&art_thumbnail(song.art.as_ref(), &self.worker));
        card.append(&song_text_box(&song.title, &song.artists_name()));
        root.append(&card);

        root.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

        let actions_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .margin_bottom(16)
            .build();

        // Add to playlist
        let sheet = self.sheet.clone();
        let add_to_playlist_sheet = self.add_to_playlist_sheet.clone();
        let model_atp = self.model.clone();
        let song_atp = song.clone();
        actions_box.append(&action_row(
            "playlist-symbolic",
            &gettext("Add to playlist"),
            move || {
                set_sheet_open(&sheet, false);
                model_atp
                    .dispatcher
                    .dispatch(AppAction::ShowAddToPlaylist(song_atp.clone()));
                set_sheet_open(&add_to_playlist_sheet, true);
            },
        ));

        // Add to queue
        let sheet = self.sheet.clone();
        let model_q = self.model.clone();
        let song_q = song.clone();
        actions_box.append(&action_row(
            "media-playlist-consecutive-symbolic",
            &gettext("Add to queue"),
            move || {
                set_sheet_open(&sheet, false);
                model_q
                    .dispatcher
                    .dispatch(PlaybackAction::Queue(vec![song_q.clone()]).into());
            },
        ));

        // View album
        let album_id = song.album.id.clone();
        let model_va = self.model.clone();
        let close_va = self.close_all_sheets_fn();
        actions_box.append(&action_row(
            "media-optical-symbolic",
            &gettext("View album"),
            move || {
                close_va();
                model_va
                    .dispatcher
                    .dispatch(AppAction::ViewAlbum(album_id.clone()));
            },
        ));

        // View artist — one row per artist
        for artist in &song.artists {
            let label = if song.artists.len() == 1 {
                gettext("View artist")
            } else {
                // translators: %s is an artist name
                format!("View {}", artist.name)
            };
            let artist_id = artist.id.clone();
            let model_vr = self.model.clone();
            let close_vr = self.close_all_sheets_fn();
            actions_box.append(&action_row("system-users-symbolic", &label, move || {
                close_vr();
                model_vr
                    .dispatcher
                    .dispatch(AppAction::ViewArtist(artist_id.clone()));
            }));
        }

        // Copy link
        let track_id = song.id.clone();
        let sheet_cl = self.sheet.clone();
        actions_box.append(&action_row(
            "edit-copy-symbolic",
            &gettext("Copy link"),
            move || {
                set_sheet_open(&sheet_cl, false);
                let link = format!("https://open.spotify.com/track/{track_id}");
                let clipboard = gdk::Display::default().unwrap().clipboard();
                clipboard
                    .set_content(Some(&gdk::ContentProvider::for_value(&link.to_value())))
                    .expect("failed to set clipboard");
            },
        ));

        // Like / Unlike toggle
        let is_liked = self.model.is_song_liked(&song.id);
        let (like_icon, like_label) = if is_liked {
            ("starred-symbolic", gettext("Remove from Liked Songs"))
        } else {
            ("non-starred-symbolic", gettext("Add to Liked Songs"))
        };
        let model_lk = self.model.clone();
        let song_lk = song.clone();
        let sheet_lk = self.sheet.clone();
        actions_box.append(&action_row(like_icon, &like_label, move || {
            set_sheet_open(&sheet_lk, false);
            if is_liked {
                model_lk.remove_track(song_lk.id.clone());
            } else {
                model_lk.save_track(song_lk.clone());
            }
        }));

        root.append(&actions_box);
        self.host.set_child(Some(&root));
    }

    fn close_all_sheets_fn(&self) -> impl Fn() {
        let sheet = self.sheet.clone();
        let queue_sheet = self.queue_sheet.clone();
        let now_playing_sheet = self.now_playing_sheet.clone();
        move || {
            set_sheet_open(&sheet, false);
            set_sheet_open(&queue_sheet, false);
            set_sheet_open(&now_playing_sheet, false);
        }
    }
}

fn art_thumbnail(art: Option<&ImageSet>, worker: &Worker) -> gtk::Image {
    let image = gtk::Image::builder()
        .pixel_size(48)
        .valign(gtk::Align::Center)
        .build();
    if let Some(url) = art.and_then(|s| s.best_for_width(48)).map(str::to_owned) {
        let weak = image.downgrade();
        worker.send_local_task(async move {
            if let Some(img) = weak.upgrade() {
                let loader = ImageLoader::new();
                if let Some(pixbuf) = loader.load_remote(&url, "jpg", 48, 48).await {
                    let texture = gdk::Texture::for_pixbuf(&pixbuf);
                    img.set_paintable(Some(&texture));
                }
            }
        });
    }
    image
}

fn song_text_box(title: &str, artist: &str) -> gtk::Box {
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
            .label(artist)
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

impl Component for SongMenu {
    fn get_root_widget(&self) -> &gtk::Widget {
        self.host.upcast_ref()
    }
}

impl EventListener for SongMenu {
    fn on_event(&mut self, event: &AppEvent) {
        match event {
            AppEvent::SongMenuShown(song) => {
                *self.current_song.borrow_mut() = Some(song.clone());
                self.build_for(song);
                set_sheet_open(&self.sheet, true);
            }
            AppEvent::BrowserEvent(BrowserEvent::SavedTracksUpdated) => {
                if let Some(song) = self.current_song.borrow().clone() {
                    if self.sheet.property::<bool>("open") {
                        self.build_for(&song);
                    }
                }
            }
            _ => {}
        }
    }
}
