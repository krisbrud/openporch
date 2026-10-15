//! Browser logins in progress. A login starts with a `state` the browser carries to
//! the provider and back, bound to a cookie in the same browser, so a callback that
//! another browser started (login CSRF) is refused. A finished login is held under a
//! one-time login code until the page it returned to exchanges it for a session.
//!
//! Both kinds are kept in memory, keyed by hashes, used once, and expire quickly; a
//! restart abandons the logins in progress, which only means signing in again.

use std::collections::HashMap;
use std::sync::Mutex;

use openporch_store::ExternalIdentity;

use crate::auth;

/// How long a browser has to sign in with the provider.
pub const PENDING_TTL_MS: i64 = 10 * 60 * 1000;
/// How long a finished login waits for its page to exchange the login code.
pub const COMPLETED_TTL_MS: i64 = 2 * 60 * 1000;
/// The most logins of each kind held at once; more are refused until some expire.
const CAPACITY: usize = 10_000;

/// A started login.
#[derive(Debug)]
struct Pending {
    binding_hash: String,
    return_to: String,
    expires_at: i64,
}

/// A finished login, waiting to be exchanged.
#[derive(Debug)]
struct Completed {
    identity: ExternalIdentity,
    expires_at: i64,
}

/// A started login: the `state` for the provider, the `binding` for the cookie.
#[derive(Debug)]
pub struct Started {
    /// Sent to the provider and carried back to the callback.
    pub state: String,
    /// Kept in the browser's cookie.
    pub binding: String,
}

/// Why a callback was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallbackError {
    /// No login with this state is in progress: unknown, used, or expired.
    UnknownState,
    /// The login was started in another browser.
    WrongBrowser,
}

/// Logins in progress.
#[derive(Debug, Default)]
pub struct BrowserLogins {
    pending: Mutex<HashMap<String, Pending>>,
    completed: Mutex<HashMap<String, Completed>>,
}

fn prune<T>(map: &mut HashMap<String, T>, now: i64, expires: impl Fn(&T) -> i64) {
    map.retain(|_, v| expires(v) > now);
}

impl BrowserLogins {
    /// Starts a login that returns to `return_to`. `None` when too many are in
    /// progress.
    pub fn start(&self, return_to: &str, now: i64) -> Option<Started> {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        prune(&mut pending, now, |p| p.expires_at);
        if pending.len() >= CAPACITY {
            return None;
        }
        let started = Started {
            state: auth::random_id(32),
            binding: auth::random_id(32),
        };
        pending.insert(
            auth::hash_token(&started.state),
            Pending {
                binding_hash: auth::hash_token(&started.binding),
                return_to: return_to.to_owned(),
                expires_at: now + PENDING_TTL_MS,
            },
        );
        Some(started)
    }

    /// Ends the login `state` names, which `binding` from the browser's cookie must
    /// have started, and returns where it returns to. A state is accepted once, even
    /// when the binding is wrong, so a guessed or replayed callback cannot be retried.
    pub fn callback(
        &self,
        state: &str,
        binding: Option<&str>,
        now: i64,
    ) -> Result<String, CallbackError> {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        let login = pending
            .remove(&auth::hash_token(state))
            .filter(|p| p.expires_at > now)
            .ok_or(CallbackError::UnknownState)?;
        let presented = binding.map(auth::hash_token).unwrap_or_default();
        if !constant_time_eq(presented.as_bytes(), login.binding_hash.as_bytes()) {
            return Err(CallbackError::WrongBrowser);
        }
        Ok(login.return_to)
    }

    /// Holds a verified identity under a new one-time login code. `None` when too
    /// many finished logins wait.
    pub fn complete(&self, identity: ExternalIdentity, now: i64) -> Option<String> {
        let mut completed = self.completed.lock().unwrap_or_else(|e| e.into_inner());
        prune(&mut completed, now, |c| c.expires_at);
        if completed.len() >= CAPACITY {
            return None;
        }
        let code = auth::random_id(32);
        completed.insert(
            auth::hash_token(&code),
            Completed {
                identity,
                expires_at: now + COMPLETED_TTL_MS,
            },
        );
        Some(code)
    }

    /// Takes the identity a login code holds, once.
    pub fn exchange(&self, login_code: &str, now: i64) -> Option<ExternalIdentity> {
        let mut completed = self.completed.lock().unwrap_or_else(|e| e.into_inner());
        completed
            .remove(&auth::hash_token(login_code))
            .filter(|c| c.expires_at > now)
            .map(|c| c.identity)
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> ExternalIdentity {
        ExternalIdentity {
            provider: "github".into(),
            subject: "1".into(),
            name: "Octo".into(),
            login: Some("octocat".into()),
            verified_emails: vec![],
        }
    }

    #[test]
    fn a_state_is_used_once_by_the_browser_that_started_it() {
        let logins = BrowserLogins::default();
        let a = logins.start("https://portal.example/done", 0).unwrap();
        assert_eq!(
            logins.callback(&a.state, Some(&a.binding), 1),
            Ok("https://portal.example/done".into())
        );
        assert_eq!(
            logins.callback(&a.state, Some(&a.binding), 2),
            Err(CallbackError::UnknownState)
        );

        // Another browser's cookie, or none, is refused, and the state is spent.
        let b = logins.start("https://portal.example/done", 0).unwrap();
        let c = logins.start("https://portal.example/done", 0).unwrap();
        assert_eq!(
            logins.callback(&b.state, Some(&c.binding), 1),
            Err(CallbackError::WrongBrowser)
        );
        assert_eq!(
            logins.callback(&b.state, Some(&b.binding), 1),
            Err(CallbackError::UnknownState)
        );
        assert_eq!(
            logins.callback(&c.state, None, 1),
            Err(CallbackError::WrongBrowser)
        );
    }

    #[test]
    fn logins_expire() {
        let logins = BrowserLogins::default();
        let a = logins.start("https://portal.example/done", 0).unwrap();
        assert_eq!(
            logins.callback(&a.state, Some(&a.binding), PENDING_TTL_MS),
            Err(CallbackError::UnknownState)
        );
        let code = logins.complete(identity(), 0).unwrap();
        assert!(logins.exchange(&code, COMPLETED_TTL_MS).is_none());
        let code = logins.complete(identity(), 0).unwrap();
        assert_eq!(logins.exchange(&code, 1), Some(identity()));
        assert!(
            logins.exchange(&code, 2).is_none(),
            "a login code is used once"
        );
    }
}
