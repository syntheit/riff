use gettextrs::*;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{glib, CompositeTemplate};
use libadwaita::subclass::prelude::BinImpl;

use crate::app::components::utils::{format_duration, Clock, Debouncer};
use crate::app::components::DeviceSelectorWidget;
use crate::app::loader::ImageLoader;
use crate::app::models::RepeatMode;
use crate::app::Worker;

use super::now_playing_controls::NowPlayingControlsWidget;

mod imp {

    use super::*;

    #[derive(Debug, Default, CompositeTemplate)]
    #[template(resource = "/dev/diegovsky/Riff/components/now_playing_full.ui")]
    pub struct NowPlayingFullWidget {
        #[template_child]
        pub art: TemplateChild<gtk::Picture>,

        #[template_child]
        pub title: TemplateChild<gtk::Label>,

        #[template_child]
        pub artist: TemplateChild<gtk::Label>,

        #[template_child]
        pub scrubber: TemplateChild<gtk::Scale>,

        #[template_child]
        pub position: TemplateChild<gtk::Label>,

        #[template_child]
        pub duration: TemplateChild<gtk::Label>,

        #[template_child]
        pub controls: TemplateChild<NowPlayingControlsWidget>,

        #[template_child]
        pub queue_button: TemplateChild<gtk::Button>,

        #[template_child]
        pub title_button: TemplateChild<gtk::Button>,

        #[template_child]
        pub artist_button: TemplateChild<gtk::Button>,

        #[template_child]
        pub add_to_playlist_button: TemplateChild<gtk::Button>,

        #[template_child]
        pub add_to_playlist_icon: TemplateChild<gtk::Image>,

        #[template_child]
        pub menu_button: TemplateChild<gtk::Button>,

        #[template_child]
        pub close_button: TemplateChild<gtk::Button>,

        #[template_child]
        pub source_button: TemplateChild<gtk::Button>,

        #[template_child]
        pub source_name_label: TemplateChild<gtk::Label>,

        #[template_child]
        pub device_selector: TemplateChild<DeviceSelectorWidget>,

        #[template_child]
        pub playing_on_label: TemplateChild<gtk::Label>,

        pub clock: Clock,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for NowPlayingFullWidget {
        const NAME: &'static str = "NowPlayingFullWidget";
        type Type = super::NowPlayingFullWidget;
        type ParentType = libadwaita::Bin;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for NowPlayingFullWidget {}
    impl WidgetImpl for NowPlayingFullWidget {}
    impl BinImpl for NowPlayingFullWidget {}
}

glib::wrapper! {
    pub struct NowPlayingFullWidget(ObjectSubclass<imp::NowPlayingFullWidget>) @extends gtk::Widget, libadwaita::Bin;
}

impl NowPlayingFullWidget {
    pub fn set_title_and_artist(&self, title: &str, artist: &str) {
        let imp = self.imp();
        imp.title.set_text(title);
        imp.artist.set_text(artist);
    }

    fn set_artwork(&self, image: &gdk_pixbuf::Pixbuf) {
        let texture = gdk::Texture::for_pixbuf(image);
        self.imp().art.set_paintable(Some(&texture));
    }

    pub fn set_artwork_from_url(&self, url: String, worker: &Worker) {
        let weak_self = self.downgrade();
        worker.send_local_task(async move {
            let loader = ImageLoader::new();
            let result = loader.load_remote(&url, "jpg", 320, 320).await;
            if let (Some(_self), Some(ref result)) = (weak_self.upgrade(), result) {
                _self.set_artwork(result);
            }
        });
    }

    pub fn set_song_duration(&self, duration: Option<f64>) {
        let imp = self.imp();
        if let Some(duration) = duration {
            imp.scrubber.set_range(0.0, duration);
            imp.scrubber.set_value(0.0);
            self.update_track_time(0.0, duration);
        } else {
            imp.scrubber.set_range(0.0, 0.0);
            self.update_track_time(0.0, 0.0);
        }
    }

    pub fn set_seek_position(&self, pos: f64) {
        let imp = self.imp();
        imp.scrubber.set_value(pos);
        let duration = imp.scrubber.adjustment().upper();
        self.update_track_time(pos, duration);
    }

    fn update_track_time(&self, pos: f64, duration: f64) {
        let imp = self.imp();
        imp.position.set_text(&format_duration(pos));
        imp.duration.set_text(&format_duration(duration));
    }

    pub fn increment_seek_position(&self) {
        let value = self.imp().scrubber.value() + 1_000.0;
        self.set_seek_position(value);
    }

    pub fn set_playing(&self, is_playing: bool) {
        let imp = self.imp();
        imp.controls.set_playing(is_playing);
        if is_playing {
            imp.clock.start(clone!(
                #[weak(rename_to = _self)]
                self,
                move || _self.increment_seek_position()
            ));
        } else {
            imp.clock.stop();
        }
    }

    pub fn set_repeat_mode(&self, mode: RepeatMode) {
        self.imp().controls.set_repeat_mode(mode);
    }

