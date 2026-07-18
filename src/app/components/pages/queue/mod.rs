use gettextrs::gettext;
use gtk::prelude::*;
use std::ops::Deref;
use std::rc::Rc;

use crate::app::components::{Component, EventListener};
use crate::app::loader::ImageLoader;
use crate::app::models::SongDescription;
use crate::app::state::{PlaybackAction, PlaybackEvent, PlaybackState};
use crate::app::{ActionDispatcher, AppEvent, AppModel, Worker};

// The queue view: current track, then "Next in queue" (tracks the user manually
// queued, in order), then "Next up" (the upcoming context tracks). Mirrors
// Spotify's queue split; backed by PlaybackState::manual_queue + next_context_tracks.
pub struct QueueModel {
    app_model: Rc<AppModel>,
    dispatcher: Box<dyn ActionDispatcher>,
}

impl QueueModel {
    pub fn new(app_model: Rc<AppModel>, dispatcher: Box<dyn ActionDispatcher>) -> Self {
        Self {
            app_model,
            dispatcher,
        }
    }

    fn playback(&self) -> impl Deref<Target = PlaybackState> + '_ {
        self.app_model.map_state(|s| &s.playback)
    }

    fn current_song(&self) -> Option<SongDescription> {
        self.playback().current_song()
    }

    fn manual_tracks(&self) -> Vec<SongDescription> {
        self.playback().manual_queue().cloned().collect()
    }

    fn context_tracks(&self) -> Vec<SongDescription> {
        self.playback().next_context_tracks(100)
    }

    fn remove(&self, id: &str) {
        self.dispatcher
            .dispatch(PlaybackAction::Dequeue(id.to_string()).into());
    }

    fn play(&self, id: &str) {
        self.dispatcher
            .dispatch(PlaybackAction::Load(id.to_string()).into());
    }

    pub fn move_in_queue(&self, id: &str, to: usize) {
        self.dispatcher.dispatch(
            PlaybackAction::MoveInQueue {
                id: id.to_string(),
                to,
            }
            .into(),
        );
    }
}

pub struct Queue {
    model: Rc<QueueModel>,
    // The component root is a vertical Box: [header strip] + [ScrolledWindow].
    // The header is non-scrolling so the AdwBottomSheet can receive swipe-down
    // drags on it, letting the card dismiss without fighting the ScrolledWindow.
    root: gtk::Box,
    list: gtk::Box,
    worker: Worker,
}

impl Queue {
    pub fn new(model: QueueModel, worker: Worker) -> Self {
        // Non-scrolling header — "Queue" title + padding. Swiping down here
        // reaches the AdwBottomSheet drag handler instead of scrolling the list.
        let header = gtk::Label::builder()
            .label(gettext("Queue"))
            .css_classes(["title-3"])
            .xalign(0.5)
            .margin_top(12)
            .margin_bottom(8)
            .margin_start(16)
            .margin_end(16)
            .build();

        let list = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .margin_start(12)
            .margin_end(12)
            .margin_top(8)
            .margin_bottom(24)
            .build();
        let clamp = libadwaita::Clamp::builder()
            .maximum_size(600)
            .child(&list)
            .build();
        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&clamp)
            .build();

        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();
        root.append(&header);
        root.append(&scroll);

