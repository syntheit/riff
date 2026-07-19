use gettextrs::gettext;
use gtk::prelude::*;
use libadwaita::prelude::BinExt;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use crate::api::SpotifyApiClient;
use crate::app::components::{Component, EventListener};
use crate::app::loader::ImageLoader;
use crate::app::models::{ImageSet, PlaylistDescription, SongDescription};
use crate::app::state::{BrowserEvent, LoginEvent};
use crate::app::{ActionDispatcher, AppAction, AppEvent, AppModel, Worker};

fn set_sheet_open(sheet: &gtk::Widget, open: bool) {
    sheet.set_property("open", open);
}

// Which order the playlist list is displayed in.
// Spotify's /me/playlists returns no per-playlist timestamps, so only
// Alphabetical and the raw API order (which approximates recent activity) are honest.
#[derive(Clone, Copy, PartialEq, Default)]
enum SortMode {
    #[default]
    Alphabetical,
    Default,
}

// Per-playlist membership index entry. Keyed by playlist id.
struct PlaylistIndexEntry {
    snapshot_id: String,
    track_ids: HashSet<String>,
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
                    snapshot_id: card.snapshot_id(),
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
    /// Pending adds: playlist ids checked during this session.
    /// Persists across build_for() rebuilds. Cleared only for a new song.
    staged_adds: Rc<RefCell<HashSet<String>>>,
    /// Pending removes: playlist ids unchecked during this session.
    staged_removes: Rc<RefCell<HashSet<String>>>,
    /// Playlist ids seen at the last build_for() call. Used to detect
    /// newly-created playlists (via UserPlaylistsLoaded) for auto-staging.
    known_playlist_ids: RefCell<HashSet<String>>,
    /// Snapshot-id-keyed track membership index. Persists across songs so
    /// unchanged playlists are never re-fetched within an app session.
    membership_index: Rc<RefCell<HashMap<String, PlaylistIndexEntry>>>,
    /// Monotonically increasing counter bumped each time the drawer opens for
    /// a new song. Background refresh tasks carry the token they were launched
    /// with; if it no longer matches, the result is discarded.
    session_token: Rc<Cell<u64>>,
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
            membership_index: Rc::new(RefCell::new(HashMap::new())),
            session_token: Rc::new(Cell::new(0)),
        }
    }

    fn build_for(&self, song: &SongDescription) {
        let playlists = self.model.user_playlists();

        // Detect newly-created playlists and auto-stage them as adds.
        // On the very first build_for for a song, known_playlist_ids has already
        // been seeded with the current ids (done in on_event before calling here),
        // so nothing is treated as "new" until the user actually creates one.
        {
            let current_ids: HashSet<String> = playlists.iter().map(|p| p.id.clone()).collect();
            let mut known = self.known_playlist_ids.borrow_mut();
            for new_id in current_ids.difference(&*known) {
                self.staged_adds.borrow_mut().insert(new_id.clone());
            }
            *known = current_ids;
        }

        // Membership comes from the snapshot-id-cached index. Playlists not yet
        // indexed show as not-containing the song until the background refresh
        // completes and triggers a rebuild.
        let initially_contains: HashSet<String> = {
            let index = self.membership_index.borrow();
            playlists
                .iter()
                .filter(|p| {
                    index
                        .get(&p.id)
                        .is_some_and(|e| e.track_ids.contains(&song.id))
                })
                .map(|p| p.id.clone())
                .collect()
        };

        build_drawer_ui(
            song,
            &playlists,
            initially_contains,
            &self.sheet,
            &self.host,
            &self.model,
            &self.worker,
            &self.staged_adds,
            &self.staged_removes,
        );
    }

    /// Kick off a background refresh of the membership index.
    /// For each playlist, re-fetches track ids only when snapshot_id changed
    /// (or the playlist has no entry yet). Unchanged playlists are free.
    /// Discards the result if the session token changed (drawer re-targeted).
    /// Does NOT touch staged_adds/staged_removes.
    fn refresh_membership_index(
        &self,
        song: SongDescription,
        playlists: Vec<PlaylistDescription>,
        api: Arc<dyn SpotifyApiClient + Send + Sync>,
    ) {
        let index_rc = self.membership_index.clone();
        let session_token = self.session_token.clone();
        let my_token = session_token.get();
        let sheet = self.sheet.clone();

        // host is the Bin that contains the drawer root built by build_for.
        // After the refresh we call build_drawer_ui again; to do that without a
        // self reference in the async block we store the inputs we need.
        let model_rc = self.model.clone();
        let worker = self.worker.clone();
        let host = self.host.clone();
        let staged_adds = self.staged_adds.clone();
        let staged_removes = self.staged_removes.clone();

        // Collect which playlists need re-fetching before entering the async block.
        let to_fetch: Vec<(String, String)> = {
            let index = index_rc.borrow();
            playlists
                .iter()
                .filter_map(|p| {
                    let snap = p.snapshot_id.as_deref()?;
                    let needs_fetch = index.get(&p.id).is_none_or(|e| e.snapshot_id != snap);
                    if needs_fetch {
                        Some((p.id.clone(), snap.to_owned()))
                    } else {
                        None
                    }
                })
                .collect()
        };

        if to_fetch.is_empty() {
            return;
        }

        self.worker.send_local_task(async move {
            for (playlist_id, snapshot_id) in to_fetch {
                // If the sheet was closed or the user moved to a different song,
                // discard remaining fetches for this session.
                if session_token.get() != my_token || !sheet.property::<bool>("open") {
                    return;
                }

                let Ok(ids) = api.get_playlist_track_ids(&playlist_id).await else {
                    continue;
                };

                index_rc.borrow_mut().insert(
                    playlist_id,
                    PlaylistIndexEntry {
                        snapshot_id,
                        track_ids: ids.into_iter().collect(),
                    },
                );
            }

            // Session still valid — rebuild the drawer so membership checkmarks
            // and the "Saved in" section reflect the freshly indexed data.
            // staged_adds/staged_removes live on their Rc handles and are untouched.
            if session_token.get() != my_token || !sheet.property::<bool>("open") {
                return;
            }

            // Rebuild the playlist list and compute membership from the updated index.
            let playlists_now = model_rc.user_playlists();
            let initially_contains: HashSet<String> = {
                let index = index_rc.borrow();
                let song_id = &song.id;
                playlists_now
                    .iter()
                    .filter(|p| {
                        index
                            .get(&p.id)
                            .is_some_and(|e| e.track_ids.contains(song_id))
                    })
                    .map(|p| p.id.clone())
                    .collect()
            };

            build_drawer_ui(
                &song,
                &playlists_now,
                initially_contains,
                &sheet,
                &host,
                &model_rc,
                &worker,
                &staged_adds,
                &staged_removes,
            );
        });
    }
}

