//! Recycled row/card widgets for the virtualized library views.
//!
//! One `LibraryRow` (compact horizontal list row) and one `LibraryCard` (vertical
//! grid tile) instance is created per realized `ListView`/`GridView` slot and
//! recycled across items via bind/unbind. Each owns its own art image, labels and
//! a pin indicator, and tracks the currently-bound item id so a late-arriving art
//! load is dropped if the widget was recycled onto a different item in the meantime.

use gdk::prelude::*;
use gettextrs::ngettext;
use gtk::prelude::*;
use gtk::subclass::prelude::*;

use crate::app::loader::ImageLoader;
use crate::app::models::{CardKind, CardModel};
use crate::app::Worker;

/// Fixed cover thumbnail size (px) for a compact list row.
const ROW_COVER_SIZE: i32 = 56;
/// Cover size (px) for a grid tile on a phone (3 columns fit ~540 px logical width).
const CARD_COVER_SIZE: i32 = 100;

/// Compose the "{Kind} • {subtitle} • {N songs}" secondary line shown under the
/// title. Artists have no subtitle/count; imageless synthetic rows (Liked Songs)
/// still get their kind label.
fn compose_subtitle(model: &CardModel) -> String {
    let subtitle = model.subtitle();
    let count = model.track_count();
    let count_suffix = match model.card_kind() {
        CardKind::Album | CardKind::Playlist if count > 0 => {
            let s = ngettext!("{} song", "{} songs", count, count);
            format!(" • {s}")
        }
        _ => String::new(),
    };
    match model.card_kind().label() {
        Some(kind) if subtitle.is_empty() => format!("{kind}{count_suffix}"),
        Some(kind) => format!("{kind} • {subtitle}{count_suffix}"),
        None => subtitle,
    }
}

/// Load cover art for a bound row/card, guarding against recycling: if the widget
/// was rebound to a different id by the time the image arrives, the result is
/// dropped. `round` clips the image to a circle (followed artists).
fn load_cover(
    image: &gtk::Image,
    model: &CardModel,
    worker: &Worker,
    size: i32,
    bound_id: std::rc::Rc<std::cell::RefCell<String>>,
) {
    // Clear first so a recycled widget never shows the previous item's cover.
    image.set_paintable(gdk::Paintable::NONE);
    if let Some(url) = model.image() {
        let weak = image.downgrade();
        let expected = model.id();
        worker.send_local_task(async move {
            let loader = ImageLoader::new();
            if let Some(pixbuf) = loader.load_remote(&url, "jpg", size, size).await {
                if *bound_id.borrow() != expected {
                    return; // recycled onto another item
                }
                if let Some(img) = weak.upgrade() {
                    img.set_paintable(Some(&gdk::Texture::for_pixbuf(&pixbuf)));
                }
            }
        });
    } else {
        // No remote art — themed icon so the tile is never blank.
        let icon = if model.card_kind() == CardKind::Playlist
            && model.id().starts_with("__riff_liked")
        {
            "emblem-favorite-symbolic"
        } else {
            "library-music-symbolic"
        };
        image.set_icon_name(Some(icon));
    }
}

// ── Compact horizontal list row ────────────────────────────────────────────────

glib::wrapper! {
    pub struct LibraryRow(ObjectSubclass<imp_row::LibraryRow>)
        @extends gtk::Box, gtk::Widget,
        @implements gtk::Orientable;
}

impl Default for LibraryRow {
    fn default() -> Self {
        Self::new()
    }
}

impl LibraryRow {
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
        imp.pin.set_visible(model.is_pinned());

        // Round artwork for artists, square otherwise.
        if model.is_round() {
            imp.art.add_css_class("library-cover-round");
        } else {
            imp.art.remove_css_class("library-cover-round");
        }

        load_cover(&imp.art, model, worker, ROW_COVER_SIZE, imp.bound_id.clone());
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

mod imp_row {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    pub struct LibraryRow {
        pub art: gtk::Image,
        pub title: gtk::Label,
        pub subtitle: gtk::Label,
        pub pin: gtk::Image,
        pub bound_id: Rc<RefCell<String>>,
    }

