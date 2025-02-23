//! Utilities for authorization of logged in and anonymous users.

use super::errors::FrontendErrorResponse;
use crate::app::{UserKeyData, Web3ProxyApp};
use crate::jsonrpc::JsonRpcRequest;
use anyhow::Context;
use axum::headers::{authorization::Bearer, Origin, Referer, UserAgent};
use axum::TypedHeader;
use chrono::Utc;
use deferred_rate_limiter::DeferredRateLimitResult;
use entities::{user, user_keys};
use ipnet::IpNet;
use redis_rate_limiter::redis::AsyncCommands;
use redis_rate_limiter::RedisRateLimitResult;
use sea_orm::{prelude::Decimal, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};
use serde::Serialize;
use std::fmt::Display;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64};
use std::{net::IpAddr, str::FromStr, sync::Arc};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;
use tracing::{error, trace};
use ulid::Ulid;
use uuid::Uuid;

/// This lets us use UUID and ULID while we transition to only ULIDs
/// TODO: include the key's description.
#[derive(Copy, Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub enum UserKey {
    Ulid(Ulid),
    Uuid(Uuid),
}

#[derive(Debug)]
pub enum RateLimitResult {
    /// contains the IP of the anonymous user
    /// TODO: option inside or outside the arc?
    AllowedIp(IpAddr, OwnedSemaphorePermit),
    /// contains the user_key_id of an authenticated user
    AllowedUser(UserKeyData, Option<OwnedSemaphorePermit>),
    /// contains the IP and retry_at of the anonymous user
    RateLimitedIp(IpAddr, Option<Instant>),
    /// contains the user_key_id and retry_at of an authenticated user key
    RateLimitedUser(UserKeyData, Option<Instant>),
    /// This key is not in our database. Deny access!
    UnknownKey,
}

#[derive(Clone, Debug, Serialize)]
pub struct AuthorizedKey {
    pub ip: IpAddr,
    pub origin: Option<String>,
    pub user_id: u64,
    pub user_key_id: u64,
    // TODO: just use an f32? even an f16 is probably fine
    pub log_revert_chance: Decimal,
}

#[derive(Debug)]
pub struct RequestMetadata {
    pub start_datetime: chrono::DateTime<Utc>,
    pub start_instant: tokio::time::Instant,
    // TODO: better name for this
    pub period_seconds: u64,
    pub request_bytes: u64,
    /// if this is 0, there was a cache_hit
    pub backend_requests: AtomicU32,
    pub no_servers: AtomicU32,
    pub error_response: AtomicBool,
    pub response_bytes: AtomicU64,
    pub response_millis: AtomicU64,
}

#[derive(Clone, Debug)]
pub enum AuthorizedRequest {
    /// Request from this app
    Internal,
    /// Request from an anonymous IP address
    Ip(IpAddr, Option<Origin>),
    /// Request from an authenticated and authorized user
    User(Option<DatabaseConnection>, AuthorizedKey),
}

impl RequestMetadata {
    pub fn new(period_seconds: u64, request: &JsonRpcRequest) -> anyhow::Result<Self> {
        // TODO: how can we do this without turning it into a string first. this is going to slow us down!
        let request_bytes = serde_json::to_string(request)
            .context("finding request size")?
            .len()
            .try_into()?;

        let new = Self {
            start_instant: Instant::now(),
            start_datetime: Utc::now(),
            period_seconds,
            request_bytes,
            backend_requests: 0.into(),
            no_servers: 0.into(),
            error_response: false.into(),
            response_bytes: 0.into(),
            response_millis: 0.into(),
        };

        Ok(new)
    }
}

impl UserKey {
    pub fn new() -> Self {
        Ulid::new().into()
    }
}

impl Display for UserKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // TODO: do this without dereferencing
        let ulid: Ulid = (*self).into();

        ulid.fmt(f)
    }
}

impl Default for UserKey {
    fn default() -> Self {
        Self::new()
    }
}

impl FromStr for UserKey {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Ok(ulid) = s.parse::<Ulid>() {
            Ok(ulid.into())
        } else if let Ok(uuid) = s.parse::<Uuid>() {
            Ok(uuid.into())
        } else {
            // TODO: custom error type so that this shows as a 400
            Err(anyhow::anyhow!("UserKey was not a ULID or UUID"))
        }
    }
}

impl From<Ulid> for UserKey {
    fn from(x: Ulid) -> Self {
        UserKey::Ulid(x)
    }
}

