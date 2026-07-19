mod membership_cache;
use membership_cache::SharedMembershipCache;

use gettextrs::gettext;
use gtk::prelude::*;
use libadwaita::prelude::BinExt;
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;

use crate::api::SpotifyApiClient;
use crate::app::components::{Component, EventListener};
use crate::app::loader::ImageLoader;
use crate::app::models::{PlaylistDescription, SongDescription};
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

// Membership state of the target track in a given playlist row.
// Drives whether the checkbox may be shown and pre-checked.
#[derive(Clone, Copy, PartialEq)]
enum Membership {
    Unknown,
    In,
    NotIn,
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
}

// ─────────────────────────────────────────────────────────────────────────────

// GObject row model backing the virtualized ListView. One instance per playlist;
// only rows scrolled into view are ever bound to a widget, and art/membership are
// resolved lazily on bind. Properties are read/written directly by the factory.
mod row_model {
    use super::Membership;
    use glib::prelude::*;
    use glib::subclass::prelude::*;
    use glib::Properties;
    use std::cell::{Cell, RefCell};

    glib::wrapper! {
        pub struct PlaylistRowModel(ObjectSubclass<imp::PlaylistRowModel>);
    }

    impl PlaylistRowModel {
        pub fn new(id: &str, title: &str, art: Option<&str>, snapshot_id: Option<&str>) -> Self {
            glib::Object::builder()
                .property("id", id)
                .property("title", title)
                .property("art", art.map(str::to_owned))
                .property("snapshot-id", snapshot_id.map(str::to_owned))
                .build()
        }

        pub(super) fn membership(&self) -> Membership {
            match self.imp().membership.get() {
                1 => Membership::In,
                2 => Membership::NotIn,
                _ => Membership::Unknown,
            }
        }

        pub(super) fn set_membership(&self, membership: Membership) {
            self.imp().membership.set(match membership {
                Membership::Unknown => 0,
                Membership::In => 1,
                Membership::NotIn => 2,
            });
        }
    }

    mod imp {
        use super::*;

        #[derive(Default, Properties)]
        #[properties(wrapper_type = super::PlaylistRowModel)]
        pub struct PlaylistRowModel {
            #[property(get, set)]
            pub id: RefCell<String>,
            #[property(get, set)]
            pub title: RefCell<String>,
            #[property(get, set)]
            pub art: RefCell<Option<String>>,
            #[property(get, set, name = "snapshot-id")]
            pub snapshot_id: RefCell<Option<String>>,
            // 0 = Unknown, 1 = In, 2 = NotIn. Backed by a plain Cell rather than a
            // property because it is read/written imperatively, never bound.
            pub membership: Cell<i32>,
        }

        #[glib::object_subclass]
        impl ObjectSubclass for PlaylistRowModel {
            const NAME: &'static str = "AddToPlaylistRowModel";
            type Type = super::PlaylistRowModel;
            type ParentType = glib::Object;
        }

        #[glib::derived_properties]
        impl ObjectImpl for PlaylistRowModel {}
    }
}
use row_model::PlaylistRowModel;

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
    /// Snapshot-id-keyed track membership cache, persisted to disk. Shared with
    /// the lazy per-row fetch tasks and the Save diff.
    cache: SharedMembershipCache,
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
            cache: SharedMembershipCache::load(),
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

        build_drawer_ui(
            song,
            &playlists,
            &self.sheet,
            &self.host,
            &self.model,
            &self.worker,
            &self.staged_adds,
            &self.staged_removes,
            &self.cache,
        );
    }
}

