use gettextrs::gettext;
use gtk::prelude::*;
use libadwaita::prelude::BinExt;
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;

use crate::app::components::{Component, EventListener};
use crate::app::loader::ImageLoader;
use crate::app::models::{ImageSet, PlaylistDescription, SongDescription};
use crate::app::state::{BrowserEvent, LoginEvent};
use crate::app::{ActionDispatcher, AppAction, AppEvent, AppModel, Worker};

fn set_sheet_open(sheet: &gtk::Widget, open: bool) {
    sheet.set_property("open", open);
}

// Which order the playlist list is displayed in.
#[derive(Clone, Copy, PartialEq, Default)]
enum SortMode {
    #[default]
    Alphabetical,
    FetchOrder,
}

pub struct AddToPlaylistModel {
    app_model: Rc<AppModel>,
    dispatcher: Box<dyn ActionDispatcher>,
}

impl AddToPlaylistModel {
    pub fn new(app_model: Rc<AppModel>, dispatcher: Box<dyn ActionDispatcher>) -> Self {
        Self {
            app_model,
            dispatcher,
        }
    }

    fn user_playlists(&self) -> Vec<PlaylistDescription> {
        let state = self.app_model.get_state();
        let Some(home) = state.browser.home_state() else {
            return vec![];
        };
        let browser = &state.browser;
        home.playlists
            .iter()
            .map(|card| {
                let id = card.id();
                // Use the full PlaylistDescription if it's already in the browser
                // (gives us songs for "Saved in" detection and higher-res art).
                if let Some(pds) = browser.playlist_details_state(&id) {
                    if let Some(pl) = pds.playlist.as_ref() {
                        return pl.clone();
                    }
                }
                PlaylistDescription {
                    id,
                    title: card.title(),
                    art: card.image().and_then(|url| {
                        crate::app::models::ImageSet::from_images(std::iter::once((
                            Some(300u32),
                            url,
                        )))
                    }),
                    songs: crate::app::models::SongBatch::empty(),
                    owner: crate::app::models::UserRef {
                        id: String::new(),
                        display_name: String::new(),
                    },
                }
            })
            .collect()
    }

    fn user_id(&self) -> Option<String> {
        self.app_model.get_state().logged_user.user.clone()
    }

    fn create_new_playlist(&self, name: String) {
        let Some(user_id) = self.user_id() else {
            return;
        };
        let api = self.app_model.get_spotify();
        self.dispatcher
            .call_spotify_and_dispatch(move || async move {
                api.create_new_playlist(name.as_str(), user_id.as_str())
                    .await
                    .map(AppAction::CreatePlaylist)
            });
    }

    fn add_to_playlist(&self, playlist_id: String, uri: String) {
        let api = self.app_model.get_spotify();
        self.dispatcher
            .call_spotify_and_dispatch_many(move || async move {
                api.add_to_playlist(&playlist_id, vec![uri]).await?;
                Ok(vec![AppAction::ShowNotification(gettext(
                    "Added to playlist",
                ))])
            });
    }

    fn remove_from_playlist(&self, playlist_id: String, uri: String) {
        let api = self.app_model.get_spotify();
        self.dispatcher
            .call_spotify_and_dispatch_many(move || async move {
                api.remove_from_playlist(&playlist_id, vec![uri]).await?;
                Ok(vec![])
            });
    }
}

// ─────────────────────────────────────────────────────────────────────────────

pub struct AddToPlaylist {
    model: Rc<AddToPlaylistModel>,
    worker: Worker,
    sheet: gtk::Widget,
    host: libadwaita::Bin,
    current_song: RefCell<Option<SongDescription>>,
    /// Pending adds: playlist ids the user has checked during this session.
    /// Persists across build_for() rebuilds (e.g. after UserPlaylistsLoaded).
    /// Cleared only when opening the drawer for a new song.
    staged_adds: Rc<RefCell<HashSet<String>>>,
    /// Pending removes: playlist ids the user has unchecked during this session.
    /// Same lifetime as staged_adds.
    staged_removes: Rc<RefCell<HashSet<String>>>,
    /// All playlist ids seen at the last build_for() call. Used to detect
    /// newly-created playlists on UserPlaylistsLoaded so they can be
    /// auto-staged as adds (Spotify creates and immediately adds the song).
    known_playlist_ids: RefCell<HashSet<String>>,
}

impl AddToPlaylist {
    pub fn new(
        model: AddToPlaylistModel,
        host: libadwaita::Bin,
        sheet: gtk::Widget,
        worker: Worker,
    ) -> Self {
        Self {
            model: Rc::new(model),
            worker,
            sheet,
            host,
            current_song: RefCell::new(None),
            staged_adds: Rc::new(RefCell::new(HashSet::new())),
            staged_removes: Rc::new(RefCell::new(HashSet::new())),
            known_playlist_ids: RefCell::new(HashSet::new()),
        }
    }

