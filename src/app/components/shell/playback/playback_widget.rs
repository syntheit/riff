use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{glib, CompositeTemplate};
use std::cell::RefCell;

use crate::app::components::display_add_css_provider;
use crate::app::components::utils::{format_duration, Clock, Debouncer};
use crate::app::loader::ImageLoader;
use crate::app::models::RepeatMode;
use crate::app::Worker;

use super::playback_controls::PlaybackControlsWidget;
use super::playback_info::PlaybackInfoWidget;

mod imp {

    use super::*;

    #[derive(Default, CompositeTemplate)]
    #[template(resource = "/dev/diegovsky/Riff/components/playback_widget.ui")]
    pub struct PlaybackWidget {
        #[template_child]
        pub controls: TemplateChild<PlaybackControlsWidget>,

        #[template_child]
        pub now_playing: TemplateChild<PlaybackInfoWidget>,

        // Mobile mini-player strip.
        #[template_child]
        pub mobile_bar: TemplateChild<gtk::Box>,

        #[template_child]
        pub mobile_art: TemplateChild<gtk::Image>,

        #[template_child]
        pub mobile_title: TemplateChild<gtk::Label>,

        #[template_child]
        pub mobile_artist: TemplateChild<gtk::Label>,

        #[template_child]
        pub mobile_add: TemplateChild<gtk::Button>,

        #[template_child]
        pub mobile_add_icon: TemplateChild<gtk::Image>,

        #[template_child]
        pub mobile_play_pause: TemplateChild<gtk::Button>,

        #[template_child]
        pub mini_progress: TemplateChild<gtk::ProgressBar>,

        #[template_child]
        pub seek_bar: TemplateChild<gtk::Scale>,

        #[template_child]
        pub seek_overlay: TemplateChild<gtk::Overlay>,

        #[template_child]
        pub track_position: TemplateChild<gtk::Label>,

        #[template_child]
        pub track_duration: TemplateChild<gtk::Label>,

        #[template_child]
        pub volume_slider: TemplateChild<gtk::Scale>,

        pub clock: Clock,

        // Gesture callbacks on the mini-player: horizontal swipe = skip, and a
        // tap (with no drag) opens the now-playing sheet.
        pub next_cb: RefCell<Option<Box<dyn Fn()>>>,
        pub prev_cb: RefCell<Option<Box<dyn Fn()>>>,
        pub open_cb: RefCell<Option<Box<dyn Fn()>>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for PlaybackWidget {
        const NAME: &'static str = "PlaybackWidget";
        type Type = super::PlaybackWidget;
        type ParentType = gtk::Box;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for PlaybackWidget {
        fn constructed(&self) {
            self.parent_constructed();
            self.now_playing.set_info_visible(true);
            display_add_css_provider(resource!("/components/playback.css"));

            let track_position = self.track_position.clone();
            let track_duration = self.track_duration.clone();
            let motion = gtk::EventControllerMotion::new();
            motion.connect_enter(clone!(
                #[weak]
                track_position,
                #[weak]
                track_duration,
                move |_, _, _| {
                    track_position.set_visible(true);
                    track_duration.set_visible(true);
                }
            ));
            motion.connect_leave(clone!(
                #[weak]
                track_position,
                #[weak]
                track_duration,
                move |_| {
                    track_position.set_visible(false);
                    track_duration.set_visible(false);
                }
            ));
            self.seek_overlay.add_controller(motion);

            // Horizontal swipe on the mini-player = next/previous.
            let swipe = gtk::GestureSwipe::new();
            swipe.connect_swipe(clone!(
                #[weak(rename_to = widget)]
                self,
                move |_, vel_x, vel_y| {
                    if vel_x.abs() < 200.0 || vel_x.abs() <= vel_y.abs() {
                        return;
                    }
                    let cb = if vel_x < 0.0 {
                        widget.next_cb.borrow()
                    } else {
                        widget.prev_cb.borrow()
                    };
                    if let Some(cb) = cb.as_ref() {
                        cb();
                    }
                }
            ));
            self.mobile_bar.add_controller(swipe);

            // A tap (no horizontal drag) opens the now-playing sheet. GestureSwipe
            // doesn't claim the sequence, so we guard the click by drag distance
            // rather than grouping — a swipe moves the pointer and won't open it.
            let click = gtk::GestureClick::new();
            let press_x = std::rc::Rc::new(std::cell::Cell::new(0.0f64));
            click.connect_pressed(clone!(
                #[strong]
                press_x,
                move |_, _, x, _| press_x.set(x)
            ));
            click.connect_released(clone!(
                #[weak(rename_to = widget)]
                self,
                #[strong]
                press_x,
                move |_, _, x, _| {
                    if (x - press_x.get()).abs() < 10.0 {
                        if let Some(cb) = widget.open_cb.borrow().as_ref() {
                            cb();
                        }
                    }
                }
            ));
            self.mobile_bar.add_controller(click);
        }
    }

    impl WidgetImpl for PlaybackWidget {}
    impl BoxImpl for PlaybackWidget {}
}

glib::wrapper! {
    pub struct PlaybackWidget(ObjectSubclass<imp::PlaybackWidget>) @extends gtk::Widget, gtk::Box;
}

impl PlaybackWidget {
    /// Show or hide the entire mini-player strip.
    /// When hidden the widget collapses to zero height so the tab bar
    /// sits flush at the bottom with no gap.
    pub fn set_mini_player_visible(&self, visible: bool) {
        self.set_visible(visible);
    }

