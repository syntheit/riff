use std::rc::Rc;

use gtk::prelude::Cast;

use crate::app::components::{Component, EventListener};
use crate::app::models::ConnectDevice;
use crate::app::state::{Device, LoginEvent, PlaybackAction, PlaybackEvent};
use crate::app::{ActionDispatcher, AppEvent, AppModel};

use super::widget::DeviceSelectorWidget;

pub struct DeviceSelectorModel {
    app_model: Rc<AppModel>,
    dispatcher: Box<dyn ActionDispatcher>,
}

impl DeviceSelectorModel {
    pub fn new(app_model: Rc<AppModel>, dispatcher: Box<dyn ActionDispatcher>) -> Self {
        Self {
            app_model,
            dispatcher,
        }
    }

    pub fn refresh_available_devices(&self) {
        let api = self.app_model.get_spotify();

        self.dispatcher
            .call_spotify_and_dispatch(move || async move {
                api.list_available_devices()
                    .await
                    .map(|devices| PlaybackAction::SetAvailableDevices(devices).into())
            });
    }

    // The available Connect devices EXCLUDING riff's own Spirc device (so the
    // list doesn't double-offer the device the user is already on, which has a
    // separate "This device" / Local entry). `list_available_devices` now
    // returns ALL non-restricted devices including riff's own; the self-filter
    // by id lives here at the display call site, which is collision-free where
    // the old name-based filter in the API layer was not.
    pub fn get_available_devices(&self) -> Vec<ConnectDevice> {
        let own_device_id = self
            .app_model
            .get_state()
            .playback
            .own_device_id()
            .map(|s| s.to_string());
        let devices = self
            .app_model
            .get_state()
            .playback
            .available_devices()
            .clone();
        match own_device_id {
            Some(own_id) => devices
                .iter()
                .filter(|d| d.id != own_id)
                .cloned()
                .collect(),
            None => devices.to_vec(),
        }
    }

    pub fn get_displayed_device(&self) -> Device {
        self.app_model
            .get_state()
            .playback
            .displayed_device()
    }

    pub fn set_current_device(&self, id: Option<String>) {
        let devices = self.get_available_devices();
        let connect_device = id
            .and_then(|id| devices.iter().find(|&d| d.id == id))
            .cloned();
        let device = connect_device.map(Device::Connect).unwrap_or(Device::Local);
        self.dispatcher
            .dispatch(PlaybackAction::SwitchDevice(device).into());
    }
}

pub struct DeviceSelector {
    widget: DeviceSelectorWidget,
    model: Rc<DeviceSelectorModel>,
}

impl DeviceSelector {
    pub fn new(widget: DeviceSelectorWidget, model: DeviceSelectorModel) -> Self {
        let model = Rc::new(model);

        widget.connect_refresh(clone!(
            #[weak]
            model,
            move || {
                model.refresh_available_devices();
            }
        ));

        widget.connect_switch_device(clone!(
            #[weak]
            model,
            move |id| {
                model.set_current_device(id);
            }
        ));

        Self { widget, model }
    }
}

impl Component for DeviceSelector {
    fn get_root_widget(&self) -> &gtk::Widget {
        self.widget.upcast_ref()
    }
}

impl EventListener for DeviceSelector {
    fn on_event(&mut self, event: &AppEvent) {
        match event {
            AppEvent::LoginEvent(LoginEvent::LoginCompleted) => {
                self.model.refresh_available_devices();
            }
            AppEvent::PlaybackEvent(PlaybackEvent::AvailableDevicesChanged) => {
                self.widget
                    .update_devices_list(&self.model.get_available_devices());
            }
            AppEvent::PlaybackEvent(PlaybackEvent::OwnDeviceIdSet(_)) => {
                self.widget
                    .update_devices_list(&self.model.get_available_devices());
            }
            AppEvent::PlaybackEvent(PlaybackEvent::SwitchedDevice(_))
            | AppEvent::PlaybackEvent(PlaybackEvent::RemotePlaybackChanged) => {
                self.widget
                    .set_current_device(&self.model.get_displayed_device());
            }
            _ => (),
        }
    }
}
