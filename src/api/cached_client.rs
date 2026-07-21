use super::api_models::WithImages;
use super::cache::{CacheExpiry, CacheManager, CachePolicy, FetchResult};
use super::client::*;
use crate::app::models::*;
use crate::app::state::CARD_BATCH_SIZE;
use crate::auth::TokenStore;
use futures::future::BoxFuture;
use futures::{join, FutureExt};
use regex::Regex;
use serde::de::DeserializeOwned;
use serde_json::from_slice;
use std::convert::Into;
use std::future::Future;

pub type SpotifyResult<T> = Result<T, SpotifyApiError>;

pub trait SpotifyApiClient {
    fn get_artist(&self, id: &str) -> BoxFuture<SpotifyResult<ArtistDescription>>;

    fn get_album(&self, id: &str) -> BoxFuture<SpotifyResult<AlbumFullDescription>>;

    fn get_album_tracks(
        &self,
        id: &str,
        offset: usize,
        limit: usize,
    ) -> BoxFuture<SpotifyResult<SongBatch>>;

    fn get_playlist(&self, id: &str) -> BoxFuture<SpotifyResult<PlaylistDescription>>;

    fn get_playlist_tracks(
        &self,
        id: &str,
        offset: usize,
        limit: usize,
    ) -> BoxFuture<SpotifyResult<SongBatch>>;

    /// Fetch all track ids for a playlist, paging automatically. Both the track
    /// id and its `linked_from` id (when relinked for the market) are returned so
    /// membership tests match either. Uses a fields-filtered endpoint so each page
    /// is tiny. Cost: first sync fetches all pages (one req per 50 tracks);
    /// subsequent syncs only re-fetch playlists whose snapshot_id changed.
    fn get_playlist_track_ids(&self, id: &str) -> BoxFuture<SpotifyResult<Vec<String>>>;

    fn get_saved_albums(
        &self,
        offset: usize,
        limit: usize,
    ) -> BoxFuture<SpotifyResult<Vec<AlbumDescription>>>;

    fn get_saved_tracks(&self, offset: usize, limit: usize) -> BoxFuture<SpotifyResult<SongBatch>>;

    fn save_album(&self, id: &str) -> BoxFuture<SpotifyResult<AlbumDescription>>;

    fn save_tracks(&self, ids: Vec<String>) -> BoxFuture<SpotifyResult<()>>;

    fn remove_saved_album(&self, id: &str) -> BoxFuture<SpotifyResult<()>>;

    fn remove_saved_tracks(&self, ids: Vec<String>) -> BoxFuture<SpotifyResult<()>>;

    fn get_saved_playlists(
        &self,
        offset: usize,
        limit: usize,
    ) -> BoxFuture<SpotifyResult<Vec<PlaylistDescription>>>;

    fn add_to_playlist(&self, id: &str, uris: Vec<String>) -> BoxFuture<SpotifyResult<()>>;

    fn create_new_playlist(
        &self,
        name: &str,
        user_id: &str,
    ) -> BoxFuture<SpotifyResult<PlaylistDescription>>;

    fn remove_from_playlist(&self, id: &str, uris: Vec<String>) -> BoxFuture<SpotifyResult<()>>;

    fn follow_playlist(&self, id: &str) -> BoxFuture<SpotifyResult<()>>;

    fn unfollow_playlist(&self, id: &str) -> BoxFuture<SpotifyResult<()>>;

    fn update_playlist_details(&self, id: &str, name: String) -> BoxFuture<SpotifyResult<()>>;

    fn search(
        &self,
        query: &str,
        offset: usize,
        limit: usize,
    ) -> BoxFuture<SpotifyResult<SearchResults>>;

    fn get_artist_albums(
        &self,
        id: &str,
        offset: usize,
        limit: usize,
    ) -> BoxFuture<SpotifyResult<Vec<AlbumDescription>>>;

    fn get_user(&self, id: &str) -> BoxFuture<SpotifyResult<UserDescription>>;

    /// The logged-in user's own profile (`/me`). Lightweight — just id, display
    /// name and avatar URL — so it can populate the top-right profile button
    /// without the heavy playlist fetch that `get_user` performs.
    fn get_current_user(&self) -> BoxFuture<SpotifyResult<CurrentUser>>;

    fn get_user_playlists(
        &self,
        id: &str,
        offset: usize,
        limit: usize,
    ) -> BoxFuture<SpotifyResult<Vec<PlaylistDescription>>>;

    fn list_available_devices(&self) -> BoxFuture<SpotifyResult<Vec<ConnectDevice>>>;

    fn get_player_queue(&self) -> BoxFuture<SpotifyResult<Vec<SongDescription>>>;