    pub fn set_title_and_artist(&self, title: &str, artist: &str) {
        let widget = self.imp();
        widget.now_playing.set_visible(true);
        widget.now_playing.set_title_and_artist(title, artist);
        widget.mobile_title.set_text(title);
        widget.mobile_artist.set_text(artist);
    }

    #[allow(deprecated)] // Image::set_from_pixbuf
    pub fn reset_info(&self) {
        let widget = self.imp();
        widget.now_playing.set_visible(false);
        widget.now_playing.reset_info();
        widget.mobile_title.set_text("");
        widget.mobile_artist.set_text("");
        widget.mobile_art.set_from_pixbuf(None);
        self.set_song_duration(None);
    }

    #[allow(deprecated)] // Image::set_from_pixbuf — no set_from_paintable in this binding
    fn set_artwork(&self, image: &gdk_pixbuf::Pixbuf) {
        let widget = self.imp();
        widget.now_playing.set_artwork(image);
        widget.mobile_art.set_from_pixbuf(Some(image));
    }

    pub fn set_artwork_from_url(&self, url: String, worker: &Worker) {
        let weak_self = self.downgrade();
        worker.send_local_task(async move {
            let loader = ImageLoader::new();
            let result = loader.load_remote(&url, "jpg", 48, 48).await;
            if let (Some(ref _self), Some(ref result)) = (weak_self.upgrade(), result) {
                _self.set_artwork(result);
            }
        });
    }

    pub fn set_song_duration(&self, duration: Option<f64>) {
        let widget = self.imp();
        let class = "seek-bar--active";
        if let Some(duration) = duration {
            self.add_css_class(class);
            widget.seek_bar.set_range(0.0, duration);
            widget.seek_bar.set_value(0.0);
            self.update_track_time(0.0, duration);
            widget.mini_progress.set_fraction(0.0);
        } else {
            self.remove_css_class(class);
            widget.seek_bar.set_range(0.0, 0.0);
            widget.mini_progress.set_fraction(0.0);
        }
    }

    pub fn set_seek_position(&self, pos: f64) {
        let widget = self.imp();
        widget.seek_bar.set_value(pos);
        let duration = widget.seek_bar.adjustment().upper();
        self.update_track_time(pos, duration);
        if duration > 0.0 {
            widget
                .mini_progress
                .set_fraction((pos / duration).clamp(0.0, 1.0));
        }
    }

    fn update_track_time(&self, pos: f64, duration: f64) {
        let widget = self.imp();
        widget.track_position.set_text(&format_duration(pos));
        widget.track_duration.set_text(&format_duration(duration));
    }

    pub fn increment_seek_position(&self) {
        let value = self.imp().seek_bar.value() + 1_000.0;
        self.set_seek_position(value);
    }

    pub fn connect_now_playing_clicked<F>(&self, f: F)
    where
        F: Fn() + Clone + 'static,
    {
        let widget = self.imp();
        let f_desktop = f.clone();
        widget.now_playing.connect_clicked(move |_| f_desktop());
        widget.open_cb.replace(Some(Box::new(f)));
    }

    /// Wire the mini-player "+/check" button to open the add-to-playlist drawer.
    pub fn connect_add_to_playlist<F: Fn() + 'static>(&self, f: F) {
        self.imp().mobile_add.connect_clicked(move |_| f());
    }