    fn build_for(&self, song: &SongDescription) {
        let model = &self.model;
        let worker = &self.worker;
        let sheet = self.sheet.clone();

        // ── Staging (shared, persists across rebuilds) ────────────────────────
        // staged_adds/staged_removes live on self; we clone the Rc handles so
        // closures below keep them alive. They are NOT reset here — clearing
        // happens only in on_event when a new song is targeted.
        let staged_adds = self.staged_adds.clone();
        let staged_removes = self.staged_removes.clone();

        // ── Initial "contains this song" set ─────────────────────────────────
        // Best-effort: only playlists whose tracks are already loaded in the browser.
        // No fetch is performed.
        let song_id = song.id.clone();
        let song_uri = song.uri.clone();
        let playlists = model.user_playlists();

        // Update known_playlist_ids and auto-stage any brand-new playlists.
        {
            let current_ids: HashSet<String> = playlists.iter().map(|p| p.id.clone()).collect();
            let mut known = self.known_playlist_ids.borrow_mut();
            // Any id present now but absent before is a newly-created playlist.
            // Auto-stage it as an add so it appears checked and Save adds the song.
            for new_id in current_ids.difference(&*known) {
                staged_adds.borrow_mut().insert(new_id.clone());
            }
            *known = current_ids;
        }

        let initially_contains: HashSet<String> = playlists
            .iter()
            .filter(|p| p.songs.songs.iter().any(|s| s.id == song_id))
            .map(|p| p.id.clone())
            .collect();

        // ── Sort state ───────────────────────────────────────────────────────
        let sort_mode: Rc<Cell<SortMode>> = Rc::new(Cell::new(SortMode::Alphabetical));

        // ── Root layout ──────────────────────────────────────────────────────
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();

        // Header bar
        let header_bar = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(0)
            .margin_top(20)
            .margin_start(16)
            .margin_end(16)
            .margin_bottom(8)
            .build();

        let cancel_btn = gtk::Button::builder()
            .label(gettext("Cancel"))
            .css_classes(["flat"])
            .build();
        let title_label = gtk::Label::builder()
            .label(gettext("Add to playlist"))
            .hexpand(true)
            .halign(gtk::Align::Center)
            .css_classes(["title-4"])
            .build();
        let save_btn = gtk::Button::builder()
            .label(gettext("Save"))
            .css_classes(["suggested-action"])
            .build();

        header_bar.append(&cancel_btn);
        header_bar.append(&title_label);
        header_bar.append(&save_btn);
        root.append(&header_bar);

        // Track card
        let track_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .margin_top(8)
            .margin_bottom(8)
            .margin_start(16)
            .margin_end(16)
            .build();
        track_row.append(&art_thumbnail(song.art.as_ref(), worker));
        track_row.append(&song_text_box(&song.title, &song.artists_name()));
        root.append(&track_row);
        root.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

        // Search + sort toolbar
        let toolbar = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .margin_top(8)
            .margin_start(12)
            .margin_end(12)
            .margin_bottom(4)
            .build();
        let search_entry = gtk::SearchEntry::builder()
            .placeholder_text(gettext("Find a playlist"))
            .hexpand(true)
            .build();
        // Sort: Alphabetical / Recently updated / Recently added.
        // "Recently updated" and "Recently added" both use API fetch order because
        // Spotify's playlists list endpoint returns no per-playlist timestamps.
        let sort_strings = gtk::StringList::new(&[
            &gettext("Alphabetical"),
            &gettext("Recently updated"),
            &gettext("Recently added"),
        ]);
        let sort_drop = gtk::DropDown::builder()
            .model(&sort_strings)
            .selected(0)
            .build();
        toolbar.append(&search_entry);
        toolbar.append(&sort_drop);
        root.append(&toolbar);

        // Scrollable list
        let list_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .margin_start(12)
            .margin_end(12)
            .margin_top(4)
            .margin_bottom(24)
            .build();
        let clamp = libadwaita::Clamp::builder()
            .maximum_size(600)
            .child(&list_box)
            .build();
        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&clamp)
            .build();
        root.append(&scroll);

        // ── "New playlist" row ────────────────────────────────────────────────
        let new_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .margin_top(4)
            .margin_bottom(4)
            .build();
        let new_icon = gtk::Image::builder()
            .icon_name("list-add-symbolic")
            .pixel_size(32)
            .margin_start(8)
            .margin_end(8)
            .build();
        let new_label = gtk::Label::builder()
            .label(gettext("New playlist"))
            .hexpand(true)
            .xalign(0.0)
            .build();
        let name_entry = gtk::Entry::builder()
            .placeholder_text(gettext("Playlist name"))
            .hexpand(true)
            .visible(false)
            .build();
        let confirm_btn = gtk::Button::builder()
            .icon_name("object-select-symbolic")
            .css_classes(["flat", "circular"])
            .valign(gtk::Align::Center)
            .visible(false)
            .build();
        new_row.append(&new_icon);
        new_row.append(&new_label);
        new_row.append(&name_entry);
        new_row.append(&confirm_btn);

