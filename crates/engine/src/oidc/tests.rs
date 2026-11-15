use std::sync::OnceLock;
use std::time::Duration;

use super::*;

const URL: &str = "https://porch.example.com";

/// Generating RSA keys is slow, so the tests share two.
fn keys() -> &'static [Vec<u8>; 2] {
    static KEYS: OnceLock<[Vec<u8>; 2]> = OnceLock::new();
    KEYS.get_or_init(|| [generate_key().unwrap(), generate_key().unwrap()])
}

fn stored(i: usize, created_at: i64, retires_at: Option<i64>) -> StoredKey {
    StoredKey {
        pkcs8: keys()[i].clone(),
        created_at,
        retires_at,
    }
}

fn claims(lifetime: Duration) -> Claims {
    Claims {
        sub: "acme/aws-prod".into(),
        aud: "sts.amazonaws.com".into(),
        lifetime,
    }
}

#[test]
fn discovery_names_the_issuer_jwks_and_rs256() {
    let issuer = Issuer::new(&format!("{URL}/"), &[stored(0, 1, None)]).unwrap();
    let doc = issuer.discovery();
    assert_eq!(doc["issuer"], "https://porch.example.com/oidc");
    assert_eq!(doc["jwks_uri"], "https://porch.example.com/oidc/jwks");
    assert_eq!(
        doc["id_token_signing_alg_values_supported"],
        json!(["RS256"])
    );
    assert!(doc["response_types_supported"].is_array());
}

#[test]
fn tokens_verify_against_the_published_jwks() {
    let issuer = Issuer::new(URL, &[stored(0, 1, None)]).unwrap();
    let now = 1_800_000_000_000;
    let token = issuer.mint(&claims(MAX_TOKEN_LIFETIME), now).unwrap();
    let jwks = issuer.jwks(now);
    let got = verify(&token, &jwks, issuer.url(), now).unwrap();
    assert_eq!(got["sub"], "acme/aws-prod");
    assert_eq!(got["aud"], "sts.amazonaws.com");
    assert_eq!(got["iss"], "https://porch.example.com/oidc");
    assert_eq!(
        got["exp"].as_i64().unwrap() - got["iat"].as_i64().unwrap(),
        900
    );
    // Expired after its lifetime, and rejected by another issuer's keys.
    assert!(verify(&token, &jwks, issuer.url(), now + 900_000).is_err());
    let other = Issuer::new(URL, &[stored(1, 1, None)]).unwrap();
    assert!(verify(&token, &other.jwks(now), issuer.url(), now).is_err());
    // A tampered payload does not verify.
    let mut parts: Vec<String> = token.split('.').map(str::to_owned).collect();
    parts[1] = B64.encode(
        json!({"iss": issuer.url(), "sub": "acme/aws-dev", "nbf": 0, "exp": i64::MAX}).to_string(),
    );
    assert!(verify(&parts.join("."), &jwks, issuer.url(), now).is_err());
    // The JWKS holds public components only.
    for key in jwks["keys"].as_array().unwrap() {
        let mut names: Vec<&str> = key
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        names.sort_unstable();
        assert_eq!(names, ["alg", "e", "kid", "kty", "n", "use"]);
    }
}

#[test]
fn lifetimes_are_bounded() {
    let issuer = Issuer::new(URL, &[stored(0, 1, None)]).unwrap();
    for lifetime in [MAX_TOKEN_LIFETIME + Duration::from_secs(1), Duration::ZERO] {
        assert!(matches!(
            issuer.mint(&claims(lifetime), 0),
            Err(OidcError::Lifetime)
        ));
    }
}

#[test]
fn rotation_signs_with_the_new_key_and_publishes_both_until_the_grace_ends() {
    let issuer = Issuer::new(URL, &[stored(0, 1, None)]).unwrap();
    let old = issuer.signing_kid().unwrap();
    let before = issuer.mint(&claims(MAX_TOKEN_LIFETIME), 1_000).unwrap();
    issuer
        .replace(&[stored(0, 1, Some(10_000)), stored(1, 2, None)])
        .unwrap();
    let new = issuer.signing_kid().unwrap();
    assert_ne!(new, old);
    assert_eq!(issuer.kids(5_000), [new.clone(), old.clone()]);
    let after = issuer.mint(&claims(MAX_TOKEN_LIFETIME), 5_000).unwrap();
    // Tokens from before the rotation still verify during the grace period.
    let jwks = issuer.jwks(5_000);
    assert!(verify(&before, &jwks, issuer.url(), 5_000).is_ok());
    assert!(verify(&after, &jwks, issuer.url(), 5_000).is_ok());
    // Once it ends the old key is no longer published.
    assert_eq!(issuer.kids(10_000), [new]);
    assert!(verify(&before, &issuer.jwks(10_000), issuer.url(), 10_000).is_err());
}

#[test]
fn key_ids_are_jwk_thumbprints() {
    // RFC 7638 section 3.1.
    let n = B64
        .decode(
            "0vx7agoebGcQSuuPiLJXZptN9nndrQmbXEps2aiAFbWhM78LhWx4cbbfAAtVT86zwu1RK7aPFFxuhDR1L6\
             tSoc_BJECPebWKRXjBZCiFV4n3oknjhMstn64tZ_2W-5JsGY4Hc5n9yBXArwl93lqt7_RN5w6Cf0h4QyQ5\
             v-65YGjQR0_FDW2QvzqY368QQMicAtaSqzs8KJZgnYb9c7d0zgdAZHzu6qMQvRL5hajrn1n91CbOpbISD08\
             qNLyrdkt-bFTWhAI4vMQFh6WeZu0fM4lFd2NcRwr3XPksINHaQ-G_xBniIqbw0Ls1jF44-csFCur-kEgU8a\
             wapJzKnqDKgw",
        )
        .unwrap();
    let e = B64.decode("AQAB").unwrap();
    assert_eq!(
        thumbprint(&n, &e),
        "NzbLsXh8uDCcd-6MNwXF4W_7noWXFZAfHkxZsRGC9Xs"
    );
}
