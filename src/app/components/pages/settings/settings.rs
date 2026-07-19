use crate::app::components::shell::headerbar::HeaderBarWidget;
use crate::app::components::{Component, EventListener};
use crate::app::{ActionDispatcher, AppEvent, BrowserAction, BrowserEvent};
use crate::feature_flags::{self, FeatureFlag};
use crate::settings::RiffSettings;

use gettextrs::gettext;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::CompositeTemplate;
use libadwaita::prelude::*;

use super::EqualizerWidget;
use super::PanWidget;
use super::PitchWidget;
use super::SettingsModel;

const SETTINGS: &str = "dev.diegovsky.Riff";

mod imp {

    use super::*;
    use libadwaita::subclass::prelude::*;

    #[derive(Debug, Default, CompositeTemplate)]
    #[template(resource = "/dev/diegovsky/Riff/components/settings.ui")]
    pub struct SettingsDialog {
        #[template_child]
        pub player_bitrate: TemplateChild<libadwaita::ComboRow>,

        #[template_child]
        pub alsa_device: TemplateChild<gtk::Entry>,

        #[template_child]
        pub alsa_device_row: TemplateChild<libadwaita::ActionRow>,

        #[template_child]
        pub audio_backend: TemplateChild<libadwaita::ComboRow>,

        #[template_child]
        pub gapless_playback: TemplateChild<libadwaita::ActionRow>,

        #[template_child]
        pub ap_port: TemplateChild<gtk::Entry>,

        #[template_child]
        pub theme: TemplateChild<libadwaita::ComboRow>,

        #[template_child]
        pub close_behavior: TemplateChild<libadwaita::ComboRow>,

        #[template_child]
        pub volume_curve: TemplateChild<libadwaita::ComboRow>,

        #[template_child]
        pub mono_audio_switch: TemplateChild<libadwaita::SwitchRow>,

        #[template_child]
        pub audio_format: TemplateChild<libadwaita::ComboRow>,

        #[template_child]
        pub normalisation_group: TemplateChild<libadwaita::PreferencesGroup>,

        #[template_child]
        pub normalisation_switch: TemplateChild<libadwaita::SwitchRow>,

        #[template_child]
        pub normalisation_type: TemplateChild<libadwaita::ComboRow>,

        #[template_child]
        pub normalisation_method: TemplateChild<libadwaita::ComboRow>,

        #[template_child]
        pub normalisation_pregain: TemplateChild<libadwaita::SpinRow>,

        #[template_child]
        pub normalisation_threshold: TemplateChild<libadwaita::SpinRow>,

        #[template_child]
        pub normalisation_attack: TemplateChild<libadwaita::SpinRow>,

        #[template_child]
        pub normalisation_release: TemplateChild<libadwaita::SpinRow>,

        #[template_child]
        pub normalisation_knee: TemplateChild<libadwaita::SpinRow>,

        #[template_child]
        pub equalizer: TemplateChild<EqualizerWidget>,

        #[template_child]
        pub pan: TemplateChild<PanWidget>,

        #[template_child]
        pub pitch: TemplateChild<PitchWidget>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for SettingsDialog {
        const NAME: &'static str = "SettingsWindow";
        type Type = super::SettingsDialog;
        type ParentType = libadwaita::PreferencesDialog;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for SettingsDialog {}
    impl WidgetImpl for SettingsDialog {}
    impl AdwDialogImpl for SettingsDialog {}
    impl PreferencesDialogImpl for SettingsDialog {}
}

glib::wrapper! {
    pub struct SettingsDialog(ObjectSubclass<imp::SettingsDialog>) @extends gtk::Widget, libadwaita::Dialog, libadwaita::PreferencesDialog;
}

impl Default for SettingsDialog {
    fn default() -> Self {
        Self::new()
    }
}

impl SettingsDialog {
    pub fn new() -> Self {
        let dialog: Self = glib::Object::new();

        dialog.bind_backend_and_device();
        dialog.bind_settings();
        dialog.bind_feature_flags();
        dialog.apply_feature_flag_visibility();
        dialog.connect_theme_select();
        dialog
    }

    fn apply_feature_flag_visibility(&self) {
        let widget = self.imp();
        widget
            .normalisation_group
            .set_visible(feature_flags::is_enabled(FeatureFlag::Normalisation));
    }

    fn bind_backend_and_device(&self) {
        let widget = self.imp();

        let audio_backend = widget
            .audio_backend
            .downcast_ref::<libadwaita::ComboRow>()
            .unwrap();
        let alsa_device_row = widget
            .alsa_device_row
            .downcast_ref::<libadwaita::ActionRow>()
            .unwrap();

        audio_backend
            .bind_property("selected", alsa_device_row, "visible")
            .transform_to(|_, value: u32| Some(value == 1))
            .build();

        if audio_backend.selected() == 0 {
            alsa_device_row.set_visible(false);
        }
    }

