mod membership_cache;
use membership_cache::{OwnedPlaylist, SharedMembershipCache};

use gettextrs::gettext;
use gtk::prelude::*;
use libadwaita::prelude::BinExt;
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;

use crate::app::components::{Component, EventListener};
use crate::app::loader::ImageLoader;
use crate::app::models::SongDescription;
use crate::app::state::{BrowserEvent, LoginEvent};
use crate::app::{ActionDispatcher, AppAction, AppEvent, AppModel, Worker};

// The sync walks owned playlists one at a time (awaiting each fetch) so it never
// starves the GTK main-loop executor, which also serves art loads and UI tasks.
// The open drawer is rebuilt every this-many freshly-indexed playlists so
// checkmarks appear steadily rather than after every single fetch.
const SYNC_REBUILD_BATCH: usize = 4;

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

// GObject row model backing the virtualized ListView. One instance per owned
// playlist; only rows scrolled into view are ever bound to a widget, and art is
// loaded lazily on bind. Membership is resolved synchronously from the shared index
// when the store is built (no fetch on bind), and carried on the `in-playlist`
// property so a recycled widget always reflects the right playlist.
mod row_model {
    use glib::prelude::*;
    use glib::subclass::prelude::*;
    use glib::Properties;
    use std::cell::{Cell, RefCell};

    glib::wrapper! {
        pub struct PlaylistRowModel(ObjectSubclass<imp::PlaylistRowModel>);
    }

    impl PlaylistRowModel {
        pub fn new(
            id: &str,
            title: &str,
            art: Option<&str>,
            snapshot_id: Option<&str>,
            in_playlist: bool,
        ) -> Self {
            glib::Object::builder()
                .property("id", id)
                .property("title", title)
                .property("art", art.map(str::to_owned))
                .property("snapshot-id", snapshot_id.map(str::to_owned))
                .property("in-playlist", in_playlist)
                .build()
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
            // Whether the target song is in this playlist per the shared index.
            #[property(get, set, name = "in-playlist")]
            pub in_playlist: Cell<bool>,
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
    current_song: Rc<RefCell<Option<SongDescription>>>,
    /// Pending adds: playlist ids checked during this session.
    /// Persists across build_for() rebuilds. Cleared only for a new song.
    staged_adds: Rc<RefCell<HashSet<String>>>,
    /// Pending removes: playlist ids unchecked during this session.
    staged_removes: Rc<RefCell<HashSet<String>>>,
    /// Playlist ids seen at the last build_for() call. Used to detect
    /// newly-created playlists (via UserPlaylistsLoaded) for auto-staging.
    known_playlist_ids: Rc<RefCell<HashSet<String>>>,
    /// Owned-playlist list + snapshot-keyed membership index, persisted to disk and
    /// filled by the background sync. Shared with the drawer rows and the Save diff.
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
            current_song: Rc::new(RefCell::new(None)),
            staged_adds: Rc::new(RefCell::new(HashSet::new())),
            staged_removes: Rc::new(RefCell::new(HashSet::new())),
            known_playlist_ids: Rc::new(RefCell::new(HashSet::new())),
            cache: SharedMembershipCache::load(),
        }
    }

    fn build_for(&self, song: &SongDescription) {
        rebuild_drawer(
            song,
            &self.sheet,
            &self.host,
            &self.model,
            &self.worker,
            &self.staged_adds,
            &self.staged_removes,
            &self.known_playlist_ids,
            &self.cache,
        );
    }

    // Kick the background sync: resolve the owned playlists from the authoritative
    // owner ids, then walk them sequentially to fill the index. Rebuilds the (open)
    // drawer as data lands so checkmarks converge live. Skipped while one is already
    // in flight so overlapping login events can't race two walks.
    fn start_sync(&self) {
        if self.cache.is_syncing() {
            return;
        }
        let rebuild = self.rebuild_closure();
        sync_owned_playlists(
            self.model.app_model.clone(),
            self.worker.clone(),
            self.cache.clone(),
            rebuild,
        );
    }

    // A cheap-to-clone closure that rebuilds the drawer for the current song when
    // it is open. Handed to the background sync so it can refresh as it progresses.
    fn rebuild_closure(&self) -> Rc<dyn Fn()> {
        let sheet = self.sheet.clone();
        let host = self.host.clone();
        let model = self.model.clone();
        let worker = self.worker.clone();
        let staged_adds = self.staged_adds.clone();
        let staged_removes = self.staged_removes.clone();
        let known_playlist_ids = self.known_playlist_ids.clone();
        let cache = self.cache.clone();
        let current_song = self.current_song.clone();
        Rc::new(move || {
            let Some(song) = current_song.borrow().clone() else {
                return;
            };
            if !sheet.property::<bool>("open") {
                return;
            }
            rebuild_drawer(
                &song,
                &sheet,
                &host,
                &model,
                &worker,
                &staged_adds,
                &staged_removes,
                &known_playlist_ids,
                &cache,
            );
        })
    }
}

