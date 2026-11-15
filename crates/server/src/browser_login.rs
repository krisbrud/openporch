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
use zeroize::Zeroizing;

use crate::auth;

/// How long a browser has to sign in with the provider.
pub const PENDING_TTL_MS: i64 = 10 * 60 * 1000;
/// How long a finished login waits for its page to exchange the login code.
pub const COMPLETED_TTL_MS: i64 = 2 * 60 * 1000;
/// The most logins of each kind held at once; more are refused until some expire.
const CAPACITY: usize = 10_000;

/// What an OpenID Connect login keeps until its callback: the `nonce` the ID token
/// must carry and the PKCE code verifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginSecrets {
    /// The nonce sent with the authorization request.
    pub nonce: Zeroizing<String>,
    /// The PKCE code verifier, sent with the code exchange.
    pub verifier: Zeroizing<String>,
}

/// A started login.
#[derive(Debug)]
struct Pending {
    provider: String,
    binding_hash: String,
    return_to: String,
    secrets: Option<LoginSecrets>,
    expires_at: i64,
}

/// A finished login, waiting to be exchanged.
#[derive(Debug)]
struct Completed {
    provider: String,
    identity: ExternalIdentity,
    expires_at: i64,
}

/// A login whose callback arrived from the browser that started it.
#[derive(Debug)]
pub struct Resumed {
    /// Where the browser returns to.
    pub return_to: String,
    /// The login's secrets, for an OpenID Connect login.
    pub secrets: Option<LoginSecrets>,
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
    /// Starts a login through `provider` that returns to `return_to`, keeping
    /// `secrets` for its callback. `None` when too many are in progress.
    pub fn start(
        &self,
        provider: &str,
        return_to: &str,
        secrets: Option<LoginSecrets>,
        now: i64,
    ) -> Option<Started> {
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
                provider: provider.to_owned(),
                binding_hash: auth::hash_token(&started.binding),
                return_to: return_to.to_owned(),
                secrets,
                expires_at: now + PENDING_TTL_MS,
            },
        );
        Some(started)
    }

    /// Ends the login `state` names, which `binding` from the browser's cookie must
    /// have started through `provider`, and resumes it. A state is accepted once,
    /// even when the binding or provider is wrong, so a guessed or replayed callback
    /// cannot be retried.
    pub fn callback(
        &self,
        provider: &str,
        state: &str,
        binding: Option<&str>,
        now: i64,
    ) -> Result<Resumed, CallbackError> {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        let login = pending
            .remove(&auth::hash_token(state))
            .filter(|p| p.expires_at > now && p.provider == provider)
            .ok_or(CallbackError::UnknownState)?;
        let presented = binding.map(auth::hash_token).unwrap_or_default();
        if !constant_time_eq(presented.as_bytes(), login.binding_hash.as_bytes()) {
            return Err(CallbackError::WrongBrowser);
        }
        Ok(Resumed {
            return_to: login.return_to,
            secrets: login.secrets,
        })
    }

    /// Holds an identity `provider` verified under a new one-time login code. `None`
    /// when too many finished logins wait.
    pub fn complete(&self, provider: &str, identity: ExternalIdentity, now: i64) -> Option<String> {
        let mut completed = self.completed.lock().unwrap_or_else(|e| e.into_inner());
        prune(&mut completed, now, |c| c.expires_at);
        if completed.len() >= CAPACITY {
            return None;
        }
        let code = auth::random_id(32);
        completed.insert(
            auth::hash_token(&code),
            Completed {
                provider: provider.to_owned(),
                identity,
                expires_at: now + COMPLETED_TTL_MS,
            },
        );
        Some(code)
    }

    /// Takes the identity a login code from `provider` holds, once.
    pub fn exchange(&self, provider: &str, login_code: &str, now: i64) -> Option<ExternalIdentity> {
        let mut completed = self.completed.lock().unwrap_or_else(|e| e.into_inner());
        completed
            .remove(&auth::hash_token(login_code))
            .filter(|c| c.expires_at > now && c.provider == provider)
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
        let a = logins
            .start("github", "https://portal.example/done", None, 0)
            .unwrap();
        assert_eq!(
            logins
                .callback("github", &a.state, Some(&a.binding), 1)
                .map(|r| r.return_to),
            Ok("https://portal.example/done".into())
        );
        assert_eq!(
            logins
                .callback("github", &a.state, Some(&a.binding), 2)
                .err(),
            Some(CallbackError::UnknownState)
        );

        // Another browser's cookie, or none, is refused, and the state is spent.
        let b = logins
            .start("github", "https://portal.example/done", None, 0)
            .unwrap();
        let c = logins
            .start("github", "https://portal.example/done", None, 0)
            .unwrap();
        assert_eq!(
            logins
                .callback("github", &b.state, Some(&c.binding), 1)
                .err(),
            Some(CallbackError::WrongBrowser)
        );
        assert_eq!(
            logins
                .callback("github", &b.state, Some(&b.binding), 1)
                .err(),
            Some(CallbackError::UnknownState)
        );
        assert_eq!(
            logins.callback("github", &c.state, None, 1).err(),
            Some(CallbackError::WrongBrowser)
        );
    }

    #[test]
    fn a_login_resumes_only_through_the_provider_that_started_it() {
        let logins = BrowserLogins::default();
        let secrets = LoginSecrets {
            nonce: Zeroizing::new("n".into()),
            verifier: Zeroizing::new("v".into()),
        };
        let a = logins
            .start(
                "corp",
                "https://portal.example/done",
                Some(secrets.clone()),
                0,
            )
            .unwrap();
        assert_eq!(
            logins
                .callback("github", &a.state, Some(&a.binding), 1)
                .err(),
            Some(CallbackError::UnknownState)
        );
        // The state was spent by the wrong callback.
        assert_eq!(
            logins.callback("corp", &a.state, Some(&a.binding), 1).err(),
            Some(CallbackError::UnknownState)
        );
        let b = logins
            .start(
                "corp",
                "https://portal.example/done",
                Some(secrets.clone()),
                0,
            )
            .unwrap();
        let resumed = logins
            .callback("corp", &b.state, Some(&b.binding), 1)
            .unwrap();
        assert_eq!(resumed.secrets, Some(secrets));

        let code = logins.complete("corp", identity(), 0).unwrap();
        assert!(logins.exchange("github", &code, 1).is_none());
        assert!(logins.exchange("corp", &code, 1).is_none(), "spent");
    }

    #[test]
    fn logins_expire() {
        let logins = BrowserLogins::default();
        let a = logins
            .start("github", "https://portal.example/done", None, 0)
            .unwrap();
        assert_eq!(
            logins
                .callback("github", &a.state, Some(&a.binding), PENDING_TTL_MS)
                .err(),
            Some(CallbackError::UnknownState)
        );
        let code = logins.complete("github", identity(), 0).unwrap();
        assert!(logins.exchange("github", &code, COMPLETED_TTL_MS).is_none());
        let code = logins.complete("github", identity(), 0).unwrap();
        assert_eq!(logins.exchange("github", &code, 1), Some(identity()));
        assert!(
            logins.exchange("github", &code, 2).is_none(),
            "a login code is used once"
        );
    }
}