// Shared drawer layout. All state that varies between calls is passed explicitly
// so there is exactly one copy of the widget tree.
#[allow(clippy::too_many_arguments)]
fn build_drawer_ui(
    song: &SongDescription,
    playlists: &[PlaylistDescription],
    sheet: &gtk::Widget,
    host: &libadwaita::Bin,
    model: &Rc<AddToPlaylistModel>,
    worker: &Worker,
    staged_adds: &Rc<RefCell<HashSet<String>>>,
    staged_removes: &Rc<RefCell<HashSet<String>>>,
    cache: &SharedMembershipCache,
) {
    let song_id = song.id.clone();
    let song_uri = song.uri.clone();
    let sort_mode: Rc<Cell<SortMode>> = Rc::new(Cell::new(SortMode::Alphabetical));
    let api = model.app_model.get_spotify();

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
    track_row.append(&art_thumbnail(
        song.art.as_ref().and_then(|s| s.best_for_width(48)),
        worker,
    ));
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

    // ── "New playlist" row (sits above the virtualized list) ──────────────────
    let new_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .margin_top(4)
        .margin_bottom(4)
        .margin_start(12)
        .margin_end(12)
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

    let new_btn_wrap = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .margin_start(12)
        .margin_end(12)
        .build();
    new_btn_wrap.append(&new_btn);
    new_btn_wrap.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    root.append(&new_btn_wrap);

    // ── Virtualized playlist list ─────────────────────────────────────────────
    // Backing store holds one lightweight GObject per playlist. A FilterListModel
    // applies the live search text; a SortListModel applies the sort order. The
    // ListView only realizes rows in view, so art and membership are resolved
    // lazily, per visible row, in the factory's bind callback.
    let store = gio::ListStore::new::<PlaylistRowModel>();
    for pl in playlists {
        store.append(&PlaylistRowModel::new(
            &pl.id,
            &pl.title,
            pl.art.as_ref().and_then(|s| s.best_for_width(48)),
            pl.snapshot_id.as_deref(),
        ));
    }

    let filter = gtk::CustomFilter::new(|_| true);
    let filter_model = gtk::FilterListModel::new(Some(store.clone()), Some(filter.clone()));

    let sorter = gtk::CustomSorter::new(clone!(
        #[strong]
        sort_mode,
        move |a, b| {
            if sort_mode.get() == SortMode::Default {
                // Keep the store's natural (API fetch) order.
                return gtk::Ordering::Equal;
            }
            let a = a.downcast_ref::<PlaylistRowModel>().unwrap().title();
            let b = b.downcast_ref::<PlaylistRowModel>().unwrap().title();
            a.to_lowercase().cmp(&b.to_lowercase()).into()
        }
    ));
    let sort_model = gtk::SortListModel::new(Some(filter_model), Some(sorter.clone()));

    let selection = gtk::NoSelection::new(Some(sort_model));

    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        item.set_child(Some(&PlaylistRow::new()));
    });

    factory.connect_bind(clone!(
        #[strong]
        worker,
        #[strong]
        cache,
        #[strong]
        staged_adds,
        #[strong]
        staged_removes,
        #[strong]
        api,
        #[strong]
        song_id,
        move |_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().unwrap();
            let row_model = item.item().unwrap().downcast::<PlaylistRowModel>().unwrap();
            let row = item.child().unwrap().downcast::<PlaylistRow>().unwrap();

            let id = row_model.id();
            let snapshot = row_model.snapshot_id();

            // Resolve membership from the cache if we have a snapshot-matching
            // entry; otherwise kick off a lazy fetch for just this playlist.
            match cache.borrow().contains(&id, snapshot.as_deref(), &song_id) {
                Some(true) => row_model.set_membership(Membership::In),
                Some(false) => row_model.set_membership(Membership::NotIn),
                None => row_model.set_membership(Membership::Unknown),
            }

            row.bind(&row_model, &worker, &staged_adds, &staged_removes);

            if row_model.membership() == Membership::Unknown {
                fetch_membership_for_row(
                    &worker,
                    &api,
                    &cache,
                    &row_model,
                    &row,
                    &song_id,
                    &staged_adds,
                    &staged_removes,
                );
            }
        }
    ));

    factory.connect_unbind(|_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        let row = item.child().unwrap().downcast::<PlaylistRow>().unwrap();
        row.unbind();
    });

    let list_view = gtk::ListView::builder()
        .model(&selection)
        .factory(&factory)
        .single_click_activate(false)
        .css_classes(["add-to-playlist-list"])
        .build();

    let clamp = libadwaita::Clamp::builder()
        .maximum_size(600)
        .margin_start(12)
        .margin_end(12)
        .margin_bottom(24)
        .child(&list_view)
        .build();
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&clamp)
        .build();
    root.append(&scroll);

    // ── Search filter ─────────────────────────────────────────────────────────
    search_entry.connect_search_changed(clone!(
        #[weak]
        filter,
        move |entry| {
            let needle = entry.text().to_lowercase();
            filter.set_filter_func(move |obj| {
                if needle.is_empty() {
                    return true;
                }
                obj.downcast_ref::<PlaylistRowModel>()
                    .map(|m| m.title().to_lowercase().contains(&needle))
                    .unwrap_or(false)
            });
        }
    ));

    // ── Sort control ──────────────────────────────────────────────────────────
    sort_drop.connect_selected_notify(clone!(
        #[strong]
        sort_mode,
        #[weak]
        sorter,
        move |drop| {
            sort_mode.set(if drop.selected() == 0 {
                SortMode::Alphabetical
            } else {
                SortMode::Default
            });
            sorter.changed(gtk::SorterChange::Different);
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
    // Adds come from staged_adds, removes from staged_removes. For a checked
    // playlist whose membership we never resolved, we fetch its track ids first
    // and skip the add if the track is already present, so Save can never create
    // a duplicate.
    let staged_adds_save = staged_adds.clone();
    let staged_removes_save = staged_removes.clone();
    let cache_save = cache.clone();
    let song_uri_save = song_uri;
    let song_id_save = song_id;
    let sheet_save = sheet.clone();
    let api_save = api.clone();
    let worker_save = worker.clone();
    save_btn.connect_clicked(move |_| {
        set_sheet_open(&sheet_save, false);

        let adds: Vec<String> = staged_adds_save.borrow().iter().cloned().collect();
        let removes: Vec<String> = staged_removes_save.borrow().iter().cloned().collect();

        let api = api_save.clone();
        let cache = cache_save.clone();
        let song_uri = song_uri_save.clone();
        let song_id = song_id_save.clone();
        worker_save.send_local_task(async move {
            for pid in adds {
                // If we already know the track is present, adding again would
                // duplicate it — skip. If membership is unknown, fetch first.
                let known = cache.borrow().track_ids(&pid).map(|s| s.contains(&song_id));
                let already_in = match known {
                    Some(present) => present,
                    None => match api.get_playlist_track_ids(&pid).await {
                        Ok(ids) => ids.iter().any(|i| i == &song_id),
                        Err(e) => {
                            error!("add-to-playlist: fetch before add failed for {pid}: {e:?}");
                            false
                        }
                    },
                };
                if already_in {
                    continue;
                }
                if let Err(e) = api.add_to_playlist(&pid, vec![song_uri.clone()]).await {
                    error!("add-to-playlist: add to {pid} failed: {e:?}");
                }
            }
            for pid in removes {
                if let Err(e) = api.remove_from_playlist(&pid, vec![song_uri.clone()]).await {
                    error!("add-to-playlist: remove from {pid} failed: {e:?}");
                }
            }
        });
    });

    host.set_child(Some(&root));
}

// Lazily fetch a single playlist's track ids, cache the result, then update the
// (possibly-recycled) row. The fetch runs on the same local executor as art
// loading, so it may touch widgets. The row/model are weak-referenced: if the
// row was unbound before the fetch returns, only the cache is updated.
#[allow(clippy::too_many_arguments)]
fn fetch_membership_for_row(
    worker: &Worker,
    api: &Arc<dyn SpotifyApiClient + Send + Sync>,
    cache: &SharedMembershipCache,
    row_model: &PlaylistRowModel,
    row: &PlaylistRow,
    song_id: &str,
    staged_adds: &Rc<RefCell<HashSet<String>>>,
    staged_removes: &Rc<RefCell<HashSet<String>>>,
) {
    let id = row_model.id();
    let Some(snapshot) = row_model.snapshot_id() else {
        // No snapshot to key on — treat as not-in and don't persist.
        row_model.set_membership(Membership::NotIn);
        return;
    };

    let api = api.clone();
    let cache = cache.clone();
    let song_id = song_id.to_owned();
    let weak_model = row_model.downgrade();
    let weak_row = row.downgrade();
    let staged_adds = staged_adds.clone();
    let staged_removes = staged_removes.clone();

    worker.send_local_task(async move {
        let ids = match api.get_playlist_track_ids(&id).await {
            Ok(ids) => ids,
            Err(e) => {
                error!("add-to-playlist: membership fetch for {id} failed: {e:?}");
                return;
            }
        };
        let set: HashSet<String> = ids.into_iter().collect();
        let contains = set.contains(&song_id);
        cache.borrow_mut().insert(id.clone(), snapshot, set);
        cache.schedule_save();

        // The model is unique per playlist, so recording its membership is always
        // correct even if the widget it was bound to has since been recycled.
        if let Some(model) = weak_model.upgrade() {
            model.set_membership(if contains {
                Membership::In
            } else {
                Membership::NotIn
            });

            // Only touch the widget if it is STILL showing this playlist. Rows are
            // recycled across playlists as the user scrolls; refreshing a row that
            // has moved on would flip the wrong checkbox.
            if let Some(row) = weak_row.upgrade() {
                if row.bound_id() == id {
                    row.refresh_checkbox(&model, &staged_adds, &staged_removes);
                }
            }
        }
    });
}

// ── Widget helpers ────────────────────────────────────────────────────────────

fn art_thumbnail(url: Option<&str>, worker: &Worker) -> gtk::Image {
    let image = gtk::Image::builder()
        .pixel_size(48)
        .valign(gtk::Align::Center)
        .build();
    if let Some(url) = url.map(str::to_owned) {
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

// ── Reusable playlist row widget ──────────────────────────────────────────────
// One instance is created per realized ListView slot and recycled across
// playlists via bind/unbind. It owns its own art image, name label and checkbox
// and tracks the currently-bound playlist id plus the checkbox's toggle handler.
mod playlist_row {
    use super::Membership;
    use super::PlaylistRowModel;
    use gtk::glib;
    use gtk::prelude::*;
    use gtk::subclass::prelude::*;
    use std::cell::RefCell;
    use std::collections::HashSet;
    use std::rc::Rc;

    glib::wrapper! {
        pub struct PlaylistRow(ObjectSubclass<imp::PlaylistRow>)
            @extends gtk::Box, gtk::Widget,
            @implements gtk::Orientable;
    }

    impl Default for PlaylistRow {
        fn default() -> Self {
            Self::new()
        }
    }

    impl PlaylistRow {
        pub fn new() -> Self {
            glib::Object::new()
        }

        // Bind this recycled row to a playlist: set label, load art lazily and
        // pre-check the box from the staged state / known membership.
        pub fn bind(
            &self,
            model: &PlaylistRowModel,
            worker: &crate::app::Worker,
            staged_adds: &Rc<RefCell<HashSet<String>>>,
            staged_removes: &Rc<RefCell<HashSet<String>>>,
        ) {
            let imp = self.imp();
            let id = model.id();
            imp.bound_id.replace(id.clone());

            imp.label.set_label(&model.title());

            // Load art lazily for this row only. Clear first so the recycled
            // widget never shows the previous playlist's cover.
            imp.art.set_paintable(gdk::Paintable::NONE);
            if let Some(url) = model.art() {
                let weak = imp.art.downgrade();
                let expected = id.clone();
                let row_weak = self.downgrade();
                worker.send_local_task(async move {
                    let loader = crate::app::loader::ImageLoader::new();
                    if let Some(pixbuf) = loader.load_remote(&url, "jpg", 48, 48).await {
                        // Skip if the row was recycled onto another playlist.
                        let still_bound = row_weak
                            .upgrade()
                            .map(|r| *r.imp().bound_id.borrow() == expected)
                            .unwrap_or(false);
                        if still_bound {
                            if let Some(img) = weak.upgrade() {
                                img.set_paintable(Some(&gdk::Texture::for_pixbuf(&pixbuf)));
                            }
                        }
                    }
                });
            }

            self.refresh_checkbox(model, staged_adds, staged_removes);

            // Wire the toggle so checking/unchecking updates the staged sets. The
            // handler resolves membership at click time to decide add vs remove.
            let check = imp.check.clone();
            let id_for_toggle = id.clone();
            let staged_adds = staged_adds.clone();
            let staged_removes = staged_removes.clone();
            let model_weak = model.downgrade();
            let handler = check.connect_toggled(move |btn| {
                let now = btn.is_active();
                let was_in = model_weak
                    .upgrade()
                    .map(|m| m.membership() == Membership::In)
                    .unwrap_or(false);
                if now {
                    staged_removes.borrow_mut().remove(&id_for_toggle);
                    if !was_in {
                        staged_adds.borrow_mut().insert(id_for_toggle.clone());
                    }
                } else {
                    staged_adds.borrow_mut().remove(&id_for_toggle);
                    if was_in {
                        staged_removes.borrow_mut().insert(id_for_toggle.clone());
                    }
                }
            });
            imp.toggle_handler.replace(Some(handler));
        }

        // Recompute the checkbox state from staged sets + resolved membership,
        // without emitting a toggle. Used on bind and after a lazy fetch resolves.
        pub fn refresh_checkbox(
            &self,
            model: &PlaylistRowModel,
            staged_adds: &Rc<RefCell<HashSet<String>>>,
            staged_removes: &Rc<RefCell<HashSet<String>>>,
        ) {
            let imp = self.imp();
            let id = model.id();
            let checked = if staged_adds.borrow().contains(&id) {
                true
            } else if staged_removes.borrow().contains(&id) {
                false
            } else {
                model.membership() == Membership::In
            };

            // Block the toggle handler so this programmatic update doesn't restage.
            if let Some(handler) = imp.toggle_handler.borrow().as_ref() {
                imp.check.block_signal(handler);
                imp.check.set_active(checked);
                imp.check.unblock_signal(handler);
            } else {
                imp.check.set_active(checked);
            }
        }

        // The playlist id this row is currently bound to, or empty once unbound.
        pub fn bound_id(&self) -> String {
            self.imp().bound_id.borrow().clone()
        }

        pub fn unbind(&self) {
            let imp = self.imp();
            if let Some(handler) = imp.toggle_handler.take() {
                imp.check.disconnect(handler);
            }
            imp.art.set_paintable(gdk::Paintable::NONE);
            imp.bound_id.replace(String::new());
        }
    }

    mod imp {
        use super::*;
        use gtk::glib::SignalHandlerId;

        pub struct PlaylistRow {
            pub art: gtk::Image,
            pub label: gtk::Label,
            pub check: gtk::CheckButton,
            pub bound_id: RefCell<String>,
            pub toggle_handler: RefCell<Option<SignalHandlerId>>,
        }

        impl Default for PlaylistRow {
            fn default() -> Self {
                Self {
                    art: gtk::Image::builder().pixel_size(48).build(),
                    label: gtk::Label::builder()
                        .hexpand(true)
                        .xalign(0.0)
                        .ellipsize(gtk::pango::EllipsizeMode::End)
                        .build(),
                    check: gtk::CheckButton::builder()
                        .valign(gtk::Align::Center)
                        .build(),
                    bound_id: RefCell::new(String::new()),
                    toggle_handler: RefCell::new(None),
                }
            }
        }

        #[glib::object_subclass]
        impl ObjectSubclass for PlaylistRow {
            const NAME: &'static str = "AddToPlaylistRow";
            type Type = super::PlaylistRow;
            type ParentType = gtk::Box;
        }

        impl ObjectImpl for PlaylistRow {
            fn constructed(&self) {
                self.parent_constructed();
                let obj = self.obj();
                obj.set_orientation(gtk::Orientation::Horizontal);
                obj.set_spacing(12);
                obj.set_margin_top(4);
                obj.set_margin_bottom(4);
                obj.append(&self.art);
                obj.append(&self.label);
                obj.append(&self.check);
            }
        }

        impl WidgetImpl for PlaylistRow {}
        impl BoxImpl for PlaylistRow {}
    }
}
use playlist_row::PlaylistRow;

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

                *self.current_song.borrow_mut() = Some(song.clone());
                self.build_for(song);

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
                    }
                }
            }
            _ => {}
        }
    }
}