        let new_btn = gtk::Button::builder()
            .child(&new_row)
            .css_classes(["flat"])
            .build();

        // Toggle inline entry on tap
        let entry_ref = name_entry.clone();
        let confirm_ref2 = confirm_btn.clone();
        let label_ref = new_label.clone();
        new_btn.connect_clicked(move |_| {
            let showing = gtk::prelude::WidgetExt::is_visible(&entry_ref);
            entry_ref.set_visible(!showing);
            confirm_ref2.set_visible(!showing);
            label_ref.set_visible(showing);
            if !showing {
                entry_ref.grab_focus();
            }
        });

        list_box.append(&new_btn);
        list_box.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

        // ── "Saved in" section (best-effort, loaded data only) ────────────────
        // TODO: If track membership were known without pre-loading playlist tracks,
        // this section could be populated for all playlists. Currently limited to
        // those whose track list is already in the browser's playlist_details_state.
        if !initially_contains.is_empty() {
            let saved_label = gtk::Label::builder()
                .label(gettext("Saved in"))
                .xalign(0.0)
                .margin_top(8)
                .margin_bottom(4)
                .css_classes(["heading"])
                .build();
            list_box.append(&saved_label);
            for pl in playlists
                .iter()
                .filter(|p| initially_contains.contains(&p.id))
            {
                let row = playlist_row(
                    pl,
                    true,
                    worker,
                    staged_adds.clone(),
                    staged_removes.clone(),
                    &initially_contains,
                );
                list_box.append(&row);
            }
            let sep = gtk::Separator::new(gtk::Orientation::Horizontal);
            sep.set_margin_top(8);
            sep.set_margin_bottom(4);
            list_box.append(&sep);
        }

        // ── Filtered + sorted playlist list ───────────────────────────────────
        let all_playlists: Rc<Vec<PlaylistDescription>> = Rc::new(playlists.clone());
        let initially_contains_rc = Rc::new(initially_contains.clone());
        let list_container = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .build();
        list_box.append(&list_container);

        let rebuild_list = {
            let all_playlists = all_playlists.clone();
            let initially_contains_rc = initially_contains_rc.clone();
            let staged_adds = staged_adds.clone();
            let staged_removes = staged_removes.clone();
            let sort_mode = sort_mode.clone();
            let list_container = list_container.clone();
            let worker = worker.clone();
            let search_entry = search_entry.clone();
            move || {
                while let Some(child) = list_container.first_child() {
                    list_container.remove(&child);
                }
                let filter = search_entry.text().to_lowercase();
                let mut visible: Vec<&PlaylistDescription> = all_playlists
                    .iter()
                    .filter(|p| !initially_contains_rc.contains(&p.id))
                    .filter(|p| filter.is_empty() || p.title.to_lowercase().contains(&filter))
                    .collect();
                if sort_mode.get() == SortMode::Alphabetical {
                    visible.sort_by(|a, b| a.title.cmp(&b.title));
                }
                // FetchOrder: retain the API's natural order (no timestamp metadata).
                if visible.is_empty() {
                    list_container.append(
                        &libadwaita::StatusPage::builder()
                            .title(gettext("No playlists found"))
                            .vexpand(true)
                            .build(),
                    );
                    return;
                }
                for pl in visible {
                    let checked = staged_adds.borrow().contains(&pl.id);
                    let row = playlist_row(
                        pl,
                        checked,
                        &worker,
                        staged_adds.clone(),
                        staged_removes.clone(),
                        &initially_contains_rc,
                    );
                    list_container.append(&row);
                }
            }
        };
        rebuild_list();
        let rebuild_list = Rc::new(rebuild_list);

        // Search filter
        search_entry.connect_search_changed(clone!(
            #[strong]
            rebuild_list,
            move |_| rebuild_list()
        ));

        // Sort control
        sort_drop.connect_selected_notify(clone!(
            #[strong]
            sort_mode,
            #[strong]
            rebuild_list,
            move |drop| {
                sort_mode.set(if drop.selected() == 0 {
                    SortMode::Alphabetical
                } else {
                    SortMode::FetchOrder
                });
                rebuild_list();
            }
        ));