// Rebuild the drawer for a song from the owned-playlist list, auto-staging any
// newly-created playlists. Shared by build_for and the sync's live refresh.
#[allow(clippy::too_many_arguments)]
fn rebuild_drawer(
    song: &SongDescription,
    sheet: &gtk::Widget,
    host: &libadwaita::Bin,
    model: &Rc<AddToPlaylistModel>,
    worker: &Worker,
    staged_adds: &Rc<RefCell<HashSet<String>>>,
    staged_removes: &Rc<RefCell<HashSet<String>>>,
    known_playlist_ids: &Rc<RefCell<HashSet<String>>>,
    cache: &SharedMembershipCache,
) {
    // Only playlists the user owns are listed — those are the ones you can add to,
    // and the ones the sync indexes. The list comes from the shared cache, populated
    // by the sync from the authoritative owner ids.
    let playlists = cache.owned_playlists();

    // Detect newly-created playlists and auto-stage them as adds. On the first
    // build for a song, known_playlist_ids has been seeded with the current ids
    // (done in on_event), so nothing is treated as "new" until the user creates one.
    {
        let current_ids: HashSet<String> = playlists.iter().map(|p| p.id.clone()).collect();
        let mut known = known_playlist_ids.borrow_mut();
        for new_id in current_ids.difference(&*known) {
            staged_adds.borrow_mut().insert(new_id.clone());
        }
        *known = current_ids;
    }

    build_drawer_ui(
        song,
        &playlists,
        sheet,
        host,
        model,
        worker,
        staged_adds,
        staged_removes,
        cache,
    );
}

// Background library index. Runs once per session on the local executor: pull the
// user's saved playlists, keep only the ones they own (owner.id == logged user),
// publish that list for the drawer, then walk them SEQUENTIALLY filling the track
// index. Snapshot-fresh entries are skipped for free, so after the first sync only
// changed playlists are re-fetched. Awaiting each fetch keeps the GTK main loop
// responsive; the drawer is rebuilt as data lands so checkmarks converge live.
fn sync_owned_playlists(
    app_model: Rc<AppModel>,
    worker: Worker,
    cache: SharedMembershipCache,
    rebuild: Rc<dyn Fn()>,
) {
    let Some(user_id) = app_model.get_state().logged_user.user.clone() else {
        return;
    };
    let api = app_model.get_spotify();

    cache.set_syncing(true);
    rebuild();

    worker.send_local_task(async move {
        // Resolve the owned playlists from the authoritative saved-playlists list,
        // which carries owner ids (the home cards drop them).
        const PAGE: usize = 50;
        let mut owned: Vec<OwnedPlaylist> = Vec::new();
        let mut offset = 0usize;
        loop {
            let batch = match api.get_saved_playlists(offset, PAGE).await {
                Ok(batch) => batch,
                Err(e) => {
                    error!("add-to-playlist: saved playlists fetch failed: {e:?}");
                    break;
                }
            };
            let fetched = batch.len();
            for pl in batch {
                if pl.owner.id == user_id {
                    owned.push(OwnedPlaylist {
                        id: pl.id,
                        title: pl.title,
                        art: pl
                            .art
                            .as_ref()
                            .and_then(|s| s.best_for_width(48))
                            .map(str::to_owned),
                        snapshot_id: pl.snapshot_id,
                    });
                }
            }
            offset += PAGE;
            if fetched < PAGE {
                break;
            }
        }

        cache.set_owned_playlists(owned.clone());
        rebuild();

        // Walk the owned playlists one at a time. Snapshot-fresh entries are already
        // indexed, so they cost nothing; the rest are fetched and merged in.
        let mut done_since_rebuild = 0usize;
        for pl in &owned {
            let fresh = cache.index().is_fresh(&pl.id, pl.snapshot_id.as_deref());
            if fresh {
                continue;
            }

            let ids = match api.get_playlist_track_ids(&pl.id).await {
                Ok(ids) => ids,
                Err(e) => {
                    error!("add-to-playlist: track sync for {} failed: {e:?}", pl.id);
                    continue;
                }
            };
            let set: HashSet<String> = ids.into_iter().collect();

            // Only a persistent (snapshot-keyed) entry can be skipped next launch; a
            // playlist with no snapshot is still indexed for this session's checks.
            let snapshot = pl.snapshot_id.clone().unwrap_or_default();
            cache.index_mut().insert(pl.id.clone(), snapshot, set);
            cache.schedule_save();

            // Refresh the open drawer every few playlists rather than after each
            // fetch, so checkmarks appear steadily without a rebuild storm.
            done_since_rebuild += 1;
            if done_since_rebuild >= SYNC_REBUILD_BATCH {
                done_since_rebuild = 0;
                rebuild();
            }
        }

        cache.set_syncing(false);
        cache.schedule_save();
        rebuild();
    });
}

