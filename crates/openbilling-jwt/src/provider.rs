//! jsonwebtoken's crypto, on ring.
//!
//! jsonwebtoken 11 checks the claims and leaves signatures to a process-wide [`CryptoProvider`]. Its own two
//! backends are aws-lc-rs and the RustCrypto crates; this workspace keeps ring (rustls's provider here) and nothing
//! else, so this module is the provider: it verifies `RS256`, `ES256` and `EdDSA` (Ed25519) with ring, and refuses
//! every other algorithm, `HS*` among them. It signs nothing.

use jsonwebtoken::crypto::{CryptoProvider, JwtSigner, JwtVerifier, KeyUtils};
use jsonwebtoken::errors::{ErrorKind, Result};
use jsonwebtoken::jwk::{EllipticCurve, ThumbprintHash};
use jsonwebtoken::signature::{self, Verifier};
use jsonwebtoken::{Algorithm, AlgorithmFamily, DecodingKey, DecodingKeyKind, EncodingKey};
use ring::signature::{
    ECDSA_P256_SHA256_FIXED, ED25519, RSA_PKCS1_2048_8192_SHA256, RsaPublicKeyComponents,
    UnparsedPublicKey,
};

/// The algorithms a token may be signed with. Nothing else is ever verified.
pub const ALGORITHMS: [Algorithm; 3] = [Algorithm::RS256, Algorithm::ES256, Algorithm::EdDSA];

/// The length of an uncompressed P-256 point: `0x04`, then x and y of 32 bytes each.
const P256_POINT_LEN: usize = 65;
/// The length of an Ed25519 public key.
const ED25519_KEY_LEN: usize = 32;

enum Material {
    Rsa { n: Vec<u8>, e: Vec<u8> },
    P256(Vec<u8>),
    Ed25519(Vec<u8>),
}

struct RingVerifier {
    algorithm: Algorithm,
    material: Material,
}

impl Verifier<Vec<u8>> for RingVerifier {
    fn verify(&self, message: &[u8], sig: &Vec<u8>) -> std::result::Result<(), signature::Error> {
        let verified = match &self.material {
            Material::Rsa { n, e } => RsaPublicKeyComponents {
                n: n.as_slice(),
                e: e.as_slice(),
            }
            .verify(&RSA_PKCS1_2048_8192_SHA256, message, sig),
            Material::P256(point) => {
                UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, point).verify(message, sig)
            }
            Material::Ed25519(key) => UnparsedPublicKey::new(&ED25519, key).verify(message, sig),
        };
        verified.map_err(|_| signature::Error::new())
    }
}

impl JwtVerifier for RingVerifier {
    fn algorithm(&self) -> Algorithm {
        self.algorithm
    }
}

fn refused() -> jsonwebtoken::errors::Error {
    ErrorKind::InvalidAlgorithm.into()
}

/// The verifier of `algorithm` over `key`, or `InvalidAlgorithm` when the pair is not one of [`ALGORITHMS`] with a
/// key of its own family and shape.
fn verifier(algorithm: &Algorithm, key: &DecodingKey) -> Result<Box<dyn JwtVerifier>> {
    let material = match (algorithm, key.family(), key.kind()) {
        (Algorithm::RS256, AlgorithmFamily::Rsa, DecodingKeyKind::RsaModulusExponent { n, e }) => {
            Material::Rsa {
                n: n.clone(),
                e: e.clone(),
            }
        }
        (Algorithm::ES256, AlgorithmFamily::Ec, DecodingKeyKind::SecretOrDer(point))
            if point.len() == P256_POINT_LEN =>
        {
            Material::P256(point.clone())
        }
        (Algorithm::EdDSA, AlgorithmFamily::Ed, DecodingKeyKind::SecretOrDer(key))
            if key.len() == ED25519_KEY_LEN =>
        {
            Material::Ed25519(key.clone())
        }
        _ => return Err(refused()),
    };
    Ok(Box::new(RingVerifier {
        algorithm: *algorithm,
        material,
    }))
}

fn signer(_: &Algorithm, _: &EncodingKey) -> Result<Box<dyn JwtSigner>> {
    Err(refused())
}

fn no_rsa_components(_: &[u8]) -> Result<(Vec<u8>, Vec<u8>)> {
    Err(ErrorKind::InvalidKeyFormat.into())
}

fn no_ec_components(_: &[u8], _: Algorithm) -> Result<(EllipticCurve, Vec<u8>, Vec<u8>)> {
    Err(ErrorKind::InvalidKeyFormat.into())
}

fn no_ed_components(_: &[u8], _: &EllipticCurve) -> Result<Vec<u8>> {
    Err(ErrorKind::InvalidKeyFormat.into())
}

fn digest(data: &[u8], hash: ThumbprintHash) -> Result<Vec<u8>> {
    let algorithm = match hash {
        ThumbprintHash::SHA256 => &ring::digest::SHA256,
        ThumbprintHash::SHA384 => &ring::digest::SHA384,
        ThumbprintHash::SHA512 => &ring::digest::SHA512,
        _ => return Err(ErrorKind::InvalidAlgorithm.into()),
    };
    Ok(ring::digest::digest(algorithm, data).as_ref().to_vec())
}

/// The provider. Its key utilities answer an error rather than jsonwebtoken's default panic; none is used here.
static PROVIDER: CryptoProvider = CryptoProvider {
    signer_factory: signer,
    verifier_factory: verifier,
    key_utils: KeyUtils {
        rsa_pub_components_from_private_key: no_rsa_components,
        rsa_pub_components_from_public_key: no_rsa_components,
        ec_pub_components_from_private_key: no_ec_components,
        ed_pub_components_from_private_key: no_ed_components,
        compute_digest: digest,
    },
};

/// Installs the provider process-wide. `false` when another provider was installed first: then nothing here is
/// sure of what verifies a signature, and the verifier refuses to start.
pub fn install() -> bool {
    match PROVIDER.install_default() {
        Ok(()) => true,
        Err(existing) => std::ptr::eq(existing, &PROVIDER),
    }
}