    pub fn set_shuffled(&self, shuffled: bool) {
        self.imp().controls.set_shuffled(shuffled);
    }

    pub fn connect_seek<Seek>(&self, seek: Seek)
    where
        Seek: Fn(u32) + Clone + 'static,
    {
        let debouncer = Debouncer::new();
        let imp = self.imp();
        imp.scrubber.set_increments(5_000.0, 10_000.0);
        imp.scrubber.connect_change_value(clone!(
            #[weak(rename_to = _self)]
            self,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |_, _, requested| {
                let duration = _self.imp().scrubber.adjustment().upper();
                _self.update_track_time(requested, duration);
                let seek = seek.clone();
                debouncer.debounce(200, move || seek(requested as u32));
                glib::Propagation::Proceed
            }
        ));
    }

    pub fn connect_play_pause<F: Fn() + Clone + 'static>(&self, f: F) {
        self.imp().controls.connect_play_pause(f);
    }

    pub fn connect_prev<F: Fn() + Clone + 'static>(&self, f: F) {
        self.imp().controls.connect_prev(f);
    }

    pub fn connect_next<F: Fn() + Clone + 'static>(&self, f: F) {
        self.imp().controls.connect_next(f);
    }

    pub fn connect_shuffle<F: Fn() + Clone + 'static>(&self, f: F) {
        self.imp().controls.connect_shuffle(f);
    }

    pub fn connect_repeat<F: Fn() + Clone + 'static>(&self, f: F) {
        self.imp().controls.connect_repeat(f);
    }

    pub fn connect_queue<F: Fn() + 'static>(&self, f: F) {
        self.imp().queue_button.connect_clicked(move |_| f());
    }

    pub fn connect_add_to_playlist<F: Fn() + 'static>(&self, f: F) {
        self.imp()
            .add_to_playlist_button
            .connect_clicked(move |_| f());
    }

    // Update the +/check button appearance based on liked state.
    // Liked → green filled circle with check. Not liked → gray circle with +.
    pub fn set_liked(&self, liked: bool) {
        let imp = self.imp();
        if liked {
            imp.add_to_playlist_icon
                .set_icon_name(Some("object-select-symbolic"));
            imp.add_to_playlist_button
                .add_css_class("add-to-playlist-liked");
            imp.add_to_playlist_button
                .remove_css_class("add-to-playlist-unliked");
        } else {
            imp.add_to_playlist_icon
                .set_icon_name(Some("list-add-symbolic"));
            imp.add_to_playlist_button
                .remove_css_class("add-to-playlist-liked");
            imp.add_to_playlist_button
                .add_css_class("add-to-playlist-unliked");
        }
    }

    pub fn connect_show_menu<F: Fn() + 'static>(&self, f: F) {
        self.imp().menu_button.connect_clicked(move |_| f());
    }

    pub fn connect_close<F: Fn() + 'static>(&self, f: F) {
        self.imp().close_button.connect_clicked(move |_| f());
    }

    pub fn connect_view_album<F: Fn() + 'static>(&self, f: F) {
        self.imp().title_button.connect_clicked(move |_| f());
    }

    pub fn connect_view_artist<F: Fn() + 'static>(&self, f: F) {
        self.imp().artist_button.connect_clicked(move |_| f());
    }

    // Wire the tappable playback-source name. The callback navigates to the
    // current source and closes the sheet.
    pub fn connect_source<F: Fn() + 'static>(&self, f: F) {
        self.imp().source_button.connect_clicked(move |_| f());
    }

    // Set (and show) the source name, or hide it when there is no source. Remote
    // snapshots do not contain a navigable Spotify context, so their album-name
    // fallback is rendered as a disabled header rather than a stale local link.
    pub fn set_source(&self, info: Option<(&str, bool)>) {
        let imp = self.imp();
        match info {
            Some((name, navigable)) => {
                imp.source_name_label.set_text(name);
                imp.source_button.set_sensitive(navigable);
                imp.source_button.set_visible(true);
            }
            None => {
                imp.source_name_label.set_text("");
                imp.source_button.set_sensitive(false);
                imp.source_button.set_visible(false);
            }
        }
    }

    // The embedded Spotify Connect device selector, instantiated as part of this
    // widget's template. Returned so the DeviceSelector component can drive it.
    pub fn device_selector(&self) -> DeviceSelectorWidget {
        self.imp().device_selector.get()
    }

    // Show "Playing on <device>" while a Connect device is active; hide it when
    // playing locally (`name == None`).
    pub fn set_playing_on(&self, name: Option<&str>) {
        let imp = self.imp();
        match name {
            Some(name) => {
                // translators: shown in the now-playing view when playback is on
                // a remote Spotify Connect device; {} is the device name.
                imp.playing_on_label
                    .set_text(&gettext!("Playing on {}", name));
                imp.playing_on_label.set_visible(true);
            }
            None => {
                imp.playing_on_label.set_text("");
                imp.playing_on_label.set_visible(false);
            }
        }
    }
}