// Shared drawer layout. Called by both build_for (via self) and the async
// refresh path. All state that differs between the two call-sites is passed
// as explicit arguments so there is exactly one copy of the widget tree.
#[allow(clippy::too_many_arguments)]
fn build_drawer_ui(
    song: &SongDescription,
    playlists: &[PlaylistDescription],
    initially_contains: HashSet<String>,
    sheet: &gtk::Widget,
    host: &libadwaita::Bin,
    model: &Rc<AddToPlaylistModel>,
    worker: &Worker,
    staged_adds: &Rc<RefCell<HashSet<String>>>,
    staged_removes: &Rc<RefCell<HashSet<String>>>,
) {
    let song_uri = song.uri.clone();
    let sort_mode: Rc<Cell<SortMode>> = Rc::new(Cell::new(SortMode::Alphabetical));

    // ── Root layout ──────────────────────────────────────────────────────────
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
    // Two options: Alphabetical or Default (API fetch order, which
    // approximates Spotify's recent-activity ordering).
    let sort_strings = gtk::StringList::new(&[&gettext("Alphabetical"), &gettext("Default")]);
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

    // ── "New playlist" row ────────────────────────────────────────────────────
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

    // ── "Saved in" section ────────────────────────────────────────────────────
    // Populated from the membership index; shows real membership once the
    // background refresh finishes and the next build is triggered.
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

    // ── Filtered + sorted playlist list ───────────────────────────────────────
    let all_playlists: Rc<Vec<PlaylistDescription>> = Rc::new(playlists.to_vec());
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
            // Default: retain the API's natural order (recent-activity proxy).
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
                SortMode::Default
            });
            rebuild_list();
        }
    ));

    // ── New playlist confirm ──────────────────────────────────────────────────
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
        // The new playlist appears via UserPlaylistsLoaded; on_event rebuilds
        // the drawer and auto-stages the new id via known_playlist_ids diffing.
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

    // ── Cancel ────────────────────────────────────────────────────────────────
    let sheet_clone = sheet.clone();
    cancel_btn.connect_clicked(move |_| set_sheet_open(&sheet_clone, false));

    // ── Save: apply the diff ──────────────────────────────────────────────────
    // adds    = staged_adds  \ initially_contains
    // removes = staged_removes ∩ initially_contains
    let model_save = model.clone();
    let staged_adds_save = staged_adds.clone();
    let staged_removes_save = staged_removes.clone();
    let initially_contains_save = initially_contains_rc.as_ref().clone();
    let song_uri_save = song_uri;
    let sheet_save = sheet.clone();
    save_btn.connect_clicked(move |_| {
        set_sheet_open(&sheet_save, false);
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
    });

    host.set_child(Some(&root));
}

// ── Widget helpers ────────────────────────────────────────────────────────────

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
                self.staged_adds.borrow_mut().clear();
                self.staged_removes.borrow_mut().clear();

                // Seed known_playlist_ids with the current set BEFORE build_for
                // so that on the first build nothing is treated as "new" and
                // auto-checked. Only playlists created during this session
                // (whose ids appear on subsequent UserPlaylistsLoaded events)
                // will be auto-staged.
                {
                    let playlists = self.model.user_playlists();
                    let current_ids: HashSet<String> =
                        playlists.iter().map(|p| p.id.clone()).collect();
                    *self.known_playlist_ids.borrow_mut() = current_ids;
                }

                // Bump the session token so any in-flight refresh for the
                // previous song discards its results.
                self.session_token.set(self.session_token.get() + 1);

                *self.current_song.borrow_mut() = Some(song.clone());
                self.build_for(song);

                let playlists = self.model.user_playlists();
                let api = self.model.app_model.get_spotify();
                self.refresh_membership_index(song.clone(), playlists, api);

                set_sheet_open(&self.sheet, true);
            }
            // Rebuild when playlists change (new playlist created, etc.).
            // staged_adds/staged_removes survive the rebuild; newly-created
            // playlists are auto-staged via known_playlist_ids diffing in build_for.
            AppEvent::LoginEvent(LoginEvent::UserPlaylistsLoaded)
            | AppEvent::BrowserEvent(BrowserEvent::SavedPlaylistsUpdated) => {
                if let Some(song) = self.current_song.borrow().clone() {
                    if self.sheet.property::<bool>("open") {
                        self.build_for(&song);
                        let playlists = self.model.user_playlists();
                        let api = self.model.app_model.get_spotify();
                        self.refresh_membership_index(song.clone(), playlists, api);
                    }
                }
            }
            _ => {}
        }
    }
}