impl From<Uuid> for UserKey {
    fn from(x: Uuid) -> Self {
        UserKey::Uuid(x)
    }
}

impl From<UserKey> for Ulid {
    fn from(x: UserKey) -> Self {
        match x {
            UserKey::Ulid(x) => x,
            UserKey::Uuid(x) => Ulid::from(x.as_u128()),
        }
    }
}

impl From<UserKey> for Uuid {
    fn from(x: UserKey) -> Self {
        match x {
            UserKey::Ulid(x) => Uuid::from_u128(x.0),
            UserKey::Uuid(x) => x,
        }
    }
}

impl AuthorizedKey {
    pub fn try_new(
        ip: IpAddr,
        origin: Option<Origin>,
        referer: Option<Referer>,
        user_agent: Option<UserAgent>,
        user_key_data: UserKeyData,
    ) -> anyhow::Result<Self> {
        // check ip
        match &user_key_data.allowed_ips {
            None => {}
            Some(allowed_ips) => {
                if !allowed_ips.iter().any(|x| x.contains(&ip)) {
                    return Err(anyhow::anyhow!("IP is not allowed!"));
                }
            }
        }

        // check origin
        // TODO: do this with the Origin type instead of a String?
        let origin = origin.map(|x| x.to_string());
        match (&origin, &user_key_data.allowed_origins) {
            (None, None) => {}
            (Some(_), None) => {}
            (None, Some(_)) => return Err(anyhow::anyhow!("Origin required")),
            (Some(origin), Some(allowed_origins)) => {
                let origin = origin.to_string();

                if !allowed_origins.contains(&origin) {
                    return Err(anyhow::anyhow!("IP is not allowed!"));
                }
            }
        }

        // check referer
        match (referer, &user_key_data.allowed_referers) {
            (None, None) => {}
            (Some(_), None) => {}
            (None, Some(_)) => return Err(anyhow::anyhow!("Referer required")),
            (Some(referer), Some(allowed_referers)) => {
                if !allowed_referers.contains(&referer) {
                    return Err(anyhow::anyhow!("Referer is not allowed!"));
                }
            }
        }

        // check user_agent
        match (user_agent, &user_key_data.allowed_user_agents) {
            (None, None) => {}
            (Some(_), None) => {}
            (None, Some(_)) => return Err(anyhow::anyhow!("User agent required")),
            (Some(user_agent), Some(allowed_user_agents)) => {
                if !allowed_user_agents.contains(&user_agent) {
                    return Err(anyhow::anyhow!("User agent is not allowed!"));
                }
            }
        }

        Ok(Self {
            ip,
            origin,
            user_id: user_key_data.user_id,
            user_key_id: user_key_data.user_key_id,
            log_revert_chance: user_key_data.log_revert_chance,
        })
    }
}

impl AuthorizedRequest {
    /// Only User has a database connection in case it needs to save a revert to the database.
    pub fn db_conn(&self) -> Option<&DatabaseConnection> {
        match self {
            Self::User(x, _) => x.as_ref(),
            _ => None,
        }
    }
}

impl Display for &AuthorizedRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthorizedRequest::Internal => f.write_str("int"),
            AuthorizedRequest::Ip(x, _) => f.write_str(&format!("ip-{}", x)),
            AuthorizedRequest::User(_, x) => f.write_str(&format!("uk-{}", x.user_key_id)),
        }
    }
}

pub async fn login_is_authorized(
    app: &Web3ProxyApp,
    ip: IpAddr,
) -> Result<(AuthorizedRequest, OwnedSemaphorePermit), FrontendErrorResponse> {
    // TODO: i think we could write an `impl From` for this
    // TODO: move this to an AuthorizedUser extrator
    let (ip, semaphore) = match app.rate_limit_login(ip).await? {
        RateLimitResult::AllowedIp(x, semaphore) => (x, semaphore),
        RateLimitResult::RateLimitedIp(x, retry_at) => {
            return Err(FrontendErrorResponse::RateLimitedIp(x, retry_at));
        }
        // TODO: don't panic. give the user an error
        x => unimplemented!("rate_limit_login shouldn't ever see these: {:?}", x),
    };

    Ok((AuthorizedRequest::Ip(ip, None), semaphore))
}

