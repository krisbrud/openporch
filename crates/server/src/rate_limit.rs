//! In-memory rate limits for the login and token endpoints. Each key (a client
//! address or a principal) has a token bucket that holds a minute's allowance and
//! refills evenly; a request that finds it empty gets `429` with `Retry-After`.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::sync::Mutex;
use std::time::Instant;

use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::request::Parts;

use crate::error::ApiError;
use crate::state::AppState;

/// Buckets kept at most. When the map is full, full buckets are dropped (at most
/// once a second), and keys that still find no room share one overflow bucket, so
/// memory stays bounded however many addresses a client cycles through.
const MAX_KEYS: usize = 10_000;
/// The key shared by requests that find the map full.
const OVERFLOW: &str = "overflow";

/// A per-key limit of `per_minute` requests. Zero disables the limit.
#[derive(Debug)]
pub struct RateLimiter {
    per_minute: u32,
    buckets: Mutex<Buckets>,
}

#[derive(Debug, Default)]
struct Buckets {
    map: HashMap<String, Bucket>,
    swept: Option<Instant>,
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
            buckets: Mutex::new(Buckets::default()),
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
        let mut guard = self
            .buckets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let buckets = &mut *guard;
        let mut key = key;
        if buckets.map.len() >= MAX_KEYS && !buckets.map.contains_key(key) {
            let due = buckets
                .swept
                .is_none_or(|t| now.saturating_duration_since(t).as_secs() >= 1);
            if due {
                buckets
                    .map
                    .retain(|_, b| refill(*b, now, per_second, capacity) < capacity);
                buckets.swept = Some(now);
            }
            if buckets.map.len() >= MAX_KEYS {
                key = OVERFLOW;
            }
        }
        let bucket = buckets.map.entry(key.to_owned()).or_insert(Bucket {
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
/// added; otherwise the peer address. IPv6 addresses are keyed by their /64, since
/// one host can usually use every address in it.
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
            return Ok(Self(client_key(last)));
        }
        let peer = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map_or_else(
                || "unknown".to_owned(),
                |c| client_key(&c.0.ip().to_string()),
            );
        Ok(Self(peer))
    }
}

/// The limit key for an address: IPv4 as is, IPv6 by its /64.
fn client_key(addr: &str) -> String {
    match addr.parse::<IpAddr>() {
        Ok(IpAddr::V6(v6)) => match v6.to_ipv4_mapped() {
            Some(v4) => v4.to_string(),
            None => {
                let s = v6.segments();
                let net = Ipv6Addr::new(s[0], s[1], s[2], s[3], 0, 0, 0, 0);
                format!("{net}/64")
            }
        },
        Ok(IpAddr::V4(v4)) => v4.to_string(),
        Err(_) => addr.to_owned(),
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
        assert_eq!(limiter.buckets.lock().unwrap().map.len(), 1);
    }

    #[test]
    fn a_full_map_of_busy_keys_stays_bounded() {
        let limiter = RateLimiter::new(1);
        let t0 = Instant::now();
        for i in 0..MAX_KEYS {
            limiter.check_at(&i.to_string(), t0).unwrap();
        }
        // No bucket has refilled, so new keys share the overflow bucket.
        limiter.check_at("fresh-1", t0).unwrap();
        assert!(limiter.check_at("fresh-2", t0).is_err());
        assert_eq!(limiter.buckets.lock().unwrap().map.len(), MAX_KEYS + 1);
    }

    #[test]
    fn ipv6_clients_are_keyed_by_their_64() {
        assert_eq!(client_key("2001:db8:1:2:aaaa::1"), "2001:db8:1:2::/64");
        assert_eq!(client_key("2001:db8:1:2:bbbb::9"), "2001:db8:1:2::/64");
        assert_eq!(client_key("::ffff:192.0.2.1"), "192.0.2.1");
        assert_eq!(client_key("192.0.2.1"), "192.0.2.1");
    }
}