    fn bind_settings(&self) {
        let widget = self.imp();
        let settings = gio::Settings::new(SETTINGS);

        // Binds a GtkAdjustment (from a SpinRow or Scale) to a double GSettings key.
        //
        // We deliberately avoid `Settings::bind` here: its bidirectional binding
        // causes a feedback loop with spin/scale widgets where a single step is
        // written to GSettings and then immediately reverted by the `changed`
        // write-back. Instead we load the initial value and write on every change.
        let bind_double_adjustment = |key: &str, adjustment: &gtk::Adjustment| {
            adjustment.set_value(settings.double(key));
            let settings = settings.clone();
            let key = key.to_owned();
            adjustment.connect_value_changed(move |adj| {
                let _ = settings.set_double(&key, adj.value());
            });
        };

        let player_bitrate = widget
            .player_bitrate
            .downcast_ref::<libadwaita::ComboRow>()
            .unwrap();
        settings
            .bind("player-bitrate", player_bitrate, "selected")
            .mapping(|variant, _| {
                variant.str().map(|s| {
                    match s {
                        "96" => 0,
                        "160" => 1,
                        "320" => 2,
                        _ => unreachable!(),
                    }
                    .to_value()
                })
            })
            .set_mapping(|value, _| {
                value.get::<u32>().ok().map(|u| {
                    match u {
                        0 => "96",
                        1 => "160",
                        2 => "320",
                        _ => unreachable!(),
                    }
                    .to_variant()
                })
            })
            .build();

        let alsa_device = widget.alsa_device.downcast_ref::<gtk::Entry>().unwrap();
        settings.bind("alsa-device", alsa_device, "text").build();

        let audio_backend = widget
            .audio_backend
            .downcast_ref::<libadwaita::ComboRow>()
            .unwrap();
        settings
            .bind("audio-backend", audio_backend, "selected")
            .mapping(|variant, _| {
                variant.str().map(|s| {
                    match s {
                        "pulseaudio" => 0,
                        "alsa" => 1,
                        "gstreamer" => 2,
                        _ => unreachable!(),
                    }
                    .to_value()
                })
            })
            .set_mapping(|value, _| {
                value.get::<u32>().ok().map(|u| {
                    match u {
                        0 => "pulseaudio",
                        1 => "alsa",
                        2 => "gstreamer",
                        _ => unreachable!(),
                    }
                    .to_variant()
                })
            })
            .build();

        let gapless_playback = widget
            .gapless_playback
            .downcast_ref::<libadwaita::ActionRow>()
            .unwrap();
        settings
            .bind(
                "gapless-playback",
                &gapless_playback.activatable_widget().unwrap(),
                "active",
            )
            .build();

        let ap_port = widget.ap_port.downcast_ref::<gtk::Entry>().unwrap();
        settings
            .bind("ap-port", ap_port, "text")
            .mapping(|variant, _| variant.get::<u32>().map(|s| s.to_value()))
            .set_mapping(|value, _| value.get::<u32>().ok().map(|u| u.to_variant()))
            .build();

        // Volume curve
        let volume_curve = widget
            .volume_curve
            .downcast_ref::<libadwaita::ComboRow>()
            .unwrap();
        settings
            .bind("volume-curve", volume_curve, "selected")
            .mapping(|variant, _| {
                variant.str().map(|s| {
                    match s {
                        "log" => 0,
                        "linear" => 1,
                        "cubic" => 2,
                        _ => 0,
                    }
                    .to_value()
                })
            })
            .set_mapping(|value, _| {
                value.get::<u32>().ok().map(|u| {
                    match u {
                        0 => "log",
                        1 => "linear",
                        2 => "cubic",
                        _ => "log",
                    }
                    .to_variant()
                })
            })
            .build();

        // Mono audio
        let mono_audio_switch = widget
            .mono_audio_switch
            .downcast_ref::<libadwaita::SwitchRow>()
            .unwrap();
        settings
            .bind("mono-audio", mono_audio_switch, "active")
            .build();

        // Audio format
        let audio_format = widget
            .audio_format
            .downcast_ref::<libadwaita::ComboRow>()
            .unwrap();
        settings
            .bind("audio-format", audio_format, "selected")
            .mapping(|variant, _| {
                variant.str().map(|s| {
                    match s {
                        "s16" => 0,
                        "s24" => 1,
                        "s24_3" => 2,
                        "s32" => 3,
                        "f32" => 4,
                        "f64" => 5,
                        _ => 0,
                    }
                    .to_value()
                })
            })
            .set_mapping(|value, _| {
                value.get::<u32>().ok().map(|u| {
                    match u {
                        0 => "s16",
                        1 => "s24",
                        2 => "s24_3",
                        3 => "s32",
                        4 => "f32",
                        5 => "f64",
                        _ => "s16",
                    }
                    .to_variant()
                })
            })
            .build();

        // Normalisation
        let normalisation_switch = widget
            .normalisation_switch
            .downcast_ref::<libadwaita::SwitchRow>()
            .unwrap();
        settings
            .bind("normalisation", normalisation_switch, "active")
            .build();

        let normalisation_type = widget
            .normalisation_type
            .downcast_ref::<libadwaita::ComboRow>()
            .unwrap();
        settings
            .bind("normalisation-type", normalisation_type, "selected")
            .mapping(|variant, _| {
                variant.str().map(|s| {
                    match s {
                        "auto" => 0,
                        "track" => 1,
                        "album" => 2,
                        _ => 0,
                    }
                    .to_value()
                })
            })
            .set_mapping(|value, _| {
                value.get::<u32>().ok().map(|u| {
                    match u {
                        0 => "auto",
                        1 => "track",
                        2 => "album",
                        _ => "auto",
                    }
                    .to_variant()
                })
            })
            .build();

        let normalisation_method = widget
            .normalisation_method
            .downcast_ref::<libadwaita::ComboRow>()
            .unwrap();
        settings
            .bind("normalisation-method", normalisation_method, "selected")
            .mapping(|variant, _| {
                variant.str().map(|s| {
                    match s {
                        "dynamic" => 0,
                        "basic" => 1,
                        _ => 0,
                    }
                    .to_value()
                })
            })
            .set_mapping(|value, _| {
                value.get::<u32>().ok().map(|u| {
                    match u {
                        0 => "dynamic",
                        1 => "basic",
                        _ => "dynamic",
                    }
                    .to_variant()
                })
            })
            .build();

        let normalisation_pregain = widget
            .normalisation_pregain
            .downcast_ref::<libadwaita::SpinRow>()
            .unwrap();
        bind_double_adjustment(
            "normalisation-pregain-db",
            &normalisation_pregain.adjustment(),
        );

        let normalisation_threshold = widget
            .normalisation_threshold
            .downcast_ref::<libadwaita::SpinRow>()
            .unwrap();
        bind_double_adjustment(
            "normalisation-threshold-dbfs",
            &normalisation_threshold.adjustment(),
        );

        let normalisation_attack = widget
            .normalisation_attack
            .downcast_ref::<libadwaita::SpinRow>()
            .unwrap();
        bind_double_adjustment(
            "normalisation-attack-ms",
            &normalisation_attack.adjustment(),
        );

        let normalisation_release = widget
            .normalisation_release
            .downcast_ref::<libadwaita::SpinRow>()
            .unwrap();
        bind_double_adjustment(
            "normalisation-release-ms",
            &normalisation_release.adjustment(),
        );

        let normalisation_knee = widget
            .normalisation_knee
            .downcast_ref::<libadwaita::SpinRow>()
            .unwrap();
        bind_double_adjustment("normalisation-knee-db", &normalisation_knee.adjustment());

        let theme = widget.theme.downcast_ref::<libadwaita::ComboRow>().unwrap();
        settings
            .bind("theme-preference", theme, "selected")
            .mapping(|variant, _| {
                variant.str().map(|s| {
                    match s {
                        "light" => 0,
                        "dark" => 1,
                        "system" => 2,
                        _ => unreachable!(),
                    }
                    .to_value()
                })
            })
            .set_mapping(|value, _| {
                value.get::<u32>().ok().map(|u| {
                    match u {
                        0 => "light",
                        1 => "dark",
                        2 => "system",
                        _ => unreachable!(),
                    }
                    .to_variant()
                })
            })
            .build();

        let close_behavior = widget
            .close_behavior
            .downcast_ref::<libadwaita::ComboRow>()
            .unwrap();
        settings
            .bind("close-window-behavior", close_behavior, "selected")
            .mapping(|variant, _| {
                variant.str().map(|s| {
                    match s {
                        "ask" => 0,
                        "minimize-to-background" => 1,
                        "stop-and-quit" => 2,
                        _ => unreachable!(),
                    }
                    .to_value()
                })
            })
            .set_mapping(|value, _| {
                value.get::<u32>().ok().map(|u| {
                    match u {
                        0 => "ask",
                        1 => "minimize-to-background",
                        2 => "stop-and-quit",
                        _ => unreachable!(),
                    }
                    .to_variant()
                })
            })
            .build();
    }

