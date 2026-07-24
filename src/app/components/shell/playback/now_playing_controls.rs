use gettextrs::gettext;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{glib, CompositeTemplate};

use crate::app::models::RepeatMode;

mod imp {

    use super::*;

    #[derive(Debug, Default, CompositeTemplate)]
    #[template(resource = "/dev/diegovsky/Riff/components/now_playing_controls.ui")]
    pub struct NowPlayingControlsWidget {
        #[template_child]
        pub play_pause: TemplateChild<gtk::Button>,

        #[template_child]
        pub next: TemplateChild<gtk::Button>,

        #[template_child]
        pub prev: TemplateChild<gtk::Button>,

        #[template_child]
        pub shuffle: TemplateChild<gtk::ToggleButton>,

        #[template_child]
        pub repeat: TemplateChild<gtk::Button>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for NowPlayingControlsWidget {
        const NAME: &'static str = "NowPlayingControlsWidget";
        type Type = super::NowPlayingControlsWidget;
        type ParentType = gtk::Box;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for NowPlayingControlsWidget {}
    impl WidgetImpl for NowPlayingControlsWidget {}
    impl BoxImpl for NowPlayingControlsWidget {}
}

glib::wrapper! {
    pub struct NowPlayingControlsWidget(ObjectSubclass<imp::NowPlayingControlsWidget>) @extends gtk::Widget, gtk::Box;
}

impl NowPlayingControlsWidget {
    pub fn set_playing(&self, is_playing: bool) {
        let playback_icon = if is_playing {
            "media-playback-pause-symbolic"
        } else {
            "media-playback-start-symbolic"
        };

        let translated_tooltip = if is_playing {
            gettext("Pause")
        } else {
            gettext("Play")
        };

        let imp = self.imp();
        imp.play_pause.set_icon_name(playback_icon);
        imp.play_pause
            .set_tooltip_text(Some(translated_tooltip.as_str()));
    }

    pub fn set_shuffled(&self, shuffled: bool) {
        self.imp().shuffle.set_active(shuffled);
    }

    pub fn set_repeat_mode(&self, mode: RepeatMode) {
        let repeat_mode_icon = match mode {
            RepeatMode::Song => "media-playlist-repeat-song-symbolic",
            RepeatMode::Playlist => "media-playlist-repeat-symbolic",
            RepeatMode::None => "media-playlist-consecutive-symbolic",
        };

        self.imp().repeat.set_icon_name(repeat_mode_icon);
    }

    pub fn connect_play_pause<F>(&self, f: F)
    where
        F: Fn() + 'static,
    {
        self.imp().play_pause.connect_clicked(move |_| f());
    }

    pub fn connect_prev<F>(&self, f: F)
    where
        F: Fn() + 'static,
    {
        self.imp().prev.connect_clicked(move |_| f());
    }

    pub fn connect_next<F>(&self, f: F)
    where
        F: Fn() + 'static,
    {
        self.imp().next.connect_clicked(move |_| f());
    }

    pub fn connect_shuffle<F>(&self, f: F)
    where
        F: Fn() + 'static,
    {
        self.imp().shuffle.connect_clicked(move |_| f());
    }

    pub fn connect_repeat<F>(&self, f: F)
    where
        F: Fn() + 'static,
    {
        self.imp().repeat.connect_clicked(move |_| f());
    }
}
