//! PKCE (RFC 7636) helpers for the authorization-code flow.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::RngCore;
use sha2::{Digest, Sha256};

/// Random input for a PKCE code verifier: 32 bytes encode to 43 base64url
/// characters (the minimum length allowed by RFC 7636).
const VERIFIER_BYTES: usize = 32;

/// Generates a cryptographically random PKCE code verifier (43 characters
/// from the base64url alphabet without padding).
pub fn generate_verifier() -> String {
    random_urlsafe(VERIFIER_BYTES)
}

/// Computes the S256 code challenge for a PKCE code verifier:
/// `BASE64URL(SHA256(verifier))` without padding.
pub fn s256(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(digest)
}

/// A PKCE pair: the secret [`Pkce::verifier`] travels with the token request,
/// the derived [`Pkce::challenge`] travels with the authorization request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pkce {
    /// Code verifier; must stay client-side until the token exchange.
    pub verifier: String,
    /// S256 code challenge derived from the verifier.
    pub challenge: String,
}

impl Pkce {
    /// Creates a fresh random verifier/challenge pair.
    pub fn new() -> Self {
        let verifier = generate_verifier();
        Self {
            challenge: s256(&verifier),
            verifier,
        }
    }
}

impl Default for Pkce {
    fn default() -> Self {
        Self::new()
    }
}

/// Returns a random URL-safe string (base64url, no padding) over `n` random
/// bytes. Also used for the OAuth `state` parameter.
pub(crate) fn random_urlsafe(n: usize) -> String {
    let mut bytes = vec![0u8; n];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test vector from RFC 7636, appendix B.
    #[test]
    fn s256_matches_rfc7636_vector() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let expected = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        assert_eq!(s256(verifier), expected);
    }

    #[test]
    fn verifier_is_valid_and_random() {
        let a = generate_verifier();
        let b = generate_verifier();
        assert_eq!(a.len(), 43);
        assert_ne!(a, b);
        assert!(
            a.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
    }

    #[test]
    fn pkce_pair_is_consistent() {
        let pkce = Pkce::new();
        assert_eq!(pkce.challenge, s256(&pkce.verifier));
        // `default` must behave like `new`: a fresh, consistent pair.
        let other = Pkce::default();
        assert_eq!(other.challenge, s256(&other.verifier));
        assert_ne!(pkce.verifier, other.verifier);
    }
}
