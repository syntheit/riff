mod api_models;
mod cached_client;
mod client;

pub mod cache;

pub use api_models::RemotePlaybackSnapshot;
pub use cached_client::{CachedSpotifyClient, SpotifyApiClient, SpotifyResult};
pub use client::SpotifyApiError;

use crate::auth::TokenStore;
use client::SpotifyClient;

pub async fn clear_user_cache() -> Option<()> {
    cache::CacheManager::for_dir("riff/net")?
        .clear_cache_pattern(&cached_client::USER_CACHE)
        .await
        .ok()
}

/// Outcome of a premium-status probe against the Spotify Web API.
///
/// The three states must be kept distinct at the call site: a request or auth
/// failure (`ProbeFailed`) must NOT be treated as "not premium". Only a
/// *successful* `/me` response that reports a non-premium `product` should be
/// treated as a genuine non-premium account.
#[derive(Debug)]
pub enum PremiumStatus {
    /// `/me` succeeded and reported `product == "premium"`.
    Premium,
    /// `/me` succeeded and reported a non-premium `product` (e.g. "free").
    NotPremium,
    /// The probe could not be completed (network error, 401 from an expired or
    /// revoked token, rate limiting, etc.). Says nothing about premium status.
    ProbeFailed(SpotifyApiError),
}

/// Probe whether the given access token belongs to a Spotify Premium account.
///
/// This uses the standard API client infrastructure but authenticates with an
/// explicit token (for use during login). Crucially it distinguishes a genuine
/// non-premium account (`/me` succeeded, `product != "premium"`) from a probe
/// failure such as a 401 caused by an expired/revoked access token — the latter
/// must never be interpreted as "not premium".
pub async fn check_premium(token: &str) -> PremiumStatus {
    let client = SpotifyClient::new(TokenStore::new());
    let response = match client.get_me().send_with_token(token).await {
        Ok(response) => response,
        Err(e) => return PremiumStatus::ProbeFailed(e),
    };
    let user: api_models::User = match response.deserialize() {
        Some(user) => user,
        None => return PremiumStatus::ProbeFailed(SpotifyApiError::NoContent),
    };
    if user.product.as_deref() == Some("premium") {
        PremiumStatus::Premium
    } else {
        PremiumStatus::NotPremium
    }
}
