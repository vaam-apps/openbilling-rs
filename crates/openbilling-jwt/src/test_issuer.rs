//! A test issuer: an OpenID provider's discovery document and JWKS on a loopback port, and the tokens it signs.
//!
//! **Test code**, behind the feature `test-issuer`, which no shipping binary enables. It holds three signing keys:
//! an `ES256` and an `EdDSA` key made fresh at start, and the `RS256` key of RFC 7515, appendix A.2 (published in
//! the RFC, so not a secret). Each is published in the JWKS under its `kid`. A fourth `ES256` key is made but never
//! published, to sign tokens whose `kid` the issuer's JWKS does not hold.

#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::missing_panics_doc
)]

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::rand::SystemRandom;
use ring::rsa::{KeyPair as RsaKeyPair, KeyPairComponents, PublicKeyComponents};
use ring::signature::{
    ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, Ed25519KeyPair, KeyPair, RSA_PKCS1_SHA256,
};
use serde_json::{Value, json};

/// The audience the issuer's tokens name by default.
pub const AUDIENCE: &str = "openbilling-test";

/// The `kid` of each key.
pub const KID_ES256: &str = "test-es256";
pub const KID_EDDSA: &str = "test-eddsa";
pub const KID_RS256: &str = "rfc7515-a2";
pub const KID_UNPUBLISHED: &str = "test-unpublished";

/// The RSA key of RFC 7515, appendix A.2, as its JWK gives it (base64url, no padding).
pub mod rfc7515_a2 {
    pub const N: &str = "ofgWCuLjybRlzo0tZWJjNiuSfb4p4fAkd_wWJcyQoTbji9k0l8W26mPddxHmfHQp-Vaw-4qPCJrcS2mJPMEzP1Pt0Bm4d4QlL-yRT-SFd2lZS-pCgNMsD1W_YpRPEwOWvG6b32690r2jZ47soMZo9wGzjb_7OMg0LOL-bSf63kpaSHSXndS5z5rexMdbBYUsLA9e-KXBdQOS-UTo7WTBEMa2R2CapHg665xsmtdVMTBQY4uDZlxvb3qCo5ZwKh9kG4LT6_I5IhlJH7aGhyxXFvUK-DWNmoudF8NAco9_h9iaGNj8q2ethFkMLs91kzk2PAcDTW9gb54h4FRWyuXpoQ";
    pub const E: &str = "AQAB";
    pub const D: &str = "Eq5xpGnNCivDflJsRQBXHx1hdR1k6Ulwe2JZD50LpXyWPEAeP88vLNO97IjlA7_GQ5sLKMgvfTeXZx9SE-7YwVol2NXOoAJe46sui395IW_GO-pWJ1O0BkTGoVEn2bKVRUCgu-GjBVaYLU6f3l9kJfFNS3E0QbVdxzubSu3Mkqzjkn439X0M_V51gfpRLI9JYanrC4D4qAdGcopV_0ZHHzQlBjudU2QvXt4ehNYTCBr6XCLQUShb1juUO1ZdiYoFaFQT5Tw8bGUl_x_jTj3ccPDVZFD9pIuhLhBOneufuBiB4cS98l2SR_RQyGWSeWjnczT0QU91p1DhOVRuOopznQ";
    pub const P: &str = "4BzEEOtIpmVdVEZNCqS7baC4crd0pqnRH_5IB3jw3bcxGn6QLvnEtfdUdiYrqBdss1l58BQ3KhooKeQTa9AB0Hw_Py5PJdTJNPY8cQn7ouZ2KKDcmnPGBY5t7yLc1QlQ5xHdwW1VhvKn-nXqhJTBgIPgtldC-KDV5z-y2XDwGUc";
    pub const Q: &str = "uQPEfgmVtjL0Uyyx88GZFF1fOunH3-7cepKmtH4pxhtCoHqpWmT8YAmZxaewHgHAjLYsp1ZSe7zFYHj7C6ul7TjeLQeZD_YwD66t62wDmpe_HlB-TnBA-njbglfIsRLtXlnDzQkv5dTltRJ11BKBBypeeF6689rjcJIDEz9RWdc";
    pub const DP: &str = "BwKfV3Akq5_MFZDFZCnW-wzl-CCo83WoZvnLQwCTeDv8uzluRSnm71I3QCLdhrqE2e9YkxvuxdBfpT_PI7Yz-FOKnu1R6HsJeDCjn12Sk3vmAktV2zb34MCdy7cpdTh_YVr7tss2u6vneTwrA86rZtu5Mbr1C1XsmvkxHQAdYo0";
    pub const DQ: &str = "h_96-mK1R_7glhsum81dZxjTnYynPbZpHziZjeeHcXYsXaaMwkOlODsWa7I9xXDoRwbKgB719rrmI2oKr6N3Do9U0ajaHF-NKJnwgjMd2w9cjz3_-kyNlxAr2v4IKhGNpmM5iIgOS1VZnOZ68m6_pbLBSp3nssTdlqvd0tIiTHU";
    pub const QI: &str = "IYd7DHOhrWvxkwPQsRM2tOgrjbcrfvtQJipd-DlcxyVuuM9sQLdgjVk2oy26F0EmpScGLq2MowX7fhd_QJQ3ydy5cY7YIBi87w93IKLEdfnbJtoOPLUW0ITrJReOgo1cq9SbsxYawBgfp_gh6A5603k2-ZQwVK0JKSHuLFkuQ3U";
    /// The example's JWS: header `{"alg":"RS256"}`, its payload, and the RFC's signature over them.
    pub const JWS: &str = "eyJhbGciOiJSUzI1NiJ9.eyJpc3MiOiJqb2UiLA0KICJleHAiOjEzMDA4MTkzODAsDQogImh0dHA6Ly9leGFtcGxlLmNvbS9pc19yb290Ijp0cnVlfQ.cC4hiUPoj9Eetdgtv3hF80EGrhuB__dzERat0XF9g2VtQgr9PJbu3XOiZj5RZmh7AAuHIm4Bh-0Qc_lF5YKt_O8W2Fp5jujGbds9uJdbF9CUAr7t1dnZcAcQjbKBYNX4BAynRFdiuB--f_nZLgrnbyTyWzO75vRK5h6xBArLIARNPvkSjtQBMHlb1L07Qe7K0GarZRmB_eSN9383LcOLn6_dO--xi12jzDwusC-eOkHWEsqtFZESc6BfI7noOPqvhJ1phCnvWh6IeYI2w9QOYEUipUTI8np6LbgGY9Fs98rqVt5AXLIhWkWywlVmtVrBp0igcN_IoypGlUPQGe77Rw";
}