    fn player_pause(&self, device_id: String) -> BoxFuture<SpotifyResult<()>>;

    fn player_resume(&self, device_id: String) -> BoxFuture<SpotifyResult<()>>;

    fn player_next(&self, device_id: String) -> BoxFuture<SpotifyResult<()>>;

    fn player_previous(&self, device_id: String) -> BoxFuture<SpotifyResult<()>>;

    /// Transfer the active playback session to `device_id`. When `play` is true
    /// playback resumes on the target device, otherwise it is transferred paused.
    fn player_transfer(&self, device_id: String, play: bool) -> BoxFuture<SpotifyResult<()>>;

    fn player_seek(&self, device_id: String, pos: usize) -> BoxFuture<SpotifyResult<()>>;

    fn player_repeat(&self, device_id: String, mode: RepeatMode) -> BoxFuture<SpotifyResult<()>>;

    fn player_shuffle(&self, device_id: String, shuffle: bool) -> BoxFuture<SpotifyResult<()>>;

    fn player_volume(&self, device_id: String, volume: u8) -> BoxFuture<SpotifyResult<()>>;

    fn player_play_in_context(
        &self,
        device_id: String,
        context: String,
        offset: usize,
    ) -> BoxFuture<SpotifyResult<()>>;

    fn player_play_no_context(
        &self,
        device_id: String,
        uris: Vec<String>,
        offset: usize,
    ) -> BoxFuture<SpotifyResult<()>>;

    fn player_state(&self) -> BoxFuture<SpotifyResult<ConnectPlayerState>>;

    fn get_followed_artists(
        &self,
        after: Option<String>,
        limit: usize,
    ) -> BoxFuture<SpotifyResult<(Vec<ArtistSummary>, Option<String>)>>;

    /// Recently-played tracks (newest first). Returns the track strip plus the
    /// deduplicated album/playlist contexts for the "Jump back in" shelf.
    fn recently_played(
        &self,
        limit: usize,
    ) -> BoxFuture<SpotifyResult<(Vec<SongDescription>, Vec<JumpBackContext>)>>;

    fn get_top_artists(&self, limit: usize) -> BoxFuture<SpotifyResult<Vec<ArtistSummary>>>;

    fn get_top_tracks(&self, limit: usize) -> BoxFuture<SpotifyResult<Vec<SongDescription>>>;

    /// Fetch full metadata for a batch of track ids via `GET /v1/tracks?ids=…`.
    /// Order is preserved and unavailable ids are dropped. Ids beyond the Spotify
    /// 50-per-call batch limit are chunked across multiple requests.
    fn get_tracks(&self, ids: Vec<String>) -> BoxFuture<SpotifyResult<Vec<SongDescription>>>;

    fn follow_artist(&self, id: &str) -> BoxFuture<SpotifyResult<()>>;

    fn unfollow_artist(&self, id: &str) -> BoxFuture<SpotifyResult<()>>;
}

