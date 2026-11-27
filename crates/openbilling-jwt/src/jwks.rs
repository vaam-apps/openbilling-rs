//! The issuer's keys: its discovery document, its JWKS, and what of it the verifier will use.
//!
//! Only signature keys with a `kid` are kept, of three shapes: RSA (for `RS256`), EC on P-256 (`ES256`) and OKP on
//! Ed25519 (`EdDSA`). A key whose `alg` names another algorithm, whose `use` is not `sig`, or whose material does not
//! decode is left out, never guessed at. A JWKS with none left is a JWKS with no key: every token is then refused.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::{Algorithm, DecodingKey};
use serde::Deserialize;
use url::Url;

/// The most a discovery document or a JWKS may weigh.
pub const MAX_DOCUMENT_BYTES: usize = 256 * 1024;

/// One usable key.
#[derive(Clone)]
pub struct Key {
    /// Its `kid`.
    pub kid: String,
    /// The one algorithm it verifies.
    pub algorithm: Algorithm,
    /// Its material.
    pub decoding: DecodingKey,
}

impl std::fmt::Debug for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Key")
            .field("kid", &self.kid)
            .field("algorithm", &self.algorithm)
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct Document {
    keys: Vec<serde_json::Value>,
}

#[derive(Deserialize)]
struct Jwk {
    kty: String,
    kid: Option<String>,
    #[serde(rename = "use")]
    usage: Option<String>,
    alg: Option<String>,
    crv: Option<String>,
    n: Option<String>,
    e: Option<String>,
    x: Option<String>,
    y: Option<String>,
}

fn decoded_len(value: &str) -> Option<usize> {
    URL_SAFE_NO_PAD.decode(value).ok().map(|bytes| bytes.len())
}

fn key(jwk: Jwk) -> Option<Key> {
    let kid = jwk.kid.filter(|kid| !kid.is_empty())?;
    if jwk.usage.as_deref().is_some_and(|usage| usage != "sig") {
        return None;
    }
    let (algorithm, decoding) = match (jwk.kty.as_str(), jwk.crv.as_deref()) {
        ("RSA", None) => {
            let (n, e) = (jwk.n?, jwk.e?);
            // ring verifies moduli of 2048 to 8192 bits only; a shorter one is refused here, not at the first token.
            if !(256..=1024).contains(&decoded_len(&n)?) || decoded_len(&e)? == 0 {
                return None;
            }
            (
                Algorithm::RS256,
                DecodingKey::from_rsa_components(&n, &e).ok()?,
            )
        }
        ("EC", Some("P-256")) => {
            let (x, y) = (jwk.x?, jwk.y?);
            if decoded_len(&x)? != 32 || decoded_len(&y)? != 32 {
                return None;
            }
            (
                Algorithm::ES256,
                DecodingKey::from_ec_components(&x, &y).ok()?,
            )
        }
        ("OKP", Some("Ed25519")) => {
            let x = jwk.x?;
            if decoded_len(&x)? != 32 {
                return None;
            }
            (Algorithm::EdDSA, DecodingKey::from_ed_components(&x).ok()?)
        }
        _ => return None,
    };
    let named = match algorithm {
        Algorithm::RS256 => "RS256",
        Algorithm::ES256 => "ES256",
        _ => "EdDSA",
    };
    if jwk.alg.as_deref().is_some_and(|alg| alg != named) {
        return None;
    }
    Some(Key {
        kid,
        algorithm,
        decoding,
    })
}

/// The usable keys of a JWKS document, or `None` when it is not a JWKS.
#[must_use]
pub fn parse(bytes: &[u8]) -> Option<Vec<Key>> {
    let document: Document = serde_json::from_slice(bytes).ok()?;
    Some(
        document
            .keys
            .into_iter()
            .filter_map(|value| serde_json::from_value::<Jwk>(value).ok())
            .filter_map(key)
            .collect(),
    )
}

#[derive(Deserialize)]
struct Discovery {
    issuer: String,
    jwks_uri: String,
}