/// The Ed25519 example of RFC 8037, appendix A.4: its public key and its JWS.
pub mod rfc8037_a4 {
    pub const X: &str = "11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo";
    pub const JWS: &str = "eyJhbGciOiJFZERTQSJ9.RXhhbXBsZSBvZiBFZDI1NTE5IHNpZ25pbmc.hgyY0il_MGCjP0JzlnLWG1PPOt7-09PGcvMg3AIbQR6dWbhijcNR4ki4iylGjg5BhVsPt9g7sVvpAr_MuM0KAg";
}

/// Which key signs a token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signer {
    Es256,
    EdDsa,
    Rs256,
    /// An `ES256` key whose `kid` the JWKS does not hold.
    Unpublished,
}

fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

fn decode(text: &str) -> Vec<u8> {
    URL_SAFE_NO_PAD.decode(text).unwrap()
}

/// Seconds since the epoch, now.
#[must_use]
pub fn now_secs() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap()
}

struct Keys {
    es256: EcdsaKeyPair,
    eddsa: Ed25519KeyPair,
    rs256: RsaKeyPair,
    unpublished: EcdsaKeyPair,
    rng: SystemRandom,
}

#[derive(Default)]
struct Served {
    /// How many times the JWKS was fetched.
    jwks_fetches: AtomicUsize,
    /// When set, discovery and the JWKS answer `503`.
    down: AtomicBool,
    /// The JWKS as served.
    jwks: Mutex<Value>,
    /// The discovery document as served.
    discovery: Mutex<Value>,
}

/// A running test issuer.
pub struct TestIssuer {
    /// Its issuer identifier, `http://127.0.0.1:<port>`: what `iss` holds and what the verifier is configured with.
    pub issuer: String,
    keys: Arc<Keys>,
    served: Arc<Served>,
}

