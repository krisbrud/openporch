//! In-memory rate limits for the login and token endpoints. Each key (a client
//! address or a principal) has a token bucket that holds a minute's allowance and
//! refills evenly; a request that finds it empty gets `429` with `Retry-After`.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Mutex;
use std::time::Instant;

use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::request::Parts;

use crate::error::ApiError;
use crate::state::AppState;

/// Buckets kept before full ones are dropped, so the map stays bounded.
const MAX_KEYS: usize = 10_000;

/// A per-key limit of `per_minute` requests. Zero disables the limit.
#[derive(Debug)]
pub struct RateLimiter {
    per_minute: u32,
    buckets: Mutex<HashMap<String, Bucket>>,
}

#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: f64,
    updated: Instant,
}

impl RateLimiter {
    /// A limiter allowing `per_minute` requests per key.
    pub fn new(per_minute: u32) -> Self {
        Self {
            per_minute,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Takes one request for `key`, or returns `429` naming the seconds to wait.
    pub fn check(&self, key: &str) -> Result<(), ApiError> {
        self.check_at(key, Instant::now())
    }

    fn check_at(&self, key: &str, now: Instant) -> Result<(), ApiError> {
        if self.per_minute == 0 {
            return Ok(());
        }
        let capacity = f64::from(self.per_minute);
        let per_second = capacity / 60.0;
        let mut buckets = self
            .buckets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if buckets.len() >= MAX_KEYS && !buckets.contains_key(key) {
            buckets.retain(|_, b| refill(*b, now, per_second, capacity) < capacity);
        }
        let bucket = buckets.entry(key.to_owned()).or_insert(Bucket {
            tokens: capacity,
            updated: now,
        });
        bucket.tokens = refill(*bucket, now, per_second, capacity);
        bucket.updated = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            return Ok(());
        }
        let wait = ((1.0 - bucket.tokens) / per_second).ceil();
        // `wait` is at most 60 seconds, so the cast cannot truncate.
        Err(ApiError::rate_limited((wait as u64).max(1)))
    }
}

fn refill(b: Bucket, now: Instant, per_second: f64, capacity: f64) -> f64 {
    let elapsed = now.saturating_duration_since(b.updated).as_secs_f64();
    (b.tokens + elapsed * per_second).min(capacity)
}

/// The client's address, the key for unauthenticated limits. Behind a trusted
/// TLS-terminating proxy it is the last `X-Forwarded-For` entry, the one that proxy
/// added; otherwise the peer address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientAddr(pub String);

impl FromRequestParts<AppState> for ClientAddr {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        if state.config.tls_terminated_by_proxy
            && let Some(last) = parts
                .headers
                .get_all("x-forwarded-for")
                .iter()
                .filter_map(|v| v.to_str().ok())
                .flat_map(|v| v.split(','))
                .map(str::trim)
                .rfind(|v| !v.is_empty())
        {
            return Ok(Self(last.to_owned()));
        }
        let peer = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map_or_else(|| "unknown".to_owned(), |c| c.0.ip().to_string());
        Ok(Self(peer))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn a_minutes_allowance_then_429_until_refilled() {
        let limiter = RateLimiter::new(3);
        let t0 = Instant::now();
        for _ in 0..3 {
            limiter.check_at("1.2.3.4", t0).unwrap();
        }
        let e = limiter.check_at("1.2.3.4", t0).unwrap_err();
        assert_eq!(e.status, axum::http::StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(e.code, "API-007");
        assert_eq!(e.retry_after, Some(20));
        // Other keys are unaffected.
        limiter.check_at("5.6.7.8", t0).unwrap();
        // One request's worth refills in 20 seconds.
        let later = t0 + Duration::from_secs(20);
        limiter.check_at("1.2.3.4", later).unwrap();
        assert!(limiter.check_at("1.2.3.4", later).is_err());
    }

    #[test]
    fn zero_disables_the_limit() {
        let limiter = RateLimiter::new(0);
        let t0 = Instant::now();
        for _ in 0..1000 {
            limiter.check_at("k", t0).unwrap();
        }
    }

    #[test]
    fn idle_keys_are_dropped_when_the_map_is_full() {
        let limiter = RateLimiter::new(60);
        let t0 = Instant::now();
        for i in 0..MAX_KEYS {
            limiter.check_at(&i.to_string(), t0).unwrap();
        }
        let later = t0 + Duration::from_secs(5);
        limiter.check_at("new", later).unwrap();
        assert_eq!(limiter.buckets.lock().unwrap().len(), 1);
    }
}
