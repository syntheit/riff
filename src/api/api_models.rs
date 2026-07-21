use form_urlencoded::Serializer;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    convert::{Into, TryFrom, TryInto},
    vec::IntoIter,
};

use crate::app::models::*;

#[derive(Serialize)]
pub struct PlaylistDetails {
    pub name: String,
}

#[derive(Serialize)]
pub struct Uris {
    pub uris: Vec<String>,
}

#[derive(Serialize)]
pub struct PlayOffset {
    pub position: u32,
}

#[derive(Serialize)]
#[serde(untagged)]
pub enum PlayRequest {
    Contextual {
        context_uri: String,
        offset: PlayOffset,
    },
    Uris {
        uris: Vec<String>,
        offset: PlayOffset,
    },
}

#[derive(Serialize)]
pub struct Ids {
    pub ids: Vec<String>,
}

#[derive(Serialize)]
pub struct Name<'a> {
    pub name: &'a str,
}

pub struct SearchQuery {
    pub query: String,
    pub limit: usize,
    pub offset: usize,
}

impl SearchQuery {
    pub fn into_query_string(self) -> String {
        let types = "album,track,artist,playlist";

        let re = Regex::new(r"(\W|\s)+").unwrap();
        let query = re.replace_all(&self.query[..], " ");

        let serialized = Serializer::new(String::new())
            .append_pair("q", query.as_ref())
            .append_pair("offset", &self.offset.to_string()[..])
            .append_pair("limit", &self.limit.to_string()[..])
            // Omit `market`: a user-token request defaults to the account's own
            // market automatically, so hardcoding any region (e.g. US) would
            // mislocate results for non-US accounts (e.g. BR).
            .finish();

        format!("type={types}&{serialized}")
    }
}

#[derive(Deserialize, Debug, Clone)]
#[serde(bound(deserialize = "T: Deserialize<'de>"))]
pub struct Page<T> {
    // Spotify occasionally returns `null` entries inside an items list (e.g. an
    // unavailable/region-locked saved playlist or a delisted track). A plain
    // `Vec<T>` would fail to deserialize on the first null ("invalid type: null,
    // expected struct …"), taking the whole page down. Deserialize each element as
    // an `Option<T>` and drop the `None`s so one bad item never blows up the list.
    #[serde(default, deserialize_with = "deserialize_nullable_items")]
    items: Option<Vec<T>>,
    offset: Option<usize>,
    limit: Option<usize>,
    total: usize,
}

/// Deserialize an `items` array that may contain `null` elements, dropping the
/// nulls. Used by every `Page<T>` so a single null entry from Spotify doesn't fail
/// the whole page. A missing/`null` array itself deserializes to `None`.
fn deserialize_nullable_items<'de, D, T>(deserializer: D) -> Result<Option<Vec<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    let maybe: Option<Vec<Option<T>>> = Option::deserialize(deserializer)?;
    Ok(maybe.map(|items| items.into_iter().flatten().collect()))
}

impl<T> Page<T> {
    pub(crate) fn new(items: Vec<T>) -> Self {
        let l = items.len();
        Self {
            total: l,
            items: Some(items),
            offset: Some(0),
            limit: Some(l),
        }
    }

    fn map<Mapper, U>(self, mapper: Mapper) -> Page<U>
    where
        Mapper: Fn(T) -> U,
    {
        let Page {
            items,
            offset,
            limit,
            total,
        } = self;
        Page {
            items: items.map(|item| item.into_iter().map(mapper).collect()),
            offset,
            limit,
            total,
        }
    }

    pub fn limit(&self) -> usize {
        self.limit
            .or_else(|| Some(self.items.as_ref()?.len()))
            .filter(|limit| *limit > 0)
            .unwrap_or(50)
    }

    pub fn total(&self) -> usize {
        self.total
    }

    pub fn offset(&self) -> usize {
        self.offset.unwrap_or(0)
    }
}

impl<T> IntoIterator for Page<T> {
    type Item = T;
    type IntoIter = IntoIter<Self::Item>;

    fn into_iter(self) -> Self::IntoIter {
        self.items.unwrap_or_default().into_iter()
    }
}

impl<T> Default for Page<T> {
    fn default() -> Self {
        Self {
            items: None,
            total: 0,
            offset: Some(0),
            limit: Some(0),
        }
    }
}

