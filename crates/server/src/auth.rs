//! Credentials: API tokens (random, stored as SHA-256 hashes) and signed sessions.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

/// Prefix of API tokens.
pub const TOKEN_PREFIX: &str = "opt_";
/// Prefix of session tokens.
pub const SESSION_PREFIX: &str = "ops_";
/// Session lifetime.
pub const SESSION_TTL_MS: i64 = 24 * 60 * 60 * 1000;

const SESSION_CONTEXT: &[u8] = b"openporch-session-v1.";

/// Prefix of invitation codes.
pub const INVITATION_PREFIX: &str = "opi_";

/// A new random API token with 256 bits of entropy.
pub fn new_token() -> Zeroizing<String> {
    new_secret(TOKEN_PREFIX)
}

/// A new random one-time invitation code with 256 bits of entropy.
pub fn new_invitation_code() -> Zeroizing<String> {
    new_secret(INVITATION_PREFIX)
}

fn new_secret(prefix: &str) -> Zeroizing<String> {
    let mut bytes = Zeroizing::new([0u8; 32]);
    rand::thread_rng().fill_bytes(bytes.as_mut());
    Zeroizing::new(format!("{prefix}{}", B64.encode(bytes.as_ref())))
}

/// The stored hash of a token or one-time code.
pub fn hash_token(token: &str) -> String {
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Session claims.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claims {
    /// Principal ID.
    pub sub: String,
    /// Organization.
    pub org: String,
    /// Session ID, for revocation.
    pub jti: String,
    /// Issued at, Unix milliseconds.
    pub iat: i64,
    /// Expiry, Unix milliseconds.
    pub exp: i64,
}

/// Signs and verifies session tokens with an Ed25519 key.
pub struct SessionSigner {
    key: SigningKey,
}

impl std::fmt::Debug for SessionSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SessionSigner")
    }
}

impl SessionSigner {
    /// A signer from a 32-byte seed.
    pub fn from_seed(seed: &[u8; 32]) -> Self {
        Self {
            key: SigningKey::from_bytes(seed),
        }
    }

    /// A new random seed.
    pub fn generate_seed() -> Zeroizing<[u8; 32]> {
        let mut seed = Zeroizing::new([0u8; 32]);
        rand::thread_rng().fill_bytes(seed.as_mut());
        seed
    }

    /// Issues a token for `claims`.
    pub fn sign(&self, claims: &Claims) -> String {
        let payload = B64.encode(serde_json::to_vec(claims).expect("claims serialize"));
        let mut msg = SESSION_CONTEXT.to_vec();
        msg.extend_from_slice(payload.as_bytes());
        let sig = self.key.sign(&msg);
        format!("{SESSION_PREFIX}{payload}.{}", B64.encode(sig.to_bytes()))
    }

    /// Verifies a token's signature and expiry. Revocation and membership are checked
    /// separately, on every request.
    pub fn verify(&self, token: &str, now: i64) -> Option<Claims> {
        let rest = token.strip_prefix(SESSION_PREFIX)?;
        let (payload, sig) = rest.split_once('.')?;
        let sig: [u8; 64] = B64.decode(sig).ok()?.try_into().ok()?;
        let mut msg = SESSION_CONTEXT.to_vec();
        msg.extend_from_slice(payload.as_bytes());
        let verifying: VerifyingKey = self.key.verifying_key();
        verifying.verify(&msg, &Signature::from_bytes(&sig)).ok()?;
        let claims: Claims = serde_json::from_slice(&B64.decode(payload).ok()?).ok()?;
        (claims.exp > now).then_some(claims)
    }
}

/// A random hex identifier.
pub fn random_id(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::thread_rng().fill_bytes(&mut buf);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claims(exp: i64) -> Claims {
        Claims {
            sub: "alice".into(),
            org: "acme".into(),
            jti: "j1".into(),
            iat: 0,
            exp,
        }
    }

    #[test]
    fn sessions_verify_and_expire() {
        let signer = SessionSigner::from_seed(&[7; 32]);
        let token = signer.sign(&claims(1000));
        assert_eq!(signer.verify(&token, 999), Some(claims(1000)));
        assert_eq!(signer.verify(&token, 1000), None);
        // Another key, a changed payload or a changed signature fail.
        assert_eq!(SessionSigner::from_seed(&[8; 32]).verify(&token, 0), None);
        let forged = signer.sign(&Claims {
            sub: "mallory".into(),
            ..claims(1000)
        });
        let (_, forged_payload) = forged.split_once('_').unwrap();
        let (_, sig) = token.rsplit_once('.').unwrap();
        let (payload, _) = forged_payload.split_once('.').unwrap();
        assert_eq!(
            signer.verify(&format!("{SESSION_PREFIX}{payload}.{sig}"), 0),
            None
        );
        assert_eq!(signer.verify("ops_garbage", 0), None);
    }

    #[test]
    fn tokens_are_random_and_hashed() {
        let a = new_token();
        let b = new_token();
        assert_ne!(*a, *b);
        assert!(a.starts_with(TOKEN_PREFIX));
        assert_eq!(a.len(), TOKEN_PREFIX.len() + 43);
        assert_eq!(hash_token(&a).len(), 64);
        assert_ne!(hash_token(&a), hash_token(&b));
    }
}