    fn bind_feature_flags(&self) {
        let settings = gio::Settings::new(SETTINGS);
        let group = libadwaita::PreferencesGroup::new();
        group.set_title("Experimental Features");
        group.set_description(Some(
            "These settings require restarting the application to take effect.",
        ));

        for flag in FeatureFlag::ALL
            .iter()
            .filter(|f| !f.is_debug_only() || cfg!(debug_assertions))
        {
            let row = libadwaita::SwitchRow::new();
            row.set_title(flag.title());
            row.set_subtitle(flag.description());
            settings.bind(flag.key(), &row, "active").build();
            group.add(&row);
        }

        let page = self
            .upcast_ref::<libadwaita::PreferencesDialog>()
            .visible_page()
            .unwrap();
        page.add(&group);
    }

    fn connect_theme_select(&self) {
        let widget = self.imp();
        let theme = widget.theme.downcast_ref::<libadwaita::ComboRow>().unwrap();
        theme.connect_selected_notify(|theme| {
            debug!("Theme switched! --> value: {}", theme.selected());
            let manager = libadwaita::StyleManager::default();

            let pref = match theme.selected() {
                0 => libadwaita::ColorScheme::ForceLight,
                1 => libadwaita::ColorScheme::ForceDark,
                _ => libadwaita::ColorScheme::Default,
            };

            manager.set_color_scheme(pref);
        });
    }
}

/// Settings hosted as an in-app navigation page pushed onto `root_nav`.
/// The `SettingsDialog` is never presented as a dialog — it is only used as a
/// convenient container for the GSettings bindings and the `Adw.PreferencesPage`
/// child widget, which is extracted and re-hosted inside an `Adw.ToolbarView`.
pub struct SettingsPage {
    // Kept alive so its GObject signal handlers and GSettings bindings stay valid.
    _dialog: SettingsDialog,
    // Kept alive so its `connect_go_back` closure (capturing the dispatcher) stays valid.
    _headerbar: HeaderBarWidget,
    model: SettingsModel,
    settings_snapshot: RiffSettings,
    root: gtk::Widget,
}

impl SettingsPage {
    pub fn new(model: SettingsModel, dispatcher: Box<dyn ActionDispatcher>) -> Self {
        let dialog = SettingsDialog::new();

        // Extract the single preferences page from the dialog before it is
        // ever realized or presented, then unparent it so we can re-host it.
        let page = dialog
            .upcast_ref::<libadwaita::PreferencesDialog>()
            .visible_page()
            .expect("SettingsDialog must have a visible page");
        page.unparent();

        // Wrap in a Clamp so the preferences page never exceeds screen width.
        // Without this the EQ horizontal Scale box forces a wide natural size
        // and the page overflows horizontally when re-hosted outside its dialog.
        let clamp = libadwaita::Clamp::new();
        clamp.set_maximum_size(600);
        clamp.set_child(Some(&page));
        clamp.set_hexpand(true);

        let scrolled = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&clamp)
            .build();