// Shared drawer layout. All state that varies between calls is passed explicitly
// so there is exactly one copy of the widget tree.
#[allow(clippy::too_many_arguments)]
fn build_drawer_ui(
    song: &SongDescription,
    playlists: &[OwnedPlaylist],
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
    // Backing store holds one lightweight GObject per owned playlist. A
    // FilterListModel applies the live search text; a SortListModel applies the
    // sort order. The ListView only realizes rows in view, so art is loaded lazily
    // per visible row; membership is read once here from the shared index (no fetch
    // on bind). A playlist the sync has not reached yet reads as not-in until the
    // next build_for after it lands.
    let store = gio::ListStore::new::<PlaylistRowModel>();
    {
        let index = cache.index();
        for pl in playlists {
            let in_playlist = index.contains(&pl.id, &song_id);
            store.append(&PlaylistRowModel::new(
                &pl.id,
                &pl.title,
                pl.art.as_deref(),
                pl.snapshot_id.as_deref(),
                in_playlist,
            ));
        }
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
        staged_adds,
        #[strong]
        staged_removes,
        move |_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().unwrap();
            let row_model = item.item().unwrap().downcast::<PlaylistRowModel>().unwrap();
            let row = item.child().unwrap().downcast::<PlaylistRow>().unwrap();

            // Membership was resolved from the index when the store was built and
            // rides on the row model, so binding only wires the widget — no fetch.
            row.bind(&row_model, &worker, &staged_adds, &staged_removes);
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

    // Subtle hint while the first-run sync is still filling the index: some rows
    // may read unchecked until it lands. The drawer rebuilds when it finishes.
    if cache.is_syncing() {
        let hint = gtk::Label::builder()
            .label(gettext("Checking your playlists…"))
            .css_classes(["caption", "dim-label"])
            .margin_bottom(12)
            .build();
        root.append(&hint);
    }

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
            // set_filter_func does not itself notify the FilterListModel; without
            // this the model never re-evaluates, so clearing the query leaves the
            // list stuck on the previous (empty) result. Different forces a full
            // re-filter, which correctly re-includes every row when the query is
            // empty and re-narrows when it is not.
            filter.changed(gtk::FilterChange::Different);
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
    // Adds come from staged_adds, removes from staged_removes. Dedup is index-based:
    // if the index already knows the playlist contains the song, the add is skipped
    // so Save never duplicates a track. No per-save fetching.
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

        // Filter the adds against the index up front (on the main thread, cheap) so
        // the async task only issues the calls that actually change anything.
        let adds: Vec<String> = {
            let index = cache_save.index();
            staged_adds_save
                .borrow()
                .iter()
                .filter(|pid| !index.contains(pid, &song_id_save))
                .cloned()
                .collect()
        };
        let removes: Vec<String> = staged_removes_save.borrow().iter().cloned().collect();

        let api = api_save.clone();
        let song_uri = song_uri_save.clone();
        worker_save.send_local_task(async move {
            for pid in adds {
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
            // handler reads indexed membership at click time to decide add vs remove.
            let check = imp.check.clone();
            let id_for_toggle = id.clone();
            let staged_adds = staged_adds.clone();
            let staged_removes = staged_removes.clone();
            let model_weak = model.downgrade();
            let handler = check.connect_toggled(move |btn| {
                let now = btn.is_active();
                let was_in = model_weak
                    .upgrade()
                    .map(|m| m.in_playlist())
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

        // Recompute the checkbox state from staged sets + indexed membership,
        // without emitting a toggle. Used on bind.
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
                model.in_playlist()
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

                // Seed known_playlist_ids with the currently-known owned set BEFORE
                // build_for so nothing is treated as "new" (and auto-checked) on the
                // first build. Only playlists created later this session — appearing
                // in the sync's refreshed owned list — get auto-staged.
                {
                    let current_ids: HashSet<String> = self
                        .cache
                        .owned_playlists()
                        .iter()
                        .map(|p| p.id.clone())
                        .collect();
                    *self.known_playlist_ids.borrow_mut() = current_ids;
                }

                *self.current_song.borrow_mut() = Some(song.clone());
                self.build_for(song);

                set_sheet_open(&self.sheet, true);
            }
            // Kick the background sync once the user's playlists are loaded. Also
            // fires when a playlist is created/renamed/removed; the snapshot skip
            // makes those re-runs cheap and picks up the new owned list.
            AppEvent::LoginEvent(LoginEvent::UserPlaylistsLoaded)
            | AppEvent::BrowserEvent(BrowserEvent::SavedPlaylistsUpdated) => {
                self.start_sync();
            }
            _ => {}
        }
    }
}
