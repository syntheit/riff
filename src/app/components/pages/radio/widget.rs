// Widget for the "song radio" station page.
//
// Wraps a DetailsPageComponent (the shared song-list detail layout used by Liked
// Songs / playlists). The track list is populated from RadioState, which the
// player thread fills via BrowserAction::SetRadioTracks, so there is nothing to
// fetch on open — the component just renders whatever is already in state and
// refreshes on BrowserEvent::RadioTracksLoaded.

use std::rc::Rc;

use super::RadioModel;
use crate::app::components::{Component, DetailsPageComponent, EventListener, HasHeaderBarModel};
use crate::app::AppEvent;
use crate::app::Worker;

/// GTK widget for the radio station detail page.
pub struct Radio {
    #[allow(dead_code)]
    model: Rc<RadioModel>,
    component: DetailsPageComponent<RadioModel>,
}

impl Radio {
    pub fn new(model: Rc<RadioModel>, worker: Worker) -> Self {
        let mut component =
            DetailsPageComponent::new(model.clone(), model.to_headerbar_model(), worker);
        component.create_playlist(None);
        Self { model, component }
    }
}

impl Component for Radio {
    fn get_root_widget(&self) -> &gtk::Widget {
        self.component.get_root_widget()
    }
    fn get_children(&mut self) -> Option<&mut Vec<Box<dyn EventListener>>> {
        self.component.get_children()
    }
}

impl EventListener for Radio {
    fn on_event(&mut self, event: &AppEvent) {
        self.component.handle_event(event);
        self.broadcast_event(event);
    }
}
