// Domain models
mod main;
use glib::subclass::types::ObjectSubclassIsExt;
pub use main::*;

// Shared enums (used by UI, state, and settings)
mod card_enums;
pub use card_enums::*;

// UI models (GObject)
mod songs;
pub use songs::*;

mod card_model;
pub use card_model::*;

use crate::app::components::card::IMAGE_SIZE;

/// A plain (non-GObject) snapshot of a library item, carried by the
/// `ShowLibraryItemMenu` action so the long-press drawer can render the item card
/// and pick the right actions (unfollow artist vs unsave album/playlist) without
/// re-reading the store. `kind` disambiguates the "Remove from library" action.
#[derive(Clone, Debug)]
pub struct LibraryItem {
    pub id: String,
    pub title: String,
    pub subtitle: String,
    pub art: Option<String>,
    pub kind: CardKind,
    pub pinned: bool,
}

impl From<&AlbumDescription> for CardModel {
    fn from(album: &AlbumDescription) -> Self {
        let art = album
            .art
            .as_ref()
            .and_then(|s| s.best_for_width(IMAGE_SIZE))
            .map(str::to_owned);
        CardModel::new(
            &album.id,
            art.as_ref(),
            &album.title,
            &album.artists_name(),
            album.release_date.as_deref(),
            Some(album.popularity),
            None,
        )
        .with_kind(CardKind::Album)
        .with_track_count(album.songs.batch.total as u32)
    }
}

impl From<AlbumDescription> for CardModel {
    fn from(album: AlbumDescription) -> Self {
        Self::from(&album)
    }
}

impl From<&PlaylistDescription> for CardModel {
    fn from(playlist: &PlaylistDescription) -> Self {
        let art = playlist
            .art
            .as_ref()
            .and_then(|s| s.best_for_width(IMAGE_SIZE))
            .map(str::to_owned);
        CardModel::new(
            &playlist.id,
            art.as_ref(),
            &playlist.title,
            &playlist.owner.display_name,
            None,
            None,
            None,
        )
        .with_kind(CardKind::Playlist)
        .with_track_count(playlist.songs.batch.total as u32)
        .with_snapshot_id(playlist.snapshot_id.clone())
    }
}

impl From<PlaylistDescription> for PlaylistSummary {
    fn from(PlaylistDescription { id, title, .. }: PlaylistDescription) -> Self {
        Self { id, title }
    }
}

impl From<PlaylistDescription> for CardModel {
    fn from(playlist: PlaylistDescription) -> Self {
        Self::from(&playlist)
    }
}

impl From<SongDescription> for SongModel {
    fn from(song: SongDescription) -> Self {
        SongModel::new(song)
    }
}

impl From<&SongDescription> for SongModel {
    fn from(song: &SongDescription) -> Self {
        SongModel::new(song.clone())
    }
}

impl From<&ArtistSummary> for CardModel {
    fn from(artist: &ArtistSummary) -> Self {
        let photo = artist
            .photo
            .as_ref()
            .and_then(|s| s.best_for_width(IMAGE_SIZE))
            .map(str::to_owned);
        CardModel::new(
            &artist.id,
            photo.as_ref(),
            &artist.name,
            "",
            None,
            Some(artist.popularity),
            None,
        )
        .with_kind(CardKind::Artist)
        .with_round_image(true)
    }
}

impl From<&JumpBackContext> for CardModel {
    fn from(context: &JumpBackContext) -> Self {
        let art = context
            .art
            .as_ref()
            .and_then(|s| s.best_for_width(IMAGE_SIZE))
            .map(str::to_owned);
        let kind = match context.kind {
            JumpBackKind::Album => CardKind::Album,
            JumpBackKind::Playlist => CardKind::Playlist,
        };
        CardModel::new(
            &context.id,
            art.as_ref(),
            &context.title,
            &context.subtitle,
            None,
            None,
            None,
        )
        .with_kind(kind)
    }
}

impl From<&SongDescription> for CardModel {
    fn from(desc: &SongDescription) -> Self {
        let photo = desc
            .art
            .as_ref()
            .and_then(|s| s.best_for_width(IMAGE_SIZE))
            .map(str::to_owned);
        CardModel::new(
            &desc.id,
            photo.as_ref(),
            &desc.title,
            &desc.artists_name(),
            None,
            None,
            None,
        )
    }
}

/// Build a track card for the home feed's track strips (recently played, top
/// tracks). It shows the track's title and album art but carries the *album*
/// id and kind, so tapping it opens the containing album rather than a track id
/// that resolves to nothing.
pub fn song_album_card(desc: &SongDescription) -> CardModel {
    let art = desc
        .art
        .as_ref()
        .and_then(|s| s.best_for_width(IMAGE_SIZE))
        .map(str::to_owned);
    CardModel::new(
        &desc.album.id,
        art.as_ref(),
        &desc.title,
        &desc.artists_name(),
        None,
        None,
        None,
    )
    .with_kind(CardKind::Album)
}