// TODO: where should we use this?
pub async fn bearer_is_authorized(
    app: &Web3ProxyApp,
    bearer: Bearer,
) -> Result<(AuthorizedRequest, Option<OwnedSemaphorePermit>), FrontendErrorResponse> {
    let mut redis_conn = app.redis_conn().await.context("Getting redis connection")?;

    // TODO: verify that bearer.token() is a Ulid?
    let bearer_cache_key = format!("bearer:{}", bearer.token());

    // turn bearer into a user key id
    let user_id: u64 = redis_conn
        .get(bearer_cache_key)
        .await
        .context("unknown bearer token")?;

    let db_conn = app.db_conn().context("Getting database connection")?;

    // turn user key id into a user key
    let user_data = user::Entity::find_by_id(user_id)
        .one(&db_conn)
        .await
        .context("fetching user by id")?
        .context("unknown user id")?;

    todo!("rewrite this. key_is_authorized is wrong. we should check user ids instead")
}

pub async fn ip_is_authorized(
    app: &Web3ProxyApp,
    ip: IpAddr,
    origin: Option<TypedHeader<Origin>>,
) -> Result<(AuthorizedRequest, Option<OwnedSemaphorePermit>), FrontendErrorResponse> {
    let origin = origin.map(|x| x.0);

    // TODO: i think we could write an `impl From` for this
    // TODO: move this to an AuthorizedUser extrator
    let (ip, semaphore) = match app.rate_limit_by_ip(ip, origin.as_ref()).await? {
        RateLimitResult::AllowedIp(ip, semaphore) => (ip, Some(semaphore)),
        RateLimitResult::RateLimitedIp(x, retry_at) => {
            return Err(FrontendErrorResponse::RateLimitedIp(x, retry_at));
        }
        // TODO: don't panic. give the user an error
        x => unimplemented!("rate_limit_by_ip shouldn't ever see these: {:?}", x),
    };

    // semaphore won't ever be None, but its easier if key auth and ip auth work the same way
    Ok((AuthorizedRequest::Ip(ip, origin), semaphore))
}

pub async fn key_is_authorized(
    app: &Web3ProxyApp,
    user_key: UserKey,
    ip: IpAddr,
    origin: Option<Origin>,
    referer: Option<Referer>,
    user_agent: Option<UserAgent>,
) -> Result<(AuthorizedRequest, Option<OwnedSemaphorePermit>), FrontendErrorResponse> {
    // check the rate limits. error if over the limit
    let (user_data, semaphore) = match app.rate_limit_by_key(user_key).await? {
        RateLimitResult::AllowedUser(x, semaphore) => (x, semaphore),
        RateLimitResult::RateLimitedUser(x, retry_at) => {
            return Err(FrontendErrorResponse::RateLimitedUser(x, retry_at));
        }
        RateLimitResult::UnknownKey => return Err(FrontendErrorResponse::UnknownKey),
        // TODO: don't panic. give the user an error
        x => unimplemented!("rate_limit_by_key shouldn't ever see these: {:?}", x),
    };

    let authorized_user = AuthorizedKey::try_new(ip, origin, referer, user_agent, user_data)?;

    let db_conn = app.db_conn.clone();

    Ok((AuthorizedRequest::User(db_conn, authorized_user), semaphore))
}

impl Web3ProxyApp {
    pub async fn ip_semaphore(&self, ip: IpAddr) -> anyhow::Result<OwnedSemaphorePermit> {
        let semaphore = self
            .ip_semaphores
            .get_with(ip, async move {
                // TODO: get semaphore size from app config
                let s = Semaphore::const_new(10);
                Arc::new(s)
            })
            .await;

        // if semaphore.available_permits() == 0 {
        //     // TODO: concurrent limit hit! emit a stat? less important for anon users
        //     // TODO: there is probably a race here
        // }

        let semaphore_permit = semaphore.acquire_owned().await?;

        Ok(semaphore_permit)
    }

    pub async fn user_key_semaphore(
        &self,
        user_data: &UserKeyData,
    ) -> anyhow::Result<Option<OwnedSemaphorePermit>> {
        if let Some(max_concurrent_requests) = user_data.max_concurrent_requests {
            let semaphore = self
                .user_key_semaphores
                .try_get_with(user_data.user_key_id, async move {
                    let s = Semaphore::const_new(max_concurrent_requests.try_into()?);
                    trace!("new semaphore for user_key_id {}", user_data.user_key_id);
                    Ok::<_, anyhow::Error>(Arc::new(s))
                })
                .await
                // TODO: is this the best way to handle an arc
                .map_err(|err| anyhow::anyhow!(err))?;

            // if semaphore.available_permits() == 0 {
            //     // TODO: concurrent limit hit! emit a stat
            // }

            let semaphore_permit = semaphore.acquire_owned().await?;

            Ok(Some(semaphore_permit))
        } else {
            Ok(None)
        }
    }