/// The `jwks_uri` of an OpenID Provider's discovery document, if the document names exactly `issuer`.
#[must_use]
pub fn jwks_uri(bytes: &[u8], issuer: &str) -> Option<Url> {
    let discovery: Discovery = serde_json::from_slice(bytes).ok()?;
    if discovery.issuer != issuer {
        return None;
    }
    checked_url(&discovery.jwks_uri)
}

/// Where the discovery document of `issuer` is (OpenID Connect Discovery 1.0, section 4).
#[must_use]
pub fn discovery_url(issuer: &Url) -> Option<Url> {
    let base = issuer.as_str().trim_end_matches('/');
    Url::parse(&format!("{base}/.well-known/openid-configuration")).ok()
}

/// `raw` as a URL the verifier may fetch: `https`, or `http` to a loopback host only; no credentials, no fragment.
#[must_use]
pub fn checked_url(raw: &str) -> Option<Url> {
    let url = Url::parse(raw).ok()?;
    let loopback = match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        Some(url::Host::Domain(name)) => name == "localhost",
        None => false,
    };
    let scheme_ok = url.scheme() == "https" || (url.scheme() == "http" && loopback);
    let clean = url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none()
        && !url.cannot_be_a_base();
    (scheme_ok && clean).then_some(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_signature_keys_of_the_three_shapes_are_kept() {
        let x = URL_SAFE_NO_PAD.encode([7u8; 32]);
        let short = URL_SAFE_NO_PAD.encode([7u8; 31]);
        let document = serde_json::json!({"keys": [
            {"kty": "OKP", "crv": "Ed25519", "kid": "ed", "x": x},
            {"kty": "OKP", "crv": "Ed25519", "x": x},
            {"kty": "OKP", "crv": "Ed25519", "kid": "enc", "use": "enc", "x": x},
            {"kty": "OKP", "crv": "Ed25519", "kid": "short", "x": short},
            {"kty": "OKP", "crv": "Ed25519", "kid": "mislabelled", "alg": "ES256", "x": x},
            {"kty": "OKP", "crv": "X25519", "kid": "x25519", "x": x},
            {"kty": "oct", "kid": "hmac", "k": x},
            {"kty": "EC", "crv": "P-384", "kid": "p384", "x": x, "y": x},
            {"kty": "RSA", "kid": "small", "n": URL_SAFE_NO_PAD.encode([1u8; 128]), "e": "AQAB"},
            "not a key"
        ]});
        let keys = parse(&serde_json::to_vec(&document).unwrap()).unwrap();
        let kids: Vec<&str> = keys.iter().map(|k| k.kid.as_str()).collect();
        assert_eq!(kids, ["ed"]);
        assert!(parse(b"[]").is_none());
        assert!(parse(br#"{"keys": []}"#).unwrap().is_empty());
    }

    #[test]
    fn urls_are_https_or_loopback_and_carry_no_credentials() {
        assert!(checked_url("https://idp.example/realms/a").is_some());
        assert!(checked_url("http://127.0.0.1:8080/x").is_some());
        assert!(checked_url("http://localhost/x").is_some());
        assert!(checked_url("http://[::1]/x").is_some());
        for bad in [
            "http://idp.example/x",
            "https://user:pw@idp.example/x",
            "https://idp.example/x#frag",
            "ftp://idp.example/x",
            "not a url",
        ] {
            assert!(checked_url(bad).is_none(), "{bad}");
        }
        let issuer = Url::parse("https://idp.example/realms/a/").unwrap();
        assert_eq!(
            discovery_url(&issuer).unwrap().as_str(),
            "https://idp.example/realms/a/.well-known/openid-configuration"
        );
    }

    #[test]
    fn discovery_must_name_the_issuer_exactly() {
        let doc = br#"{"issuer": "https://idp.example", "jwks_uri": "https://idp.example/keys"}"#;
        assert!(jwks_uri(doc, "https://idp.example").is_some());
        assert!(jwks_uri(doc, "https://idp.example/").is_none());
        let plain = br#"{"issuer": "https://idp.example", "jwks_uri": "http://idp.example/keys"}"#;
        assert!(jwks_uri(plain, "https://idp.example").is_none());
    }
}
