mod component;
mod now_playing_full;
mod now_playing_sheet;
mod playback_controls;
mod playback_info;
mod playback_info_mobile;
mod playback_widget;
pub use component::*;
pub use now_playing_full::NowPlayingFullWidget;
pub use now_playing_sheet::{NowPlayingSheet, NowPlayingSheetModel};

use glib::prelude::*;

pub fn expose_widgets() {
    playback_controls::PlaybackControlsWidget::static_type();
    playback_widget::PlaybackWidget::static_type();
    now_playing_full::NowPlayingFullWidget::static_type();
}