    pub async fn rate_limit_login(&self, ip: IpAddr) -> anyhow::Result<RateLimitResult> {
        // TODO: dry this up with rate_limit_by_key
        let semaphore = self.ip_semaphore(ip).await?;

        if let Some(rate_limiter) = &self.login_rate_limiter {
            match rate_limiter.throttle_label(&ip.to_string(), None, 1).await {
                Ok(RedisRateLimitResult::Allowed(_)) => {
                    Ok(RateLimitResult::AllowedIp(ip, semaphore))
                }
                Ok(RedisRateLimitResult::RetryAt(retry_at, _)) => {
                    // TODO: set headers so they know when they can retry
                    // TODO: debug or trace?
                    // this is too verbose, but a stat might be good
                    trace!(?ip, "login rate limit exceeded until {:?}", retry_at);
                    Ok(RateLimitResult::RateLimitedIp(ip, Some(retry_at)))
                }
                Ok(RedisRateLimitResult::RetryNever) => {
                    // TODO: i don't think we'll get here. maybe if we ban an IP forever? seems unlikely
                    trace!(?ip, "login rate limit is 0");
                    Ok(RateLimitResult::RateLimitedIp(ip, None))
                }
                Err(err) => {
                    // internal error, not rate limit being hit
                    // TODO: i really want axum to do this for us in a single place.
                    error!(?err, "login rate limiter is unhappy. allowing ip");

                    Ok(RateLimitResult::AllowedIp(ip, semaphore))
                }
            }
        } else {
            // TODO: if no redis, rate limit with a local cache? "warn!" probably isn't right
            todo!("no rate limiter");
        }
    }

    pub async fn rate_limit_by_ip(
        &self,
        ip: IpAddr,
        origin: Option<&Origin>,
    ) -> anyhow::Result<RateLimitResult> {
        // TODO: dry this up with rate_limit_by_key
        let semaphore = self.ip_semaphore(ip).await?;

        if let Some(rate_limiter) = &self.frontend_ip_rate_limiter {
            let max_requests_per_period = origin
                .map(|origin| {
                    self.config
                        .allowed_origin_requests_per_minute
                        .get(&origin.to_string())
                        .cloned()
                })
                .unwrap_or_default();

            match rate_limiter.throttle(ip, max_requests_per_period, 1).await {
                Ok(DeferredRateLimitResult::Allowed) => {
                    Ok(RateLimitResult::AllowedIp(ip, semaphore))
                }
                Ok(DeferredRateLimitResult::RetryAt(retry_at)) => {
                    // TODO: set headers so they know when they can retry
                    // TODO: debug or trace?
                    // this is too verbose, but a stat might be good
                    trace!(?ip, "rate limit exceeded until {:?}", retry_at);
                    Ok(RateLimitResult::RateLimitedIp(ip, Some(retry_at)))
                }
                Ok(DeferredRateLimitResult::RetryNever) => {
                    // TODO: i don't think we'll get here. maybe if we ban an IP forever? seems unlikely
                    trace!(?ip, "rate limit is 0");
                    Ok(RateLimitResult::RateLimitedIp(ip, None))
                }
                Err(err) => {
                    // internal error, not rate limit being hit
                    // TODO: i really want axum to do this for us in a single place.
                    error!(?err, "rate limiter is unhappy. allowing ip");

                    Ok(RateLimitResult::AllowedIp(ip, semaphore))
                }
            }
        } else {
            // TODO: if no redis, rate limit with a local cache? "warn!" probably isn't right
            Ok(RateLimitResult::AllowedIp(ip, semaphore))
        }
    }