async fn discovery(State(served): State<Arc<Served>>) -> Response {
    if served.down.load(Ordering::SeqCst) {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    axum::Json(served.discovery.lock().unwrap().clone()).into_response()
}

async fn jwks(State(served): State<Arc<Served>>) -> Response {
    if served.down.load(Ordering::SeqCst) {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    served.jwks_fetches.fetch_add(1, Ordering::SeqCst);
    axum::Json(served.jwks.lock().unwrap().clone()).into_response()
}

fn ec_jwk(kid: &str, key: &EcdsaKeyPair) -> Value {
    let point = key.public_key().as_ref();
    json!({"kty": "EC", "crv": "P-256", "kid": kid, "use": "sig", "alg": "ES256",
           "x": b64(&point[1..33]), "y": b64(&point[33..65])})
}

impl TestIssuer {
    /// Starts an issuer on a free loopback port.
    pub async fn start() -> Self {
        let rng = SystemRandom::new();
        let ec = |rng: &SystemRandom| {
            let pkcs8 =
                EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, rng).unwrap();
            EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), rng).unwrap()
        };
        let es256 = ec(&rng);
        let unpublished = ec(&rng);
        let eddsa =
            Ed25519KeyPair::from_pkcs8(Ed25519KeyPair::generate_pkcs8(&rng).unwrap().as_ref())
                .unwrap();
        use rfc7515_a2 as a2;
        let rs256 = RsaKeyPair::from_components(&KeyPairComponents {
            public_key: PublicKeyComponents {
                n: decode(a2::N),
                e: decode(a2::E),
            },
            d: decode(a2::D),
            p: decode(a2::P),
            q: decode(a2::Q),
            dP: decode(a2::DP),
            dQ: decode(a2::DQ),
            qInv: decode(a2::QI),
        })
        .unwrap();
        let keys = Keys {
            es256,
            eddsa,
            rs256,
            unpublished,
            rng,
        };
        let served = Arc::new(Served::default());
        *served.jwks.lock().unwrap() = json!({"keys": [
            ec_jwk(KID_ES256, &keys.es256),
            {"kty": "OKP", "crv": "Ed25519", "kid": KID_EDDSA, "use": "sig",
             "x": b64(keys.eddsa.public_key().as_ref())},
            {"kty": "RSA", "kid": KID_RS256, "use": "sig", "alg": "RS256", "n": a2::N, "e": a2::E},
        ]});
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let issuer = format!("http://{address}");
        *served.discovery.lock().unwrap() = json!({
            "issuer": issuer, "jwks_uri": format!("{issuer}/jwks"),
            "id_token_signing_alg_values_supported": ["ES256", "EdDSA", "RS256"],
        });
        let app = Router::new()
            .route("/.well-known/openid-configuration", get(discovery))
            .route("/jwks", get(jwks))
            .with_state(Arc::clone(&served));
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            issuer,
            keys: Arc::new(keys),
            served,
        }
    }

    /// How many times the JWKS was fetched.
    #[must_use]
    pub fn jwks_fetches(&self) -> usize {
        self.served.jwks_fetches.load(Ordering::SeqCst)
    }

    /// Makes discovery and the JWKS answer `503` (`true`) or serve again (`false`).
    pub fn set_down(&self, down: bool) {
        self.served.down.store(down, Ordering::SeqCst);
    }

    /// Publishes the unpublished key too, as an issuer does when it rotates in a new key.
    pub fn publish_unpublished_key(&self) {
        let jwk = ec_jwk(KID_UNPUBLISHED, &self.keys.unpublished);
        let mut jwks = self.served.jwks.lock().unwrap();
        jwks["keys"].as_array_mut().unwrap().push(jwk);
    }

    /// The `kid` and `alg` a signer stands for.
    #[must_use]
    pub const fn names(signer: Signer) -> (&'static str, &'static str) {
        match signer {
            Signer::Es256 => (KID_ES256, "ES256"),
            Signer::EdDsa => (KID_EDDSA, "EdDSA"),
            Signer::Rs256 => (KID_RS256, "RS256"),
            Signer::Unpublished => (KID_UNPUBLISHED, "ES256"),
        }
    }

    /// The compact JWS of `header` and `claims`, signed by `signer` whatever the header says.
    #[must_use]
    pub fn sign(&self, signer: Signer, header: &Value, claims: &Value) -> String {
        let input = format!(
            "{}.{}",
            b64(&serde_json::to_vec(header).unwrap()),
            b64(&serde_json::to_vec(claims).unwrap())
        );
        let keys = &self.keys;
        let signature = match signer {
            Signer::Es256 => keys
                .es256
                .sign(&keys.rng, input.as_bytes())
                .unwrap()
                .as_ref()
                .to_vec(),
            Signer::Unpublished => keys
                .unpublished
                .sign(&keys.rng, input.as_bytes())
                .unwrap()
                .as_ref()
                .to_vec(),
            Signer::EdDsa => keys.eddsa.sign(input.as_bytes()).as_ref().to_vec(),
            Signer::Rs256 => {
                let mut out = vec![0; keys.rs256.public().modulus_len()];
                keys.rs256
                    .sign(&RSA_PKCS1_SHA256, &keys.rng, input.as_bytes(), &mut out)
                    .unwrap();
                out
            }
        };
        format!("{input}.{}", b64(&signature))
    }

    /// The header a signer's tokens carry: its `alg`, its `kid`, `typ` `JWT`.
    #[must_use]
    pub fn header(signer: Signer) -> Value {
        let (kid, alg) = Self::names(signer);
        json!({"alg": alg, "kid": kid, "typ": "JWT"})
    }

    /// The claims of a good token for `sub` with `roles`: this issuer, [`AUDIENCE`], five minutes to live.
    #[must_use]
    pub fn claims(&self, sub: &str, roles: &[&str]) -> Value {
        let now = now_secs();
        json!({"iss": self.issuer, "aud": AUDIENCE, "sub": sub, "roles": roles,
               "iat": now, "nbf": now, "exp": now + 300})
    }

    /// A good `ES256` token for `sub` with `roles`.
    #[must_use]
    pub fn token(&self, sub: &str, roles: &[&str]) -> String {
        self.sign(
            Signer::Es256,
            &Self::header(Signer::Es256),
            &self.claims(sub, roles),
        )
    }
}