    impl Default for LibraryRow {
        fn default() -> Self {
            Self {
                art: gtk::Image::builder()
                    .pixel_size(ROW_COVER_SIZE)
                    .css_classes(["library-cover"])
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
                pin: gtk::Image::builder()
                    .icon_name("view-pin-symbolic")
                    .pixel_size(14)
                    .css_classes(["dim-label"])
                    .valign(gtk::Align::Center)
                    .build(),
                bound_id: Rc::new(RefCell::new(String::new())),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for LibraryRow {
        const NAME: &'static str = "RiffLibraryRow";
        type Type = super::LibraryRow;
        type ParentType = gtk::Box;
    }

    impl ObjectImpl for LibraryRow {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.set_orientation(gtk::Orientation::Horizontal);
            obj.set_spacing(12);
            obj.set_margin_top(4);
            obj.set_margin_bottom(4);

            let text = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .hexpand(true)
                .valign(gtk::Align::Center)
                .build();
            text.append(&self.title);
            text.append(&self.subtitle);

            obj.append(&self.art);
            obj.append(&text);
            obj.append(&self.pin);
        }
    }

    impl WidgetImpl for LibraryRow {}
    impl BoxImpl for LibraryRow {}
}

// ── Vertical grid tile ─────────────────────────────────────────────────────────

glib::wrapper! {
    pub struct LibraryCard(ObjectSubclass<imp_card::LibraryCard>)
        @extends gtk::Box, gtk::Widget,
        @implements gtk::Orientable;
}

impl Default for LibraryCard {
    fn default() -> Self {
        Self::new()
    }
}

impl LibraryCard {
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
        imp.pin.set_visible(model.is_pinned());

        if model.is_round() {
            imp.art.add_css_class("library-cover-round");
        } else {
            imp.art.remove_css_class("library-cover-round");
        }

        load_cover(
            &imp.art,
            model,
            worker,
            CARD_COVER_SIZE,
            imp.bound_id.clone(),
        );
    }

    pub fn unbind(&self) {
        let imp = self.imp();
        imp.art.set_paintable(gdk::Paintable::NONE);
        imp.bound_id.replace(String::new());
    }

    /// The item id this card is currently bound to (empty when recycled/unbound).
    pub fn bound_id(&self) -> String {
        self.imp().bound_id.borrow().clone()
    }
}

mod imp_card {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    pub struct LibraryCard {
        pub art: gtk::Image,
        pub title: gtk::Label,
        pub subtitle: gtk::Label,
        pub pin: gtk::Image,
        pub bound_id: Rc<RefCell<String>>,
    }

    impl Default for LibraryCard {
        fn default() -> Self {
            Self {
                art: gtk::Image::builder()
                    .pixel_size(CARD_COVER_SIZE)
                    .css_classes(["library-cover"])
                    .build(),
                title: gtk::Label::builder()
                    .xalign(0.0)
                    .ellipsize(gtk::pango::EllipsizeMode::End)
                    .css_classes(["title-4"])
                    .build(),
                subtitle: gtk::Label::builder()
                    .xalign(0.0)
                    .ellipsize(gtk::pango::EllipsizeMode::End)
                    .css_classes(["caption", "dim-label"])
                    .build(),
                pin: gtk::Image::builder()
                    .icon_name("view-pin-symbolic")
                    .pixel_size(12)
                    .css_classes(["dim-label"])
                    .halign(gtk::Align::Start)
                    .build(),
                bound_id: Rc::new(RefCell::new(String::new())),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for LibraryCard {
        const NAME: &'static str = "RiffLibraryCard";
        type Type = super::LibraryCard;
        type ParentType = gtk::Box;
    }

    impl ObjectImpl for LibraryCard {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.set_orientation(gtk::Orientation::Vertical);
            obj.set_spacing(2);
            obj.set_margin_top(4);
            obj.set_margin_bottom(4);

            // Title row: title + pin indicator side by side.
            let title_row = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(4)
                .margin_top(6)
                .build();
            title_row.append(&self.title);
            title_row.append(&self.pin);

            obj.append(&self.art);
            obj.append(&title_row);
            obj.append(&self.subtitle);
        }
    }

    impl WidgetImpl for LibraryCard {}
    impl BoxImpl for LibraryCard {}
}