        // Reuse the shared HeaderBarWidget so we get a back button wired to
        // NavigationPop, matching the detail pages' pattern exactly.
        let headerbar = HeaderBarWidget::new();
        headerbar.set_title(Some(&gettext("Settings")));
        // Settings pages never have a selection mode.
        headerbar.set_selection_possible(false);
        // Back button visibility: settings is always pushed on top of tabs,
        // so there is always something to pop back to.
        headerbar.set_can_go_back(true);

        let dispatcher_clone = dispatcher.box_clone();
        headerbar.connect_go_back(move || {
            dispatcher_clone.dispatch(BrowserAction::NavigationPop.into());
        });

        let toolbar_view = libadwaita::ToolbarView::new();
        toolbar_view.add_top_bar(headerbar.upcast_ref::<gtk::Widget>());
        toolbar_view.set_content(Some(&scrolled));

        let snapshot = model.settings();

        Self {
            _dialog: dialog,
            _headerbar: headerbar,
            model,
            settings_snapshot: snapshot,
            root: toolbar_view.upcast(),
        }
    }

    fn on_navigated_away(&self) {
        let new_settings = RiffSettings::new_from_gsettings().unwrap_or_default();
        if self
            .settings_snapshot
            .player_settings
            .requires_reload(&new_settings.player_settings)
        {
            self.model.stop_player();
        }
        self.model.set_settings();
    }
}

impl Component for SettingsPage {
    fn get_root_widget(&self) -> &gtk::Widget {
        &self.root
    }
}

impl EventListener for SettingsPage {
    fn on_event(&mut self, event: &AppEvent) {
        // When the user navigates back, commit the settings changes.
        if matches!(
            event,
            AppEvent::BrowserEvent(BrowserEvent::NavigationPopped)
        ) {
            self.on_navigated_away();
        }
    }
}