        let this = Self {
            model: Rc::new(model),
            root,
            list,
            worker,
        };
        this.refresh();
        this
    }

    fn section_label(&self, text: &str) {
        let label = gtk::Label::builder()
            .label(text)
            .xalign(0.0)
            .margin_top(12)
            .margin_bottom(4)
            .css_classes(["heading"])
            .build();
        self.list.append(&label);
    }

    fn text_box(song: &SongDescription) -> gtk::Box {
        let text = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .hexpand(true)
            .valign(gtk::Align::Center)
            .build();
        text.append(
            &gtk::Label::builder()
                .label(&song.title)
                .xalign(0.0)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .build(),
        );
        text.append(
            &gtk::Label::builder()
                .label(song.artists_name())
                .xalign(0.0)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .css_classes(["caption", "dim-label"])
                .build(),
        );
        text
    }

    // A 48×48 thumbnail placeholder that will be filled asynchronously once the
    // art URL is fetched. Returns the Image widget so the caller can position it.
    fn art_thumbnail(song: &SongDescription, worker: &Worker) -> gtk::Image {
        let image = gtk::Image::builder()
            .pixel_size(48)
            .valign(gtk::Align::Center)
            .build();

        if let Some(url) = song
            .art
            .as_ref()
            .and_then(|s| s.best_for_width(48))
            .map(str::to_owned)
        {
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

    fn manual_row(&self, song: &SongDescription, position: usize, queue_len: usize) {
        let row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .margin_top(4)
            .margin_bottom(4)
            .build();

        row.append(&Self::art_thumbnail(song, &self.worker));
        row.append(&Self::text_box(song));

        // Up/down reorder buttons — reliable on touch, no DnD plumbing needed.
        let up_btn = gtk::Button::builder()
            .icon_name("go-up-symbolic")
            .css_classes(["flat", "circular"])
            .valign(gtk::Align::Center)
            .tooltip_text(gettext("Move up"))
            .sensitive(position > 0)
            .build();
        let id_up = song.id.clone();
        let model_up = self.model.clone();
        up_btn.connect_clicked(move |_| {
            model_up.move_in_queue(&id_up, position.saturating_sub(1));
        });
        row.append(&up_btn);

        let down_btn = gtk::Button::builder()
            .icon_name("go-down-symbolic")
            .css_classes(["flat", "circular"])
            .valign(gtk::Align::Center)
            .tooltip_text(gettext("Move down"))
            .sensitive(position + 1 < queue_len)
            .build();
        let id_down = song.id.clone();
        let model_down = self.model.clone();
        down_btn.connect_clicked(move |_| {
            // Target the slot just past this row; move_in_queue clamps to the
            // new length after removing the row from its current spot.
            model_down.move_in_queue(&id_down, position + 1);
        });
        row.append(&down_btn);

        let remove = gtk::Button::builder()
            .icon_name("list-remove-symbolic")
            .css_classes(["flat", "circular"])
            .valign(gtk::Align::Center)
            .tooltip_text(gettext("Remove from queue"))
            .build();
        let id = song.id.clone();
        let model = self.model.clone();
        remove.connect_clicked(move |_| model.remove(&id));
        row.append(&remove);

        self.list.append(&row);
    }

    fn context_row(&self, song: &SongDescription) {
        let inner = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .build();
        inner.append(&Self::art_thumbnail(song, &self.worker));
        inner.append(&Self::text_box(song));

        let button = gtk::Button::builder()
            .child(&inner)
            .css_classes(["flat"])
            .build();
        let id = song.id.clone();
        let model = self.model.clone();
        button.connect_clicked(move |_| model.play(&id));
        self.list.append(&button);
    }

    // The current track: shown but not tappable (it's already playing, and a
    // manual-queued override isn't a jump target in the context list).
    fn current_row(&self, song: &SongDescription) {
        let row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .margin_top(4)
            .margin_bottom(4)
            .build();
        row.append(&Self::art_thumbnail(song, &self.worker));
        row.append(&Self::text_box(song));
        self.list.append(&row);
    }

    fn refresh(&self) {
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }

        let current = self.model.current_song();
        let manual = self.model.manual_tracks();
        let context = self.model.context_tracks();

        if current.is_none() && manual.is_empty() && context.is_empty() {
            self.list.append(
                &libadwaita::StatusPage::builder()
                    .title(gettext("Queue is empty"))
                    .icon_name("music-queue-symbolic")
                    .vexpand(true)
                    .build(),
            );
            return;
        }

        if let Some(song) = current {
            self.section_label(&gettext("Now playing"));
            self.current_row(&song);
        }
        if !manual.is_empty() {
            self.section_label(&gettext("Next in queue"));
            let manual_len = manual.len();
            for (i, song) in manual.iter().enumerate() {
                self.manual_row(song, i, manual_len);
            }
        }
        if !context.is_empty() {
            self.section_label(&gettext("Next up"));
            for song in &context {
                self.context_row(song);
            }
        }
    }
}

impl Component for Queue {
    fn get_root_widget(&self) -> &gtk::Widget {
        self.root.upcast_ref()
    }
}

impl EventListener for Queue {
    fn on_event(&mut self, event: &AppEvent) {
        if matches!(
            event,
            AppEvent::PlaybackEvent(
                PlaybackEvent::PlaylistChanged
                    | PlaybackEvent::TrackChanged(_)
                    | PlaybackEvent::PlaybackStopped
                    | PlaybackEvent::SourceChanged
            )
        ) {
            self.refresh();
        }
    }
}