        // ── New playlist confirm ──────────────────────────────────────────────
        let model_np = model.clone();
        let entry_for_confirm = name_entry.clone();
        let confirm_for_cb = confirm_btn.clone();
        let label_for_confirm = new_label.clone();
        let do_confirm = move || {
            let name = entry_for_confirm.text().trim().to_string();
            if name.is_empty() {
                return;
            }
            entry_for_confirm.set_text("");
            entry_for_confirm.set_visible(false);
            confirm_for_cb.set_visible(false);
            label_for_confirm.set_visible(true);
            model_np.create_new_playlist(name);
            // The new playlist will appear in the list when UserPlaylistsLoaded
            // fires — the on_event handler rebuilds the drawer at that point,
            // and the new playlist id will be auto-staged as an add.
        };
        let do_confirm = Rc::new(do_confirm);
        confirm_btn.connect_clicked(clone!(
            #[strong]
            do_confirm,
            move |_| do_confirm()
        ));
        name_entry.connect_activate(clone!(
            #[strong]
            do_confirm,
            move |_| do_confirm()
        ));

        // ── Cancel ────────────────────────────────────────────────────────────
        cancel_btn.connect_clicked(clone!(
            #[weak]
            sheet,
            move |_| set_sheet_open(&sheet, false)
        ));

        // ── Save: apply the diff ──────────────────────────────────────────────
        // adds    = staged_adds  \ initially_contains
        // removes = staged_removes ∩ initially_contains
        let model_save = model.clone();
        let staged_adds_save = staged_adds.clone();
        let staged_removes_save = staged_removes.clone();
        let initially_contains_save = initially_contains.clone();
        let song_uri_save = song_uri.clone();
        save_btn.connect_clicked(clone!(
            #[weak]
            sheet,
            move |_| {
                set_sheet_open(&sheet, false);
                let adds: Vec<String> = staged_adds_save
                    .borrow()
                    .iter()
                    .filter(|id| !initially_contains_save.contains(*id))
                    .cloned()
                    .collect();
                let removes: Vec<String> = staged_removes_save
                    .borrow()
                    .iter()
                    .filter(|id| initially_contains_save.contains(*id))
                    .cloned()
                    .collect();
                for pid in adds {
                    model_save.add_to_playlist(pid, song_uri_save.clone());
                }
                for pid in removes {
                    model_save.remove_from_playlist(pid, song_uri_save.clone());
                }
            }
        ));

        self.host.set_child(Some(&root));
    }
}

// ── Widget helpers ────────────────────────────────────────────────────────────

/// Load a 48×48 thumbnail from an optional `ImageSet`.  Used for both track
/// art and playlist art — the caller passes `desc.art.as_ref()`.
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

fn playlist_row(
    pl: &PlaylistDescription,
    checked: bool,
    worker: &Worker,
    staged_adds: Rc<RefCell<HashSet<String>>>,
    staged_removes: Rc<RefCell<HashSet<String>>>,
    initially_contains: &HashSet<String>,
) -> gtk::Box {
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .margin_top(4)
        .margin_bottom(4)
        .build();
    row.append(&art_thumbnail(pl.art.as_ref(), worker));
    row.append(
        &gtk::Label::builder()
            .label(&pl.title)
            .hexpand(true)
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build(),
    );
    let check = gtk::CheckButton::builder()
        .active(checked)
        .valign(gtk::Align::Center)
        .build();
    let id = pl.id.clone();
    let was_in_initial = initially_contains.contains(&pl.id);
    check.connect_toggled(move |btn| {
        let now = btn.is_active();
        if now {
            staged_removes.borrow_mut().remove(&id);
            if !was_in_initial {
                staged_adds.borrow_mut().insert(id.clone());
            }
        } else {
            staged_adds.borrow_mut().remove(&id);
            if was_in_initial {
                staged_removes.borrow_mut().insert(id.clone());
            }
        }
    });
    row.append(&check);
    row
}

// ── Component + EventListener ─────────────────────────────────────────────────

impl Component for AddToPlaylist {
    fn get_root_widget(&self) -> &gtk::Widget {
        self.host.upcast_ref()
    }
}

impl EventListener for AddToPlaylist {
    fn on_event(&mut self, event: &AppEvent) {
        match event {
            AppEvent::AddToPlaylistShown(song) => {
                // Opening for a new song: reset all staged state and known ids
                // so this session starts clean.
                self.staged_adds.borrow_mut().clear();
                self.staged_removes.borrow_mut().clear();
                self.known_playlist_ids.borrow_mut().clear();
                *self.current_song.borrow_mut() = Some(song.clone());
                self.build_for(song);
                set_sheet_open(&self.sheet, true);
            }
            // Rebuild the list when the user's playlists change (e.g. a new
            // playlist was just created and appeared via CreatePlaylist dispatch).
            // staged_adds/staged_removes are NOT cleared here — prior selections
            // must survive the rebuild, and newly-created playlists are
            // auto-staged inside build_for via known_playlist_ids diffing.
            AppEvent::LoginEvent(LoginEvent::UserPlaylistsLoaded)
            | AppEvent::BrowserEvent(BrowserEvent::SavedPlaylistsUpdated) => {
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
