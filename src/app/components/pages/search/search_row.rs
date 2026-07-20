//! Recycled compact row widget for the virtualized search result list.
//!
//! One `SearchRow` instance is created per realized `ListView` slot and recycled
//! across items via bind/unbind (mirrors the library's `LibraryRow`). Each owns its
//! own art image, title/subtitle labels and tracks the currently-bound item id so a
//! late-arriving art load is dropped if the widget was recycled onto a different
//! item in the meantime.

use gdk::prelude::*;
use gtk::prelude::*;
use gtk::subclass::prelude::*;

use crate::app::loader::ImageLoader;
use crate::app::models::{CardKind, CardModel};
use crate::app::Worker;

/// Fixed cover thumbnail size (px) for a compact search result row. Deliberately
/// smaller than the library row to match Spotify's dense search list.
const ROW_COVER_SIZE: i32 = 48;

/// Compose the small type subtitle shown under the title, e.g. "Playlist",
/// "Artista" or "Album • Artist". Artists get just their kind label.
fn compose_subtitle(model: &CardModel) -> String {
    let subtitle = model.subtitle();
    match model.card_kind().label() {
        Some(kind) if subtitle.is_empty() => kind,
        Some(kind) => format!("{kind} • {subtitle}"),
        None => subtitle,
    }
}

/// Load cover art for a bound row, guarding against recycling: if the widget was
/// rebound to a different id by the time the image arrives, the result is dropped.
fn load_cover(
    image: &gtk::Image,
    model: &CardModel,
    worker: &Worker,
    bound_id: std::rc::Rc<std::cell::RefCell<String>>,
) {
    // Clear first so a recycled widget never shows the previous item's cover.
    image.set_paintable(gdk::Paintable::NONE);
    if let Some(url) = model.image() {
        let weak = image.downgrade();
        let expected = model.id();
        worker.send_local_task(async move {
            let loader = ImageLoader::new();
            if let Some(pixbuf) = loader
                .load_remote(&url, "jpg", ROW_COVER_SIZE, ROW_COVER_SIZE)
                .await
            {
                if *bound_id.borrow() != expected {
                    return; // recycled onto another item
                }
                if let Some(img) = weak.upgrade() {
                    img.set_paintable(Some(&gdk::Texture::for_pixbuf(&pixbuf)));
                }
            }
        });
    } else {
        let icon = match model.card_kind() {
            CardKind::Artist => "avatar-default-symbolic",
            _ => "emblem-music-symbolic",
        };
        image.set_icon_name(Some(icon));
    }
}

glib::wrapper! {
    pub struct SearchRow(ObjectSubclass<imp::SearchRow>)
        @extends gtk::Box, gtk::Widget,
        @implements gtk::Orientable;
}

impl Default for SearchRow {
    fn default() -> Self {
        Self::new()
    }
}

impl SearchRow {
    pub fn new() -> Self {
        glib::Object::new()
    }

    pub fn bind(&self, model: &CardModel, worker: &Worker) {
        let imp = self.imp();
        imp.bound_id.replace(model.id());

        imp.title.set_label(&model.title());
        let subtitle = compose_subtitle(model);
        imp.subtitle.set_label(&subtitle);
        imp.subtitle.set_visible(!subtitle.is_empty());

        // Round artwork for artists, square otherwise.
        if model.is_round() {
            imp.art.add_css_class("search-cover-round");
        } else {
            imp.art.remove_css_class("search-cover-round");
        }

        load_cover(&imp.art, model, worker, imp.bound_id.clone());
    }

    pub fn unbind(&self) {
        let imp = self.imp();
        imp.art.set_paintable(gdk::Paintable::NONE);
        imp.bound_id.replace(String::new());
    }

    /// The item id this row is currently bound to (empty when recycled/unbound).
    pub fn bound_id(&self) -> String {
        self.imp().bound_id.borrow().clone()
    }
}

mod imp {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    pub struct SearchRow {
        pub art: gtk::Image,
        pub title: gtk::Label,
        pub subtitle: gtk::Label,
        pub bound_id: Rc<RefCell<String>>,
    }

    impl Default for SearchRow {
        fn default() -> Self {
            Self {
                art: gtk::Image::builder()
                    .pixel_size(ROW_COVER_SIZE)
                    .css_classes(["search-cover"])
                    .build(),
                title: gtk::Label::builder()
                    .xalign(0.0)
                    .ellipsize(gtk::pango::EllipsizeMode::End)
                    .build(),
                subtitle: gtk::Label::builder()
                    .xalign(0.0)
                    .ellipsize(gtk::pango::EllipsizeMode::End)
                    .css_classes(["caption", "dim-label"])
                    .build(),
                bound_id: Rc::new(RefCell::new(String::new())),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for SearchRow {
        const NAME: &'static str = "RiffSearchRow";
        type Type = super::SearchRow;
        type ParentType = gtk::Box;
    }

    impl ObjectImpl for SearchRow {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.set_orientation(gtk::Orientation::Horizontal);
            obj.set_spacing(10);
            obj.set_margin_top(3);
            obj.set_margin_bottom(3);

            let text = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .hexpand(true)
                .valign(gtk::Align::Center)
                .build();
            text.append(&self.title);
            text.append(&self.subtitle);

            obj.append(&self.art);
            obj.append(&text);
        }
    }

    impl WidgetImpl for SearchRow {}
    impl BoxImpl for SearchRow {}
}