#[derive(Deserialize, Debug, Clone)]
pub struct Cursors {
    pub after: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
#[serde(bound(deserialize = "T: Deserialize<'de>"))]
pub struct CursorPage<T> {
    // Same null-item tolerance as `Page<T>` (see `deserialize_nullable_items`).
    #[serde(default, deserialize_with = "deserialize_nullable_items")]
    pub items: Option<Vec<T>>,
    pub cursors: Option<Cursors>,
    #[allow(dead_code)] // Part of the Spotify API response but currently unused
    pub limit: Option<usize>,
    #[allow(dead_code)] // Part of the Spotify API response but currently unused
    pub total: Option<usize>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct FollowedArtistsInner {
    pub artists: CursorPage<Artist>,
}

pub(crate) trait WithImages {
    fn images(&self) -> &[Image];
}

#[derive(Deserialize, Debug, Clone)]
pub struct Playlist {
    pub id: String,
    pub name: String,
    pub images: Option<Vec<Image>>,
    #[serde(default)]
    pub tracks: Option<Page<PlaylistTrack>>,
    pub owner: PlaylistOwner,
    pub snapshot_id: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct PlaylistOwner {
    pub id: String,
    pub display_name: String,
}

const EMPTY_IMAGE: &'static [Image] = &[Image {
    url: String::new(),
    height: Some(640),
    width: Some(640),
}];

impl WithImages for Playlist {
    fn images(&self) -> &[Image] {
        match &self.images {
            Some(x) => &x[..],
            None => &EMPTY_IMAGE[..],
        }
    }
}

#[derive(Deserialize, Debug, Clone)]
pub struct PlaylistTrack {
    pub is_local: bool,
    pub track: Option<FailibleTrackItem>,
}

// Minimal track item used when only the id is needed (fields-filtered endpoint).
#[derive(Deserialize, Debug, Clone)]
pub struct PlaylistTrackId {
    pub track: Option<PlaylistTrackIdInner>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct PlaylistTrackIdInner {
    pub id: Option<String>,
    // Present when the track was relinked for the market/context. Its id is the
    // one the playlist was built with, so both ids must be indexed to match.
    pub linked_from: Option<LinkedFrom>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct LinkedFrom {
    pub id: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct SavedTrack {
    pub added_at: String,
    pub track: TrackItem,
}

#[derive(Deserialize, Debug, Clone)]
pub struct SavedAlbum {
    pub album: Album,
}

#[derive(Deserialize, Debug, Clone)]
pub struct FullAlbum {
    #[serde(flatten)]
    pub album: Album,
    #[serde(flatten)]
    pub album_info: AlbumInfo,
}

#[derive(Deserialize, Debug, Clone)]
pub struct Album {
    pub id: String,
    pub tracks: Option<Page<AlbumTrackItem>>,
    pub artists: Vec<Artist>,
    pub release_date: Option<String>,
    pub name: String,
    pub images: Vec<Image>,
    #[serde(default)]
    pub popularity: u32,
}

#[derive(Deserialize, Debug, Clone)]
pub struct AlbumInfo {
    // `label`, `copyrights`, and `total_tracks` can be absent in some API
    // responses (dev-mode apps, re-issued albums, restricted scopes); default
    // them so a missing field doesn't fail deserialization.
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub copyrights: Vec<Copyright>,
    #[serde(default)]
    pub total_tracks: u32,
}

#[derive(Deserialize, Debug, Clone)]
pub struct Copyright {
    #[serde(default)]
    pub text: String,
    // The Spotify API returns "P" or "C" here; use String (not char) so that
    // multi-character values or null don't cause a deserialization failure.
    #[serde(alias = "type", default)]
    pub type_: String,
}

impl WithImages for Album {
    fn images(&self) -> &[Image] {
        &self.images[..]
    }
}

#[derive(Deserialize, Debug, Clone)]
pub struct Image {
    pub url: String,
    pub height: Option<u32>,
    pub width: Option<u32>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct Artist {
    pub id: String,
    pub name: String,
    pub images: Option<Vec<Image>>,
    #[serde(default)]
    pub popularity: u32,
}

impl WithImages for Artist {
    fn images(&self) -> &[Image] {
        #[allow(clippy::manual_unwrap_or_default)]
        if let Some(ref images) = self.images {
            images
        } else {
            &[]
        }
    }
}

#[derive(Deserialize, Debug, Clone)]
pub struct User {
    pub id: String,
    pub display_name: String,
    pub product: Option<String>,
    pub images: Option<Vec<Image>>,
}

impl WithImages for User {
    fn images(&self) -> &[Image] {
        match &self.images {
            Some(x) => &x[..],
            None => &[],
        }
    }
}

#[derive(Deserialize, Debug, Clone)]
pub struct Device {
    #[serde(alias = "type")]
    pub type_: String,
    pub name: String,
    pub id: String,
    pub is_active: bool,
    pub is_restricted: bool,
    pub volume_percent: u32,
}

#[derive(Deserialize, Debug, Clone)]
pub struct Devices {
    pub devices: Vec<Device>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct PlayerQueue {
    pub currently_playing: TrackItem,
    pub queue: Vec<TrackItem>,
}

#[derive(Deserialize, Debug, Clone)]
#[allow(dead_code)]
pub struct PlayerContext {
    #[serde(alias = "type")]
    pub type_: String,
    pub uri: String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct PlayerState {
    pub progress_ms: u32,
    pub is_playing: bool,
    pub repeat_state: String,
    pub shuffle_state: bool,
    pub item: FailibleTrackItem,
    #[allow(dead_code)] // Part of the Spotify API response but currently unused
    pub context: Option<PlayerContext>,
}

impl From<PlayerState> for ConnectPlayerState {
    fn from(
        PlayerState {
            progress_ms,
            is_playing,
            repeat_state,
            shuffle_state,
            item,
            ..
        }: PlayerState,
    ) -> Self {
        let repeat = match &repeat_state[..] {
            "track" => RepeatMode::Song,
            "context" => RepeatMode::Playlist,
            _ => RepeatMode::None,
        };
        let shuffle = shuffle_state;
        let current_song_id = item.get().map(|i| i.track.id);
        Self {
            is_playing,
            progress_ms,
            repeat,
            shuffle,
            current_song_id,
        }
    }
}

#[derive(Deserialize, Debug, Clone)]
pub struct TopTracks {
    pub tracks: Vec<TrackItem>,
}

// Response wrapper for `GET /v1/tracks?ids=…`: `{ "tracks": [Track | null, …] }`.
// Unlike artist top-tracks, this batch endpoint returns `null` in-place for any
// unavailable/relinked-out id, so items must be null-tolerant (a plain
// `Vec<TrackItem>` would fail to deserialize on the first null).
#[derive(Deserialize, Debug, Clone)]
pub struct Tracks {
    #[serde(default, deserialize_with = "deserialize_nullable_items")]
    pub tracks: Option<Vec<TrackItem>>,
}

impl From<Tracks> for Vec<SongDescription> {
    fn from(tracks: Tracks) -> Self {
        Page::new(tracks.tracks.unwrap_or_default()).into()
    }
}

// `/me/player/recently-played` returns a cursor-paged list of play-history
// objects (a track plus when and in what context it was played).
#[derive(Deserialize, Debug, Clone)]
pub struct RecentlyPlayed {
    pub items: Option<Vec<PlayHistory>>,
}

#[derive(Deserialize, Debug, Clone)]
#[allow(dead_code)] // `played_at`/`context` are part of the response but unused: the
                    // home feed derives its shelves from the track's album instead.
pub struct PlayHistory {
    pub track: TrackItem,
    pub played_at: Option<String>,
    pub context: Option<PlayContext>,
}

// The context a track was played in (album/playlist/artist). Absent for songs
// played outside any context, so every field is optional.
#[derive(Deserialize, Debug, Clone)]
#[allow(dead_code)] // Part of the Spotify API response but currently unused.
pub struct PlayContext {
    #[serde(alias = "type")]
    pub type_: Option<String>,
    pub uri: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct AlbumTrackItem {
    pub id: String,
    pub track_number: Option<usize>,
    pub uri: String,
    pub name: String,
    pub duration_ms: i64,
    pub artists: Vec<Artist>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct TrackItem {
    #[serde(flatten)]
    pub track: AlbumTrackItem,
    pub album: Album,
}

#[derive(Deserialize, Debug, Clone)]
pub struct BadTrackItem {}

#[derive(Deserialize, Debug, Clone)]
#[serde(untagged)]
pub enum FailibleTrackItem {
    Ok(Box<TrackItem>),
    Failing(BadTrackItem),
}

impl FailibleTrackItem {
    fn get(self) -> Option<TrackItem> {
        match self {
            Self::Ok(track) => Some(*track),
            Self::Failing(_) => None,
        }
    }
}

#[derive(Deserialize, Debug, Clone)]
pub struct RawSearchResults {
    pub albums: Page<Album>,
    pub artists: Page<Artist>,
    pub tracks: Page<TrackItem>,
    #[serde(default)]
    pub playlists: Page<Playlist>,
}

impl From<Artist> for ArtistSummary {
    fn from(artist: Artist) -> Self {
        let photo = ImageSet::from_images(artist.images().iter().map(|i| (i.width, i.url.clone())));
        let Artist {
            id,
            name,
            popularity,
            ..
        } = artist;
        Self {
            id,
            name,
            photo,
            popularity,
        }
    }
}

impl TryFrom<PlaylistTrack> for TrackItem {
    type Error = ();

    fn try_from(PlaylistTrack { is_local, track }: PlaylistTrack) -> Result<Self, Self::Error> {
        track.ok_or(())?.get().filter(|_| !is_local).ok_or(())
    }
}

impl From<SavedTrack> for TrackItem {
    fn from(track: SavedTrack) -> Self {
        track.track
    }
}

impl From<PlayerQueue> for Vec<SongDescription> {
    fn from(
        PlayerQueue {
            mut queue,
            currently_playing,
        }: PlayerQueue,
    ) -> Self {
        let mut ids = HashSet::<String>::new();
        queue.insert(0, currently_playing);
        let queue: Vec<TrackItem> = queue
            .into_iter()
            .take_while(|e| {
                if ids.contains(&e.track.id) {
                    false
                } else {
                    ids.insert(e.track.id.clone());
                    true
                }
            })
            .collect();
        Page::new(queue).into()
    }
}

impl From<TopTracks> for Vec<SongDescription> {
    fn from(top_tracks: TopTracks) -> Self {
        Page::new(top_tracks.tracks).into()
    }
}

impl<T> From<Page<T>> for Vec<SongDescription>
where
    T: TryInto<TrackItem>,
{
    fn from(page: Page<T>) -> Self {
        SongBatch::from(page).songs
    }
}

impl From<(Page<AlbumTrackItem>, &Album)> for SongBatch {
    fn from(page_and_album: (Page<AlbumTrackItem>, &Album)) -> Self {
        let (page, album) = page_and_album;
        Self::from(page.map(|track| TrackItem {
            track,
            album: album.clone(),
        }))
    }
}

impl<T> From<Page<T>> for SongBatch
where
    T: TryInto<TrackItem>,
{
    fn from(page: Page<T>) -> Self {
        let batch = Batch {
            offset: page.offset(),
            batch_size: page.limit(),
            total: page.total(),
        };
        let songs = page
            .into_iter()
            .filter_map(|t| {
                let TrackItem { track, album } = t.try_into().ok()?;
                let AlbumTrackItem {
                    artists,
                    id,
                    uri,
                    name,
                    duration_ms,
                    track_number,
                } = track;
                let artists = artists
                    .into_iter()
                    .map(|a| ArtistRef {
                        id: a.id,
                        name: a.name,
                    })
                    .collect::<Vec<ArtistRef>>();

                let art =
                    ImageSet::from_images(album.images().iter().map(|i| (i.width, i.url.clone())));
                let Album {
                    id: album_id,
                    name: album_name,
                    ..
                } = album;

                let album_ref = AlbumRef {
                    id: album_id,
                    name: album_name,
                };

                Some(SongDescription {
                    id,
                    track_number: track_number.map(|u| u as u32),
                    uri,
                    title: name,
                    artists,
                    album: album_ref,
                    duration_ms: duration_ms as u32,
                    art,
                })
            })
            .collect();
        SongBatch { songs, batch }
    }
}

impl TryFrom<Album> for SongBatch {
    type Error = ();

    fn try_from(mut album: Album) -> Result<Self, Self::Error> {
        let tracks = album.tracks.take().ok_or(())?;
        Ok((tracks, &album).into())
    }
}

impl From<FullAlbum> for AlbumFullDescription {
    fn from(full_album: FullAlbum) -> Self {
        let description = full_album.album.into();
        let release_details = full_album.album_info.into();
        Self {
            description,
            release_details,
        }
    }
}

impl From<Album> for AlbumDescription {
    fn from(album: Album) -> Self {
        let artists = album
            .artists
            .iter()
            .map(|a| ArtistRef {
                id: a.id.clone(),
                name: a.name.clone(),
            })
            .collect::<Vec<ArtistRef>>();
        let songs = album
            .clone()
            .try_into()
            .unwrap_or_else(|_| SongBatch::empty());
        let art = ImageSet::from_images(album.images().iter().map(|i| (i.width, i.url.clone())));

        Self {
            id: album.id,
            title: album.name,
            artists,
            release_date: album.release_date,
            art,
            songs,
            is_liked: false,
            popularity: album.popularity,
        }
    }
}

impl From<AlbumInfo> for AlbumReleaseDetails {
    fn from(
        AlbumInfo {
            label,
            copyrights,
            total_tracks,
        }: AlbumInfo,
    ) -> Self {
        let copyright_text = copyrights
            .iter()
            .map(|Copyright { type_, text }| format!("[{type_}] {text}"))
            .collect::<Vec<String>>()
            .join(",\n ");

        Self {
            label,
            copyright_text,
            total_tracks: total_tracks as usize,
        }
    }
}

impl From<Playlist> for PlaylistDescription {
    fn from(playlist: Playlist) -> Self {
        let art = ImageSet::from_images(playlist.images().iter().map(|i| (i.width, i.url.clone())));
        let Playlist {
            id,
            name,
            tracks,
            owner,
            snapshot_id,
            ..
        } = playlist;
        let PlaylistOwner {
            id: owner_id,
            display_name,
        } = owner;
        // `tracks` may be absent in `/me/playlists` responses from dev-mode apps;
        // treat a missing field as an empty batch (track_count = 0).
        let song_batch = tracks.unwrap_or_default().into();
        PlaylistDescription {
            id,
            title: name,
            art,
            songs: song_batch,
            owner: UserRef {
                id: owner_id,
                display_name,
            },
            snapshot_id,
        }
    }
}

impl From<Device> for ConnectDevice {
    fn from(
        Device {
            id, name, type_, ..
        }: Device,
    ) -> Self {
        let kind = match type_.to_lowercase().as_str() {
            "smartphone" => ConnectDeviceKind::Phone,
            "computer" => ConnectDeviceKind::Computer,
            "speaker" => ConnectDeviceKind::Speaker,
            _ => ConnectDeviceKind::Other,
        };
        Self {
            id,
            label: name,
            kind,
        }
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn test_playlist_track_null() {
        let track = r#"{"is_local": false, "track": null}"#;
        let deserialized: PlaylistTrack = serde_json::from_str(track).unwrap();
        let track_item: Option<TrackItem> = deserialized.try_into().ok();
        assert!(track_item.is_none());
    }

    #[test]
    fn test_playlist_track_local() {
        let track = r#"{"is_local": true, "track": {"name": ""}}"#;
        let deserialized: PlaylistTrack = serde_json::from_str(track).unwrap();
        let track_item: Option<TrackItem> = deserialized.try_into().ok();
        assert!(track_item.is_none());
    }

    #[test]
    fn test_playlist_track_ok() {
        let track = r#"{"is_local":false,"track":{"album":{"artists":[{"external_urls":{"spotify":""},"href":"","id":"","name":"","type":"artist","uri":""}],"id":"","images":[{"height":64,"url":"","width":64}],"name":""},"artists":[{"id":"","name":""}],"duration_ms":1,"id":"","name":"","uri":""}}"#;
        let deserialized: PlaylistTrack = serde_json::from_str(track).unwrap();
        let track_item: Option<TrackItem> = deserialized.try_into().ok();
        assert!(track_item.is_some());
    }

    #[test]
    fn test_recently_played_parsing() {
        let json = r#"{"items":[{"track":{"album":{"artists":[{"id":"a","name":"Artist"}],"id":"alb","images":[{"height":64,"url":"http://img","width":64}],"name":"Album"},"artists":[{"id":"a","name":"Artist"}],"duration_ms":1,"id":"t1","name":"Track","uri":"spotify:track:t1"},"played_at":"2026-07-19T00:00:00Z","context":{"type":"album","uri":"spotify:album:alb"}}]}"#;
        let parsed: RecentlyPlayed = serde_json::from_str(json).unwrap();
        let items = parsed.items.unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].track.track.id, "t1");
        assert_eq!(items[0].track.album.id, "alb");
    }

    #[test]
    fn test_top_tracks_page_parsing() {
        // `/me/top/tracks` is a paged `{items:[…]}`, distinct from the artist
        // `TopTracks` (`{tracks:[…]}`) shape.
        let json = r#"{"items":[{"album":{"artists":[{"id":"a","name":"Artist"}],"id":"alb","images":[{"height":64,"url":"http://img","width":64}],"name":"Album"},"artists":[{"id":"a","name":"Artist"}],"duration_ms":1,"id":"t1","name":"Track","uri":"spotify:track:t1"}],"total":1}"#;
        let parsed: Page<TrackItem> = serde_json::from_str(json).unwrap();
        let songs: Vec<SongDescription> = parsed.into();
        assert_eq!(songs.len(), 1);
        assert_eq!(songs[0].album.id, "alb");
    }

    #[test]
    fn test_top_artists_page_parsing() {
        let json = r#"{"items":[{"id":"a","name":"Artist","images":[{"height":64,"url":"http://img","width":64}],"popularity":80}],"total":1}"#;
        let parsed: Page<Artist> = serde_json::from_str(json).unwrap();
        let artists: Vec<ArtistSummary> = parsed.into_iter().map(ArtistSummary::from).collect();
        assert_eq!(artists.len(), 1);
        assert_eq!(artists[0].name, "Artist");
    }

    #[test]
    fn test_playlist_missing_tracks_field() {
        // Dev-mode apps may omit `tracks` entirely; the list must still parse and
        // produce a PlaylistDescription with track_count = 0.
        let json = r#"{"id":"pl1","name":"My List","images":null,"owner":{"id":"u","display_name":"U"},"snapshot_id":"snap1"}"#;
        let deserialized: Playlist = serde_json::from_str(json).unwrap();
        assert!(deserialized.tracks.is_none());
        let desc = PlaylistDescription::from(deserialized);
        assert_eq!(desc.id, "pl1");
        assert_eq!(desc.songs.batch.total, 0);
    }

    #[test]
    fn test_playlist_with_tracks_field() {
        // When `tracks` IS present (e.g. from get_playlist with fields filter),
        // it must still deserialize and produce a correct total.
        let json = r#"{"id":"pl2","name":"Full","images":null,"owner":{"id":"u","display_name":"U"},"snapshot_id":null,"tracks":{"items":[],"total":42,"offset":0,"limit":50}}"#;
        let deserialized: Playlist = serde_json::from_str(json).unwrap();
        assert!(deserialized.tracks.is_some());
        assert_eq!(deserialized.tracks.as_ref().unwrap().total(), 42);
    }

    #[test]
    fn test_saved_playlists_page_skips_null_items() {
        // Spotify can return a `null` entry in a saved-playlists list (an
        // unavailable/region-locked playlist). The page must parse and simply drop
        // the null rather than failing the whole deserialization.
        let json = r#"{"items":[{"id":"pl1","name":"A","images":null,"owner":{"id":"u","display_name":"U"},"snapshot_id":"s","tracks":{"total":10}},null,{"id":"pl2","name":"B","images":null,"owner":{"id":"u","display_name":"U"},"snapshot_id":"s","tracks":{"total":20}}],"offset":0,"limit":50,"total":3}"#;
        let parsed: Page<Playlist> = serde_json::from_str(json).unwrap();
        let playlists: Vec<Playlist> = parsed.into_iter().collect();
        assert_eq!(playlists.len(), 2);
        assert_eq!(playlists[0].id, "pl1");
        assert_eq!(playlists[1].id, "pl2");
    }

    #[test]
    fn test_saved_playlists_track_total_populated() {
        // The library "Largest" sort relies on tracks.total flowing through to the
        // PlaylistDescription's batch total. /me/playlists returns tracks as a
        // paging object with only `total` (no items) — that must still yield the
        // count.
        let json = r#"{"id":"pl1","name":"My List","images":null,"owner":{"id":"u","display_name":"U"},"snapshot_id":"s","tracks":{"total":37,"limit":100,"offset":0}}"#;
        let deserialized: Playlist = serde_json::from_str(json).unwrap();
        let desc = PlaylistDescription::from(deserialized);
        assert_eq!(desc.songs.batch.total, 37);
    }

    #[test]
    fn test_search_query_encoding() {
        let query = SearchQuery {
            query: "кириллица".to_string(),
            limit: 5,
            offset: 0,
        };

        assert_eq!(query.into_query_string(), "type=album,track,artist,playlist&q=%D0%BA%D0%B8%D1%80%D0%B8%D0%BB%D0%BB%D0%B8%D1%86%D0%B0&offset=0&limit=5");
    }

    #[test]
    fn test_search_query_spaces_and_stuff() {
        let query = SearchQuery {
            query: "test??? wow".to_string(),
            limit: 5,
            offset: 0,
        };

        assert_eq!(
            query.into_query_string(),
            "type=album,track,artist,playlist&q=test+wow&offset=0&limit=5"
        );
    }
}