    /// Update the +/check button appearance based on liked state.
    /// Liked → green filled circle with check. Not liked → gray circle with +.
    pub fn set_liked(&self, liked: bool) {
        let imp = self.imp();
        if liked {
            imp.mobile_add_icon
                .set_icon_name(Some("object-select-symbolic"));
            imp.mobile_add.add_css_class("add-to-playlist-liked");
            imp.mobile_add.remove_css_class("add-to-playlist-unliked");
        } else {
            imp.mobile_add_icon.set_icon_name(Some("list-add-symbolic"));
            imp.mobile_add.remove_css_class("add-to-playlist-liked");
            imp.mobile_add.add_css_class("add-to-playlist-unliked");
        }
    }

    pub fn connect_seek<Seek>(&self, seek: Seek)
    where
        Seek: Fn(u32) + Clone + 'static,
    {
        let debouncer = Debouncer::new();
        let widget = self.imp();
        widget.seek_bar.set_increments(5_000.0, 10_000.0);
        widget.seek_bar.connect_change_value(clone!(
            #[weak(rename_to = _self)]
            self,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_, _, requested| {
                let duration = _self.imp().seek_bar.adjustment().upper();
                _self.update_track_time(requested, duration);
                let seek = seek.clone();
                debouncer.debounce(200, move || seek(requested as u32));
                glib::Propagation::Proceed
            }
        ));
    }

    pub fn set_playing(&self, is_playing: bool) {
        let widget = self.imp();
        widget.controls.set_playing(is_playing);
        widget.mobile_play_pause.set_icon_name(if is_playing {
            "media-playback-pause-symbolic"
        } else {
            "media-playback-start-symbolic"
        });
        if is_playing {
            widget.clock.start(clone!(
                #[weak(rename_to = _self)]
                self,
                move || _self.increment_seek_position()
            ));
        } else {
            widget.clock.stop();
        }
    }

    pub fn set_repeat_mode(&self, mode: RepeatMode) {
        self.imp().controls.set_repeat_mode(mode);
    }

    pub fn set_shuffled(&self, shuffled: bool) {
        self.imp().controls.set_shuffled(shuffled);
    }

    pub fn set_seekbar_visible(&self, visible: bool) {
        let widget = self.imp();
        widget.seek_bar.set_visible(visible);
    }

    pub fn set_volume(&self, value: f64) {
        let widget = self.imp();
        widget.volume_slider.set_value(value)
    }

    pub fn connect_play_pause<F>(&self, f: F)
    where
        F: Fn() + Clone + 'static,
    {
        let widget = self.imp();
        widget.controls.connect_play_pause(f.clone());
        widget.mobile_play_pause.connect_clicked(move |_| f());
    }

    pub fn connect_prev<F>(&self, f: F)
    where
        F: Fn() + Clone + 'static,
    {
        self.imp().controls.connect_prev(f.clone());
        self.imp().prev_cb.replace(Some(Box::new(f)));
    }

    pub fn connect_next<F>(&self, f: F)
    where
        F: Fn() + Clone + 'static,
    {
        self.imp().controls.connect_next(f.clone());
        self.imp().next_cb.replace(Some(Box::new(f)));
    }

    pub fn connect_shuffle<F>(&self, f: F)
    where
        F: Fn() + Clone + 'static,
    {
        self.imp().controls.connect_shuffle(f);
    }

    pub fn connect_repeat<F>(&self, f: F)
    where
        F: Fn() + Clone + 'static,
    {
        self.imp().controls.connect_repeat(f);
    }

    pub fn connect_volume_changed<F>(&self, f: F)
    where
        F: Fn(f64) + Clone + 'static,
    {
        let debouncer = Debouncer::new();
        let widget = self.imp();
        widget.volume_slider.connect_value_changed(move |scale| {
            // Debounce dispatch: a single mouse-wheel flick emits a burst of
            // high-resolution scroll deltas, each firing `value_changed`.
            // Without this, every delta would fan out to the mixer, the Web
            // API, dconf and an MPRIS `PropertiesChanged` D-Bus signal, which
            // can flood GNOME Shell's media controls and hang the session.
            let value = scale.value();
            let f = f.clone();
            debouncer.debounce(100, move || f(value));
        });
    }
}