enum RiffCacheKey<'a> {
    SavedAlbums(usize, usize),
    SavedTracks(usize, usize),
    SavedPlaylists(usize, usize),
    Album(&'a str),
    AlbumLiked(&'a str),
    AlbumTracks(&'a str, usize, usize),
    Playlist(&'a str),
    PlaylistTracks(&'a str, usize, usize),
    ArtistAlbums(&'a str, usize, usize),
    Artist(&'a str),
    ArtistTopTracks(&'a str),
    User(&'a str),
    Me,
    UserPlaylists(&'a str, usize, usize),
    RecentlyPlayed(usize),
    TopArtists(usize),
    TopTracks(usize),
}

impl RiffCacheKey<'_> {
    fn into_raw(self) -> String {
        match self {
            Self::SavedAlbums(offset, limit) => format!("me_albums_{offset}_{limit}.json"),
            Self::SavedTracks(offset, limit) => format!("me_tracks_{offset}_{limit}.json"),
            Self::SavedPlaylists(offset, limit) => format!("me_playlists_{offset}_{limit}.json"),
            Self::Album(id) => format!("album_{id}.json"),
            Self::AlbumTracks(id, offset, limit) => {
                format!("album_item_{id}_{offset}_{limit}.json")
            }
            Self::AlbumLiked(id) => format!("album_liked_{id}.json"),
            Self::Playlist(id) => format!("playlist_{id}.json"),
            Self::PlaylistTracks(id, offset, limit) => {
                format!("playlist_item_{id}_{offset}_{limit}.json")
            }
            Self::ArtistAlbums(id, offset, limit) => {
                format!("artist_albums_{id}_{offset}_{limit}.json")
            }
            Self::Artist(id) => format!("artist_{id}.json"),
            Self::ArtistTopTracks(id) => format!("artist_top_tracks_{id}.json"),
            Self::User(id) => format!("user_{id}.json"),
            Self::Me => "me.json".to_string(),
            Self::UserPlaylists(id, offset, limit) => {
                format!("user_playlists_{id}_{offset}_{limit}.json")
            }
            Self::RecentlyPlayed(limit) => format!("me_recently_played_{limit}.json"),
            Self::TopArtists(limit) => format!("me_top_artists_{limit}.json"),
            Self::TopTracks(limit) => format!("me_top_tracks_{limit}.json"),
        }
    }
}

lazy_static! {
    pub static ref ME_TRACKS_CACHE: Regex = Regex::new(r"^me_tracks_\w+_\w+\.json$").unwrap();
    pub static ref ME_ALBUMS_CACHE: Regex = Regex::new(r"^me_albums_\w+_\w+\.json$").unwrap();
    pub static ref ME_PLAYLISTS_CACHE: Regex = Regex::new(r"^me_playlists_\w+_\w+\.json$").unwrap();
    pub static ref USER_CACHE: Regex = Regex::new(
        r"^me\.json$|^me_(albums|playlists|tracks)_\w+_\w+\.json$|^me_(recently_played|top_artists|top_tracks)_\w+\.json$"
    )
    .unwrap();
}

fn playlist_cache_key(id: &str) -> Regex {
    Regex::new(&format!(r"^playlist(_{id}|item_{id}_\w+_\w+)\.json$")).unwrap()
}

pub struct CachedSpotifyClient {
    client: SpotifyClient,
    cache: CacheManager,
}

impl CachedSpotifyClient {
    pub fn new(client: TokenStore) -> CachedSpotifyClient {
        CachedSpotifyClient {
            client: SpotifyClient::new(client),
            cache: CacheManager::for_dir("riff/net").unwrap(),
        }
    }

    fn default_cache_policy(&self) -> CachePolicy {
        if self.client.has_token() {
            CachePolicy::Default
        } else {
            debug!("Forcing cache");
            CachePolicy::IgnoreExpiry
        }
    }

    async fn wrap_write<T, O, F>(write: &F, etag: Option<String>) -> SpotifyResult<FetchResult>
    where
        O: Future<Output = SpotifyResult<SpotifyResponse<T>>>,
        F: Fn(Option<String>) -> O,
    {
        write(etag)
            .map(|r| {
                let SpotifyResponse {
                    kind,
                    max_age,
                    etag,
                } = r?;
                let expiry = CacheExpiry::expire_in_seconds(max_age, etag);
                SpotifyResult::Ok(match kind {
                    SpotifyResponseKind::Ok(content, _) => {
                        debug!("Did not hit cache");
                        FetchResult::Modified(content.into_bytes(), expiry)
                    }
                    SpotifyResponseKind::NotModified => FetchResult::NotModified(expiry),
                })
            })
            .await
    }

    async fn cache_get_or_write<T, O, F>(
        &self,
        key: RiffCacheKey<'_>,
        cache_policy: Option<CachePolicy>,
        write: F,
    ) -> SpotifyResult<T>
    where
        O: Future<Output = SpotifyResult<SpotifyResponse<T>>>,
        F: Fn(Option<String>) -> O,
        T: DeserializeOwned,
    {
        let write = &write;
        let cache_key = key.into_raw();
        let raw = self
            .cache
            .get_or_write(
                &cache_key,
                cache_policy.unwrap_or_else(|| self.default_cache_policy()),
                |etag| Self::wrap_write(write, etag),
            )
            .await?;

        let result = from_slice::<T>(&raw);
        match result {
            Ok(t) => Ok(t),
            // parsing failed: cache is likely invalid, request again, ignoring cache
            Err(e) => {
                dbg!(&cache_key, e);
                let new_raw = self
                    .cache
                    .get_or_write(&cache_key, CachePolicy::IgnoreCached, |etag| {
                        Self::wrap_write(write, etag)
                    })
                    .await?;
                Ok(from_slice::<T>(&new_raw)?)
            }
        }
    }
}

impl SpotifyApiClient for CachedSpotifyClient {
    fn get_saved_albums(
        &self,
        offset: usize,
        limit: usize,
    ) -> BoxFuture<SpotifyResult<Vec<AlbumDescription>>> {
        Box::pin(async move {
            let page = self
                .cache_get_or_write(RiffCacheKey::SavedAlbums(offset, limit), None, |etag| {
                    self.client
                        .get_saved_albums(offset, limit)
                        .etag(etag)
                        .send()
                })
                .await?;

            let albums = page
                .into_iter()
                .map(|saved| saved.album.into())
                .collect::<Vec<AlbumDescription>>();

            Ok(albums)
        })
    }

    fn get_saved_tracks(&self, offset: usize, limit: usize) -> BoxFuture<SpotifyResult<SongBatch>> {
        Box::pin(async move {
            let page = self
                .cache_get_or_write(RiffCacheKey::SavedTracks(offset, limit), None, |etag| {
                    self.client
                        .get_saved_tracks(offset, limit)
                        .etag(etag)
                        .send()
                })
                .await?;

            Ok(page.into())
        })
    }

    fn get_saved_playlists(
        &self,
        offset: usize,
        limit: usize,
    ) -> BoxFuture<SpotifyResult<Vec<PlaylistDescription>>> {
        Box::pin(async move {
            let page = self
                .cache_get_or_write(RiffCacheKey::SavedPlaylists(offset, limit), None, |etag| {
                    self.client
                        .get_saved_playlists(offset, limit)
                        .etag(etag)
                        .send()
                })
                .await?;

            let albums = page
                .into_iter()
                .map(|playlist| playlist.into())
                .collect::<Vec<PlaylistDescription>>();

            Ok(albums)
        })
    }

    fn add_to_playlist(&self, id: &str, uris: Vec<String>) -> BoxFuture<SpotifyResult<()>> {
        let id = id.to_owned();

        Box::pin(async move {
            self.cache
                .set_expired_pattern(&playlist_cache_key(&id))
                .await
                .unwrap_or(());

            self.client
                .add_to_playlist(&id, uris)
                .send_no_response()
                .await?;
            Ok(())
        })
    }

    fn create_new_playlist(
        &self,
        name: &str,
        user_id: &str,
    ) -> BoxFuture<SpotifyResult<PlaylistDescription>> {
        let name = name.to_owned();
        let user_id = user_id.to_owned();

        Box::pin(async move {
            let playlist = self
                .client
                .create_new_playlist(&name, &user_id)
                .send()
                .await?
                .deserialize()
                .unwrap();

            Ok(playlist.into())
        })
    }

    fn remove_from_playlist(&self, id: &str, uris: Vec<String>) -> BoxFuture<SpotifyResult<()>> {
        let id = id.to_owned();

        Box::pin(async move {
            self.cache
                .set_expired_pattern(&playlist_cache_key(&id))
                .await
                .unwrap_or(());

            self.client
                .remove_from_playlist(&id, uris)
                .send_no_response()
                .await?;
            Ok(())
        })
    }

    fn follow_playlist(&self, id: &str) -> BoxFuture<SpotifyResult<()>> {
        let id = id.to_owned();

        Box::pin(async move {
            let _ = self.cache.set_expired_pattern(&ME_PLAYLISTS_CACHE).await;

            self.client.follow_playlist(&id).send_no_response().await?;
            Ok(())
        })
    }

    fn unfollow_playlist(&self, id: &str) -> BoxFuture<SpotifyResult<()>> {
        let id = id.to_owned();

        Box::pin(async move {
            let _ = self.cache.set_expired_pattern(&ME_PLAYLISTS_CACHE).await;
            let _ = self
                .cache
                .set_expired_pattern(&playlist_cache_key(&id))
                .await;

            self.client
                .unfollow_playlist(&id)
                .send_no_response()
                .await?;
            Ok(())
        })
    }

    fn update_playlist_details(&self, id: &str, name: String) -> BoxFuture<SpotifyResult<()>> {
        let id = id.to_owned();

        Box::pin(async move {
            self.cache
                .set_expired_pattern(&playlist_cache_key(&id))
                .await
                .unwrap_or(());

            self.client
                .update_playlist_details(&id, name)
                .send_no_response()
                .await?;

            Ok(())
        })
    }

    fn get_album(&self, id: &str) -> BoxFuture<SpotifyResult<AlbumFullDescription>> {
        let id = id.to_owned();

        Box::pin(async move {
            let album = self.cache_get_or_write(RiffCacheKey::Album(&id), None, |etag| {
                self.client.get_album(&id).etag(etag).send()
            });

            let liked = self.cache_get_or_write(
                RiffCacheKey::AlbumLiked(&id),
                Some(if self.client.has_token() {
                    CachePolicy::Revalidate
                } else {
                    CachePolicy::IgnoreExpiry
                }),
                |etag| self.client.is_album_saved(&id).etag(etag).send(),
            );

            let (album, liked) = join!(album, liked);

            let mut album: AlbumFullDescription = album?.into();
            // `/me/albums/contains` returns 403 for dev-mode client ids; treat
            // a failed check as "not saved" so the album detail page still loads.
            album.description.is_liked = liked.ok().and_then(|v| v.first().copied()).unwrap_or(false);

            Ok(album)
        })
    }

    fn save_album(&self, id: &str) -> BoxFuture<SpotifyResult<AlbumDescription>> {
        let id = id.to_owned();

        Box::pin(async move {
            let _ = self.cache.set_expired_pattern(&ME_ALBUMS_CACHE).await;
            self.client.save_album(&id).send_no_response().await?;
            self.get_album(&id[..]).await.map(|a| a.description)
        })
    }

    fn save_tracks(&self, ids: Vec<String>) -> BoxFuture<SpotifyResult<()>> {
        Box::pin(async move {
            let _ = self.cache.set_expired_pattern(&ME_TRACKS_CACHE).await;
            self.client.save_tracks(ids).send_no_response().await?;
            Ok(())
        })
    }

    fn remove_saved_album(&self, id: &str) -> BoxFuture<SpotifyResult<()>> {
        let id = id.to_owned();

        Box::pin(async move {
            let _ = self.cache.set_expired_pattern(&ME_ALBUMS_CACHE).await;
            self.client.remove_saved_album(&id).send_no_response().await
        })
    }

    fn remove_saved_tracks(&self, ids: Vec<String>) -> BoxFuture<SpotifyResult<()>> {
        Box::pin(async move {
            let _ = self.cache.set_expired_pattern(&ME_TRACKS_CACHE).await;
            self.client
                .remove_saved_tracks(ids)
                .send_no_response()
                .await
        })
    }

    fn get_album_tracks(
        &self,
        id: &str,
        offset: usize,
        limit: usize,
    ) -> BoxFuture<SpotifyResult<SongBatch>> {
        let id = id.to_owned();

        Box::pin(async move {
            let album = self.cache_get_or_write(
                RiffCacheKey::Album(&id),
                Some(CachePolicy::IgnoreExpiry),
                |etag| self.client.get_album(&id).etag(etag).send(),
            );

            let songs = self.cache_get_or_write(
                RiffCacheKey::AlbumTracks(&id, offset, limit),
                None,
                |etag| {
                    self.client
                        .get_album_tracks(&id, offset, limit)
                        .etag(etag)
                        .send()
                },
            );

            let (album, songs) = join!(album, songs);
            Ok((songs?, &album?.album).into())
        })
    }

    fn get_playlist(&self, id: &str) -> BoxFuture<SpotifyResult<PlaylistDescription>> {
        let id = id.to_owned();

        Box::pin(async move {
            let playlist = self
                .cache_get_or_write(RiffCacheKey::Playlist(&id), None, |etag| {
                    self.client.get_playlist(&id).etag(etag).send()
                })
                .await?;

            Ok(playlist.into())
        })
    }

    fn get_playlist_tracks(
        &self,
        id: &str,
        offset: usize,
        limit: usize,
    ) -> BoxFuture<SpotifyResult<SongBatch>> {
        let id = id.to_owned();

        Box::pin(async move {
            let songs = self
                .cache_get_or_write(
                    RiffCacheKey::PlaylistTracks(&id, offset, limit),
                    None,
                    |etag| {
                        self.client
                            .get_playlist_tracks(&id, offset, limit)
                            .etag(etag)
                            .send()
                    },
                )
                .await?;

            Ok(songs.into())
        })
    }

    fn get_playlist_track_ids(&self, id: &str) -> BoxFuture<SpotifyResult<Vec<String>>> {
        let id = id.to_owned();

        Box::pin(async move {
            const PAGE: usize = 50;
            let mut ids: Vec<String> = Vec::new();
            let mut offset = 0usize;

            loop {
                let page = self
                    .client
                    .get_playlist_track_ids(&id, offset, PAGE)
                    .send()
                    .await?
                    .deserialize()
                    .ok_or(SpotifyApiError::NoContent)?;

                let total = page.total();
                for item in page {
                    let Some(track) = item.track else { continue };
                    if let Some(id) = track.id {
                        ids.push(id);
                    }
                    // Index the pre-relink id too so a relinked track still matches.
                    if let Some(id) = track.linked_from.and_then(|l| l.id) {
                        ids.push(id);
                    }
                }
                offset += PAGE;

                if offset >= total {
                    break;
                }
            }

            Ok(ids)
        })
    }

    fn get_artist_albums(
        &self,
        id: &str,
        offset: usize,
        limit: usize,
    ) -> BoxFuture<SpotifyResult<Vec<AlbumDescription>>> {
        let id = id.to_owned();

        Box::pin(async move {
            let albums = self
                .cache_get_or_write(
                    RiffCacheKey::ArtistAlbums(&id, offset, limit),
                    None,
                    |etag| {
                        self.client
                            .get_artist_albums(&id, offset, limit)
                            .etag(etag)
                            .send()
                    },
                )
                .await?;

            let albums = albums
                .into_iter()
                .map(|a| a.into())
                .collect::<Vec<AlbumDescription>>();

            Ok(albums)
        })
    }

    fn get_artist(&self, id: &str) -> BoxFuture<SpotifyResult<ArtistDescription>> {
        let id = id.to_owned();

        Box::pin(async move {
            let artist = self.cache_get_or_write(RiffCacheKey::Artist(&id), None, |etag| {
                self.client.get_artist(&id).etag(etag).send()
            });

            let albums = self.get_artist_albums(&id, 0, CARD_BATCH_SIZE);

            let top_tracks =
                self.cache_get_or_write(RiffCacheKey::ArtistTopTracks(&id), None, |etag| {
                    self.client.get_artist_top_tracks(&id).etag(etag).send()
                });

            let is_followed = async {
                self.client
                    .is_artist_followed(&id)
                    .send()
                    .await
                    .ok()
                    .and_then(|r| r.deserialize())
                    .and_then(|v: Vec<bool>| v.first().copied())
                    .unwrap_or(false)
            };

            let (artist, albums, top_tracks, is_followed) =
                join!(artist, albums, top_tracks, is_followed);

            let artist = artist?;
            let photo =
                ImageSet::from_images(artist.images().iter().map(|i| (i.width, i.url.clone())));
            // `/v1/artists/{id}/top-tracks` returns 403 for dev-mode client ids
            // (hard API deprecation). Treat a failure as an empty track list so
            // the rest of the artist detail page still loads.
            let top_tracks_vec: Vec<SongDescription> = top_tracks
                .map(|tt| tt.into())
                .unwrap_or_default();
            let result = ArtistDescription {
                id: artist.id,
                name: artist.name,
                photo,
                albums: albums?,
                top_tracks: top_tracks_vec,
                is_followed,
            };
            Ok(result)
        })
    }

    fn search(
        &self,
        query: &str,
        offset: usize,
        limit: usize,
    ) -> BoxFuture<SpotifyResult<SearchResults>> {
        let query = query.to_owned();

        Box::pin(async move {
            let results = self
                .client
                .search(query, offset, limit)
                .send()
                .await?
                .deserialize()
                .ok_or(SpotifyApiError::NoContent)?;

            let albums = results
                .albums
                .into_iter()
                .map(|saved| saved.into())
                .collect::<Vec<AlbumDescription>>();

            let artists = results
                .artists
                .into_iter()
                .map(|saved| saved.into())
                .collect::<Vec<ArtistSummary>>();

            let tracks = SongBatch::from(results.tracks);

            let playlists = results
                .playlists
                .into_iter()
                .map(|p| p.into())
                .collect::<Vec<PlaylistDescription>>();

            Ok(SearchResults {
                albums,
                artists,
                tracks,
                playlists,
            })
        })
    }

    fn get_user_playlists(
        &self,
        id: &str,
        offset: usize,
        limit: usize,
    ) -> BoxFuture<SpotifyResult<Vec<PlaylistDescription>>> {
        let id = id.to_owned();

        Box::pin(async move {
            let playlists = self
                .cache_get_or_write(
                    RiffCacheKey::UserPlaylists(&id, offset, limit),
                    None,
                    |etag| {
                        self.client
                            .get_user_playlists(&id, offset, limit)
                            .etag(etag)
                            .send()
                    },
                )
                .await?;

            let playlists = playlists
                .into_iter()
                .map(|a| a.into())
                .collect::<Vec<PlaylistDescription>>();

            Ok(playlists)
        })
    }

    fn get_user(&self, id: &str) -> BoxFuture<SpotifyResult<UserDescription>> {
        let id = id.to_owned();

        Box::pin(async move {
            let user = self.cache_get_or_write(RiffCacheKey::User(&id), None, |etag| {
                self.client.get_user(&id).etag(etag).send()
            });

            let playlists = self.get_user_playlists(&id, 0, CARD_BATCH_SIZE);

            let (user, playlists) = join!(user, playlists);

            let user = user?;
            let photo =
                ImageSet::from_images(user.images().iter().map(|i| (i.width, i.url.clone())));
            let result = UserDescription {
                id: user.id,
                name: user.display_name,
                photo,
                playlists: playlists?,
            };
            Ok(result)
        })
    }

    fn get_current_user(&self) -> BoxFuture<SpotifyResult<CurrentUser>> {
        Box::pin(async move {
            let user = self
                .cache_get_or_write(RiffCacheKey::Me, None, |etag| {
                    self.client.get_me().etag(etag).send()
                })
                .await?;
            // Pick the first non-empty avatar URL (Spotify lists them small→large).
            let image_url = user
                .images()
                .iter()
                .map(|i| i.url.clone())
                .find(|u| !u.is_empty());
            Ok(CurrentUser {
                id: user.id,
                display_name: user.display_name,
                image_url,
            })
        })
    }

    fn list_available_devices(&self) -> BoxFuture<SpotifyResult<Vec<ConnectDevice>>> {
        Box::pin(async move {
            let devices = self
                .client
                .get_player_devices()
                .send()
                .await?
                .deserialize()
                .ok_or(SpotifyApiError::NoContent)?;
            Ok(devices
                .devices
                .into_iter()
                .filter(|d| {
                    debug!("found device: {:?}", d);
                    !d.is_restricted
                })
                .map(ConnectDevice::from)
                .collect())
        })
    }

    fn get_player_queue(&self) -> BoxFuture<SpotifyResult<Vec<SongDescription>>> {
        Box::pin(async move {
            let queue = self
                .client
                .get_player_queue()
                .send()
                .await?
                .deserialize()
                .ok_or(SpotifyApiError::NoContent)?;
            Ok(queue.into())
        })
    }

    fn player_pause(&self, device_id: String) -> BoxFuture<SpotifyResult<()>> {
        Box::pin(self.client.player_pause(&device_id).send_no_response())
    }

    fn player_resume(&self, device_id: String) -> BoxFuture<SpotifyResult<()>> {
        Box::pin(self.client.player_resume(&device_id).send_no_response())
    }

    fn player_next(&self, device_id: String) -> BoxFuture<SpotifyResult<()>> {
        Box::pin(self.client.player_next(&device_id).send_no_response())
    }

    fn player_previous(&self, device_id: String) -> BoxFuture<SpotifyResult<()>> {
        Box::pin(self.client.player_previous(&device_id).send_no_response())
    }

    fn player_transfer(&self, device_id: String, play: bool) -> BoxFuture<SpotifyResult<()>> {
        Box::pin(
            self.client
                .player_transfer(&device_id, play)
                .send_no_response(),
        )
    }

    fn player_play_in_context(
        &self,
        device_id: String,
        context_uri: String,
        offset: usize,
    ) -> BoxFuture<SpotifyResult<()>> {
        Box::pin(
            self.client
                .player_set_playing(
                    &device_id,
                    PlayRequest::Contextual {
                        context_uri,
                        offset: PlayOffset {
                            position: offset as u32,
                        },
                    },
                )
                .send_no_response(),
        )
    }

    fn player_play_no_context(
        &self,
        device_id: String,
        uris: Vec<String>,
        offset: usize,
    ) -> BoxFuture<SpotifyResult<()>> {
        Box::pin(
            self.client
                .player_set_playing(
                    &device_id,
                    PlayRequest::Uris {
                        uris,
                        offset: PlayOffset {
                            position: offset as u32,
                        },
                    },
                )
                .send_no_response(),
        )
    }

    fn player_seek(&self, device_id: String, pos: usize) -> BoxFuture<SpotifyResult<()>> {
        Box::pin(self.client.player_seek(&device_id, pos).send_no_response())
    }

    fn player_state(&self) -> BoxFuture<SpotifyResult<ConnectPlayerState>> {
        Box::pin(async move {
            let result = self
                .client
                .player_state()
                .send()
                .await?
                .deserialize()
                .ok_or(SpotifyApiError::NoContent)?;
            Ok(result.into())
        })
    }

    fn player_repeat(&self, device_id: String, mode: RepeatMode) -> BoxFuture<SpotifyResult<()>> {
        Box::pin(
            self.client
                .player_repeat(
                    &device_id,
                    match mode {
                        RepeatMode::Song => "track",
                        RepeatMode::Playlist => "context",
                        RepeatMode::None => "off",
                    },
                )
                .send_no_response(),
        )
    }

    fn player_shuffle(&self, device_id: String, shuffle: bool) -> BoxFuture<SpotifyResult<()>> {
        Box::pin(
            self.client
                .player_shuffle(&device_id, shuffle)
                .send_no_response(),
        )
    }

    fn player_volume(&self, device_id: String, volume: u8) -> BoxFuture<SpotifyResult<()>> {
        Box::pin(
            self.client
                .player_volume(&device_id, volume)
                .send_no_response(),
        )
    }

    fn get_followed_artists(
        &self,
        after: Option<String>,
        limit: usize,
    ) -> BoxFuture<SpotifyResult<(Vec<ArtistSummary>, Option<String>)>> {
        Box::pin(async move {
            let result = self
                .client
                .get_followed_artists(after.as_deref(), limit)
                .send()
                .await?
                .deserialize()
                .ok_or(SpotifyApiError::NoContent)?;

            let cursor = result.artists.cursors.and_then(|c| c.after);
            let artists = result
                .artists
                .items
                .unwrap_or_default()
                .into_iter()
                .map(ArtistSummary::from)
                .collect();

            Ok((artists, cursor))
        })
    }

    fn recently_played(
        &self,
        limit: usize,
    ) -> BoxFuture<SpotifyResult<(Vec<SongDescription>, Vec<JumpBackContext>)>> {
        Box::pin(async move {
            // Short TTL so Home feels live; the response's own cache-control is
            // short, so `wrap_write` already keeps this from going stale for long.
            let recent = self
                .cache_get_or_write(RiffCacheKey::RecentlyPlayed(limit), None, |etag| {
                    self.client.recently_played(limit).etag(etag).send()
                })
                .await?;

            let items = recent.items.unwrap_or_default();

            // Jump-back contexts: the album each track was played from, deduped by
            // album id and kept in play order (newest first). Only albums carry
            // real art/title from this endpoint, so playlist-context plays fall
            // back to their album — every card stays visually correct.
            let mut seen_albums = std::collections::HashSet::<String>::new();
            let contexts = items
                .iter()
                .filter_map(|history| {
                    let album = &history.track.album;
                    if album.id.is_empty() || !seen_albums.insert(album.id.clone()) {
                        return None;
                    }
                    let art = ImageSet::from_images(
                        album.images().iter().map(|i| (i.width, i.url.clone())),
                    );
                    let subtitle = history
                        .track
                        .track
                        .artists
                        .iter()
                        .map(|a| a.name.clone())
                        .collect::<Vec<_>>()
                        .join(", ");
                    Some(JumpBackContext {
                        id: album.id.clone(),
                        kind: JumpBackKind::Album,
                        title: album.name.clone(),
                        subtitle,
                        art,
                    })
                })
                .collect();

            // Track strip: dedup consecutive/repeat plays of the same track,
            // keeping the most recent occurrence's order.
            let mut seen_tracks = std::collections::HashSet::<String>::new();
            let tracks: Vec<TrackItem> = items
                .into_iter()
                .map(|history| history.track)
                .filter(|t| seen_tracks.insert(t.track.id.clone()))
                .collect();

            let songs: Vec<SongDescription> = Page::new(tracks).into();
            Ok((songs, contexts))
        })
    }

    fn get_top_artists(&self, limit: usize) -> BoxFuture<SpotifyResult<Vec<ArtistSummary>>> {
        Box::pin(async move {
            let page = self
                .cache_get_or_write(RiffCacheKey::TopArtists(limit), None, |etag| {
                    self.client.get_top_artists(limit).etag(etag).send()
                })
                .await?;

            Ok(page.into_iter().map(ArtistSummary::from).collect())
        })
    }

    fn get_top_tracks(&self, limit: usize) -> BoxFuture<SpotifyResult<Vec<SongDescription>>> {
        Box::pin(async move {
            let page = self
                .cache_get_or_write(RiffCacheKey::TopTracks(limit), None, |etag| {
                    self.client.get_top_tracks(limit).etag(etag).send()
                })
                .await?;

            Ok(page.into())
        })
    }

    fn get_tracks(&self, ids: Vec<String>) -> BoxFuture<SpotifyResult<Vec<SongDescription>>> {
        Box::pin(async move {
            let mut songs: Vec<SongDescription> = Vec::with_capacity(ids.len());
            // Spotify caps /v1/tracks at 50 ids per request; chunk to stay under.
            for chunk in ids.chunks(50) {
                let tracks: Tracks = self
                    .client
                    .get_tracks(chunk)
                    .send()
                    .await?
                    .deserialize()
                    .ok_or(SpotifyApiError::NoContent)?;
                let batch: Vec<SongDescription> = tracks.into();
                songs.extend(batch);
            }
            Ok(songs)
        })
    }

    fn follow_artist(&self, id: &str) -> BoxFuture<SpotifyResult<()>> {
        let id = id.to_owned();
        Box::pin(async move { self.client.follow_artist(&id).send_no_response().await })
    }

    fn unfollow_artist(&self, id: &str) -> BoxFuture<SpotifyResult<()>> {
        let id = id.to_owned();
        Box::pin(async move { self.client.unfollow_artist(&id).send_no_response().await })
    }
}