    // check the local cache for user data, or query the database
    pub(crate) async fn user_data(&self, user_key: UserKey) -> anyhow::Result<UserKeyData> {
        let user_data: Result<_, Arc<anyhow::Error>> = self
            .user_key_cache
            .try_get_with(user_key.into(), async move {
                trace!(?user_key, "user_cache miss");

                let db_conn = self.db_conn().context("Getting database connection")?;

                let user_uuid: Uuid = user_key.into();

                // TODO: join the user table to this to return the User? we don't always need it
                match user_keys::Entity::find()
                    .filter(user_keys::Column::ApiKey.eq(user_uuid))
                    .filter(user_keys::Column::Active.eq(true))
                    .one(&db_conn)
                    .await?
                {
                    Some(user_key_model) => {
                        let allowed_ips: Option<Vec<IpNet>> =
                            user_key_model.allowed_ips.map(|allowed_ips| {
                                serde_json::from_str::<Vec<String>>(&allowed_ips)
                                    .expect("allowed_ips should always parse")
                                    .into_iter()
                                    // TODO: try_for_each
                                    .map(|x| {
                                        x.parse::<IpNet>().expect("ip address should always parse")
                                    })
                                    .collect()
                            });

                        // TODO: should this be an Option<Vec<Origin>>?
                        let allowed_origins =
                            user_key_model.allowed_origins.map(|allowed_origins| {
                                serde_json::from_str::<Vec<String>>(&allowed_origins)
                                    .expect("allowed_origins should always parse")
                            });

                        let allowed_referers =
                            user_key_model.allowed_referers.map(|allowed_referers| {
                                serde_json::from_str::<Vec<String>>(&allowed_referers)
                                    .expect("allowed_referers should always parse")
                                    .into_iter()
                                    // TODO: try_for_each
                                    .map(|x| {
                                        x.parse::<Referer>().expect("referer should always parse")
                                    })
                                    .collect()
                            });

                        let allowed_user_agents =
                            user_key_model
                                .allowed_user_agents
                                .map(|allowed_user_agents| {
                                    serde_json::from_str::<Vec<String>>(&allowed_user_agents)
                                        .expect("allowed_user_agents should always parse")
                                        .into_iter()
                                        // TODO: try_for_each
                                        .map(|x| {
                                            x.parse::<UserAgent>()
                                                .expect("user agent should always parse")
                                        })
                                        .collect()
                                });

                        Ok(UserKeyData {
                            user_id: user_key_model.user_id,
                            user_key_id: user_key_model.id,
                            max_requests_per_period: user_key_model.requests_per_minute,
                            max_concurrent_requests: user_key_model.max_concurrent_requests,
                            allowed_ips,
                            allowed_origins,
                            allowed_referers,
                            allowed_user_agents,
                            log_revert_chance: user_key_model.log_revert_chance,
                        })
                    }
                    None => Ok(UserKeyData::default()),
                }
            })
            .await;

        // TODO: what's the best way to handle this arc? try_unwrap will not work
        user_data.map_err(|err| anyhow::anyhow!(err))
    }

    pub async fn rate_limit_by_key(&self, user_key: UserKey) -> anyhow::Result<RateLimitResult> {
        let user_data = self.user_data(user_key).await?;

        if user_data.user_key_id == 0 {
            return Ok(RateLimitResult::UnknownKey);
        }

        let semaphore = self.user_key_semaphore(&user_data).await?;

        let user_max_requests_per_period = match user_data.max_requests_per_period {
            None => {
                return Ok(RateLimitResult::AllowedUser(user_data, semaphore));
            }
            Some(x) => x,
        };

        // user key is valid. now check rate limits
        if let Some(rate_limiter) = &self.frontend_key_rate_limiter {
            match rate_limiter
                .throttle(user_key.into(), Some(user_max_requests_per_period), 1)
                .await
            {
                Ok(DeferredRateLimitResult::Allowed) => {
                    Ok(RateLimitResult::AllowedUser(user_data, semaphore))
                }
                Ok(DeferredRateLimitResult::RetryAt(retry_at)) => {
                    // TODO: set headers so they know when they can retry
                    // TODO: debug or trace?
                    // this is too verbose, but a stat might be good
                    // TODO: keys are secrets! use the id instead
                    // TODO: emit a stat
                    trace!(?user_key, "rate limit exceeded until {:?}", retry_at);
                    Ok(RateLimitResult::RateLimitedUser(user_data, Some(retry_at)))
                }
                Ok(DeferredRateLimitResult::RetryNever) => {
                    // TODO: keys are secret. don't log them!
                    trace!(?user_key, "rate limit is 0");
                    // TODO: emit a stat
                    Ok(RateLimitResult::RateLimitedUser(user_data, None))
                }
                Err(err) => {
                    // internal error, not rate limit being hit
                    // TODO: i really want axum to do this for us in a single place.
                    error!(?err, "rate limiter is unhappy. allowing ip");

                    Ok(RateLimitResult::AllowedUser(user_data, semaphore))
                }
            }
        } else {
            // TODO: if no redis, rate limit with just a local cache?
            Ok(RateLimitResult::AllowedUser(user_data, semaphore))
        }
    }
}
