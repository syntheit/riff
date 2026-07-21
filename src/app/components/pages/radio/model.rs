// Model for the "song radio" station page.
//
// Unlike the other detail pages, a radio station's tracks are NOT fetched from
// the Web API here: they are resolved on the player thread (librespot internal
// API) and stored into `RadioState` via `BrowserAction::SetRadioTracks`. This
// model only reads that list and drives playback/selection through the shared
// DetailsPage framework, mirroring the SavedTracks (Liked Songs) page.

use gettextrs::gettext;
use std::ops::Deref;
use std::rc::Rc;

use crate::{impl_playlist_model_base, impl_toggle_play};

use crate::app::components::DetailsPageModel;
use crate::app::components::{
    HasHeaderBarModel, HeaderImageShape, PageModel, PlaylistModel, SimpleHeaderBarModel,
};
use crate::app::models::*;
use crate::app::state::SelectionContext;
use crate::app::state::{BrowserEvent, PlaybackAction, SelectionAction, SelectionState};
use crate::app::{ActionDispatcher, AppAction, AppEvent, AppModel, SongsSource};

/// Data model for the radio station page. Composes `DetailsPageModel` via Deref.
///
/// `id` (from the base) is the seed track's base62 id — the station's identity,
/// matching `SongsSource::Radio { seed_id }` and `ScreenName::Radio { seed_id }`.
pub struct RadioModel {
    base: DetailsPageModel,
    seed_name: String,
}

impl Deref for RadioModel {
    type Target = DetailsPageModel;
    fn deref(&self) -> &Self::Target {
        &self.base
    }
}

impl HasHeaderBarModel for RadioModel {}

impl RadioModel {
    pub fn new(
        seed_id: String,
        seed_name: String,
        app_model: Rc<AppModel>,
        dispatcher: Box<dyn ActionDispatcher>,
    ) -> Self {
        Self {
            base: DetailsPageModel::new(seed_id, app_model, dispatcher),
            seed_name,
        }
    }

    fn source(&self) -> SongsSource {
        SongsSource::Radio {
            seed_id: self.id.clone(),
            seed_name: self.seed_name.clone(),
        }
    }
}

impl PageModel for RadioModel {
    fn get_title(&self) -> Option<String> {
        // translators: {} is the seed track title the station is based on.
        Some(gettextrs::gettext!("Radio · based on {}", self.seed_name))
    }

    fn get_subtitle(&self) -> Option<String> {
        let count = PlaylistModel::song_list_model(self).len();
        Some(gettextrs::ngettext!(
            "{} Track",
            "{} Tracks",
            count as u32,
            count
        ))
    }

    fn get_caption(&self) -> Option<String> {
        Some(gettext("Radio"))
    }

    fn header_image_shape(&self) -> HeaderImageShape {
        HeaderImageShape::Square
    }

    fn default_icon(&self) -> Option<&str> {
        // Same broadcast-style glyph as the "Go to radio" song-menu row.
        Some("network-cellular-signal-excellent-symbolic")
    }

    fn is_loaded(&self) -> bool {
        // Always "loaded": the tracks are pushed in from the player thread, there
        // is nothing to fetch on open. Graceful degradation: even a seed-only
        // station renders (the list simply has one row).
        true
    }

    fn has_play_button(&self) -> bool {
        true
    }

    fn source_is_playing(&self) -> bool {
        matches!(
            self.app_model.get_state().playback.current_source(),
            Some(SongsSource::Radio { seed_id, .. }) if seed_id == &self.id
        )
    }

    impl_toggle_play!();

    fn should_refresh_details(&self, event: &AppEvent) -> bool {
        matches!(
            event,
            AppEvent::BrowserEvent(BrowserEvent::RadioTracksLoaded(id)) if id == &self.id
        )
    }
}

impl PlaylistModel for RadioModel {
    fn song_list_model(&self) -> SongListModel {
        self.app_model
            .get_state()
            .browser
            .radio_state(&self.id)
            .expect("illegal attempt to read radio_state")
            .songs
            .clone()
    }

    fn autoscroll_to_playing(&self) -> bool {
        true
    }

    impl_playlist_model_base!();

    fn enable_selection(&self) -> bool {
        self.enable_selection_with_context(SelectionContext::Default)
    }

    fn play_song_at(&self, pos: usize, id: &str) {
        // Tapping a track plays it with the WHOLE station as the queue/context and
        // sets the playback source to this radio (so the "Playing from radio"
        // header shows and back-navigates here). The station is flat and not
        // paged, so load the full list as one batch.
        let batch = PlaylistModel::song_list_model(self).song_batch_for(pos);
        if let Some(batch) = batch {
            self.dispatcher
                .dispatch(PlaybackAction::LoadPagedSongs(self.source(), batch).into());
            self.dispatcher
                .dispatch(PlaybackAction::Load(id.to_string()).into());
        }
    }

    fn actions_for(&self, _song: &SongDescription) -> Option<gio::ActionGroup> {
        None
    }

    fn open_song_menu(&self, song: &SongDescription) {
        self.dispatcher
            .dispatch(AppAction::ShowSongMenu(song.clone()));
    }
}

impl SimpleHeaderBarModel for RadioModel {
    fn title(&self) -> Option<String> {
        PageModel::get_title(self)
    }
    fn title_updated(&self, event: &AppEvent) -> bool {
        PageModel::should_refresh_details(self, event)
    }

    fn selection_context(&self) -> Option<SelectionContext> {
        None
    }

    fn select_all(&self) {
        let songs: Vec<SongDescription> = PlaylistModel::song_list_model(self).collect();
        self.dispatcher
            .dispatch(SelectionAction::Select(songs).into());
    }
}
