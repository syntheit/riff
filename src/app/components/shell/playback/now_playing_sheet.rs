use std::ops::Deref;
use std::rc::Rc;

use gtk::prelude::*;

use crate::app::components::EventListener;
use crate::app::models::{RepeatMode, SongDescription};
use crate::app::state::{PlaybackAction, PlaybackEvent};
use crate::app::{ActionDispatcher, AppAction, AppEvent, AppModel, AppState, Worker};

use super::now_playing_full::NowPlayingFullWidget;

// The libadwaita 0.7 Rust binding predates AdwBottomSheet (1.6), so the sheet is
// created in the blueprint and driven here through the generic `open` property.
fn set_sheet_open(sheet: &gtk::Widget, open: bool) {
    sheet.set_property("open", open);
}

pub struct NowPlayingSheetModel {
    app_model: Rc<AppModel>,
    dispatcher: Box<dyn ActionDispatcher>,
}

impl NowPlayingSheetModel {
    pub fn new(app_model: Rc<AppModel>, dispatcher: Box<dyn ActionDispatcher>) -> Self {
        Self {
            app_model,
            dispatcher,
        }
    }

    fn state(&self) -> impl Deref<Target = AppState> + '_ {
        self.app_model.get_state()
    }

    fn toggle_playback(&self) {
        self.dispatcher.dispatch(PlaybackAction::TogglePlay.into());
    }

    fn play_next(&self) {
        self.dispatcher.dispatch(PlaybackAction::Next.into());
    }

    fn play_prev(&self) {
        self.dispatcher.dispatch(PlaybackAction::Previous.into());
    }

    fn toggle_shuffle(&self) {
        self.dispatcher
            .dispatch(PlaybackAction::ToggleShuffle.into());
    }

    fn toggle_repeat(&self) {
        self.dispatcher
            .dispatch(PlaybackAction::ToggleRepeat.into());
    }

    fn seek_to(&self, position: u32) {
        self.dispatcher
            .dispatch(PlaybackAction::Seek(position).into());
    }

    fn view_queue(&self) {
        // Pushes the queue page on top of the tab shell.
        self.dispatcher.dispatch(AppAction::ViewNowPlaying);
    }

    fn is_playing(&self) -> bool {
        self.state().playback.is_playing()
    }

    fn is_shuffled(&self) -> bool {
        self.state().playback.is_shuffled()
    }

    fn repeat_mode(&self) -> RepeatMode {
        self.state().playback.repeat_mode()
    }

    fn current_song(&self) -> Option<SongDescription> {
        self.state().playback.current_song()
    }
}

pub struct NowPlayingSheet {
    model: Rc<NowPlayingSheetModel>,
    widget: NowPlayingFullWidget,
    sheet: gtk::Widget,
    worker: Worker,
    // Last reported playback position, so the scrubber shows the right spot when
    // the sheet is opened mid-track (its clock/seek events fill in from there).
    last_position: u32,
}

impl NowPlayingSheet {
    pub fn new(
        model: NowPlayingSheetModel,
        sheet: gtk::Widget,
        widget: NowPlayingFullWidget,
        worker: Worker,
    ) -> Self {
        let model = Rc::new(model);

        widget.connect_play_pause(clone!(
            #[weak]
            model,
            move || model.toggle_playback()
        ));
        widget.connect_next(clone!(
            #[weak]
            model,
            move || model.play_next()
        ));
        widget.connect_prev(clone!(
            #[weak]
            model,
            move || model.play_prev()
        ));
        widget.connect_shuffle(clone!(
            #[weak]
            model,
            move || model.toggle_shuffle()
        ));
        widget.connect_repeat(clone!(
            #[weak]
            model,
            move || model.toggle_repeat()
        ));
        widget.connect_seek(clone!(
            #[weak]
            model,
            move |position| model.seek_to(position)
        ));
        widget.connect_queue(clone!(
            #[weak]
            model,
            #[weak]
            sheet,
            move || {
                model.view_queue();
                set_sheet_open(&sheet, false);
            }
        ));
        widget.connect_close(clone!(
            #[weak]
            sheet,
            move || set_sheet_open(&sheet, false)
        ));

        Self {
            model,
            widget,
            sheet,
            worker,
            last_position: 0,
        }
    }

    fn update_current_info(&self) {
        if let Some(song) = self.model.current_song() {
            self.widget
                .set_title_and_artist(&song.title, &song.artists_name());
            self.widget.set_song_duration(Some(song.duration_ms as f64));
            if let Some(url) = song.art.as_ref().and_then(|s| s.best_for_width(320)) {
                self.widget
                    .set_artwork_from_url(url.to_owned(), &self.worker);
            }
        }
    }

    // Populate the whole view from current state — used when opening the sheet,
    // since the playback events that fill it may have fired before it was shown.
    fn sync_all(&self) {
        self.widget.set_playing(self.model.is_playing());
        self.widget.set_shuffled(self.model.is_shuffled());
        self.widget.set_repeat_mode(self.model.repeat_mode());
        self.update_current_info();
        self.widget.set_seek_position(self.last_position as f64);
    }
}

impl EventListener for NowPlayingSheet {
    fn on_event(&mut self, event: &AppEvent) {
        match event {
            AppEvent::PlaybackEvent(PlaybackEvent::PlaybackPaused)
            | AppEvent::PlaybackEvent(PlaybackEvent::PlaybackResumed) => {
                self.widget.set_playing(self.model.is_playing());
            }
            AppEvent::PlaybackEvent(PlaybackEvent::RepeatModeChanged(mode)) => {
                self.widget.set_repeat_mode(*mode);
            }
            AppEvent::PlaybackEvent(PlaybackEvent::ShuffleChanged(_)) => {
                self.widget.set_shuffled(self.model.is_shuffled());
            }
            AppEvent::PlaybackEvent(PlaybackEvent::TrackChanged(_)) => {
                self.last_position = 0;
                self.widget.set_playing(self.model.is_playing());
                self.update_current_info();
            }
            AppEvent::PlaybackEvent(PlaybackEvent::PlaybackStopped) => {
                self.widget.set_playing(self.model.is_playing());
                self.update_current_info();
            }
            AppEvent::PlaybackEvent(PlaybackEvent::SeekSynced(pos))
            | AppEvent::PlaybackEvent(PlaybackEvent::TrackSeeked(pos)) => {
                self.last_position = *pos;
                self.widget.set_seek_position(*pos as f64);
            }
            AppEvent::NowPlayingSheetShown => {
                self.sync_all();
                set_sheet_open(&self.sheet, true);
            }
            _ => {}
        }
    }
}
